//! 窗口采集流水线:WGC 按窗口采集 → GPU 裁掉非客户区 → NVENC。
//!
//! 全部在一个专用线程里跑;FrameArrived 回调只负责唤醒这个线程。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use tokio::sync::broadcast;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HMODULE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetClassNameW, GetClientRect, GetWindow, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE, ShowWindow,
};
use windows::core::{BOOL, Interface, PWSTR};

use super::compositor::{self, Canvas, Layer};
use super::display;
use super::hooklink::{self, HookLink};
use super::nvenc::{self, Encoder, EncoderConfig};
use crate::host::{EncodedFrame, ProbeTiming, Stream, Target};

/// 内容不变时 WGC 不出帧;隔这么久重编一次,让新加入的观众也能拿到画面
const IDLE_REENCODE: Duration = Duration::from_millis(500);
/// 丢包恢复靠浏览器的 PLI/FIR(见 keyframe.rs);定期 IDR 只防 PLI 也丢了的极端情况。
/// IDR 比 P 帧大好几倍,太频繁会周期性糊一下
const PERIODIC_IDR: Duration = Duration::from_secs(20);

/// 延迟探针投递后这么久还没有新帧来认领(点击没改变画面),就丢掉
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 一次延迟探针点击(PLAN 0.8):输入线程投递消息后登记,采集线程拿到的下一帧认领它
#[derive(Clone, Copy, Debug)]
pub struct PendingProbe {
    pub id: u32,
    pub received: Instant,
    pub posted: Instant,
}

/// 采集线程的对外句柄
#[derive(Clone)]
pub struct Pipeline {
    pub frames: broadcast::Sender<EncodedFrame>,
    keyframe: Arc<AtomicBool>,
    /// 正在采集的窗口句柄,0 表示还没找到或已关闭。HWND 不是 Send,这里存数值
    window: Arc<AtomicIsize>,
    hook: Arc<Mutex<HookState>>,
    probe: Arc<Mutex<Option<PendingProbe>>>,
    /// 整屏采集时画面对应的屏幕区域(物理像素),注入按它换算坐标;None 表示画面就是窗口客户区
    view: Arc<Mutex<Option<RECT>>>,
    /// 整屏采集改过窗口的样式和位置,退出时还原
    placement: Arc<Mutex<Option<display::Placement>>>,
}

/// 输入隔离的状态:连接对象,以及给浏览器 HUD 显示的一句话
struct HookState {
    link: Option<HookLink>,
    note: String,
}

impl Pipeline {
    /// 输入隔离当前状态,给浏览器 HUD 显示
    pub fn hook_note(&self) -> String {
        self.hook.lock().unwrap_or_else(|e| e.into_inner()).note.clone()
    }

    /// 在输入隔离连接上做点事;没有连接(没启用或装失败)时不执行
    pub fn with_hook(&self, f: impl FnOnce(&mut HookLink)) {
        if let Some(link) = self.hook.lock().unwrap_or_else(|e| e.into_inner()).link.as_mut() {
            f(link);
        }
    }

    /// 撤掉输入隔离,目标进程里的钩子回落到真实输入
    pub fn detach_hook(&self) {
        let mut h = self.hook.lock().unwrap_or_else(|e| e.into_inner());
        if h.link.take().is_some() {
            h.note = "输入隔离: 已撤销".into();
        }
    }

    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn window(&self) -> Option<HWND> {
        let v = self.window.load(Ordering::Relaxed);
        (v != 0).then_some(HWND(v as *mut _))
    }

    pub fn set_probe(&self, probe: PendingProbe) {
        *self.probe.lock().unwrap_or_else(|e| e.into_inner()) = Some(probe);
    }

