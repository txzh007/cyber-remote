//! 平台无关的核心类型:推流目标、编码参数、编码帧、输入事件,以及平台后端要实现的 [`Host`]。
//!
//! 一个 agent 进程推一路(一个应用实例)。HTTP、信令、WebRTC 只依赖这里;
//! 找到或启动应用、采集编码、注入输入由 `platform` 下各平台的后端实现。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bytes::Bytes;
use protocol::Input;
use tokio::sync::broadcast;

/// 编码参数
#[derive(Clone, Copy, Debug)]
pub struct Stream {
    pub fps: u32,
    /// bit/s
    pub bitrate: u32,
}

/// 推流目标:要启动的程序,以及怎么认出它的窗口
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// 找不到窗口时启动的程序;不填就只找已打开的窗口
    pub exe: Option<PathBuf>,
    pub args: Vec<String>,
    /// 窗口类别,精确匹配。含义随平台,Windows 上是窗口类名
    pub class: Option<String>,
    /// 窗口标题,包含即可
    pub title: Option<String>,
    /// 总是新开一个进程,不复用已打开的窗口(多开用)
    pub fresh: bool,
    /// 启用输入隔离
    pub hook: bool,
    /// 把窗口铺满这块屏并采整块屏(Windows 上是 `\\.\DISPLAYn` 的 n);None 表示只采这个窗口
    pub display: Option<u32>,
}

/// 一帧编码好的视频
#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub captured_at: Instant,
    /// 某次延迟探针点击之后的第一帧,带上它在 agent 内各环节的时刻(PLAN 0.8)
    pub probe: Option<ProbeTiming>,
}

/// 一帧编码好的音频(Opus)
#[derive(Clone)]
pub struct AudioFrame {
    pub data: Bytes,
    pub duration: Duration,
}

/// 延迟探针在 agent 内各环节的时刻
#[derive(Clone, Copy, Debug)]
pub struct ProbeTiming {
    pub id: u32,
    /// 数据通道收到这次点击
    pub received: Instant,
    /// 输入消息投递给应用
    pub posted: Instant,
    /// 系统合成出这一帧,拿不到时为 None
    pub composed: Option<Instant>,
    /// 采集线程拿到这一帧
    pub captured: Instant,
    /// 编码完成
    pub encoded: Instant,
}

/// 浏览器发来的输入事件,带上 agent 收到它的时刻
#[derive(Debug)]
pub struct InputEvent {
    pub at: Instant,
    pub input: Input,
}

impl InputEvent {
    pub fn now(input: Input) -> Self {
        Self { at: Instant::now(), input }
    }
}

/// 一路推流(一个应用实例)的平台后端
pub trait Host: Send + Sync + 'static {
    /// 编码帧的订阅端;没人订阅时帧直接丢弃
    fn subscribe(&self) -> broadcast::Receiver<EncodedFrame>;
    /// 音频帧的订阅端;这个后端没有声音(不支持、没开输入隔离、被关掉)时是 None
    fn subscribe_audio(&self) -> Option<broadcast::Receiver<AudioFrame>> {
        None
    }
    /// 让下一帧编成关键帧
    fn request_keyframe(&self);
    /// 给一个浏览器会话开输入通道。发送端全部丢弃后,后端松开这个会话按下的所有键
    fn open_input(&self) -> Result<mpsc::Sender<InputEvent>>;
    /// 给浏览器 HUD 的状态提示,比如输入隔离有没有启用
    fn status(&self) -> String;
    /// agent 退出前调用:撤销输入隔离、恢复窗口状态
    fn shutdown(&self);
}

/// `Instant` → Unix 毫秒(带小数)。浏览器的 `performance.timeOrigin + performance.now()`
/// 也是这条时间轴,同一台机器上两边的时刻可以直接相减
pub fn epoch_ms(t: Instant) -> f64 {
    static BASE: OnceLock<(Instant, f64)> = OnceLock::new();
    let &(base, base_ms) = BASE.get_or_init(|| {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        (Instant::now(), wall.as_secs_f64() * 1000.0)
    });
    if t >= base {
        base_ms + t.duration_since(base).as_secs_f64() * 1000.0
    } else {
        base_ms - base.duration_since(t).as_secs_f64() * 1000.0
    }
}
