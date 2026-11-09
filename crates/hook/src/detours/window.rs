//! 窗口状态:`GetForegroundWindow`、`GetActiveWindow`、`GetFocus`、`GetCapture`。
//!
//! 前三个要快照带 `FLAG_OVERRIDE_FOREGROUND` 且对应字段非 0 才接管,让后台的实例
//! 以为自己在前台、有焦点,不会因为失焦而暂停。`GetCapture` 只看字段是否非 0。
//! 字段为 0 表示不覆盖,走真实 API。
//!
//! `SetCapture` 不钩:捕获状态属于调用线程自己的输入队列,多个实例是不同进程,互不影响。

use core::ffi::c_void;

use hook_abi::{FLAG_OVERRIDE_FOREGROUND, Snapshot};
use windows::Win32::Foundation::HWND;

use super::Hook;
use crate::state;

type GetWindowFn = unsafe extern "system" fn() -> HWND;

static GET_FOREGROUND_WINDOW: Hook<GetWindowFn> = Hook::new();
static GET_ACTIVE_WINDOW: Hook<GetWindowFn> = Hook::new();
static GET_FOCUS: Hook<GetWindowFn> = Hook::new();
static GET_CAPTURE: Hook<GetWindowFn> = Hook::new();

pub fn install() -> Result<(), String> {
    GET_FOREGROUND_WINDOW.install("user32", "GetForegroundWindow", get_foreground_window)?;
    GET_ACTIVE_WINDOW.install("user32", "GetActiveWindow", get_active_window)?;
    GET_FOCUS.install("user32", "GetFocus", get_focus)?;
    GET_CAPTURE.install("user32", "GetCapture", get_capture)?;
    Ok(())
}

/// 快照里的 HWND 数值 → 句柄。窗口句柄只有低 32 位有效,32 位进程截断没有损失
fn hwnd_of(v: u64) -> Option<HWND> {
    (v != 0).then_some(HWND(v as usize as *mut c_void))
}

/// 前台类字段:要求 `FLAG_OVERRIDE_FOREGROUND`
fn foreground_field(pick: fn(&Snapshot) -> u64) -> Option<HWND> {
    let snap = state::snapshot()?;
    if snap.flags & FLAG_OVERRIDE_FOREGROUND == 0 {
        return None;
    }
    hwnd_of(pick(&snap))
}

unsafe extern "system" fn get_foreground_window() -> HWND {
    if let Some(h) = foreground_field(|s| s.foreground) {
        return h;
    }
    match GET_FOREGROUND_WINDOW.original() {
        Some(f) => unsafe { f() },
        None => HWND::default(),
    }
}

unsafe extern "system" fn get_active_window() -> HWND {
    if let Some(h) = foreground_field(|s| s.active) {
        return h;
    }
    match GET_ACTIVE_WINDOW.original() {
        Some(f) => unsafe { f() },
        None => HWND::default(),
    }
}

unsafe extern "system" fn get_focus() -> HWND {
    if let Some(h) = foreground_field(|s| s.focus) {
        return h;
    }
    match GET_FOCUS.original() {
        Some(f) => unsafe { f() },
        None => HWND::default(),
    }
}

unsafe extern "system" fn get_capture() -> HWND {
    if let Some(h) = state::snapshot().and_then(|s| hwnd_of(s.capture)) {
        return h;
    }
    match GET_CAPTURE.original() {
        Some(f) => unsafe { f() },
        None => HWND::default(),
    }
}