    /// 整屏采集时画面对应的屏幕区域,见 `view` 字段
    pub fn view(&self) -> Option<RECT> {
        *self.view.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 还原整屏采集改过的窗口样式和位置。可以在任意线程调用,只还原一次
    pub fn restore_window(&self) {
        let placement = self.placement.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(pl) = placement {
            pl.restore();
        }
    }

    /// 新帧认领探针。投递之前就合成好的帧(点击前已在路上的旧画面)不算;超时的直接丢掉
    fn take_probe(&self, composed: Option<Instant>, now: Instant) -> Option<PendingProbe> {
        let mut slot = self.probe.lock().unwrap_or_else(|e| e.into_inner());
        let p = (*slot)?;
        if now.duration_since(p.posted) > PROBE_TIMEOUT {
            *slot = None;
            return None;
        }
        if composed.is_some_and(|c| c < p.posted) {
            return None;
        }
        slot.take()
    }
}

pub fn spawn(target: Target, stream: Stream) -> Result<Pipeline> {
    let (frames, _) = broadcast::channel(8);
    let pipeline = Pipeline {
        frames,
        keyframe: Arc::new(AtomicBool::new(true)),
        window: Arc::new(AtomicIsize::new(0)),
        probe: Arc::new(Mutex::new(None)),
        view: Arc::new(Mutex::new(None)),
        placement: Arc::new(Mutex::new(None)),
        hook: Arc::new(Mutex::new(HookState { link: None, note: "输入隔离: 未启用".into() })),
    };
    let p = pipeline.clone();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = run(&p, &target, stream) {
                tracing::error!("采集线程退出: {e:#}");
            }
            p.window.store(0, Ordering::Relaxed);
            p.detach_hook();
            p.restore_window();
        })?;
    Ok(pipeline)
}

/// 装输入隔离。失败不影响推流,只是退回无隔离模式,原因写进 HUD 的提示
fn attach_hook(p: &Pipeline, hwnd: HWND) {
    let result = std::env::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|exe| hooklink::attach(hwnd, &exe.with_file_name("cyber_remote_hook.dll")));
    let mut h = p.hook.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(link) => {
            tracing::info!("输入隔离已启用");
            h.link = Some(link);
            h.note = "输入隔离: 已启用".into();
        }
        Err(e) => {
            tracing::warn!("输入隔离未启用: {e:#}");
            h.note = format!("输入隔离: 未启用({e:#})");
        }
    }
}

