//! 安装流程:打开共享内存 → 创建全部钩子 → 一次性启用 → 装声音钩子 → 通知 agent 就绪。
//!
//! 共享内存打不开或校验失败时一个钩子都不装,实例照常用真实输入,不会处在半隔离状态。
//! 声音钩子要建临时音频流,比较慢,但必须在通知 agent 就绪之前装完:agent 等到就绪事件
//! 就可能让目标继续往下跑,这时声音钩子必须已经在位,否则目标建输出流这一下会截不到。

use std::time::Duration;

use minhook::MinHook;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{EVENT_MODIFY_STATE, GetCurrentProcessId, OpenEventW, SetEvent};
use windows::core::HSTRING;

use crate::debug_log;
use crate::{detours, state};

/// 游戏可能在启动后才加载 XInput,装完之后再盯一阵(120 × 0.5s = 60s)
const LATE_POLL_ROUNDS: u32 = 120;
const LATE_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn install() -> Result<(), String> {
    state::open()?;
    detours::install_user32()?;

    // 手柄是可选能力,没用 XInput 的程序不受影响,失败只记日志
    let mut tried = Vec::new();
    if let Err(e) = detours::install_loaded_xinput(&mut tried) {
        debug_log(&format!("cyber_remote_hook: 手柄钩子创建失败: {e}"));
    }
    enable_all()?;
    debug_log("cyber_remote_hook: 钩子已启用");

    // 声音钩子放在通知就绪之前装:建临时音频流要几十毫秒,但不能让 agent 先放行目标、
    // 目标再抢在声音钩子装完之前建好输出流。失败只记日志,不影响其他钩子,也照常 SetEvent
    if let Err(e) = detours::install_audio() {
        debug_log(&format!("cyber_remote_hook: 声音钩子安装失败: {e}"));
    }
    signal_ready();

    for _ in 0..LATE_POLL_ROUNDS {
        std::thread::sleep(LATE_POLL_INTERVAL);
        let result = detours::install_loaded_xinput(&mut tried);
        if let Err(e) = &result {
            debug_log(&format!("cyber_remote_hook: 手柄钩子创建失败: {e}"));
        }
        // 出错时可能已经建好了几个,一并启用
        if !matches!(result, Ok(0)) {
            enable_all()?;
        }
    }
    Ok(())
}

/// 通知 agent 钩子已经启用。事件不存在(比如手工注入做测试)不算错误
fn signal_ready() {
    let name = HSTRING::from(hook_abi::ready_event_name(unsafe { GetCurrentProcessId() }));
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &name) } {
        Ok(event) => unsafe {
            let _ = SetEvent(event);
            let _ = CloseHandle(event);
        },
        Err(e) => debug_log(&format!("cyber_remote_hook: 没找到就绪事件 {name}: {e}")),
    }
}

fn enable_all() -> Result<(), String> {
    unsafe { MinHook::enable_all_hooks() }.map_err(|e| format!("启用钩子失败: {e}"))
}
