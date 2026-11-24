//! 声音:截 WASAPI 共享模式的输出。
//!
//! 钩 `IAudioClient::Initialize`、`IAudioClient3::InitializeSharedAudioStream`、`GetService`,
//! 以及 `IAudioRenderClient::GetBuffer`、`ReleaseBuffer`。COM 方法没有导出名,所以装之前先在
//! 本进程里建一个临时的共享模式流,从它的虚表取出函数地址;同一个类的实例共用这些函数,
//! 之后任何代码建的输出流都会经过替身。
//!
//! `ReleaseBuffer` 里把这次写的 PCM 抄进共享内存的环(agent 每 10 ms 取一次),再给原调用
//! 加上 `AUDCLNT_BUFFERFLAGS_SILENT`,主机扬声器就不出这个实例的声音。agent 的心跳过期
//! (没在收,或者崩了)时什么都不做,声音照常走主机扬声器。
//!
//! 注入之前就建好的流,hook 没见过它的 `Initialize`,只能按默认设备的混音格式去猜,猜错了就会
//! 按错的每帧字节数读缓冲区。所以这样的流每个包抄之前都要验:数据区在可读内存里、放得下猜的
//! 长度,样本按 f32 看像声音。验不过就放弃这一路:往环里写一个放弃包通知 agent,之后这一路的
//! `ReleaseBuffer` 原样放行(不截,不加静音),主机照常出声。已知格式的流不验。
//!
//! 替身是 `extern "system"`,panic 会直接 abort 整个目标进程,所以这里不用 unwrap 和越界下标。
//! 热路径(`GetBuffer`、`ReleaseBuffer`)不分配内存、不打日志;放弃一路的那一刻例外,每路只有一次。

use core::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