fn run(p: &Pipeline, target: &Target, stream: Stream) -> Result<()> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
    let hwnd = find_or_launch(target)?;
    p.window.store(hwnd.0 as isize, Ordering::Relaxed);
    if target.hook {
        attach_hook(p, hwnd);
    }
    let (device, context) = create_nvidia_device()?;
    let api = nvenc::Api::load()?;
    if let Some(n) = target.display {
        // 对照实验:CYBER_REMOTE_DISPLAY_CAPTURE=window 时窗口照样铺满那块屏,但仍按窗口采集(WGC),
        // 在同样大小下比较桌面复制和 WGC 的延迟。画面就是客户区,注入不用 view
        if std::env::var("CYBER_REMOTE_DISPLAY_CAPTURE").as_deref() != Ok("window") {
            return run_display(p, hwnd, n, &device, &context, &api, stream);
        }
        let (_, _, work) = display::find_output(&device, n)?;
        let placement = display::place(hwnd, work)?;
        *p.placement.lock().unwrap_or_else(|e| e.into_inner()) = Some(placement);
        tracing::info!("窗口已铺满 \\\\.\\DISPLAY{n} 的工作区,按窗口采集(对照实验)");
    }

    let winrt_device: IDirect3DDevice = unsafe {
        let dxgi: IDXGIDevice = device.cast()?;
        CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?
    };
    // 上次运行可能已经删掉了任务栏按钮,没有按钮 CreateForWindow 会报 E_INVALIDARG,先加回来
    if let Err(e) = taskbar(hwnd, true) {
        tracing::warn!("恢复任务栏按钮失败: {e:#}");
    }
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
    let mut pool_size = item.Size()?;
    tracing::info!("采集目标尺寸 {}x{}", pool_size.Width, pool_size.Height);

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        pool_size,
    )?;
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
        let _ = wake_tx.send(());
        Ok(())
    }))?;
    let session = pool.CreateCaptureSession(&item)?;
    session.SetIsCursorCaptureEnabled(false)?;
    session.StartCapture()?;
    // 必须在 CreateForWindow 之后:没有任务栏按钮的窗口 CreateForWindow 会报 E_INVALIDARG。
    // 不影响推流,失败只记日志
    match taskbar(hwnd, false) {
        Ok(()) => tracing::info!("已从任务栏移除窗口"),
        Err(e) => tracing::warn!("从任务栏移除窗口失败: {e:#}"),
    }

    let mut out: Option<(ID3D11Texture2D, Encoder)> = None;
    let mut tx = FrameSender::new();
    let mut logged_crop = None;

    loop {
        let woke = wake_rx.recv_timeout(Duration::from_millis(50));
        // 这一帧认领到的延迟探针,以及它的合成、采集时刻
        let mut frame_probe = None;
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            bail!("目标窗口已关闭");
        }

        // 取最新一帧,丢掉积压的旧帧
        let mut latest = None;
        if woke.is_ok() {
            while let Ok(frame) = pool.TryGetNextFrame() {
                latest = Some(frame);
            }
        }

        if let Some(frame) = latest {
            let content = frame.ContentSize()?;
            if content != pool_size {
                pool_size = content;
                pool.Recreate(
                    &winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    pool_size,
                )?;
                continue;
            }
            if unsafe { IsIconic(hwnd) }.as_bool() {
                continue;
            }
            let captured = Instant::now();
            let composed = composed_at(&frame);
            frame_probe = p.take_probe(composed, captured).map(|pp| (pp, composed, captured));
            let src: ID3D11Texture2D = unsafe {
                frame
                    .Surface()?
                    .cast::<IDirect3DDxgiInterfaceAccess>()?
                    .GetInterface()?
            };
            let Some(crop) = client_crop(hwnd, content)? else {
                continue;
            };
            if logged_crop != Some(crop) {
                tracing::info!(
                    "窗口 {}x{},裁剪客户区 x={} y={} {}x{}",
                    content.Width,
                    content.Height,
                    crop.left,
                    crop.top,
                    crop.right - crop.left,
                    crop.bottom - crop.top
                );
                logged_crop = Some(crop);
            }
            let (w, h) = (crop.right - crop.left, crop.bottom - crop.top);

            let size_changed = out.as_ref().is_none_or(|(tex, _)| {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                unsafe { tex.GetDesc(&mut d) };
                (d.Width, d.Height) != (w, h)
            });
            if size_changed {
                drop(out.take()); // 先释放旧编码器再建新的
                let tex = create_texture(&device, w, h)?;
                let cfg = EncoderConfig {
                    width: w,
                    height: h,
                    fps: stream.fps,
                    bitrate: stream.bitrate,
                };
                let enc = Encoder::new(&api, &device, &tex, &cfg).context("创建 NVENC 编码器")?;
                tracing::info!(
                    "编码器就绪 {w}x{h} H.264 {}fps CBR {} kbps",
                    stream.fps,
                    stream.bitrate / 1000
                );
                out = Some((tex, enc));
                p.request_keyframe();
            }
            let (tex, _) = out.as_ref().unwrap();
            let bx = D3D11_BOX {
                left: crop.left,
                top: crop.top,
                front: 0,
                right: crop.right,
                bottom: crop.bottom,
                back: 1,
            };
            unsafe { context.CopySubresourceRegion(tex, 0, 0, 0, 0, &src, 0, Some(&bx)) };
            drop(frame);
        } else if out.is_none() || tx.idle(p) {
            continue;
        }

        let Some((_, enc)) = out.as_mut() else { continue };
        tx.send(p, enc, frame_probe)?;
    }
}

