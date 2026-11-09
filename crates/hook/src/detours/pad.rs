//! 手柄:`XInputGetState`。
//!
//! 只模拟手柄 0(`PAD_SLOT`),其余槽位一律报未接。主机上的真手柄永远不会漏进实例,
//! 所以替身不回落到原函数,同一个替身可以装进每个已加载的 XInput DLL 变体。
//! 快照读不到或没置 `FLAG_PAD_CONNECTED` 时同样报未接。

use core::ffi::c_void;
use std::sync::Mutex;

use hook_abi::{FLAG_PAD_CONNECTED, PAD_SLOT, Pad};
use minhook::MinHook;
use windows::Win32::Foundation::{ERROR_DEVICE_NOT_CONNECTED, ERROR_INVALID_PARAMETER, ERROR_SUCCESS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD, XINPUT_GAMEPAD_BUTTON_FLAGS, XINPUT_STATE,
};
use windows::core::HSTRING;

use crate::state;

/// 游戏可能加载其中任何一个
const XINPUT_DLLS: &[&str] =
    &["xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll", "xinput1_2.dll", "xinput1_1.dll"];

/// 上一次的手柄状态(压成一个整数方便比较)和对应的包序号
static PACKET: Mutex<(u128, u32)> = Mutex::new((0, 0));

/// 给当前已加载、且还没处理过的 XInput DLL 创建钩子(只创建不启用),返回新建的个数。
/// `tried` 记录处理过的 DLL,调用方反复调用时不会重复创建
pub fn install_loaded_xinput(tried: &mut Vec<&'static str>) -> Result<usize, String> {
    let mut created = 0;
    for &dll in XINPUT_DLLS {
        if tried.contains(&dll) || unsafe { GetModuleHandleW(&HSTRING::from(dll)) }.is_err() {
            continue;
        }
        tried.push(dll);
        unsafe { MinHook::create_hook_api(dll, "XInputGetState", xinput_get_state as *mut c_void) }
            .map_err(|e| format!("{dll}!XInputGetState: {e}"))?;
        created += 1;
    }
    Ok(created)
}

/// 状态变了包序号才加一,应用靠它判断要不要重读
fn packet_number(p: &Pad) -> u32 {
    let key = p.buttons as u128
        | (p.left_trigger as u128) << 16
        | (p.right_trigger as u128) << 24
        | (p.thumb_lx as u16 as u128) << 32
        | (p.thumb_ly as u16 as u128) << 48
        | (p.thumb_rx as u16 as u128) << 64
        | (p.thumb_ry as u16 as u128) << 80;
    let mut last = PACKET.lock().unwrap_or_else(|e| e.into_inner());
    if last.0 != key {
        *last = (key, last.1.wrapping_add(1));
    }
    last.1
}

unsafe extern "system" fn xinput_get_state(user: u32, state: *mut XINPUT_STATE) -> u32 {
    if state.is_null() {
        return ERROR_INVALID_PARAMETER.0;
    }
    let snap = state::snapshot().filter(|s| user == PAD_SLOT && s.flags & FLAG_PAD_CONNECTED != 0);
    let Some(snap) = snap else {
        return ERROR_DEVICE_NOT_CONNECTED.0;
    };
    let p = snap.pad;
    unsafe {
        state.write(XINPUT_STATE {
            dwPacketNumber: packet_number(&p),
            Gamepad: XINPUT_GAMEPAD {
                wButtons: XINPUT_GAMEPAD_BUTTON_FLAGS(p.buttons),
                bLeftTrigger: p.left_trigger,
                bRightTrigger: p.right_trigger,
                sThumbLX: p.thumb_lx,
                sThumbLY: p.thumb_ly,
                sThumbRX: p.thumb_rx,
                sThumbRY: p.thumb_ry,
            },
        })
    };
    ERROR_SUCCESS.0
}
