//! agent 与注入 DLL 之间的共享内存布局(`#[repr(C)]`,纯 POD)。
//!
//! 一个目标进程一块命名文件映射 `Local\cyber_remote_hook_{pid}`。agent 是写方,
//! hook 是读方;两边都按本文件的布局解释这块内存,所以这个 crate 不依赖 windows,
//! 32 位和 64 位 DLL 都用同一份定义(字段全用定长整数,布局与位宽无关)。
//!
//! 同步:快照块(按键、光标、窗口、手柄)用 seqlock(`seq` 为奇数表示正在写);
//! Raw Input 用环形缓冲,agent 递增 `raw_head`,把绝对序号编进伪造的 `HRAWINPUT`
//! 一起 `PostMessage(WM_INPUT)`,hook 用序号对 `RAW_RING` 取模读回对应事件。
//!
//! 唯一方向相反的是声音:`Shared::audio` 是 hook 写、agent 读的环形缓冲(hook 截下目标进程
//! 写给 WASAPI 的 PCM),只有它的心跳 `alive_ms` 还是 agent 写、hook 读。所以映射必须是
//! 读写的,hook 只许碰 `audio` 里的 `head` 和数据区。

#![cfg_attr(not(test), no_std)]

extern crate alloc;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// 映射头部的校验字,防止读到未初始化或别的进程的内存
pub const MAGIC: u32 = 0x4359_4248; // "CYBH"
/// 布局版本,agent 与 DLL 不一致时 DLL 拒绝挂钩
pub const ABI_VERSION: u32 = 2;
/// Raw Input 环形缓冲槽位数,必须是 2 的幂
pub const RAW_RING: usize = 256;

/// 伪造 `HRAWINPUT` 的高位标记:`(RAW_HANDLE_TAG | 绝对序号的低位)`。
/// 系统的真句柄理论上也可能落进这个区间,所以 hook 还要求序号在环里对得上
/// (`Shared::raw_from_handle`),两者都满足才当伪造句柄处理
pub const RAW_HANDLE_TAG: usize = 0xCB00_0000;
pub const RAW_HANDLE_MASK: usize = 0x00FF_FFFF;

/// 绝对序号 → 伪造的 `HRAWINPUT`(放进 `WM_INPUT` 的 lParam)
pub const fn raw_handle(seq: u32) -> usize {
    RAW_HANDLE_TAG | (seq as usize & RAW_HANDLE_MASK)
}

/// 伪造的 `HRAWINPUT` → 序号低 24 位;不带标记的是系统真句柄
pub const fn raw_handle_seq(handle: usize) -> Option<u32> {
    if handle & !RAW_HANDLE_MASK == RAW_HANDLE_TAG {
        Some((handle & RAW_HANDLE_MASK) as u32)
    } else {
        None
    }
}

/// 要模拟的 XInput 槽位(只做手柄 0,其余槽位 hook 一律报未接)
pub const PAD_SLOT: u32 = 0;

pub const RAW_KIND_NONE: u32 = 0;
pub const RAW_KIND_MOUSE: u32 = 1;
pub const RAW_KIND_KEYBOARD: u32 = 2;

/// 命名文件映射的名字,按目标进程 PID 区分(多开隔离)
pub fn mapping_name(pid: u32) -> String {
    format!("Local\\cyber_remote_hook_{pid}")
}

/// 就绪事件的名字。agent 注入前创建,DLL 把钩子全部启用后 `SetEvent`,
/// agent 等到它才认为该实例已经隔离
pub fn ready_event_name(pid: u32) -> String {
    format!("Local\\cyber_remote_hook_ready_{pid}")
}

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// Snapshot.flags 的位
pub const FLAG_OVERRIDE_CURSOR: u32 = 1 << 0;
pub const FLAG_OVERRIDE_FOREGROUND: u32 = 1 << 1;
pub const FLAG_PAD_CONNECTED: u32 = 1 << 2;
/// 总开关:agent 在线且接管输入。没置这一位(agent 退出时清掉)时,按键、光标、窗口和
/// 手柄的钩子全部回落到真实 API,Raw Input 的钩子也放行真实事件。
pub const FLAG_ACTIVE: u32 = 1 << 3;

