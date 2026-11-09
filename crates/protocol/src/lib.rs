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
    MouseButton { button: MouseButton, down: bool, x: f32, y: f32 },
    /// 滚动量用 Windows 的单位:一格 = 120,`dy` 为正表示向上滚
    Wheel { dx: i32, dy: i32, x: f32, y: f32 },
    /// 不可打印的键和组合键。`code` 是 DOM `KeyboardEvent.code`,`vk` 是 Windows 虚拟键码
    Key { code: String, vk: u16, down: bool, repeat: bool },
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
            Input::MouseButton { button: MouseButton::Left, down: true, x: 0.5, y: 0.25 }
        );
        let ev = Input::from_json(r#"{"type":"key","code":"KeyA","vk":65,"down":false,"repeat":false}"#)
            .unwrap();
        assert!(matches!(ev, Input::Key { vk: 65, down: false, .. }));
        assert_eq!(Input::from_json(r#"{"type":"release_all"}"#).unwrap(), Input::ReleaseAll);
    }

    #[test]
    fn signal_json_roundtrip() {
        let offer = Signal::Offer { sdp: "v=0\r\n".into() };
        let json = offer.to_json();
        assert_eq!(json, r#"{"type":"offer","sdp":"v=0\r\n"}"#);
        assert_eq!(Signal::from_json(&json).unwrap(), offer);
    }
}