use hook_abi::{
    AUDIO_F32, AUDIO_I16, AUDIO_I24, AUDIO_I32, AUDIO_PACKET_ABANDONED, AUDIO_PACKET_SILENT,
    AUDIO_U8, AudioHeader, AudioRing,
};
use windows::Win32::Foundation::{E_FAIL, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
    VirtualQuery,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::core::{GUID, HRESULT, Interface};

use super::Hook;
use crate::{debug_log, state};

type InitializeFn = unsafe extern "system" fn(
    *mut c_void,
    i32,
    u32,
    i64,
    i64,
    *const WAVEFORMATEX,
    *const GUID,
) -> HRESULT;
type InitializeSharedFn =
    unsafe extern "system" fn(*mut c_void, u32, u32, *const WAVEFORMATEX, *const GUID) -> HRESULT;
type GetServiceFn =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type GetBufferFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HRESULT;
type ReleaseBufferFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;

static INITIALIZE: Hook<InitializeFn> = Hook::new();
static INITIALIZE_SHARED: Hook<InitializeSharedFn> = Hook::new();
static GET_SERVICE: Hook<GetServiceFn> = Hook::new();
static GET_BUFFER: Hook<GetBufferFn> = Hook::new();
static RELEASE_BUFFER: Hook<ReleaseBufferFn> = Hook::new();

/// 没有默认输出设备时 `GetDefaultAudioEndpoint` 的返回值,即 HRESULT_FROM_WIN32(ERROR_NOT_FOUND)
const E_NOTFOUND: HRESULT = HRESULT(0x8007_0490_u32 as i32);

// windows crate 里这几个常量分散在 KernelStreaming 和 Multimedia 两个大模块,自己定义省得多编
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// 进程内最多同时记这么多个 client 和 render,满了替换最旧的
const TABLE_SIZE: usize = 64;

/// 一个输出流的格式
#[derive(Clone, Copy)]
struct Fmt {
    rate: u32,
    channels: u16,
    /// `hook_abi::AUDIO_*`,0 表示不支持:这样的流不截也不静音
    format: u16,
    /// 每帧字节数
    frame_bytes: u32,
}

impl Fmt {
    const NONE: Fmt = Fmt {
        rate: 0,
        channels: 0,
        format: 0,
        frame_bytes: 0,
    };
}

#[derive(Clone, Copy)]
struct ClientEntry {
    /// `IAudioClient` 指针,0 表示空槽
    key: usize,
    fmt: Fmt,
    stamp: u64,
}

#[derive(Clone, Copy)]
struct RenderEntry {
    /// `IAudioRenderClient` 指针,0 表示空槽
    key: usize,
    stream: u32,
    fmt: Fmt,
    /// 没见过它的 Initialize,`fmt` 是按后备格式猜的:每个包抄之前都要验
    guess: bool,
    /// 验不过,这一路放弃了:`ReleaseBuffer` 原样放行,不截也不静音
    abandoned: bool,
    /// 数据指针最近查到的可读内存区域 `[起, 止)`,(0, 0) 是还没查;只有猜格式的流用
    region: (usize, usize),
    /// 最近一次 `GetBuffer` 给出的数据指针,用完清零
    data: usize,
    /// 那次 `GetBuffer` 要的帧数,`ReleaseBuffer` 不许超过
    requested: u32,
    stamp: u64,
}

impl RenderEntry {
    const EMPTY: RenderEntry = RenderEntry {
        key: 0,
        stream: 0,
        fmt: Fmt::NONE,
        guess: false,
        abandoned: false,
        region: (0, 0),
        data: 0,
        requested: 0,
        stamp: 0,
    };
}

/// 进程内的表。环形缓冲的写入也在这把锁里,所以锁同时保证了环只有一个写者
struct Table {
    clients: [ClientEntry; TABLE_SIZE],
    renders: [RenderEntry; TABLE_SIZE],
    stamp: u64,
    /// 下一个流编号,从 1 开始
    next_stream: u32,
    /// 注入之前就建好的流:没见过它的 Initialize,只能猜它用的是默认设备的混音格式。
    /// 只会是 f32 或 `Fmt::NONE`(见 `install`):别的格式没法检查猜得对不对,这样的流不截
    fallback: Fmt,
}

static TABLE: Mutex<Table> = Mutex::new(Table::new());

fn table() -> MutexGuard<'static, Table> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

impl Table {
    const fn new() -> Self {
        Self {
            clients: [ClientEntry {
                key: 0,
                fmt: Fmt::NONE,
                stamp: 0,
            }; TABLE_SIZE],
            renders: [RenderEntry::EMPTY; TABLE_SIZE],
            stamp: 0,
            next_stream: 0,
            fallback: Fmt::NONE,
        }
    }

    fn tick(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    /// 记下 client 的格式。同一个指针(对象释放后地址被复用)直接覆盖
    fn set_client(&mut self, key: usize, fmt: Fmt) {
        let stamp = self.tick();
        let i = pick(self.clients.iter().map(|e| (e.key, e.stamp)), key);
        self.clients[i] = ClientEntry { key, fmt, stamp };
    }

    /// GetService 拿到 render:分配新的流编号,格式取 client 的,client 没记录过就用后备格式(猜)
    fn add_render(&mut self, key: usize, client: usize) {
        let known = self.clients.iter().find(|e| e.key == client).map(|e| e.fmt);
        let (fmt, guess) = known.map_or((self.fallback, true), |f| (f, false));
        self.next_stream += 1;
        let (stream, stamp) = (self.next_stream, self.tick());
        let i = pick(self.renders.iter().map(|e| (e.key, e.stamp)), key);
        self.renders[i] = RenderEntry {
            key,
            stream,
            fmt,
            guess,
            stamp,
            ..RenderEntry::EMPTY
        };
    }

    /// 找 render 的记录;没见过的(注入之前就建好的流)就分配新编号,用后备格式(猜)
    fn render(&mut self, key: usize) -> &mut RenderEntry {
        let stamp = self.tick();
        let found = self.renders.iter().position(|e| e.key == key);
        let i = match found {
            Some(i) => i,
            None => {
                self.next_stream += 1;
                let i = pick(self.renders.iter().map(|e| (e.key, e.stamp)), key);
                self.renders[i] = RenderEntry {
                    key,
                    stream: self.next_stream,
                    fmt: self.fallback,
                    guess: true,
                    stamp,
                    ..RenderEntry::EMPTY
                };
                i
            }
        };
        let e = &mut self.renders[i];
        e.stamp = stamp;
        e
    }
}

/// 在 (key, stamp) 序列里挑槽位:已有同 key 的用它;否则用空槽;否则用 stamp 最小(最旧)的。
/// 序列长度固定是 `TABLE_SIZE`,返回的下标一定在范围内
fn pick(entries: impl Iterator<Item = (usize, u64)>, key: usize) -> usize {
    let (mut empty, mut oldest) = (None, (0usize, u64::MAX));
    for (i, (k, stamp)) in entries.enumerate() {
        if k == key {
            return i;
        }
        if k == 0 && empty.is_none() {
            empty = Some(i);
        }
        if stamp < oldest.1 {
            oldest = (i, stamp);
        }
    }
    empty.unwrap_or(oldest.0)
}

/// 解析 `WAVEFORMATEX`(或 `0xFFFE` 的 EXTENSIBLE,看 SubFormat)。不支持的返回原因,给日志用
///
/// # Safety
/// `p` 要么是空,要么指向一个完整的格式结构(EXTENSIBLE 的话要有 22 字节的扩展部分)
unsafe fn parse_format(p: *const WAVEFORMATEX) -> Result<Fmt, String> {
    if p.is_null() {
        return Err("格式指针为空".into());
    }
    // WAVEFORMATEX 是 packed(1),按值整个读出来再用字段
    let f = unsafe { p.read_unaligned() };
    let (tag, channels, rate, bits, cb) = (
        f.wFormatTag,
        f.nChannels,
        f.nSamplesPerSec,
        f.wBitsPerSample,
        f.cbSize,
    );
    let mut valid_bits = bits;
    let float = if tag == WAVE_FORMAT_EXTENSIBLE {
        if cb < 22 {
            return Err(format!("EXTENSIBLE 格式的 cbSize 只有 {cb}"));
        }
        let ext = unsafe { p.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        let v = unsafe { ext.Samples.wValidBitsPerSample };
        if v != 0 {
            valid_bits = v;
        }
        let sub = ext.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            false
        } else {
            return Err(format!("EXTENSIBLE 的子类型不是 PCM 或 float: {sub:?}"));
        }
    } else if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_PCM {
        false
    } else {
        return Err(format!("格式标签 {tag:#x}"));
    };
    let format = match (float, bits) {
        (true, 32) => AUDIO_F32,
        (false, 8) => AUDIO_U8,
        (false, 16) => AUDIO_I16,
        (false, 24) => AUDIO_I24,
        // 32 位容器里放 24 位或 32 位有效位,都是左对齐
        (false, 32) if valid_bits == 24 || valid_bits == 32 => AUDIO_I32,
        _ => {
            let kind = if float { "float" } else { "PCM" };
            return Err(format!("{kind} 容器 {bits} 位、有效 {valid_bits} 位"));
        }
    };
    if channels == 0 || rate == 0 {
        return Err(format!("{channels} 声道 {rate} Hz"));
    }
    Ok(Fmt {
        rate,
        channels,
        format,
        frame_bytes: u32::from(channels) * u32::from(bits / 8),
    })
}

/// 人读的格式描述,日志用
fn describe(f: &Fmt) -> String {
    let name = match f.format {
        AUDIO_F32 => "f32",
        AUDIO_I16 => "i16",
        AUDIO_I24 => "i24",
        AUDIO_I32 => "i32",
        AUDIO_U8 => "u8",
        _ => "?",
    };
    format!("{} Hz {} 声道 {name}", f.rate, f.channels)
}

/// Initialize 之后记下 client 的格式。不是共享模式、或格式不支持的,记成 `Fmt::NONE`:
/// 从它建出来的 render 不截也不静音,日志只在这里记一次(热路径不打日志)
unsafe fn remember_client(client: *mut c_void, shared: bool, fmt: *const WAVEFORMATEX) {
    let parsed = if shared {
        unsafe { parse_format(fmt) }
    } else {
        Err("独占模式".into())
    };
    let entry = match &parsed {
        Ok(f) => *f,
        Err(why) => {
            debug_log(&format!("cyber_remote_hook: 声音: 这个流不截({why})"));
            Fmt::NONE
        }
    };
    table().set_client(client as usize, entry);
    if let Ok(f) = &parsed {
        debug_log(&format!(
            "cyber_remote_hook: 声音: 记下一个输出流 {}",
            describe(f)
        ));
    }
}

unsafe extern "system" fn initialize(
    this: *mut c_void,
    share_mode: i32,
    flags: u32,
    buffer: i64,
    period: i64,
    fmt: *const WAVEFORMATEX,
    session: *const GUID,
) -> HRESULT {
    let Some(original) = INITIALIZE.original() else {
        return E_FAIL;
    };
    let hr = unsafe { original(this, share_mode, flags, buffer, period, fmt, session) };
    if hr.is_ok() {
        unsafe { remember_client(this, share_mode == AUDCLNT_SHAREMODE_SHARED.0, fmt) };
    }
    hr
}

unsafe extern "system" fn initialize_shared_audio_stream(
    this: *mut c_void,
    flags: u32,
    period_frames: u32,
    fmt: *const WAVEFORMATEX,
    session: *const GUID,
) -> HRESULT {
    let Some(original) = INITIALIZE_SHARED.original() else {
        return E_FAIL;
    };
    let hr = unsafe { original(this, flags, period_frames, fmt, session) };
    if hr.is_ok() {
        unsafe { remember_client(this, true, fmt) };
    }
    hr
}

unsafe extern "system" fn get_service(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    let Some(original) = GET_SERVICE.original() else {
        return E_FAIL;
    };
    let hr = unsafe { original(this, riid, ppv) };
    if hr.is_ok()
        && !riid.is_null()
        && !ppv.is_null()
        && unsafe { *riid } == IAudioRenderClient::IID
    {
        let render = unsafe { *ppv } as usize;
        if render != 0 {
            table().add_render(render, this as usize);
        }
    }
    hr
}

unsafe extern "system" fn get_buffer(
    this: *mut c_void,
    frames: u32,
    data: *mut *mut u8,
) -> HRESULT {
    let Some(original) = GET_BUFFER.original() else {
        return E_FAIL;
    };
    let hr = unsafe { original(this, frames, data) };
    if hr.is_ok() && !data.is_null() {
        let ptr = unsafe { *data } as usize;
        let mut t = table();
        let e = t.render(this as usize);
        e.data = ptr;
        e.requested = frames;
    }
    hr
}

unsafe extern "system" fn release_buffer(this: *mut c_void, frames: u32, flags: u32) -> HRESULT {
    let Some(original) = RELEASE_BUFFER.original() else {
        return E_FAIL;
    };
    let flags = capture(this as usize, frames, flags);
    unsafe { original(this, frames, flags) }
}

/// 这些字节按小端 f32 读,是不是像声音:不许有 NaN、Inf,绝对值不超过 16。
/// 别的格式的数据被当成 f32 读,指数位乱落,几乎必然出现 NaN、Inf 或大得离谱的值;
/// 真正的浮点声音再响也到不了 16。全零(静音)过得去,所以每个包都要查,有声音时才会露馅
fn f32_plausible(data: &[u8]) -> bool {
    // 不足 4 字节的尾巴不看
    data.as_chunks::<4>().0.iter().all(|&b| {
        let v = f32::from_le_bytes(b);
        v.is_finite() && v.abs() <= 16.0
    })
}

/// `VirtualQuery` 报的这段内存能不能读:已提交,有读权限,不是 NOACCESS 也不是 GUARD
/// (碰一下 GUARD 页会触发异常)
fn region_readable(state: u32, protect: u32) -> bool {
    const READABLE: u32 = PAGE_READONLY.0
        | PAGE_READWRITE.0
        | PAGE_WRITECOPY.0
        | PAGE_EXECUTE_READ.0
        | PAGE_EXECUTE_READWRITE.0
        | PAGE_EXECUTE_WRITECOPY.0;
    state == MEM_COMMIT.0 && protect & PAGE_GUARD.0 == 0 && protect & READABLE != 0
}

/// 从 `ptr` 起想读 `want` 字节,可读区域是 `[起, 止)`:返回读得了的字节数,最多到区域末尾。
/// 指针不在区域里是 0。只比较不做加法,区域贴着地址空间顶部也不会回绕
fn clamp_len(ptr: usize, want: usize, (start, end): (usize, usize)) -> usize {
    if ptr < start || ptr >= end {
        0
    } else {
        want.min(end - ptr)
    }
}

/// `ptr` 所在的内存区域,是已提交的可读内存才返回 `[起, 止)`。判断在 `region_readable`,这里只是系统调用
fn query_region(ptr: usize) -> Option<(usize, usize)> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    // VirtualQuery 只查属性不碰内容,任何地址都能问
    let got = unsafe {
        VirtualQuery(
            Some(ptr as *const c_void),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if got == 0 || !region_readable(info.State.0, info.Protect.0) {
        return None;
    }
    let start = info.BaseAddress as usize;
    Some((start, start.saturating_add(info.RegionSize)))
}

/// 数据指针所在的可读区域。指针还落在上次查到的区域里就不再查
fn readable_region(cached: (usize, usize), ptr: usize) -> Option<(usize, usize)> {
    if (cached.0..cached.1).contains(&ptr) {
        Some(cached)
    } else {
        query_region(ptr)
    }
}

/// 猜格式的流验不过的原因
#[derive(Debug, PartialEq)]
enum Distrust {
    /// 数据指针不在已提交的可读内存里
    Unreadable,
    /// 猜的长度超出了所在区域,说明每帧字节数猜大了
    Short,
    /// 样本按 f32 看不像声音
    NotSound,
}

impl Distrust {
    fn why(&self) -> &'static str {
        match self {
            Self::Unreadable => "数据区不是可读内存",
            Self::Short => "数据区放不下猜的长度",
            Self::NotSound => "数据按 f32 看不像声音",
        }
    }
}

/// 猜格式的流在抄数据之前的检查:`None` 是验过了,可以抄。
/// `len` 是按猜的每帧字节数算出的这次要读的字节数,`e.data` 不为 0
fn distrust(e: &mut RenderEntry, len: usize) -> Option<Distrust> {
    let Some(region) = readable_region(e.region, e.data) else {
        return Some(Distrust::Unreadable);
    };
    e.region = region;
    // 截短了,说明猜的每帧字节数偏大,读下去就越界
    if clamp_len(e.data, len, region) < len {
        return Some(Distrust::Short);
    }
    // 上面确认了这 `len` 字节都可读;后备格式只会是 f32
    let data = unsafe { core::slice::from_raw_parts(e.data as *const u8, len) };
    if !f32_plausible(data) {
        return Some(Distrust::NotSound);
    }
    None
}

/// 把这次写的数据抄进环,返回交给原函数的 flags(截到了就带上 SILENT,主机不出声)。
/// 猜格式的流验不过就放弃这一路,flags 原样返回
fn capture(render: usize, frames: u32, flags: u32) -> u32 {
    let Some(shared) = state::shared() else {
        return flags;
    };
    capture_into(
        &shared.audio,
        unsafe { GetTickCount64() },
        render,
        frames,
        flags,
    )
}

/// `capture` 的本体:环和当前时刻由调用方给,测试里用自己的环
fn capture_into(ring: &AudioRing, now_ms: u64, render: usize, frames: u32, flags: u32) -> u32 {
    let silent_flag = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
    if frames == 0 || !ring.alive(now_ms) {
        return flags;
    }
    let mut t = table();
    let e = t.render(render);
    // 放弃了的流原样放行:不截,也不加静音
    if e.fmt.format == 0 || e.abandoned {
        return flags;
    }
    // 程序要的比 GetBuffer 给的多:这是个错误调用,让系统自己去拒绝
    if e.data != 0 && frames > e.requested {
        return flags;
    }
    let mut header = AudioHeader {
        stream: e.stream,
        rate: e.fmt.rate,
        frames,
        channels: e.fmt.channels,
        format: e.fmt.format,
        ..Default::default()
    };
    let len = (frames as usize).checked_mul(e.fmt.frame_bytes as usize);
    let mut written = false;
    if flags & silent_flag == 0
        && e.data != 0
        && let Some(len) = len
    {
        // 猜格式的流先验货,验不过就放弃:这一包也原样放行,主机照常出声
        if e.guess
            && let Some(why) = distrust(e, len)
        {
            e.abandoned = true;
            let stream = e.stream;
            header.frames = 0;
            header.flags = AUDIO_PACKET_ABANDONED;
            unsafe { ring.write(header, &[]) };
            // 日志别在锁里打
            drop(t);
            debug_log(&format!(
                "cyber_remote_hook: 声音: 第 {stream} 路是注入前建好的,格式猜不对({}),放弃这一路,主机照常放",
                why.why()
            ));
            return flags;
        }
        // 已知格式的流:数据区是程序刚写完的 GetBuffer 缓冲,`frames <= requested` 保证读不越界
        // (猜格式的流在上面验过了)。环有单写者的要求,靠 TABLE 这把锁
        let data = unsafe { core::slice::from_raw_parts(e.data as *const u8, len) };
        written = unsafe { ring.write(header, data) };
    }
    if !written {
        // 程序自己标了静音、数据写不进(太大)、或没见过这次 GetBuffer:写一个静音包,
        // 时间轴不断,agent 那边这一路照样走
        header.flags = AUDIO_PACKET_SILENT;
        unsafe { ring.write(header, &[]) };
    }
    e.data = 0;
    flags | silent_flag
}

/// 临时流探测到的东西:要钩的函数地址和默认设备的混音格式
struct Probe {
    initialize: *mut c_void,
    get_service: *mut c_void,
    /// 系统没有 `IAudioClient3`(Win10 之前)时没有
    initialize_shared: Option<*mut c_void>,
    get_buffer: *mut c_void,
    release_buffer: *mut c_void,
    mix: Result<Fmt, String>,
}

/// 建一个临时的共享模式输出流,从虚表里取函数地址。没有默认输出设备返回 `Ok(None)`。
/// 所有 COM 对象在返回前释放
unsafe fn probe() -> Result<Option<Probe>, String> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("创建设备枚举器失败: {e}"))?;
    let device = match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) } {
        Ok(d) => d,
        Err(e) if e.code() == E_NOTFOUND => return Ok(None),
        Err(e) => return Err(format!("取默认输出设备失败: {e}")),
    };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| format!("激活 IAudioClient 失败: {e}"))?;
    let mix = unsafe { client.GetMixFormat() }.map_err(|e| format!("取混音格式失败: {e}"))?;
    let mix_fmt = unsafe { parse_format(mix) };
    // 1 秒的缓冲(100 ns 单位),不 Start,不会真的出声
    let init = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 10_000_000, 0, mix, None) };
    unsafe { CoTaskMemFree(Some(mix.cast())) };
    init.map_err(|e| format!("临时流 Initialize 失败: {e}"))?;
    let render: IAudioRenderClient =
        unsafe { client.GetService() }.map_err(|e| format!("取 IAudioRenderClient 失败: {e}"))?;

    let client_vtbl = client.vtable();
    let render_vtbl = render.vtable();
    let initialize_shared = client
        .cast::<IAudioClient3>()
        .ok()
        .map(|c3| c3.vtable().InitializeSharedAudioStream as *mut c_void);
    Ok(Some(Probe {
        initialize: client_vtbl.Initialize as *mut c_void,
        get_service: client_vtbl.GetService as *mut c_void,
        initialize_shared,
        get_buffer: render_vtbl.GetBuffer as *mut c_void,
        release_buffer: render_vtbl.ReleaseBuffer as *mut c_void,
        mix: mix_fmt,
    }))
}