/// 手柄状态,布局同 `XINPUT_GAMEPAD`(不依赖 windows crate)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Pad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// 一次 Raw Input 事件。布局与位宽无关;hook 读出后按本进程位宽拼 `RAWINPUT`。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RawEvent {
    pub kind: u32,
    /// 绝对序号,用于确认槽位没被覆写
    pub seq: u32,
    pub mouse_last_x: i32,
    pub mouse_last_y: i32,
    /// `RI_MOUSE_*` 按钮标志
    pub mouse_flags: u16,
    /// 滚轮增量(放进 usButtonData)
    pub mouse_button_data: u16,
    pub kbd_vkey: u16,
    /// `RI_KEY_MAKE`(0)/`RI_KEY_BREAK`(1)等
    pub kbd_flags: u16,
    pub kbd_scan: u16,
    pub _pad: u16,
}

/// seqlock 保护的那块状态:按键、光标、窗口、手柄。agent 整块写,hook 整块读,
/// 所以这里的字段都是普通 POD(不是原子),一致性由 `Shared::seq` 保证。
///
/// 字段按对齐从大到小排:四个 `u64` 在最前,32 位和 64 位编译出的布局一致
/// (`u64` 在两个目标上都是 8 字节对齐,不会因位宽差异插入不同的填充)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Snapshot {
    /// 下面四个是 `HWND` 的数值(u64 容纳 64 位句柄),0 表示不覆盖
    pub foreground: u64,
    pub active: u64,
    pub focus: u64,
    pub capture: u64,
    /// `FLAG_*` 的位或
    pub flags: u32,
    /// 伪造的光标位置(屏幕坐标),`GetCursorPos` 返回它
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub pad: Pad,
    /// `GetAsyncKeyState`/`GetKeyState`/`GetKeyboardState` 用:高位 0x80 表示按下
    pub keys: [u8; 256],
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            foreground: 0,
            active: 0,
            focus: 0,
            capture: 0,
            flags: 0,
            cursor_x: 0,
            cursor_y: 0,
            pad: Pad::default(),
            keys: [0; 256],
        }
    }
}

/// 声音环形缓冲的字节数,必须是 2 的幂
pub const AUDIO_RING_BYTES: usize = 1 << 20;
/// 包长都是它的倍数,回绕时环尾剩下的空间一定放得下包头
pub const AUDIO_ALIGN: usize = 32;
/// agent 的心跳超过这么久没更新,hook 就不截也不静音(agent 崩了,声音自动回主机扬声器)
pub const AUDIO_ALIVE_MS: u64 = 300;

/// 采样格式,值写进 `AudioHeader::format`
pub const AUDIO_F32: u16 = 1;
pub const AUDIO_I16: u16 = 2;
/// 3 字节紧凑
pub const AUDIO_I24: u16 = 3;
/// 也用于 32 位容器里的 24 位
pub const AUDIO_I32: u16 = 4;
pub const AUDIO_U8: u16 = 5;

/// `AudioHeader::flags` 的位:这段是静音,没有数据,只占 `frames` 帧的时间
pub const AUDIO_PACKET_SILENT: u32 = 1;
/// `AudioHeader::flags` 的位:hook 放弃了这一路(注入前就建好的流,猜的格式不对),之后不会再有
/// 这一路的包。`frames` 为 0,没有数据;读方收到就把这一路从混音里移走
pub const AUDIO_PACKET_ABANDONED: u32 = 2;
/// 回绕填充包的流编号,读方直接跳过
pub const AUDIO_STREAM_PAD: u32 = u32::MAX;

const AUDIO_HEADER_BYTES: usize = 32;
/// 单个包(含包头)的最大长度:数据最多 `AUDIO_RING_BYTES / 4`,再向上对齐
const AUDIO_MAX_PACKET: usize = (AUDIO_HEADER_BYTES + AUDIO_RING_BYTES / 4).next_multiple_of(AUDIO_ALIGN);
/// 读方能信的最大积压。写方每次先写 PAD 再写整包,最多比已公布的 `head` 多出两个最大包,
/// 这一段盖掉的正是环里最旧的数据,所以可读的上限要把它扣掉
const AUDIO_SAFE_BYTES: u64 = (AUDIO_RING_BYTES - 2 * AUDIO_MAX_PACKET) as u64;