/// 整屏采集(`Target::display`,见 display.rs):窗口铺满那块屏,DXGI 桌面复制采整块屏
fn run_display(
    p: &Pipeline,
    hwnd: HWND,
    n: u32,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    api: &nvenc::Api,
    stream: Stream,
) -> Result<()> {
    // 窗口铺满工作区(不和任务栏抢),只采这块区域
    let (output, screen, work) = display::find_output(device, n)?;
    let placement = display::place(hwnd, work)?;
    *p.placement.lock().unwrap_or_else(|e| e.into_inner()) = Some(placement);
    *p.view.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
    // 和按窗口采集一样,实例窗口不占任务栏,退出时 shutdown 还回去
    if let Err(e) = taskbar(hwnd, false) {
        tracing::warn!("从任务栏移除窗口失败: {e:#}");
    }
    let full = ((screen.right - screen.left) as u32, (screen.bottom - screen.top) as u32);
    // H.264 4:2:0 要偶数宽高
    let (w, h) = (((work.right - work.left) & !1) as u32, ((work.bottom - work.top) & !1) as u32);
    let (x0, y0) = ((work.left - screen.left) as u32, (work.top - screen.top) as u32);
    let mut dup = display::Duplication::new(output, device.clone())?;
    let tex = create_texture(device, w, h)?;
    let cfg = EncoderConfig { width: w, height: h, fps: stream.fps, bitrate: stream.bitrate };
    let mut enc = Encoder::new(api, device, &tex, &cfg).context("创建 NVENC 编码器")?;
    tracing::info!(
        "整屏采集 \\\\.\\DISPLAY{n} {w}x{h},窗口已铺满这块屏的工作区;编码器 H.264 {}fps CBR {} kbps",
        stream.fps,
        stream.bitrate / 1000
    );
    p.request_keyframe();
    let bx = D3D11_BOX { left: x0, top: y0, front: 0, right: x0 + w, bottom: y0 + h, back: 1 };
    let mut tx = FrameSender::new();
    // 第一帧到之前纹理是空的,不编码
    let mut ready = false;
    loop {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            bail!("目标窗口已关闭");
        }
        let mut frame_probe = None;
        match dup.acquire(50)? {
            Some(frame) => {
                let captured = Instant::now();
                if frame.size() != full {
                    let (fw, fh) = frame.size();
                    bail!("这块屏的分辨率变了({}x{} → {fw}x{fh}),重启 agent", full.0, full.1);
                }
                frame_probe = p
                    .take_probe(frame.presented, captured)
                    .map(|pp| (pp, frame.presented, captured));
                unsafe {
                    context.CopySubresourceRegion(&tex, 0, 0, 0, 0, &frame.texture, 0, Some(&bx))
                };
                drop(frame);
                dup.release();
                ready = true;
            }
            None if !ready || tx.idle(p) => continue,
            None => {}
        }
        tx.send(p, &mut enc, frame_probe)?;
    }
}

/// 编码并广播一帧,两种采集方式共用:请求的关键帧、定期 IDR、延迟探针的时刻
struct FrameSender {
    start: Instant,
    last_encode: Instant,
    last_idr: Instant,
}

impl FrameSender {
    fn new() -> Self {
        let now = Instant::now();
        Self { start: now, last_encode: now, last_idr: now }
    }

    /// 没有新画面时可以跳过:离上次编码不到 IDLE_REENCODE,也没人要关键帧
    fn idle(&self, p: &Pipeline) -> bool {
        self.last_encode.elapsed() < IDLE_REENCODE && !p.keyframe.load(Ordering::Relaxed)
    }

    fn send(
        &mut self,
        p: &Pipeline,
        enc: &mut Encoder,
        probe: Option<(PendingProbe, Option<Instant>, Instant)>,
    ) -> Result<()> {
        let mut idr = p.keyframe.swap(false, Ordering::Relaxed);
        if self.last_idr.elapsed() >= PERIODIC_IDR {
            idr = true;
        }
        let now = Instant::now();
        let data = enc.encode(idr, now.duration_since(self.start).as_micros() as u64)?;
        let encoded = Instant::now();
        let probe = probe.map(|(pp, composed, captured)| ProbeTiming {
            id: pp.id,
            received: pp.received,
            posted: pp.posted,
            composed,
            captured,
            encoded,
        });
        self.last_encode = now;
        if idr {
            self.last_idr = now;
        }
        // 没有观众时 send 返回 Err,忽略即可
        let _ = p.frames.send(EncodedFrame { data: Bytes::from(data), captured_at: now, probe });
        Ok(())
    }
}

/// 一个窗口的 WGC 会话。合成时每个参与窗口各一个
struct Source {
    hwnd: HWND,
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    size: SizeInt32,
    /// 本次轮询取到的最新一帧,下次轮询时丢弃
    latest: Option<ID3D11Texture2D>,
}

