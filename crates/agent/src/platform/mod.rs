//! 平台后端。每个平台提供同样的三个函数:
//! - `init()`:进程级初始化
//! - `preset(name)`:内置的推流目标档案(`--app`)
//! - `spawn(target, stream)`:找到或启动目标应用,开始采集编码,返回 [`Host`](crate::host::Host)
//!
//! 一路 = 一个隔离的"座位":画面、输入、音频各自独立。各平台的做法见 PLAN 4.1。

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{init, preset, spawn};

#[cfg(not(windows))]
mod unsupported;
#[cfg(not(windows))]
pub use self::unsupported::{init, preset, spawn};