/// 声音包头,环里每个包以它开头,正好 32 字节
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioHeader {
    /// 整个包的字节数(含包头和对齐填充),`AudioRing::write` 写入时自己填
    pub len: u32,
    /// 流编号(hook 里每个 `IAudioRenderClient` 一个);`AUDIO_STREAM_PAD` 是填充包
    pub stream: u32,
    pub rate: u32,
    /// 这个包占的帧数
    pub frames: u32,
    /// `AUDIO_PACKET_*`
    pub flags: u32,
    pub channels: u16,
    /// `AUDIO_F32` 等
    pub format: u16,
    pub _pad: [u32; 2],
}

/// 声音环形缓冲:hook 写,agent 读,方向和 `Shared` 里其他字段相反。
///
/// `head` 是写入的字节总数(绝对位置,不回绕),只有 hook 写;每个包长是 32 的倍数,
/// 数据先落地,最后才 Release 公布 `head`。`alive_ms` 反过来是 agent 写的心跳。
#[repr(C)]
pub struct AudioRing {
    pub head: AtomicU64,
    /// agent 的心跳(`GetTickCount64`),0 表示 agent 没在收
    pub alive_ms: AtomicU64,
    data: UnsafeCell<[u8; AUDIO_RING_BYTES]>,
}

// 数据区的并发访问由 head 的 Release/Acquire 和读方的覆盖检测约束,见 write / drain
unsafe impl Sync for AudioRing {}

impl AudioRing {
    /// hook:写一个包。包长 = 包头 + 数据向上对齐到 32;数据比 `AUDIO_RING_BYTES / 4` 大就返回 false。
    /// 到环尾放不下时,先在当前位置写一个 PAD 包占满剩余空间,新包从环头开始。
    ///
    /// # Safety
    /// 同一时刻只能有一个写者,调用方自己保证(hook 里靠进程内的锁)
    pub unsafe fn write(&self, mut header: AudioHeader, data: &[u8]) -> bool {
        if data.len() > AUDIO_RING_BYTES / 4 {
            return false;
        }
        let len = (AUDIO_HEADER_BYTES + data.len()).next_multiple_of(AUDIO_ALIGN);
        let mut head = self.head.load(Ordering::Relaxed);
        let mut pos = head as usize & (AUDIO_RING_BYTES - 1);
        let remain = AUDIO_RING_BYTES - pos;
        if remain < len {
            let pad = AudioHeader { len: remain as u32, stream: AUDIO_STREAM_PAD, ..Default::default() };
            unsafe { self.put(pos, header_bytes(&pad)) };
            head += remain as u64;
            pos = 0;
        }
        header.len = len as u32;
        unsafe {
            self.put(pos, header_bytes(&header));
            self.put(pos + AUDIO_HEADER_BYTES, data);
            // 对齐填充补零,读方拿到的数据后面是确定的零而不是上一圈的残留
            for i in AUDIO_HEADER_BYTES + data.len()..len {
                core::ptr::write_volatile(self.base().add(pos + i), 0);
            }
        }
        // 包头和数据先落地,再公布 head
        self.head.store(head + len as u64, Ordering::Release);
        true
    }

    /// hook:agent 的心跳是不是还新鲜。过期或从没写过(0)就不截也不静音
    pub fn alive(&self, now_ms: u64) -> bool {
        let t = self.alive_ms.load(Ordering::Relaxed);
        t != 0 && now_ms.saturating_sub(t) < AUDIO_ALIVE_MS
    }

    fn base(&self) -> *mut u8 {
        self.data.get().cast()
    }

    /// volatile 逐字节写进环里。`pos + src.len()` 不许越过环尾
    unsafe fn put(&self, pos: usize, src: &[u8]) {
        debug_assert!(pos + src.len() <= AUDIO_RING_BYTES);
        let dst = unsafe { self.base().add(pos) };
        for (i, &b) in src.iter().enumerate() {
            unsafe { core::ptr::write_volatile(dst.add(i), b) };
        }
    }