impl Source {
    fn new(device: &IDirect3DDevice, interop: &IGraphicsCaptureItemInterop, hwnd: HWND) -> Result<Self> {
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(false)?;
        session.StartCapture()?;
        Ok(Self { hwnd, pool, session, size, latest: None })
    }

    /// 取最新一帧,丢掉积压的旧帧。尺寸变化时重建帧池,这一残缺帧丢弃
    fn poll(&mut self, device: &IDirect3DDevice) -> Result<()> {
        let mut last = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            let content = frame.ContentSize()?;
            if content != self.size {
                self.size = content;
                self.pool.Recreate(
                    device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    self.size,
                )?;
                self.latest = None;
                return Ok(());
            }
            let tex: ID3D11Texture2D = unsafe {
                frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()?
            };
            last = Some(tex);
        }
        if last.is_some() {
            self.latest = last;
        }
        Ok(())
    }

    fn stop(&self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// 客户区在采集纹理里的位置(像素,右/下开区间)
#[derive(Clone, Copy, PartialEq, Debug)]
struct Crop {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

/// WGC 的帧以 DWM 扩展边框(不含阴影)为原点。客户区偏移 = 客户区屏幕坐标 − 边框左上角。
/// 宽高向下取偶数,H.264 4:2:0 需要。
fn client_crop(hwnd: HWND, content: SizeInt32) -> Result<Option<Crop>> {
    unsafe {
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client)?;
        let mut origin = POINT::default();
        if !ClientToScreen(hwnd, &mut origin).as_bool() {
            bail!("ClientToScreen 失败");
        }
        let mut frame = RECT::default();
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )?;

        let left = (origin.x - frame.left).clamp(0, content.Width);
        let top = (origin.y - frame.top).clamp(0, content.Height);
        let w = (client.right - client.left).min(content.Width - left) & !1;
        let h = (client.bottom - client.top).min(content.Height - top) & !1;
        if w < 16 || h < 16 {
            return Ok(None);
        }
        Ok(Some(Crop {
            left: left as u32,
            top: top as u32,
            right: (left + w) as u32,
            bottom: (top + h) as u32,
        }))
    }
}

/// 帧的合成时刻。`SystemRelativeTime` 是 QPC 时间(100ns 为单位),按当前 QPC 换算成 `Instant`;
/// 读不到或差值不合理(负数或超过 1 秒)时返回 None
fn composed_at(frame: &Direct3D11CaptureFrame) -> Option<Instant> {
    let rendered = frame.SystemRelativeTime().ok()?.Duration;
    let (mut qpc, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut qpc).ok()?;
        QueryPerformanceFrequency(&mut freq).ok()?;
    }
    let now = Instant::now();
    let now_100ns = (i128::from(qpc) * 10_000_000 / i128::from(freq.max(1))) as i64;
    let age = now_100ns - rendered;
    if !(0..10_000_000).contains(&age) {
        return None;
    }
    now.checked_sub(Duration::from_nanos(age as u64 * 100))
}

/// 按 `Target` 认窗口。`pid` 有值时只认这个进程的窗口(`fresh` 新启动的进程)
#[derive(Clone, Copy)]
struct Matcher<'a> {
    target: &'a Target,
    pid: Option<u32>,
}

impl Matcher<'_> {
    /// 填了类名或标题就只按它们认窗口(启动器拉起别的 exe 时也能找到);
    /// 都没填就认 exe 自己进程的窗口
    fn matches(&self, hwnd: HWND) -> bool {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return false;
            }
        }
        if let Some(pid) = self.pid {
            // 新启动的进程:先认进程,再按类名/标题进一步筛(都没填就是它的第一个顶层窗口)
            let mut owner = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
            if owner != pid {
                return false;
            }
        } else if self.target.class.is_none() && self.target.title.is_none() {
            return self.target.exe.as_ref().is_some_and(|exe| window_exe_is(hwnd, exe));
        }
        if let Some(class) = &self.target.class {
            let mut buf = [0u16; 256];
            let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
            if String::from_utf16_lossy(&buf[..n]) != *class {
                return false;
            }
        }
        if let Some(title) = &self.target.title {
            let mut buf = [0u16; 512];
            let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
            if !String::from_utf16_lossy(&buf[..n]).contains(title.as_str()) {
                return false;
            }
        }
        true
    }

    fn find(&self) -> Option<HWND> {
        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let (matcher, found) = unsafe { &mut *(lparam.0 as *mut (&Matcher, Option<HWND>)) };
            if matcher.matches(hwnd) {
                *found = Some(hwnd);
                return false.into(); // 停止枚举
            }
            true.into()
        }
        let mut state: (&Matcher, Option<HWND>) = (self, None);
        // 回调返回 false 时 EnumWindows 也会报错,结果只看 state
        let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut state as *mut _ as isize)) };
        state.1
    }
}

