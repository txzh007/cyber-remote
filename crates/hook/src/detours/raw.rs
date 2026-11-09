//! Raw Input:`GetRawInputData`、`GetRawInputBuffer`。
//!
//! agent 把事件写进环形缓冲,再 PostMessage 一个带伪造 `HRAWINPUT` 的 `WM_INPUT`。
//! 这里把伪造句柄还原成事件,拼成系统格式的 `RAWINPUT` 交给应用。
//! agent 接管期间,真实句柄一律拒绝(返回 ERROR_INVALID_HANDLE):放行的话主机真鼠标的
//! Raw Input 会漏进实例,隔离就破了。agent 没接管(`FLAG_ACTIVE` 没置)时全部放行。
//! `hDevice` 填空,和 SendInput 注入的事件一致。

use core::ffi::c_void;
use std::sync::Mutex;

use hook_abi::{RAW_KIND_KEYBOARD, RAW_KIND_MOUSE, RAW_RING, RawEvent};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, SetLastError,
};
use windows::Win32::UI::Input::{
    HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE, RAWMOUSE_0, RAWMOUSE_0_0,
    RID_HEADER, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{RI_KEY_BREAK, WM_KEYDOWN, WM_KEYUP};

use super::Hook;
use crate::state;

type GetRawInputDataFn = unsafe extern "system" fn(HRAWINPUT, u32, *mut c_void, *mut u32, u32) -> u32;
type GetRawInputBufferFn = unsafe extern "system" fn(*mut RAWINPUT, *mut u32, u32) -> u32;

static GET_RAW_INPUT_DATA: Hook<GetRawInputDataFn> = Hook::new();
static GET_RAW_INPUT_BUFFER: Hook<GetRawInputBufferFn> = Hook::new();

/// `GetRawInputBuffer` 的读取游标:下一个要交出去的绝对序号,None 表示还没读过
static BUFFER_CURSOR: Mutex<Option<u32>> = Mutex::new(None);

pub fn install() -> Result<(), String> {
    GET_RAW_INPUT_DATA.install("user32", "GetRawInputData", get_raw_input_data)?;
    GET_RAW_INPUT_BUFFER.install("user32", "GetRawInputBuffer", get_raw_input_buffer)?;
    Ok(())
}

/// 事件 → 系统格式的 `RAWINPUT` 和它的真实字节数(头 + 鼠标或键盘部分)
fn build(ev: &RawEvent) -> Option<(RAWINPUT, usize)> {
    let mut raw = RAWINPUT::default();
    let payload = match ev.kind {
        RAW_KIND_MOUSE => {
            // usFlags 缺省 0 就是 MOUSE_MOVE_RELATIVE
            raw.header.dwType = RIM_TYPEMOUSE.0;
            raw.data.mouse = RAWMOUSE {
                Anonymous: RAWMOUSE_0 {
                    Anonymous: RAWMOUSE_0_0 {
                        usButtonFlags: ev.mouse_flags,
                        usButtonData: ev.mouse_button_data,
                    },
                },
                lLastX: ev.mouse_last_x,
                lLastY: ev.mouse_last_y,
                ..Default::default()
            };
            size_of::<RAWMOUSE>()
        }
        RAW_KIND_KEYBOARD => {
            raw.header.dwType = RIM_TYPEKEYBOARD.0;
            raw.data.keyboard = RAWKEYBOARD {
                MakeCode: ev.kbd_scan,
                Flags: ev.kbd_flags,
                VKey: ev.kbd_vkey,
                Message: if ev.kbd_flags as u32 & RI_KEY_BREAK != 0 { WM_KEYUP } else { WM_KEYDOWN },
                ..Default::default()
            };
            size_of::<RAWKEYBOARD>()
        }
        _ => return None,
    };
    let size = size_of::<RAWINPUTHEADER>() + payload;
    raw.header.dwSize = size as u32;
    Some((raw, size))
}

/// 把 `RAWINPUT` 的前 `len` 个字节拷到调用方的缓冲区。按字节拷,不要求目标对齐
unsafe fn copy_out(raw: &RAWINPUT, dst: *mut u8, len: usize) {
    debug_assert!(len <= size_of::<RAWINPUT>());
    unsafe { core::ptr::copy_nonoverlapping(raw as *const RAWINPUT as *const u8, dst, len) };
}

unsafe extern "system" fn get_raw_input_data(
    handle: HRAWINPUT,
    command: u32,
    data: *mut c_void,
    size: *mut u32,
    header_size: u32,
) -> u32 {
    let Some(shared) = state::active_shared() else {
        return match GET_RAW_INPUT_DATA.original() {
            Some(f) => unsafe { f(handle, command, data, size, header_size) },
            None => u32::MAX,
        };
    };
    if size.is_null() || header_size as usize != size_of::<RAWINPUTHEADER>() {
        unsafe { SetLastError(ERROR_INVALID_PARAMETER) };
        return u32::MAX;
    }
    let Some((raw, total)) = shared.raw_from_handle(handle.0 as usize).and_then(|ev| build(&ev))
    else {
        // 真句柄,或伪造句柄对应的事件已被环形缓冲覆写
        unsafe { SetLastError(ERROR_INVALID_HANDLE) };
        return u32::MAX;
    };
    let want = match command {
        c if c == RID_HEADER.0 => size_of::<RAWINPUTHEADER>(),
        c if c == RID_INPUT.0 => total,
        _ => {
            unsafe { SetLastError(ERROR_INVALID_PARAMETER) };
            return u32::MAX;
        }
    };
    if data.is_null() {
        unsafe { *size = want as u32 };
        return 0;
    }
    if (unsafe { *size } as usize) < want {
        unsafe {
            *size = want as u32;
            SetLastError(ERROR_INSUFFICIENT_BUFFER);
        }
        return u32::MAX;
    }
    unsafe { copy_out(&raw, data as *mut u8, want) };
    want as u32
}

unsafe extern "system" fn get_raw_input_buffer(
    data: *mut RAWINPUT,
    size: *mut u32,
    header_size: u32,
) -> u32 {
    let Some(shared) = state::active_shared() else {
        return match GET_RAW_INPUT_BUFFER.original() {
            Some(f) => unsafe { f(data, size, header_size) },
            None => u32::MAX,
        };
    };
    if size.is_null() || header_size as usize != size_of::<RAWINPUTHEADER>() {
        unsafe { SetLastError(ERROR_INVALID_PARAMETER) };
        return u32::MAX;
    }
    if data.is_null() {
        // 查询最小缓冲区大小
        unsafe { *size = size_of::<RAWINPUT>() as u32 };
        return 0;
    }
    let capacity = unsafe { *size } as usize;
    let head = shared.raw_head();
    let mut cursor = BUFFER_CURSOR.lock().unwrap_or_else(|e| e.into_inner());
    // 第一次读从当前位置开始,不回放历史;落后超过一圈就跳到最旧的还在的那条
    let mut next = cursor.unwrap_or(head);
    if head.wrapping_sub(next) as usize > RAW_RING {
        next = head.wrapping_sub(RAW_RING as u32);
    }
    let out = data as *mut u8;
    let (mut written, mut count) = (0usize, 0u32);
    while next != head {
        let Some((raw, total)) = shared.raw_at(next).and_then(|ev| build(&ev)) else {
            next = next.wrapping_add(1); // 已被覆写或无效,跳过
            continue;
        };
        if written + total > capacity {
            if count == 0 {
                // 连一条都放不下:游标不动,应用扩大缓冲区后重试
                *cursor = Some(next);
                unsafe {
                    *size = total as u32;
                    SetLastError(ERROR_INSUFFICIENT_BUFFER);
                }
                return u32::MAX;
            }
            break;
        }
        unsafe { copy_out(&raw, out.add(written), total) };
        // 相邻两条之间按指针宽度对齐(NEXTRAWINPUTBLOCK)
        written += total.next_multiple_of(size_of::<usize>());
        count += 1;
        next = next.wrapping_add(1);
    }
    *cursor = Some(next);
    count
}
