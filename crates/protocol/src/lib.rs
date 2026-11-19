//! 浏览器与 agent 之间的消息定义。
//!
//! WebSocket 上走 [`Signal`];DataChannel 上走 [`Input`]。两者都是 JSON。

use serde::{Deserialize, Serialize};

/// WebSocket 信令消息,JSON 编码,`type` 字段区分种类。
///
/// 浏览器发 `offer`,agent 回 `answer`。Phase 0 不做 trickle ICE:
/// 双方都等候选收集完成后再发送完整 SDP。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Signal {
    Offer { sdp: String },
    Answer { sdp: String },
    Error { message: String },
    /// agent 给浏览器 HUD 的提示,比如输入隔离有没有启用
    Info { text: String },
}

impl Signal {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Signal 序列化不会失败")
    }

    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

/// 鼠标移动走的 DataChannel:不可靠、无序,靠 `seq` 丢弃过时的移动
pub const CHANNEL_INPUT_FAST: &str = "input-fast";
/// 按键、鼠标按钮、滚轮、文本走的 DataChannel:可靠、有序
pub const CHANNEL_INPUT_KEY: &str = "input-key";
/// agent 发给浏览器的消息走的 DataChannel:可靠、有序。浏览器创建,但不往里发
pub const CHANNEL_CONTROL: &str = "control";

/// agent 经 [`CHANNEL_CONTROL`] 发给浏览器的消息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// 延迟探针在 agent 内各环节的时刻,Unix 毫秒(带小数),和浏览器的
    /// `performance.timeOrigin + performance.now()` 是同一条时间轴,同一台机器上可以直接相减。
    /// `composed`(系统合成出这一帧)拿不到时为 null
    Probe {
        id: u32,
        received: f64,
        posted: f64,
        composed: Option<f64>,
        captured: f64,
        encoded: f64,
        sent: f64,
    },
}

impl Control {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Control 序列化不会失败")
    }
}

/// 浏览器发给 agent 的输入事件。
///
/// 坐标 `x`、`y` 是视频画面内的归一化位置(0..1,左上为原点),
/// agent 按目标窗口当前客户区尺寸换算,窗口缩放时不用重新协商。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    /// `rel` 为真时 `dx`/`dy` 是鼠标相对位移(浏览器 Pointer Lock 下的 `movementX/Y`),
    /// 此时 `x`/`y` 是虚拟光标当前所在的归一化位置,只用于按下等消息。
    /// 为假(没有 Pointer Lock)时按绝对位置移动,字段 `x`/`y` 有效
    MouseMove { seq: u32, x: f32, y: f32, #[serde(default)] dx: i32, #[serde(default)] dy: i32, #[serde(default)] rel: bool },
    /// `probe` 只在延迟测量(PLAN 0.8)时带:agent 把这次点击之后第一帧在主机内各环节的
    /// 时刻经 [`CHANNEL_CONTROL`] 回报,见 [`Control::Probe`]
    MouseButton {
        button: MouseButton,
        down: bool,
        x: f32,
        y: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probe: Option<u32>,
    },
    /// 滚动量用 Windows 的单位:一格 = 120,`dy` 为正表示向上滚
    Wheel { dx: i32, dy: i32, x: f32, y: f32 },
    /// 按键。`code` 是 DOM `KeyboardEvent.code`(物理键位),各平台后端以它为准换算成本地键码;
    /// `vk` 是浏览器给的 Windows 虚拟键码(`keyCode`),只作提示,`code` 认不出时才用
    Key {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vk: Option<u16>,
        down: bool,
        repeat: bool,
    },
    /// 可打印字符与输入法上屏的文字
    Text { text: String },
    /// 浏览器失焦或断线前:松开所有按键和鼠标按钮
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl Input {
    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_move_defaults_to_absolute() {
        // 老客户端不带 dx/dy/rel
        let ev = Input::from_json(r#"{"type":"mouse_move","seq":3,"x":0.5,"y":0.25}"#).unwrap();
        assert_eq!(
            ev,
            Input::MouseMove { seq: 3, x: 0.5, y: 0.25, dx: 0, dy: 0, rel: false }
        );
        let ev = Input::from_json(
            r#"{"type":"mouse_move","seq":4,"x":0.5,"y":0.25,"dx":7,"dy":-3,"rel":true}"#,
        )
        .unwrap();
        assert!(matches!(ev, Input::MouseMove { dx: 7, dy: -3, rel: true, .. }));
    }

    #[test]
    fn input_json_matches_browser_format() {
        let ev = Input::from_json(r#"{"type":"mouse_button","button":"left","down":true,"x":0.5,"y":0.25}"#)
            .unwrap();
        assert_eq!(
            ev,
            Input::MouseButton { button: MouseButton::Left, down: true, x: 0.5, y: 0.25, probe: None }
        );
        let ev = Input::from_json(r#"{"type":"key","code":"KeyA","vk":65,"down":false,"repeat":false}"#)
            .unwrap();
        assert!(matches!(ev, Input::Key { vk: Some(65), down: false, .. }));
        // vk 可以不带,以 code 为准
        let ev = Input::from_json(r#"{"type":"key","code":"KeyA","down":true,"repeat":false}"#).unwrap();
        assert!(matches!(ev, Input::Key { vk: None, down: true, .. }));
        assert_eq!(Input::from_json(r#"{"type":"release_all"}"#).unwrap(), Input::ReleaseAll);
    }

    #[test]
    fn latency_probe_json() {
        let ev = Input::from_json(
            r#"{"type":"mouse_button","button":"left","down":true,"x":0.5,"y":0.5,"probe":7}"#,
        )
        .unwrap();
        assert!(matches!(ev, Input::MouseButton { probe: Some(7), .. }));
        let msg = Control::Probe {
            id: 7,
            received: 1.5,
            posted: 2.0,
            composed: None,
            captured: 3.0,
            encoded: 4.0,
            sent: 5.0,
        };
        assert_eq!(
            msg.to_json(),
            r#"{"type":"probe","id":7,"received":1.5,"posted":2.0,"composed":null,"captured":3.0,"encoded":4.0,"sent":5.0}"#
        );
    }

    #[test]
    fn signal_json_roundtrip() {
        let offer = Signal::Offer { sdp: "v=0\r\n".into() };
        let json = offer.to_json();
        assert_eq!(json, r#"{"type":"offer","sdp":"v=0\r\n"}"#);
        assert_eq!(Signal::from_json(&json).unwrap(), offer);
    }
}