/// 窗口所属进程是不是这个 exe。exe 带目录就比完整路径,否则只比文件名
fn window_exe_is(hwnd: HWND, exe: &Path) -> bool {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }
    .is_ok();
    let _ = unsafe { CloseHandle(process) };
    if !ok {
        return false;
    }
    let image = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    if exe.parent().is_some_and(|d| !d.as_os_str().is_empty()) {
        lower(&image) == lower(exe)
    } else {
        image.file_name().map(|n| lower(Path::new(n))) == Some(lower(exe))
    }
}

/// 游戏启动可能比较慢
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);

fn find_or_launch(target: &Target) -> Result<HWND> {
    if !target.fresh
        && let Some(h) = (Matcher { target, pid: None }).find()
    {
        tracing::info!("找到已打开的窗口");
        // 最小化的窗口 WGC 只给标题栏大小的帧,先还原
        if unsafe { IsIconic(h) }.as_bool() {
            let _ = unsafe { ShowWindow(h, SW_RESTORE) };
            std::thread::sleep(Duration::from_millis(300));
        }
        return Ok(h);
    }
    let exe = target.exe.as_ref().ok_or_else(|| anyhow!("没找到匹配的窗口,也没指定要启动的程序"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&target.args);
    // 相对路径的资源一般按 exe 所在目录找
    if let Some(dir) = exe.parent().filter(|d| !d.as_os_str().is_empty()) {
        cmd.current_dir(dir);
    }
    let child = cmd.spawn().with_context(|| format!("启动 {}", exe.display()))?;
    // 多开时只认刚启动的这个进程的窗口,别抢到另一路已经在用的窗口
    let scoped = Matcher { target, pid: target.fresh.then(|| child.id()) };
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(h) = scoped.find() {
            tracing::info!("已启动 {} (进程 {})", exe.display(), child.id());
            return Ok(h);
        }
    }
    Err(anyhow!("{} 秒内没等到 {} 的窗口", LAUNCH_TIMEOUT.as_secs(), exe.display()))
}

/// 增删窗口的任务栏按钮,窗口本身照常显示,WGC 采集和输入注入不受影响。
/// 不改窗口样式;窗口重新 ShowWindow 或 Explorer 重启后按钮会回来。
fn taskbar(hwnd: HWND, show: bool) -> Result<()> {
    unsafe {
        let tb: ITaskbarList = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)?;
        tb.HrInit()?;
        if show { tb.AddTab(hwnd)? } else { tb.DeleteTab(hwnd)? }
    }
    Ok(())
}

/// 退出前把任务栏按钮还给窗口,否则用户找不到它。可以在任意线程调用
pub fn restore_taskbar(hwnd: HWND) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    match taskbar(hwnd, true) {
        Ok(()) => tracing::info!("已恢复任务栏按钮"),
        Err(e) => tracing::warn!("恢复任务栏按钮失败: {e:#}"),
    }
}

/// 在 NVIDIA 显卡上建 D3D11 设备,NVENC 要求设备在同一块 GPU 上
fn create_nvidia_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut i = 0;
        let adapter: IDXGIAdapter1 = loop {
            let a = factory
                .EnumAdapters1(i)
                .map_err(|_| anyhow!("没找到 NVIDIA 显卡"))?;
            let desc = a.GetDesc1()?;
            if desc.VendorId == 0x10DE {
                let name = String::from_utf16_lossy(&desc.Description);
                tracing::info!("使用显卡 {}", name.trim_end_matches('\0'));
                break a;
            }
            i += 1;
        };

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        Ok((device.unwrap(), context.unwrap()))
    }
}

fn create_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.unwrap())
}
