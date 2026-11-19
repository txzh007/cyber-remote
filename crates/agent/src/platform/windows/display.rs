//! 整屏采集(PLAN 第 8 节"每路一块虚拟屏"spike 的第一步):把目标窗口去掉边框铺满一块屏,
//! 用 DXGI 桌面复制采整块屏。比按窗口采集多三点:菜单、提示框这些弹出窗口天然在画面里;
//! 没有 WGC 的黄框;每帧带合成时刻(`LastPresentTime`)。先拿实体副屏验证,之后换成虚拟屏。
//! 这块屏上的东西都会被采进去,别的窗口挡在目标上面也一样。

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
    IDXGIOutput1, IDXGIOutputDuplication,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, GetWindowPlacement, HWND_NOTOPMOST, HWND_TOPMOST,
    IsIconic, IsZoomed, SW_RESTORE, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
    ShowWindow, WINDOWPLACEMENT, WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME,
    WS_EX_STATICEDGE, WS_EX_TOPMOST, WS_EX_WINDOWEDGE, WS_THICKFRAME,
};
use windows::core::Interface;

/// 设备所在显卡上叫 `\\.\DISPLAYn` 的屏。返回它、整块屏的区域、去掉任务栏的工作区
/// (屏幕坐标,物理像素)。副屏也可能有任务栏,窗口铺满工作区才不会被它盖住
pub fn find_output(device: &ID3D11Device, n: u32) -> Result<(IDXGIOutput1, RECT, RECT)> {
    let want = format!(r"\\.\DISPLAY{n}");
    let adapter = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()? };
    let mut seen = Vec::new();
    for i in 0.. {
        let Ok(output) = (unsafe { adapter.EnumOutputs(i) }) else { break };
        let desc = unsafe { output.GetDesc()? };
        let name = String::from_utf16_lossy(&desc.DeviceName);
        let name = name.trim_end_matches('\0');
        if name.eq_ignore_ascii_case(&want) && desc.AttachedToDesktop.as_bool() {
            let mut info =
                MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            let work = if unsafe { GetMonitorInfoW(desc.Monitor, &mut info) }.as_bool() {
                info.rcWork
            } else {
                desc.DesktopCoordinates
            };
            return Ok((output.cast()?, desc.DesktopCoordinates, work));
        }
        seen.push(name.to_owned());
    }
    bail!("NVIDIA 显卡上没有接着桌面的 {want},现有: {}", seen.join(", "))
}

/// 窗口原来的样式和位置,退出时还原
pub struct Placement {
    hwnd: isize,
    style: isize,
    ex_style: isize,
    placement: WINDOWPLACEMENT,
}

/// 去掉标题栏和边框,铺满 `rect`(屏幕坐标,物理像素)。菜单栏不算边框,会留着
pub fn place(hwnd: HWND, rect: RECT) -> Result<Placement> {
    let frame = (WS_CAPTION | WS_THICKFRAME).0 as isize;
    let edges =
        (WS_EX_DLGMODALFRAME | WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_STATICEDGE).0 as isize;
    unsafe {
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        GetWindowPlacement(hwnd, &mut placement).context("GetWindowPlacement")?;
        let saved = Placement {
            hwnd: hwnd.0 as isize,
            style: GetWindowLongPtrW(hwnd, GWL_STYLE),
            ex_style: GetWindowLongPtrW(hwnd, GWL_EXSTYLE),
            placement,
        };
        if IsZoomed(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, saved.style & !frame);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, saved.ex_style & !edges);
        // 挪到 DPI 不同的屏上会触发 WM_DPICHANGED,程序可能按建议尺寸改大小,所以再设一次。
        // 置顶:采的是屏上看得见的东西,别的窗口挡在上面也会被采进去;后台进程用 HWND_TOP
        // 抬不起别的进程的窗口(实测被副屏上原有的窗口挡住),TOPMOST 可以。不激活,不抢焦点
        for _ in 0..2 {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_FRAMECHANGED | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            )
            .context("SetWindowPos")?;
        }
        Ok(saved)
    }
}