    /// volatile 逐字节从环里读出
    unsafe fn take(&self, pos: usize, dst: &mut [u8]) {
        debug_assert!(pos + dst.len() <= AUDIO_RING_BYTES);
        let src = unsafe { self.base().add(pos) };
        for (i, d) in dst.iter_mut().enumerate() {
            *d = unsafe { core::ptr::read_volatile(src.add(i)) };
        }
    }
}

fn header_bytes(h: &AudioHeader) -> &[u8] {
    unsafe { core::slice::from_raw_parts((h as *const AudioHeader).cast(), AUDIO_HEADER_BYTES) }
}

/// agent:从环里按序取包。只能有一个读者
pub struct AudioReader {
    /// 下一个要读的绝对位置
    tail: u64,
    scratch: Vec<u8>,
}

impl AudioReader {
    /// 从当前 head 开始读,不管之前积压了什么
    pub fn new(ring: &AudioRing) -> Self {
        Self { tail: ring.head.load(Ordering::Acquire), scratch: Vec::new() }
    }

    /// 取走已公布的全部包,逐个回调(PAD 包不回调);回调拿到的数据带着对齐填充,
    /// 调用方按 `frames` 和格式自己截取。返回这次是否丢过数据(读得太慢被写方追上,或包头不对)。
    ///
    /// 写方不等读方,所以读完要再看一次 head:读的过程中被覆盖的包整个丢掉,不回调
    pub fn drain(&mut self, ring: &AudioRing, mut f: impl FnMut(&AudioHeader, &[u8])) -> bool {
        let mut lost = false;
        let head = ring.head.load(Ordering::Acquire);
        if head.wrapping_sub(self.tail) > AUDIO_SAFE_BYTES {
            self.tail = head;
            lost = true;
        }
        while self.tail < head {
            let pos = self.tail as usize & (AUDIO_RING_BYTES - 1);
            let mut raw = [0u8; AUDIO_HEADER_BYTES];
            unsafe { ring.take(pos, &mut raw) };
            let h: AudioHeader = unsafe { core::ptr::read_unaligned(raw.as_ptr().cast()) };
            let len = h.len as usize;
            let sane = len >= AUDIO_HEADER_BYTES
                && len % AUDIO_ALIGN == 0
                && len <= AUDIO_MAX_PACKET
                && len <= AUDIO_RING_BYTES - pos
                && len as u64 <= head - self.tail;
            if !sane {
                self.tail = head;
                return true;
            }
            self.scratch.clear();
            self.scratch.resize(len - AUDIO_HEADER_BYTES, 0);
            unsafe { ring.take(pos + AUDIO_HEADER_BYTES, &mut self.scratch) };
            // 数据读完才看 head:这之后 head 不会比读的时候更旧
            fence(Ordering::Acquire);
            let now = ring.head.load(Ordering::Acquire);
            if now.wrapping_sub(self.tail) > AUDIO_SAFE_BYTES {
                self.tail = now;
                return true;
            }
            if h.stream != AUDIO_STREAM_PAD {
                f(&h, &self.scratch);
            }
            self.tail += len as u64;
        }
        lost
    }
}

/// 整块共享内存的布局。agent 创建命名映射(OS 清零),调 `init` 写入校验字;
/// hook 映射进来先 `valid()` 校验,再按 seqlock 读快照、按环形缓冲读 Raw Input,
/// 并往 `audio` 里写截下来的声音(这是唯一 hook 写 agent 读的部分)。
///
/// 这块内存跨进程共享,另一边随时可能改写,所以非原子字段都放在 `UnsafeCell` 里
/// (`repr(transparent)`,不改布局),只通过 volatile 读写访问。
#[repr(C)]
pub struct Shared {
    magic: AtomicU32,
    version: AtomicU32,
    /// seqlock:奇数表示 agent 正在写快照
    seq: AtomicU32,
    /// 已写入的 Raw Input 事件总数,下一个事件的绝对序号
    raw_head: AtomicU32,
    snapshot: UnsafeCell<Snapshot>,
    raw: UnsafeCell<[RawEvent; RAW_RING]>,
    /// 放在最后,前面的布局不动
    pub audio: AudioRing,
}

// 并发访问由 seqlock 和环形缓冲的序号校验约束,见各方法
unsafe impl Sync for Shared {}

