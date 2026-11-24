//! 注入目标进程的 DLL:把一组输入查询 API 改成读 agent 写进共享内存的虚拟状态,
//! 让每个被注入的实例只看到属于自己那一路的键鼠和手柄,互不干扰,也不碰主机真输入。
//!
//! 覆盖的 API(都按 PLAN.md 2.3–2.5):
//! - 按键:`GetAsyncKeyState`、`GetKeyState`、`GetKeyboardState`
//! - 光标:`GetCursorPos`、`SetCursorPos`、`ClipCursor`
//! - 焦点:`GetForegroundWindow`、`GetActiveWindow`、`GetFocus`
//! - 捕获:`SetCapture`、`GetCapture`
//! - Raw Input:`GetRawInputData`、`GetRawInputBuffer`(配合 agent 伪造的 `WM_INPUT`)
//! - 手柄:`XInputGetState`(只做手柄 0)
//! - 声音:`IAudioClient::Initialize`、`IAudioClient3::InitializeSharedAudioStream`、`GetService`、
//!   `IAudioRenderClient::GetBuffer`、`ReleaseBuffer`,截共享模式的输出(见 `detours/audio.rs`)
//!
//! 共享内存布局见 `hook-abi`。输入这几项 agent 是写方,这里是读方;声音反过来,这里把截到的 PCM
//! 写进环,agent 读。DLL 在 `DllMain` 里起一个线程,打开 `Local\cyber_remote_hook_{pid}`,
//! 校验通过后用 minhook 装上所有钩子。
//! 不支持带反作弊的游戏,也不做任何绕过。

#![cfg(windows)]

mod state;
mod detours;
mod install;

use windows::Win32::Foundation::{HINSTANCE, TRUE};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::core::BOOL;

/// DLL 入口。注入后系统在加载器锁内调用它,所以这里只起一个线程,真正的
/// 打开映射、装钩子都放到线程里做(加载器锁内不能再 LoadLibrary / 等别的线程)。
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut core::ffi::c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // 线程里失败就静默放弃:注入失败不应该把目标进程带崩
        std::thread::spawn(|| {
            if let Err(e) = install::install() {
                // 目标进程通常没有控制台,写到调试器输出
                debug_log(&format!("cyber_remote_hook 安装失败: {e}"));
            }
        });
    }
    TRUE
}

/// 往调试器输出一行(目标进程一般没有 stdout)
pub(crate) fn debug_log(msg: &str) {
    use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
    use windows::core::HSTRING;
    let line = HSTRING::from(format!("{msg}\n"));
    unsafe { OutputDebugStringW(&line) };
}