impl Placement {
    /// 还原样式、位置和置顶状态。最后一步异步做:目标卡住时不拖住 agent 退出
    pub fn restore(&self) {
        let hwnd = HWND(self.hwnd as *mut _);
        // 置顶不能靠改扩展样式还原,得用 SetWindowPos
        let topmost = self.ex_style & WS_EX_TOPMOST.0 as isize != 0;
        unsafe {
            SetWindowLongPtrW(hwnd, GWL_STYLE, self.style);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, self.ex_style);
            let _ = SetWindowPlacement(hwnd, &self.placement);
            let _ = SetWindowPos(
                hwnd,
                Some(if topmost { HWND_TOPMOST } else { HWND_NOTOPMOST }),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
        tracing::info!("已还原窗口的边框和位置");
    }
}

/// 一块屏的 DXGI 桌面复制。画面有变化才出帧
pub struct Duplication {
    output: IDXGIOutput1,
    device: ID3D11Device,
    dup: Option<IDXGIOutputDuplication>,
    /// 手上有一帧还没还:AcquireNextFrame 之后要 ReleaseFrame,才会出下一帧
    held: bool,
}

/// 复制来的一帧:整块屏的纹理,和它的合成时刻
pub struct DupFrame {
    pub texture: ID3D11Texture2D,
    pub presented: Option<Instant>,
}

impl DupFrame {
    pub fn size(&self) -> (u32, u32) {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { self.texture.GetDesc(&mut d) };
        (d.Width, d.Height)
    }
}

impl Duplication {
    pub fn new(output: IDXGIOutput1, device: ID3D11Device) -> Result<Self> {
        let dup = unsafe { output.DuplicateOutput(&device) }.context("DuplicateOutput")?;
        Ok(Self { output, device, dup: Some(dup), held: false })
    }

    /// 等下一帧,最多等 `timeout_ms`。超时、只有光标动了、复制失效正在重建时返回 None。
    /// 拿到的帧用完尽快 `release`
    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Option<DupFrame>> {
        self.release();
        let Some(dup) = self.dup.clone() else {
            self.reopen();
            return Ok(None);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => self.held = true,
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            // 改分辨率、切到 UAC 安全桌面、全屏独占切换时复制会失效,要重建
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                tracing::warn!("桌面复制失效,重建");
                self.dup = None;
                self.reopen();
                return Ok(None);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        // 0 表示画面没变,只是光标动了
        if info.LastPresentTime == 0 {
            return Ok(None);
        }
        let texture = resource.context("AcquireNextFrame 没给画面")?.cast()?;
        Ok(Some(DupFrame { texture, presented: qpc_instant(info.LastPresentTime) }))
    }

    pub fn release(&mut self) {
        if std::mem::take(&mut self.held)
            && let Some(dup) = &self.dup
        {
            unsafe {
                let _ = dup.ReleaseFrame();
            }
        }
    }

    fn reopen(&mut self) {
        match unsafe { self.output.DuplicateOutput(&self.device) } {
            Ok(dup) => {
                tracing::info!("桌面复制已重建");
                self.dup = Some(dup);
            }
            // 还在安全桌面上之类,过一会儿再试
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

impl Drop for Duplication {
    fn drop(&mut self) {
        self.release();
    }
}

/// QPC 计数 → `Instant`。差值不合理(负数或超过 1 秒)时返回 None
fn qpc_instant(ticks: i64) -> Option<Instant> {
    let (mut now, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut now).ok()?;
        QueryPerformanceFrequency(&mut freq).ok()?;
    }
    let at = Instant::now();
    let age = now - ticks;
    if freq <= 0 || !(0..freq).contains(&age) {
        return None;
    }
    at.checked_sub(Duration::from_nanos(
        (i128::from(age) * 1_000_000_000 / i128::from(freq)) as u64,
    ))
}