/// 读快照最多自旋这么多次,免得 agent 崩了之后 hook 卡死
const READ_SPINS: u32 = 1024;

impl Shared {
    /// 在刚创建、清零的映射上写入校验字。最后写 magic 当作“就绪”标志。
    pub fn init(&self) {
        self.version.store(ABI_VERSION, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }

    /// 校验字对得上才信这块内存
    pub fn valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == MAGIC
            && self.version.load(Ordering::Relaxed) == ABI_VERSION
    }

    /// Raw Input 已写入的事件总数(下一个事件的序号)
    pub fn raw_head(&self) -> u32 {
        self.raw_head.load(Ordering::Acquire)
    }

    /// agent:整块写入快照(seqlock 写方)。只允许一个写方。
    pub fn write_snapshot(&self, snap: &Snapshot) {
        let s = self.seq.load(Ordering::Relaxed);
        // 置奇数,宣告开始写
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        unsafe { core::ptr::write_volatile(self.snapshot.get(), *snap) };
        // 置偶数,写完
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// hook:读一份一致的快照(seqlock 读方)。agent 没在写、读期间没被覆写才成功。
    pub fn read_snapshot(&self) -> Option<Snapshot> {
        for _ in 0..READ_SPINS {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let snap = unsafe { core::ptr::read_volatile(self.snapshot.get()) };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some(snap);
            }
        }
        None
    }

    /// agent:写一个 Raw Input 事件,返回它的绝对序号(编进伪造的 `HRAWINPUT`)。只允许一个写方。
    pub fn push_raw(&self, mut ev: RawEvent) -> u32 {
        let seq = self.raw_head.load(Ordering::Relaxed);
        ev.seq = seq;
        unsafe { core::ptr::write_volatile(self.slot(seq), ev) };
        // 事件内容先落地,再公布 head
        self.raw_head.store(seq.wrapping_add(1), Ordering::Release);
        seq
    }

    /// hook:按绝对序号读回 Raw Input 事件;还没写到、已被覆写或为空则返回 None
    pub fn raw_at(&self, seq: u32) -> Option<RawEvent> {
        let head = self.raw_head.load(Ordering::Acquire);
        // 只认 [head - RAW_RING, head) 区间内的序号
        let age = head.wrapping_sub(seq);
        if age == 0 || age as usize > RAW_RING {
            return None;
        }
        let ev = unsafe { core::ptr::read_volatile(self.slot(seq)) };
        fence(Ordering::Acquire);
        // 读的过程中槽位可能被新事件覆写,事件自带的序号对得上才算数
        (ev.seq == seq && ev.kind != RAW_KIND_NONE).then_some(ev)
    }

    /// hook:伪造的 `HRAWINPUT` → 事件。句柄只带序号低 24 位,按当前 head 还原成
    /// 最近的那个绝对序号;不是我们的句柄返回 None
    pub fn raw_from_handle(&self, handle: usize) -> Option<RawEvent> {
        let low = raw_handle_seq(handle)?;
        let head = self.raw_head.load(Ordering::Acquire);
        // head-1 往回数,低位对得上的第一个序号
        let back = head.wrapping_sub(1).wrapping_sub(low) & RAW_HANDLE_MASK as u32;
        self.raw_at(head.wrapping_sub(1).wrapping_sub(back))
    }

    fn slot(&self, seq: u32) -> *mut RawEvent {
        let slot = (seq as usize) & (RAW_RING - 1);
        unsafe { (self.raw.get() as *mut RawEvent).add(slot) }
    }
}

// 环形槽位数必须是 2 的幂,取模才能用位与
const _: () = assert!(RAW_RING.is_power_of_two());