/// 装声音钩子。没有输出设备不算失败,只是不截。放在其他钩子都启用、就绪事件发出之后调用
pub fn install() -> Result<(), String> {
    // 本线程是安装线程,自己初始化 COM,用完还回去。线程已经是别的套间时不动它(也不配对 Uninitialize)
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let inited = hr.is_ok();
    if !inited && hr != RPC_E_CHANGED_MODE {
        return Err(format!("CoInitializeEx 失败: {hr:?}"));
    }
    // probe 里的 COM 对象在它返回时全部释放,之后才能 CoUninitialize
    let probed = unsafe { probe() };
    if inited {
        unsafe { CoUninitialize() };
    }
    let Some(p) = probed? else {
        debug_log("cyber_remote_hook: 没有音频输出设备,不截声音");
        return Ok(());
    };
    match &p.mix {
        Ok(f) if f.format == AUDIO_F32 => {
            debug_log(&format!(
                "cyber_remote_hook: 声音: 默认设备混音格式 {}",
                describe(f)
            ));
            table().fallback = *f;
        }
        // 只有 f32 能查数据像不像声音;别的格式猜错了没法发现,宁可不截
        Ok(f) => debug_log(&format!(
            "cyber_remote_hook: 声音: 默认设备混音格式 {} 不是 f32,没法检查数据,注入前建好的流不截",
            describe(f)
        )),
        Err(why) => debug_log(&format!(
            "cyber_remote_hook: 声音: 默认设备混音格式不支持({why}),注入前建好的流不截"
        )),
    }

    // 先装只记录的钩子,最后才装 ReleaseBuffer:它会给程序的调用加静音,
    // 前面任何一个没装上都不能让它单独生效(不然 GetBuffer 的数据指针拿不到,整个实例变哑)
    INITIALIZE.install_at(p.initialize, initialize)?;
    if let Some(addr) = p.initialize_shared
        && let Err(e) = INITIALIZE_SHARED.install_at(addr, initialize_shared_audio_stream)
    {
        // 少这一个只是 IAudioClient3 的低延迟流认不出格式,退回后备格式
        debug_log(&format!(
            "cyber_remote_hook: 声音: InitializeSharedAudioStream 钩子失败: {e}"
        ));
    }
    GET_SERVICE.install_at(p.get_service, get_service)?;
    GET_BUFFER.install_at(p.get_buffer, get_buffer)?;
    RELEASE_BUFFER.install_at(p.release_buffer, release_buffer)?;
    debug_log("cyber_remote_hook: 声音钩子已启用");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use std::alloc::{Layout, alloc_zeroed};

    use hook_abi::{AUDIO_ALIVE_MS, AudioReader};
    use windows::Win32::System::Memory::{
        MEM_FREE, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    const PAGE: usize = 4096;
    const F32_STEREO: Fmt = Fmt {
        rate: 48_000,
        channels: 2,
        format: AUDIO_F32,
        frame_bytes: 8,
    };

    fn f32_bytes(samples: impl IntoIterator<Item = f32>) -> Vec<u8> {
        samples.into_iter().flat_map(f32::to_le_bytes).collect()
    }

    /// 48 kHz 立体声正弦,左右同相,`frames` 帧
    fn sine_stereo(hz: f32, amp: f32, frames: usize) -> impl Iterator<Item = f32> {
        (0..frames).flat_map(move |i| {
            let v = amp * (core::f32::consts::TAU * hz * i as f32 / 48_000.0).sin();
            [v, v]
        })
    }

    /// 同样的正弦存成 16 位整数(`gametest --tone-now` 放的就是这个)
    fn i16_bytes(hz: f32, amp: f32, frames: usize) -> Vec<u8> {
        sine_stereo(hz, amp, frames)
            .flat_map(|v| ((v * 32767.0).round() as i16).to_le_bytes())
            .collect()
    }

    /// 申请几页已提交的读写内存,出作用域释放。`.1` 是字节数
    struct Pages(*mut c_void, usize);

    impl Pages {
        fn new(pages: usize) -> Self {
            let size = pages * PAGE;
            let p = unsafe { VirtualAlloc(None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
            assert!(!p.is_null(), "VirtualAlloc 失败");
            Self(p, size)
        }

        fn addr(&self, offset: usize) -> usize {
            self.0 as usize + offset
        }

        fn write(&self, offset: usize, bytes: &[u8]) {
            assert!(offset + bytes.len() <= self.1);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.addr(offset) as *mut u8,
                    bytes.len(),
                );
            }
        }

        /// 改第 `page` 页的保护属性
        fn protect(&self, page: usize, flags: PAGE_PROTECTION_FLAGS) {
            let mut old = PAGE_PROTECTION_FLAGS(0);
            unsafe {
                VirtualProtect(
                    self.addr(page * PAGE) as *const c_void,
                    PAGE,
                    flags,
                    &mut old,
                )
            }
            .expect("VirtualProtect 失败");
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            unsafe {
                let _ = VirtualFree(self.0, 0, MEM_RELEASE);
            }
        }
    }

    /// 在堆上直接分配全零的环(1 MiB,经栈搬一趟会爆栈)
    fn heap_ring() -> Box<AudioRing> {
        let layout = Layout::new::<AudioRing>();
        unsafe {
            let p = alloc_zeroed(layout).cast::<AudioRing>();
            assert!(!p.is_null());
            Box::from_raw(p)
        }
    }

    /// 取走环里新到的包:(流, 标志, 帧数, 数据),数据带着对齐填充
    fn packets(reader: &mut AudioReader, ring: &AudioRing) -> Vec<(u32, u32, u32, Vec<u8>)> {
        let mut got = vec![];
        let lost = reader.drain(ring, |h, d| {
            got.push((h.stream, h.flags, h.frames, d.to_vec()))
        });
        assert!(!lost);
        got
    }

    #[test]
    fn f32_sine_and_silence_pass() {
        assert!(f32_plausible(&f32_bytes(sine_stereo(440.0, 0.2, 480))));
        // 过载到 4 倍满量程也还在范围里
        assert!(f32_plausible(&f32_bytes(sine_stereo(440.0, 4.0, 480))));
        // 全零(静音)和空的都过得去
        assert!(f32_plausible(&[0u8; 3840]));
        assert!(f32_plausible(&[]));
    }

    #[test]
    fn i16_sine_read_as_f32_is_rejected() {
        for hz in [100.0, 440.0, 1000.0, 5000.0] {
            for amp in [0.001, 0.01, 0.2, 0.9] {
                let bytes = i16_bytes(hz, amp, 480);
                assert!(
                    !f32_plausible(&bytes),
                    "{hz} Hz 幅度 {amp} 的 16 位正弦通过了检查"
                );
            }
        }
    }

    #[test]
    fn nan_inf_and_huge_are_rejected() {
        let bad = [
            f32::NAN,
            f32::from_bits(0xFFFF_FFFF),
            f32::INFINITY,
            f32::NEG_INFINITY,
            16.5,
            -16.5,
            1e36,
        ];
        for v in bad {
            assert!(!f32_plausible(&f32_bytes([0.1, v, 0.1])), "{v} 通过了检查");
        }
        // 16 本身还算
        assert!(f32_plausible(&f32_bytes([16.0, -16.0])));
        // 不足 4 字节的尾巴不看
        assert!(f32_plausible(&[0, 0, 0]));
    }

    #[test]
    fn clamp_len_stops_at_region_end() {
        let region = (0x1000, 0x2000);
        assert_eq!(clamp_len(0x1000, 0x1000, region), 0x1000);
        assert_eq!(clamp_len(0x1000, 0x1001, region), 0x1000);
        assert_eq!(clamp_len(0x1ff0, 0x10, region), 0x10);
        assert_eq!(clamp_len(0x1ff0, 0x11, region), 0x10);
        assert_eq!(clamp_len(0x1fff, 100, region), 1);
        assert_eq!(clamp_len(0x1800, 0, region), 0);
        // 指针不在区域里:一个字节也不能读
        assert_eq!(clamp_len(0x0fff, 16, region), 0);
        assert_eq!(clamp_len(0x2000, 16, region), 0);
        // 区域贴着地址空间顶部,长度大到会回绕也不越界
        let top = (usize::MAX - 0xfff, usize::MAX);
        assert_eq!(clamp_len(usize::MAX - 7, usize::MAX, top), 7);
    }

    #[test]
    fn region_readable_rules() {
        let commit = MEM_COMMIT.0;
        for p in [
            PAGE_READONLY,
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ] {
            assert!(region_readable(commit, p.0), "{:#x} 应该可读", p.0);
            // 缓存修饰位(NOCACHE 0x200)不影响
            assert!(region_readable(commit, p.0 | 0x200));
            // GUARD 页一碰就触发异常,不能读
            assert!(!region_readable(commit, p.0 | PAGE_GUARD.0));
        }
        assert!(!region_readable(commit, PAGE_NOACCESS.0));
        assert!(!region_readable(commit, PAGE_EXECUTE.0));
        assert!(!region_readable(commit, 0));
        // 没提交的(保留的、空闲的),保护位是什么都不行
        assert!(!region_readable(MEM_RESERVE.0, PAGE_READWRITE.0));
        assert!(!region_readable(MEM_FREE.0, PAGE_READWRITE.0));
    }

    #[test]
    fn page_end_truncates_the_copy() {
        // 一页:后面紧跟着的是空闲地址,区域正好到页尾
        let mem = Pages::new(1);
        let region = query_region(mem.addr(100)).expect("已提交的读写页应该可读");
        assert_eq!(region, (mem.addr(0), mem.addr(PAGE)));
        // 指针在页尾前 5 字节,想读 16 字节:只剩 5 字节
        let ptr = mem.addr(PAGE - 5);
        assert_eq!(clamp_len(ptr, 16, region), 5);
        assert_eq!(clamp_len(ptr, 5, region), 5);
        assert_eq!(clamp_len(ptr, 4, region), 4);
        // 整页读得下,多一个字节就截
        assert_eq!(clamp_len(mem.addr(0), PAGE, region), PAGE);
        assert_eq!(clamp_len(mem.addr(0), PAGE + 1, region), PAGE);
    }

    #[test]
    fn unreadable_memory_is_refused() {
        // 四页:1 号 NOACCESS,2 号 GUARD,0 号和 3 号正常
        let mem = Pages::new(4);
        mem.protect(1, PAGE_NOACCESS);
        mem.protect(2, PAGE_READWRITE | PAGE_GUARD);
        assert_eq!(
            query_region(mem.addr(10)),
            Some((mem.addr(0), mem.addr(PAGE)))
        );
        assert_eq!(query_region(mem.addr(PAGE + 10)), None);
        assert_eq!(query_region(mem.addr(2 * PAGE + 10)), None);
        assert_eq!(
            query_region(mem.addr(3 * PAGE + 10)),
            Some((mem.addr(3 * PAGE), mem.addr(4 * PAGE)))
        );
        // 只保留地址没提交的
        let reserved = unsafe { VirtualAlloc(None, PAGE, MEM_RESERVE, PAGE_READWRITE) };
        assert!(!reserved.is_null());
        assert_eq!(query_region(reserved as usize), None);
        unsafe {
            let _ = VirtualFree(reserved, 0, MEM_RELEASE);
        }
        // 低地址是空闲的
        assert_eq!(query_region(0), None);
        assert_eq!(query_region(16), None);
    }

    #[test]
    fn cached_region_skips_the_query() {
        // 缓存的区域是编的(0x1000..0x2000 实际是空闲地址):落在里面就直接用缓存,真查反而是 None
        assert_eq!(query_region(0x1800), None);
        assert_eq!(
            readable_region((0x1000, 0x2000), 0x1800),
            Some((0x1000, 0x2000))
        );
        // 不在缓存里,或者没有缓存,就查真的
        let mem = Pages::new(1);
        let real = Some((mem.addr(0), mem.addr(PAGE)));
        assert_eq!(readable_region((0x1000, 0x2000), mem.addr(8)), real);
        assert_eq!(readable_region((0, 0), mem.addr(8)), real);
        assert_eq!(readable_region((0, 0), 0x1800), None);
    }

    #[test]
    fn distrust_checks_memory_then_samples() {
        let mem = Pages::new(1);
        let entry = |ptr: usize| RenderEntry {
            key: 1,
            stream: 1,
            fmt: F32_STEREO,
            guess: true,
            data: ptr,
            requested: 480,
            ..RenderEntry::EMPTY
        };
        // 全零:通过,区域缓存下来
        let mut e = entry(mem.addr(0));
        assert_eq!(distrust(&mut e, 3840), None);
        assert_eq!(e.region, (mem.addr(0), mem.addr(PAGE)));
        // 真正的 f32 正弦:通过
        mem.write(0, &f32_bytes(sine_stereo(440.0, 0.2, 480)));
        assert_eq!(distrust(&mut e, 3840), None);
        // 前半换成 16 位正弦:不像声音
        mem.write(0, &i16_bytes(440.0, 0.2, 480));
        assert_eq!(distrust(&mut e, 3840), Some(Distrust::NotSound));
        // 指针离页尾 1920 字节(16 位 480 帧的真实长度),猜的 3840 放不下
        let mut e = entry(mem.addr(PAGE - 1920));
        assert_eq!(distrust(&mut e, 3840), Some(Distrust::Short));
        assert_eq!(distrust(&mut e, 1920), None);
        // 指针指向不可读的地方
        let mut e = entry(16);
        assert_eq!(distrust(&mut e, 8), Some(Distrust::Unreadable));
    }

    /// 整条 `capture_into`:已知格式的流不变,猜格式的流验不过就放弃并从此原样放行
    #[test]
    fn capture_abandons_guessed_streams_and_leaves_known_ones() {
        let ring = heap_ring();
        ring.alive_ms.store(1000, Ordering::Relaxed);
        let mut reader = AudioReader::new(&ring);
        let silent = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
        let mem = Pages::new(1);
        table().fallback = F32_STEREO;
        let stream_of = |render: usize| table().render(render).stream;
        // 模拟程序:GetBuffer 拿到 ptr,写好数据,再 ReleaseBuffer
        let release = |render: usize, ptr: usize, frames: u32, flags: u32| {
            {
                let mut t = table();
                let e = t.render(render);
                e.data = ptr;
                e.requested = frames;
            }
            capture_into(&ring, 1000, render, frames, flags)
        };
        let sine16 = i16_bytes(440.0, 0.2, 480);
        let sine32 = f32_bytes(sine_stereo(440.0, 0.2, 480));

        // A:Initialize 过的 16 位流,和以前一样:截下来,主机静音
        let (client, render) = (0xA001, 0xA002);
        let i16_stereo = Fmt {
            rate: 44_100,
            channels: 2,
            format: AUDIO_I16,
            frame_bytes: 4,
        };
        {
            let mut t = table();
            t.set_client(client, i16_stereo);
            t.add_render(render, client);
        }
        mem.write(0, &sine16);
        assert_eq!(release(render, mem.addr(0), 480, 0), silent);
        let got = packets(&mut reader, &ring);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].0, got[0].1, got[0].2), (stream_of(render), 0, 480));
        assert_eq!(&got[0].3[..sine16.len()], &sine16[..]);

        // B:没见过 Initialize 的流,数据是 16 位的:放弃。这一包不加静音,环里只留一个放弃包
        let render = 0xB002;
        table().add_render(render, 0xB001);
        assert_eq!(release(render, mem.addr(0), 480, 0), 0);
        let got = packets(&mut reader, &ring);
        assert_eq!(
            got,
            [(stream_of(render), AUDIO_PACKET_ABANDONED, 0, vec![])]
        );
        // 之后原样放行,不再写环
        assert_eq!(release(render, mem.addr(0), 480, 0), 0);
        assert!(packets(&mut reader, &ring).is_empty());

        // C:猜格式的流先是静音(全零骗得过检查),照常截;有声音了才露馅,放弃
        let render = 0xC002;
        table().add_render(render, 0xC001);
        mem.write(0, &[0u8; 3840]);
        assert_eq!(release(render, mem.addr(0), 480, 0), silent);
        let got = packets(&mut reader, &ring);
        assert_eq!((got.len(), got[0].1, got[0].2), (1, 0, 480));
        mem.write(0, &sine32);
        assert_eq!(release(render, mem.addr(0), 480, 0), silent);
        assert_eq!(packets(&mut reader, &ring).len(), 1);
        mem.write(0, &sine16);
        assert_eq!(release(render, mem.addr(0), 480, 0), 0);
        let got = packets(&mut reader, &ring);
        assert_eq!(
            got,
            [(stream_of(render), AUDIO_PACKET_ABANDONED, 0, vec![])]
        );

        // D:数据指针离页尾只剩 16 位 480 帧的长度,猜的长度放不下:不读,直接放弃
        let render = 0xD002;
        table().add_render(render, 0xD001);
        assert_eq!(release(render, mem.addr(PAGE - 1920), 480, 0), 0);
        let got = packets(&mut reader, &ring);
        assert_eq!(
            got,
            [(stream_of(render), AUDIO_PACKET_ABANDONED, 0, vec![])]
        );

        // E:程序自己标了静音:不读数据,照旧写静音包,不算放弃
        let render = 0xE002;
        table().add_render(render, 0xE001);
        assert_eq!(release(render, mem.addr(PAGE - 100), 480, silent), silent);
        let got = packets(&mut reader, &ring);
        assert_eq!(got, [(stream_of(render), AUDIO_PACKET_SILENT, 480, vec![])]);
        assert!(!table().render(render).abandoned);

        // F:agent 的心跳过期:什么都不做,也不会提前放弃
        let render = 0xF002;
        table().add_render(render, 0xF001);
        mem.write(0, &sine16);
        {
            let mut t = table();
            let e = t.render(render);
            e.data = mem.addr(0);
            e.requested = 480;
        }
        assert_eq!(
            capture_into(&ring, 1000 + AUDIO_ALIVE_MS, render, 480, 0),
            0
        );
        assert!(packets(&mut reader, &ring).is_empty());
        assert!(!table().render(render).abandoned);

        // G:后备格式不是 f32(这里用 NONE 代表 install 没设):猜格式的流不截也不静音
        table().fallback = Fmt::NONE;
        let render = 0x1_0002;
        table().add_render(render, 0x1_0001);
        assert_eq!(release(render, mem.addr(0), 480, 0), 0);
        assert!(packets(&mut reader, &ring).is_empty());
        table().fallback = F32_STEREO;
    }
}
