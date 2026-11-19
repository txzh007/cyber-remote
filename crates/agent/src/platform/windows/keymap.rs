//! DOM `KeyboardEvent.code`(物理键位)→ Windows 虚拟键码。
//!
//! 按键以 `code` 为准,不受浏览器端键盘布局影响:字母、数字、符号键按美式键位换算,
//! 等于主机用美式布局时同一个物理键的虚拟键码。表里没有的键(媒体键、日文键等)
//! 退回浏览器给的 `vk`。

/// 解析按键的虚拟键码。小键盘的数字键和小数点键优先用浏览器给的 `vk`:
/// NumLock 关着时浏览器给的是 End、方向键这些,NumLock 状态只有客户端知道
pub fn resolve(code: &str, hint: Option<u16>) -> Option<u16> {
    let numpad_digit = code == "NumpadDecimal"
        || code
            .strip_prefix("Numpad")
            .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit());
    if numpad_digit { hint.or_else(|| vk(code)) } else { vk(code).or(hint) }
}

fn vk(code: &str) -> Option<u16> {
    let single = |prefix: &str| match code.strip_prefix(prefix).map(str::as_bytes) {
        Some(&[c]) => Some(c),
        _ => None,
    };
    if let Some(c @ b'A'..=b'Z') = single("Key") {
        return Some(u16::from(c));
    }
    if let Some(c @ b'0'..=b'9') = single("Digit") {
        return Some(u16::from(c));
    }
    if let Some(c @ b'0'..=b'9') = single("Numpad") {
        return Some(0x60 + u16::from(c - b'0'));
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u16>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x6F + n); // F1 = 0x70
    }
    Some(match code {
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Enter" | "NumpadEnter" => 0x0D,
        "ShiftLeft" | "ShiftRight" => 0x10,
        "ControlLeft" | "ControlRight" => 0x11,
        "AltLeft" | "AltRight" => 0x12,
        "Pause" => 0x13,
        "CapsLock" => 0x14,
        "Escape" => 0x1B,
        "Space" => 0x20,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "End" => 0x23,
        "Home" => 0x24,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "ContextMenu" => 0x5D,
        "NumpadMultiply" => 0x6A,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadDecimal" => 0x6E,
        "NumpadDivide" => 0x6F,
        "NumLock" => 0x90,
        "ScrollLock" => 0x91,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        "IntlBackslash" => 0xE2,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn code_wins_over_browser_vk() {
        // AZERTY 上物理 Q 键浏览器给 keyCode 65('A'),按物理键位应是 'Q'
        assert_eq!(resolve("KeyQ", Some(65)), Some(0x51));
        assert_eq!(resolve("Digit1", None), Some(0x31));
        assert_eq!(resolve("F12", None), Some(0x7B));
        assert_eq!(resolve("Semicolon", Some(186)), Some(0xBA));
        assert_eq!(resolve("ShiftRight", Some(16)), Some(0x10));
    }

    #[test]
    fn numpad_and_unknown_keys_fall_back() {
        // NumLock 关着时 Numpad1 是 End,听浏览器的
        assert_eq!(resolve("Numpad1", Some(0x23)), Some(0x23));
        assert_eq!(resolve("Numpad1", None), Some(0x61));
        assert_eq!(resolve("AudioVolumeUp", Some(0xAF)), Some(0xAF));
        assert_eq!(resolve("Fn", None), None);
    }
}