// 布局不含指针,32 位和 64 位进程必须看到完全一致的大小、对齐和字段偏移。
// 任一目标上这些断言不成立就是布局跟位宽相关了,编译期就拦住。
const _: () = assert!(core::mem::size_of::<Pad>() == 12);
const _: () = assert!(core::mem::size_of::<RawEvent>() == 28);
const _: () = assert!(core::mem::align_of::<RawEvent>() == 4);
const _: () = assert!(core::mem::size_of::<Snapshot>() == 312);
const _: () = assert!(core::mem::align_of::<Snapshot>() == 8);
const _: () = assert!(core::mem::offset_of!(Snapshot, flags) == 32);
const _: () = assert!(core::mem::offset_of!(Snapshot, pad) == 44);
const _: () = assert!(core::mem::offset_of!(Snapshot, keys) == 56);
const _: () = assert!(core::mem::size_of::<AudioHeader>() == AUDIO_HEADER_BYTES);
const _: () = assert!(AUDIO_RING_BYTES.is_power_of_two() && AUDIO_RING_BYTES % AUDIO_ALIGN == 0);
const _: () = assert!(core::mem::size_of::<AudioRing>() == 16 + AUDIO_RING_BYTES);
const _: () = assert!(core::mem::size_of::<Shared>() == 16 + 312 + 28 * RAW_RING + 16 + AUDIO_RING_BYTES);
const _: () = assert!(core::mem::align_of::<Shared>() == 8);
const _: () = assert!(core::mem::offset_of!(Shared, snapshot) == 16);
const _: () = assert!(core::mem::offset_of!(Shared, raw) == 328);
const _: () = assert!(core::mem::offset_of!(Shared, audio) == 328 + 28 * RAW_RING);

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{Layout, alloc_zeroed};

    /// 在堆上直接分配全零的 T。映射由 OS 清零,测试里同样从全零开始;
    /// `Shared` 和 `AudioRing` 都有 1 MiB,经栈搬一趟会爆栈
    fn heap_zeroed<T>() -> Box<T> {
        let layout = Layout::new::<T>();
        unsafe {
            let p = alloc_zeroed(layout).cast::<T>();
            assert!(!p.is_null());
            Box::from_raw(p)
        }
    }

    fn zeroed() -> Box<Shared> {
        heap_zeroed()
    }

    #[test]
    fn init_marks_valid() {
        let s = zeroed();
        assert!(!s.valid());
        s.init();
        assert!(s.valid());
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = zeroed();
        s.init();
        let mut snap = Snapshot { cursor_x: 12, foreground: 0xABCD, ..Default::default() };
        snap.keys[0x41] = 0x80;
        s.write_snapshot(&snap);
        let got = s.read_snapshot().unwrap();
        assert_eq!((got.cursor_x, got.foreground, got.keys[0x41]), (12, 0xABCD, 0x80));
    }

    #[test]
    fn raw_ring_by_handle() {
        let s = zeroed();
        s.init();
        let ev = |x| RawEvent { kind: RAW_KIND_MOUSE, mouse_last_x: x, ..Default::default() };
        let first = s.push_raw(ev(1));
        assert_eq!(s.raw_from_handle(raw_handle(first)).unwrap().mouse_last_x, 1);
        // 还没写到的序号不认
        assert!(s.raw_from_handle(raw_handle(first + 1)).is_none());
        // 灌满一圈后最早的被覆写
        for i in 0..RAW_RING as i32 {
            s.push_raw(ev(i + 2));
        }
        assert!(s.raw_at(first).is_none());
        let last = s.raw_head() - 1;
        assert_eq!(s.raw_from_handle(raw_handle(last)).unwrap().mouse_last_x, RAW_RING as i32 + 1);
        // 不带标记的句柄是系统真句柄
        assert!(s.raw_from_handle(0x1234_5678).is_none());
    }

    /// 第 seq 个包:`frames` 放 seq,`rate` 借来放数据长度,数据每个字节都是 seq 的低 8 位。
    /// 读方回调里据此核对内容,串了位或读到残留都能看出来
    fn audio_packet(seq: u32, size: usize) -> (AudioHeader, Vec<u8>) {
        let h = AudioHeader {
            stream: 1 + seq % 3,
            rate: size as u32,
            frames: seq,
            channels: 2,
            format: AUDIO_F32,
            ..Default::default()
        };
        (h, vec![seq as u8; size])
    }

    /// 回调拿到的数据 = 原数据 + 补到 32 对齐的零
    fn check_packet(h: &AudioHeader, d: &[u8]) {
        let size = h.rate as usize;
        assert_eq!(d.len(), (AUDIO_HEADER_BYTES + size).next_multiple_of(AUDIO_ALIGN) - AUDIO_HEADER_BYTES);
        assert_eq!(h.len as usize, AUDIO_HEADER_BYTES + d.len());
        assert!(d[..size].iter().all(|&b| b == h.frames as u8), "seq {} 的数据被串了", h.frames);
        assert!(d[size..].iter().all(|&b| b == 0), "seq {} 的对齐填充不是零", h.frames);
    }

    #[test]
    fn audio_header_is_32_bytes() {
        assert_eq!(size_of::<AudioHeader>(), 32);
        assert_eq!(size_of::<AudioRing>(), 16 + AUDIO_RING_BYTES);
    }

    #[test]
    fn audio_roundtrip_wraps() {
        let ring: Box<AudioRing> = heap_zeroed();
        let mut reader = AudioReader::new(&ring);
        // 大小不一的包:大包会撞上环尾放不下的情况,总量三圈多
        let sizes = [3840usize, 17, 200_000, 1, 100_000, 31, 64, 250_000, 0, 992];
        let (mut seq, mut written) = (0u32, 0u64);
        while written < 3 * AUDIO_RING_BYTES as u64 + 1000 {
            // 一批 1..=4 个包,一起读,顺序和内容都要对
            let mut expect = vec![];
            for _ in 0..1 + seq % 4 {
                seq += 1;
                let (h, d) = audio_packet(seq, sizes[seq as usize % sizes.len()]);
                assert!(unsafe { ring.write(h, &d) });
                written += (AUDIO_HEADER_BYTES + d.len()).next_multiple_of(AUDIO_ALIGN) as u64;
                expect.push(seq);
            }
            let mut got = vec![];
            let lost = reader.drain(&ring, |h, d| {
                check_packet(h, d);
                got.push(h.frames);
            });
            assert!(!lost);
            assert_eq!(got, expect);
        }
        // head 比所有包长之和多出来的,就是回绕时写的 PAD 包
        assert!(ring.head.load(Ordering::Relaxed) > written);
        // 空读不回调也不报丢
        assert!(!reader.drain(&ring, |_, _| panic!("没有新包")));
    }

    #[test]
    fn audio_size_limit() {
        let ring: Box<AudioRing> = heap_zeroed();
        let mut reader = AudioReader::new(&ring);
        let big = vec![7u8; AUDIO_RING_BYTES / 4];
        let (h, _) = audio_packet(7, big.len());
        assert!(unsafe { ring.write(h, &big) });
        assert!(!unsafe { ring.write(h, &vec![0u8; AUDIO_RING_BYTES / 4 + 1]) });
        let mut n = 0;
        assert!(!reader.drain(&ring, |h, d| {
            check_packet(h, d);
            n += 1;
        }));
        assert_eq!(n, 1);
    }

    #[test]
    fn audio_overrun_reports_lost_without_garbage() {
        let ring: Box<AudioRing> = heap_zeroed();
        let mut reader = AudioReader::new(&ring);
        let mut seq = 0u32;
        // 写超过一圈,一直不读
        while ring.head.load(Ordering::Relaxed) < AUDIO_RING_BYTES as u64 * 3 / 2 {
            seq += 1;
            let (h, d) = audio_packet(seq, 3840);
            assert!(unsafe { ring.write(h, &d) });
        }
        let mut called = 0;
        assert!(reader.drain(&ring, |_, _| called += 1));
        assert_eq!(called, 0, "被覆盖的数据不能回调");
        // 重新同步之后,只收到新写的包
        seq += 1;
        let (h, d) = audio_packet(seq, 100);
        assert!(unsafe { ring.write(h, &d) });
        let mut got = vec![];
        let lost = reader.drain(&ring, |h, d| {
            check_packet(h, d);
            got.push(h.frames);
        });
        assert!(!lost);
        assert_eq!(got, vec![seq]);
    }

    #[test]
    fn audio_backlog_limit() {
        // 积压没到 AUDIO_SAFE_BYTES 就全收,过了就整段丢掉
        for (backlog, expect_lost) in [(AUDIO_SAFE_BYTES - 10_000, false), (AUDIO_SAFE_BYTES + 10_000, true)] {
            let ring: Box<AudioRing> = heap_zeroed();
            let mut reader = AudioReader::new(&ring);
            let mut count = 0;
            while ring.head.load(Ordering::Relaxed) < backlog {
                count += 1;
                let (h, d) = audio_packet(count, 3840);
                assert!(unsafe { ring.write(h, &d) });
            }
            let mut got = 0;
            let lost = reader.drain(&ring, |h, d| {
                check_packet(h, d);
                got += 1;
            });
            assert_eq!(lost, expect_lost);
            assert_eq!(got, if expect_lost { 0 } else { count });
        }
    }

    /// 写方限速(约 10 MB/s,debug 构建里读方也跟得上)、读方隔一阵睡一觉被追上:
    /// 收到的每个包都必须完整、序号只增不减,丢了就丢了,不许收到残留
    #[test]
    fn audio_concurrent_never_delivers_garbage() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let ring: Box<AudioRing> = heap_zeroed();
        let mut reader = AudioReader::new(&ring);
        let stop = AtomicBool::new(false);
        let sizes = [3840usize, 60_000, 7, 30_000, 960, 31];
        let (mut delivered, mut lost_times, mut last) = (0u32, 0u32, 0u32);
        std::thread::scope(|s| {
            s.spawn(|| {
                let (start, mut seq, mut bytes) = (Instant::now(), 0u32, 0u128);
                while !stop.load(Ordering::Relaxed) {
                    seq += 1;
                    let (h, d) = audio_packet(seq, sizes[seq as usize % sizes.len()]);
                    assert!(unsafe { ring.write(h, &d) });
                    bytes += d.len() as u128;
                    // 10 字节/微秒 = 10 MB/s
                    while bytes > start.elapsed().as_micros() * 10 && !stop.load(Ordering::Relaxed) {
                        std::thread::yield_now();
                    }
                }
            });
            let start = Instant::now();
            let mut slept = false;
            while start.elapsed() < Duration::from_millis(900) {
                let lost = reader.drain(&ring, |h, d| {
                    check_packet(h, d);
                    assert!(h.frames > last, "序号倒退 {last} -> {}", h.frames);
                    last = h.frames;
                    delivered += 1;
                });
                lost_times += u32::from(lost);
                // 中途睡 120 ms:写方这段时间写的比环能留的多,读方醒来必须重新同步
                if !slept && start.elapsed() > Duration::from_millis(400) {
                    slept = true;
                    std::thread::sleep(Duration::from_millis(120));
                } else {
                    std::thread::sleep(Duration::from_micros(200));
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        eprintln!("收到 {delivered} 个包,丢数据 {lost_times} 次");
        assert!(delivered > 50);
        assert!(lost_times > 0, "没有触发过丢数据路径");
    }

    #[test]
    fn audio_abandoned_packet_is_header_only() {
        // 和静音包的位不重叠;放弃包只有包头,读方能原样收到流编号和标志
        assert_eq!(AUDIO_PACKET_ABANDONED & AUDIO_PACKET_SILENT, 0);
        let ring: Box<AudioRing> = heap_zeroed();
        let mut reader = AudioReader::new(&ring);
        let h = AudioHeader {
            stream: 3,
            rate: 48_000,
            channels: 2,
            format: AUDIO_F32,
            flags: AUDIO_PACKET_ABANDONED,
            ..Default::default()
        };
        assert!(unsafe { ring.write(h, &[]) });
        let mut got = vec![];
        let lost = reader.drain(&ring, |h, d| got.push((h.stream, h.flags, h.frames, d.len())));
        assert!(!lost);
        assert_eq!(got, [(3, AUDIO_PACKET_ABANDONED, 0, 0)]);
    }

    #[test]
    fn audio_alive_boundary() {
        let ring: Box<AudioRing> = heap_zeroed();
        // 从没写过心跳
        assert!(!ring.alive(0));
        assert!(!ring.alive(10_000));
        ring.alive_ms.store(1000, Ordering::Relaxed);
        assert!(ring.alive(1000));
        assert!(ring.alive(1000 + AUDIO_ALIVE_MS - 1));
        assert!(!ring.alive(1000 + AUDIO_ALIVE_MS));
        // 两边取时刻有先后,心跳比 now 还新不算过期
        assert!(ring.alive(990));
        // agent 退出时清零
        ring.alive_ms.store(0, Ordering::Relaxed);
        assert!(!ring.alive(1000));
    }
}
