//! 把目标进程的多个顶层窗口合成到一张画布上。
//!
//! WGC 一次只能采一个窗口,而菜单、右键菜单、tooltip、下拉框都是独立的顶层窗口。
//! 这里只负责"有哪些窗口、各自画在画布的哪个位置";真正的像素合成在 `capture.rs`
//! 里做:每个窗口一个 WGC 会话,按 Z 序从底到顶逐块 `CopySubresourceRegion`。

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GW_HWNDNEXT, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, WS_CHILD, WS_EX_TRANSPARENT,
};

/// 一个参与合成的窗口,以及它在输出画布上的位置(像素)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub hwnd: HWND,
    /// 画布原点(主窗口客户区左上角)到本窗口 DWM 边框左上角的偏移
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 画布尺寸(像素,偶数)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

/// 窗口的 DWM 扩展边框(不含阴影)。失败时退回普通窗口矩形
fn frame_rect(hwnd: HWND) -> RECT {
    let mut rc = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rc as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .is_err()
            || GetWindowRect(hwnd, &mut rc).is_err()
        {
            return rc;
        }
    }
    rc
}

fn style(hwnd: HWND) -> (i32, i32) {
    unsafe { (GetWindowLongW(hwnd, GWL_STYLE), GetWindowLongW(hwnd, GWL_EXSTYLE)) }
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// 从 `top` 出发按 Z 序往桌面方向走,收集同一进程、可见、属于这次会话的顶层窗口。
/// 顺序是底 → 顶(后面画的盖住前面)。
pub fn layers(top: HWND, client: RECT, target_pid: u32) -> Vec<Layer> {
    let mut out = Vec::new();
    let mut h = top;
    loop {
        if h.is_invalid() {
            break;
        }
        let (style, ex) = style(h);
        let visible = unsafe { IsWindowVisible(h) }.as_bool();
        // 同一进程、不是子窗口、可见、不是点击穿透的窗口。
        // 不排除 tooltip:很多应用的 tooltip 是 WS_EX_TOOLWINDOW,排除掉就看不到提示了
        let ours = pid_of(h) == target_pid
            && style & WS_CHILD.0 as i32 == 0
            && visible
            && ex & WS_EX_TRANSPARENT.0 as i32 == 0;
        if ours {
            let rc = frame_rect(h);
            // `top` 本身只取客户区(裁掉标题栏和边框),其余窗口按完整边框放进画布
            let (left, top_px) = if h == top { (client.left, client.top) } else { (rc.left, rc.top) };
            let (right, bottom) = if h == top { (client.right, client.bottom) } else { (rc.right, rc.bottom) };
            let (w, hgt) = (right - left, bottom - top_px);
            if w >= 1 && hgt >= 1 {
                out.push(Layer {
                    hwnd: h,
                    x: left - client.left,
                    y: top_px - client.top,
                    width: w as u32,
                    height: hgt as u32,
                });
            }
        }
        match unsafe { GetWindow(h, GW_HWNDNEXT) } {
            Ok(next) => h = next,
            Err(_) => break,
        }
    }
    out.reverse(); // 上面的循环是顶 → 底,翻过来让主窗口先画
    out
}

/// 画布尺寸:主窗口客户区至少这么大,再扩展到容纳所有弹出窗口。取偶数,4:2:0 需要
pub fn canvas(client: RECT, layers: &[Layer]) -> Canvas {
    let mut w = (client.right - client.left) as u32;
    let mut h = (client.bottom - client.top) as u32;
    for l in layers {
        w = w.max((l.x + l.width as i32).max(0) as u32);
        h = h.max((l.y + l.height as i32).max(0) as u32);
    }
    Canvas { width: (w & !1).max(16), height: (h & !1).max(16) }
}