//! 按键状态:`GetAsyncKeyState`、`GetKeyState`、`GetKeyboardState`。
//!
//! 快照里 `keys[vk]` 的格式与 `GetKeyboardState` 一致:0x80 位表示按下,0x01 位表示
//! 切换状态(CapsLock 等)。agent 写快照时,Shift/Ctrl/Alt 要把通用键码和左右键码都置位。
//! 读不到快照(agent 崩在写到一半)时回落到真实 API。

use windows::core::BOOL;

use super::Hook;
use crate::state;

type GetAsyncKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyStateFn = unsafe extern "system" fn(i32) -> i16;
type GetKeyboardStateFn = unsafe extern "system" fn(*mut u8) -> BOOL;

static GET_ASYNC_KEY_STATE: Hook<GetAsyncKeyStateFn> = Hook::new();
static GET_KEY_STATE: Hook<GetKeyStateFn> = Hook::new();
static GET_KEYBOARD_STATE: Hook<GetKeyboardStateFn> = Hook::new();

pub fn install() -> Result<(), String> {
    GET_ASYNC_KEY_STATE.install("user32", "GetAsyncKeyState", get_async_key_state)?;
    GET_KEY_STATE.install("user32", "GetKeyState", get_key_state)?;
    GET_KEYBOARD_STATE.install("user32", "GetKeyboardState", get_keyboard_state)?;
    Ok(())
}

/// 越界的键码当作没按下,不能 panic(替身是 extern "system",panic 会直接 abort)
fn key_byte(keys: &[u8; 256], vkey: i32) -> u8 {
    usize::try_from(vkey).ok().and_then(|i| keys.get(i)).copied().unwrap_or(0)
}

unsafe extern "system" fn get_async_key_state(vkey: i32) -> i16 {
    if let Some(snap) = state::snapshot() {
        // 低位“上次调用后按过”不模拟
        return if key_byte(&snap.keys, vkey) & 0x80 != 0 { i16::MIN } else { 0 };
    }
    match GET_ASYNC_KEY_STATE.original() {
        Some(f) => unsafe { f(vkey) },
        None => 0,
    }
}

unsafe extern "system" fn get_key_state(vkey: i32) -> i16 {
    if let Some(snap) = state::snapshot() {
        let b = key_byte(&snap.keys, vkey);
        // 高位:按下;低位:切换状态
        return ((((b & 0x80) as u16) << 8) | (b & 0x01) as u16) as i16;
    }
    match GET_KEY_STATE.original() {
        Some(f) => unsafe { f(vkey) },
        None => 0,
    }
}

unsafe extern "system" fn get_keyboard_state(buf: *mut u8) -> BOOL {
    if !buf.is_null()
        && let Some(snap) = state::snapshot()
    {
        unsafe { core::ptr::copy_nonoverlapping(snap.keys.as_ptr(), buf, snap.keys.len()) };
        return BOOL::from(true);
    }
    match GET_KEYBOARD_STATE.original() {
        Some(f) => unsafe { f(buf) },
        None => BOOL::from(false),
    }
}
