//! Windows 后端:WGC 按窗口采集(或 DXGI 桌面复制采整块屏)→ NVENC;输入走窗口消息注入,可选 hook DLL 做输入隔离。

mod capture;
mod compositor;
mod display;
mod hooklink;
mod inject;
mod keymap;
mod nvenc;

use std::sync::{Arc, mpsc};

use anyhow::{Result, bail};
use tokio::sync::broadcast;

use crate::host::{EncodedFrame, Host, InputEvent, Stream, Target};

/// 进程级初始化:让窗口坐标和 WGC 帧都用物理像素,裁剪才对得上
pub fn init() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 内置档案 notepad、gametest
pub fn preset(name: &str) -> Result<Target> {
    Ok(match name {
        "notepad" => Target {
            exe: Some("notepad.exe".into()),
            class: Some("Notepad".into()),
            ..Default::default()
        },
        // gametest 和 agent 在同一个 target 目录下
        "gametest" => {
            let exe = std::env::current_exe()?.with_file_name("gametest.exe");
            if !exe.exists() {
                bail!("找不到 {},先 cargo build -p gametest", exe.display());
            }
            Target {
                exe: Some(exe),
                class: Some("CyberRemoteGameTest".into()),
                ..Default::default()
            }
        }
        _ => bail!("未知的 --app {name},可选 notepad、gametest"),
    })
}

pub fn spawn(target: Target, stream: Stream) -> Result<Arc<dyn Host>> {
    Ok(Arc::new(capture::spawn(target, stream)?))
}

impl Host for capture::Pipeline {
    fn subscribe(&self) -> broadcast::Receiver<EncodedFrame> {
        self.frames.subscribe()
    }

    fn request_keyframe(&self) {
        capture::Pipeline::request_keyframe(self);
    }

    fn open_input(&self) -> Result<mpsc::Sender<InputEvent>> {
        Ok(inject::spawn(self.clone())?)
    }

    fn status(&self) -> String {
        self.hook_note()
    }

    fn shutdown(&self) {
        // 先撤钩子:目标进程里的钩子立刻回落到真实输入,再还原窗口边框、位置和任务栏按钮
        self.detach_hook();
        self.restore_window();
        if let Some(hwnd) = self.window() {
            capture::restore_taskbar(hwnd);
        }
    }
}
