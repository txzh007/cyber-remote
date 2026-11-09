//! 光标:`GetCursorPos`、`SetCursorPos`、`ClipCursor`。
//!
//! 只在快照带 `FLAG_OVERRIDE_CURSOR` 时接管。接管后 `GetCursorPos` 返回 agent 给的虚拟位置,
//! `SetCursorPos` 和 `ClipCursor` 直接吞掉:它们改的是整个会话共享的真实光标,放行的话
//! 实例会把主机鼠标拽走或锁在自己窗口里。
//!
//! 已知限制:每帧 `SetCursorPos` 回中再用 `GetCursorPos` 算位移的游戏,会读到不变的虚拟位置。
//! 这类游戏一般也读 Raw Input,位移走 Raw Input 路径。

use hook_abi::FLAG_OVERRIDE_CURSOR;
use windows::Win32::Foundation::{POINT, RECT};
use windows::core::BOOL;

use super::Hook;
use crate::state;

type GetCursorPosFn = unsafe extern "system" fn(*mut POINT) -> BOOL;
type SetCursorPosFn = unsafe extern "system" fn(i32, i32) -> BOOL;
type ClipCursorFn = unsafe extern "system" fn(*const RECT) -> BOOL;

static GET_CURSOR_POS: Hook<GetCursorPosFn> = Hook::new();
static SET_CURSOR_POS: Hook<SetCursorPosFn> = Hook::new();
static CLIP_CURSOR: Hook<ClipCursorFn> = Hook::new();

pub fn install() -> Result<(), String> {
    GET_CURSOR_POS.install("user32", "GetCursorPos", get_cursor_pos)?;
    SET_CURSOR_POS.install("user32", "SetCursorPos", set_cursor_pos)?;
    CLIP_CURSOR.install("user32", "ClipCursor", clip_cursor)?;
    Ok(())
}

/// 快照读得到且要求接管光标
fn overriding() -> Option<hook_abi::Snapshot> {
    state::snapshot().filter(|s| s.flags & FLAG_OVERRIDE_CURSOR != 0)
}

unsafe extern "system" fn get_cursor_pos(pt: *mut POINT) -> BOOL {
    if !pt.is_null()
        && let Some(snap) = overriding()
    {
        unsafe { pt.write(POINT { x: snap.cursor_x, y: snap.cursor_y }) };
        return BOOL::from(true);
    }
    match GET_CURSOR_POS.original() {
        Some(f) => unsafe { f(pt) },
        None => BOOL::from(false),
    }
}

unsafe extern "system" fn set_cursor_pos(x: i32, y: i32) -> BOOL {
    if overriding().is_some() {
        return BOOL::from(true);
    }
    match SET_CURSOR_POS.original() {
        Some(f) => unsafe { f(x, y) },
        None => BOOL::from(false),
    }
}

unsafe extern "system" fn clip_cursor(rect: *const RECT) -> BOOL {
    if overriding().is_some() {
        return BOOL::from(true);
    }
    match CLIP_CURSOR.original() {
        Some(f) => unsafe { f(rect) },
        None => BOOL::from(false),
    }
}
