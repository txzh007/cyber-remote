//! 游戏输入测试窗口:并排显示窗口消息、按键查询、Raw Input、XInput 各自看到的输入。
//!
//! 用来看 agent 的注入覆盖到哪一层。Phase 1 只投递窗口消息,所以窗口消息和
//! GetKeyState 应该有反应;GetAsyncKeyState、Raw Input、XInput 读的是系统级输入,
//! 要等 Phase 2 的 hook 才会动。主机本地的真鼠标、真键盘会出现在后三栏,属正常。

#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleBitmap,
    CreateCompatibleDC, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DeleteDC, DeleteObject,
    EndPaint, FW_NORMAL, FillRect, FrameRect, HDC, HFONT, InvalidateRect, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, SRCCOPY, ScreenToClient, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    TextOutW, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyNameTextW, GetKeyState};
use windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, GetClientRect, GetCursorPos, GetForegroundWindow, GetMessageW, IDC_ARROW,
    LoadCursorW, MSG, PostQuitMessage, RI_KEY_BREAK, RI_MOUSE_LEFT_BUTTON_DOWN,
    RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP,
    RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, RegisterClassW, SetTimer,
    SetWindowTextW, TranslateMessage, WINDOW_EX_STYLE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_CHAR, WM_DESTROY, WM_ERASEBKGND, WM_INPUT,
    WM_KEYDOWN, WM_KILLFOCUS, WM_MOUSEFIRST, WM_MOUSELAST, WM_MOUSEWHEEL, WM_PAINT, WM_SETFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};
use windows::core::{PCWSTR, Result, w};

/// agent 按这个类名找窗口
const CLASS_NAME: PCWSTR = w!("CyberRemoteGameTest");
const WIDTH: i32 = 960;
const HEIGHT: i32 = 600;
const TIMER_ID: usize = 1;
/// 约 60Hz 轮询按键、手柄并重画
const TICK_MS: u32 = 16;
/// 没接的手柄槽位查询很慢,隔一阵才重新扫
const PAD_RESCAN: Duration = Duration::from_secs(2);
const LINE: i32 = 24;

/// 按键查询栏要看的键
const KEYS: [(&str, i32); 10] = [
    ("W", 0x57),
    ("A", 0x41),
    ("S", 0x53),
    ("D", 0x44),
    ("Space", 0x20),
    ("Shift", 0x10),
    ("Ctrl", 0x11),
    ("Alt", 0x12),
    ("左键", 0x01),
    ("右键", 0x02),
];

/// 与 `KEYS` 一一对应的 ASCII 名,写进窗口标题用
const KEY_ASCII: [&str; KEYS.len()] =
    ["W", "A", "S", "D", "Space", "Shift", "Ctrl", "Alt", "LMB", "RMB"];

const PAD_BUTTONS: [(u16, &str); 14] = [
    (0x1000, "A"),
    (0x2000, "B"),
    (0x4000, "X"),
    (0x8000, "Y"),
    (0x0100, "LB"),
    (0x0200, "RB"),
    (0x0010, "Start"),
    (0x0020, "Back"),
    (0x0040, "LS"),
    (0x0080, "RS"),
    (0x0001, "↑"),
    (0x0002, "↓"),
    (0x0004, "←"),
    (0x0008, "→"),
];

#[derive(Default)]
struct State {
    font: HFONT,
    // 窗口消息
    msg_count: u64,
    mouse_msg: &'static str,
    mouse_pos: Option<(i32, i32)>,
    mk: usize,
    wheel: i32,
    last_key: String,
    typed: String,
    focused: bool,
    active: bool,
    // 按键查询
    key_state: [bool; KEYS.len()],
    async_state: [bool; KEYS.len()],
    // Raw Input
    raw_events: u64,
    raw_total: (i64, i64),
    raw_last: (i32, i32),
    raw_buttons: [bool; 3],
    raw_keys: Vec<String>,
    // XInput
    pads: [Option<XINPUT_STATE>; 4],
    pad_scan: Option<Instant>,
    // 主机
    host_cursor: (i32, i32),
    foreground: bool,
    /// 收到过 WM_ACTIVATEAPP(TRUE):应用级"我被切到前台"
    app_active: bool,
    last_title: String,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn main() -> Result<()> {
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        // 和 agent 一样用物理像素,裁剪和坐标换算才对得上
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // --latency:延迟测量用的黑白窗口(见 latency_wndproc)
        let latency = std::env::args().any(|a| a == "--latency");
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(if latency { latency_wndproc } else { wndproc }),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let mut rc = RECT { left: 0, top: 0, right: WIDTH, bottom: HEIGHT };
        AdjustWindowRectEx(&mut rc, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0))?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            if latency { w!("gametest latency") } else { w!("gametest") },
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        STATE.with_borrow_mut(|s| {
            s.font = CreateFontW(
                -18,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Microsoft YaHei UI"),
            );
        });

        // INPUTSINK:不在前台也收 Raw Input,和很多游戏的做法一样
        let devices = [
            RAWINPUTDEVICE { usUsagePage: 1, usUsage: 2, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd },
            RAWINPUTDEVICE { usUsagePage: 1, usUsage: 6, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd },
        ];
        // 延迟模式不轮询也不重画:画面只在点击时变,采集到的新帧就是点击的结果
        if !latency {
            RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32)?;
            SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // BeginPaint 会同步给本窗口发 WM_ERASEBKGND(最小化后还原时必现),这时外层 WM_PAINT
    // 还借着 STATE。paint 自己整块双缓冲重画,擦背景不用碰状态,借用之前直接返回
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    // 其他重入的消息也不能 panic(extern "system" 里 panic 直接 abort),借不到就走默认处理。
    // 借用只在 handle 里持有,DefWindowProcW 可能重入 wndproc
    let handled = STATE.with(|cell| {
        let mut s = cell.try_borrow_mut().ok()?;
        handle(&mut s, hwnd, msg, wparam, lparam)
    });
    if let Some(r) = handled {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 返回 None 表示还要交给 DefWindowProcW
fn handle(s: &mut State, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        WM_MOUSEWHEEL => {
            s.msg_count += 1;
            s.wheel += (wparam.0 >> 16) as u16 as i16 as i32;
            s.mk = wparam.0 & 0xFFFF;
            s.mouse_msg = "WM_MOUSEWHEEL";
            Some(LRESULT(0))
        }
        WM_MOUSEFIRST..=WM_MOUSELAST => {
            s.msg_count += 1;
            s.mouse_msg = mouse_name(msg);
            s.mouse_pos = Some(lparam_point(lparam));
            s.mk = wparam.0 & 0xFFFF;
            None
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            s.msg_count += 1;
            let l = lparam.0 as u32;
            let vk = wparam.0 as u16;
            let scan = (l >> 16) & 0xFF;
            let ext = if l & (1 << 24) != 0 { " 扩展" } else { "" };
            let rep = if l & (1 << 30) != 0 { " 重复" } else { "" };
            let sys = if msg == WM_SYSKEYDOWN { "SYS" } else { "" };
            s.last_key = format!("{sys}按下 {} vk={vk:#04x} scan={scan:#04x}{ext}{rep}", key_name(l));
            // Alt+F4 等仍交给系统
            None
        }
        WM_SYSKEYUP => None,
        WM_CHAR => {
            s.msg_count += 1;
            let c = wparam.0 as u32;
            match c {
                0x08 => s.typed.push('⌫'),
                0x0D => s.typed.push('⏎'),
                0x09 => s.typed.push('⇥'),
                c if c < 0x20 => s.typed.push_str(&format!("^{}", char::from(b'@' + c as u8))),
                // 代理对的两半分开到达,先按替换字符显示
                c => s.typed.push(char::from_u32(c).unwrap_or('\u{FFFD}')),
            }
            let n = s.typed.chars().count();
            if n > 40 {
                s.typed = s.typed.chars().skip(n - 40).collect();
            }
            Some(LRESULT(0))
        }
        WM_SETFOCUS => {
            s.focused = true;
            None
        }
        WM_KILLFOCUS => {
            s.focused = false;
            None
        }
        WM_ACTIVATE => {
            s.active = wparam.0 & 0xFFFF != 0;
            None
        }
        WM_ACTIVATEAPP => {
            s.app_active = wparam.0 != 0;
            None
        }
        WM_INPUT => {
            raw_input(s, lparam);
            None // 系统要求 WM_INPUT 也走 DefWindowProcW 做清理
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            poll(s, hwnd);
            update_title(s, hwnd);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            Some(LRESULT(0))
        }
        WM_PAINT => {
            paint(s, hwnd);
            Some(LRESULT(0))
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            Some(LRESULT(0))
        }
        _ => None,
    }
}

fn raw_input(s: &mut State, lparam: LPARAM) {
    let mut raw = RAWINPUT::default();
    let mut size = size_of::<RAWINPUT>() as u32;
    let n = unsafe {
        GetRawInputData(
            HRAWINPUT(lparam.0 as *mut _),
            RID_INPUT,
            Some(&mut raw as *mut RAWINPUT as *mut _),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if n == u32::MAX || n == 0 {
        return;
    }
    s.raw_events += 1;
    if raw.header.dwType == RIM_TYPEMOUSE.0 {
        let m = unsafe { raw.data.mouse };
        // 绝对坐标多见于远程桌面和触控板,累计相对位移时跳过
        if m.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0 {
            s.raw_last = (m.lLastX, m.lLastY);
            s.raw_total.0 += m.lLastX as i64;
            s.raw_total.1 += m.lLastY as i64;
        }
        let flags = unsafe { m.Anonymous.Anonymous.usButtonFlags } as u32;
        for (i, (down, up)) in [
            (RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP),
            (RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP),
            (RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP),
        ]
        .into_iter()
        .enumerate()
        {
            if flags & down != 0 {
                s.raw_buttons[i] = true;
            }
            if flags & up != 0 {
                s.raw_buttons[i] = false;
            }
        }
    } else if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
        let k = unsafe { raw.data.keyboard };
        let name = format!("{:#04x}", k.VKey);
        let up = k.Flags as u32 & RI_KEY_BREAK != 0;
        s.raw_keys.retain(|n| *n != name);
        if !up {
            s.raw_keys.push(name);
        }
    }
}

fn poll(s: &mut State, hwnd: HWND) {
    for (i, &(_, vk)) in KEYS.iter().enumerate() {
        unsafe {
            s.key_state[i] = GetKeyState(vk) < 0;
            s.async_state[i] = GetAsyncKeyState(vk) < 0;
        }
    }
    // 没接的槽位每次查询要几毫秒,只在定期重扫时查
    let rescan = s.pad_scan.is_none_or(|t| t.elapsed() >= PAD_RESCAN);
    if rescan {
        s.pad_scan = Some(Instant::now());
    }
    for (i, pad) in s.pads.iter_mut().enumerate() {
        if pad.is_none() && !rescan {
            continue;
        }
        let mut st = XINPUT_STATE::default();
        *pad = (unsafe { XInputGetState(i as u32, &mut st) } == 0).then_some(st);
    }
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_ok() {
        s.host_cursor = (p.x, p.y);
    }
    s.foreground = unsafe { GetForegroundWindow() } == hwnd;
}

/// 把状态摘要写进窗口标题(纯 ASCII)。窗口标题栏在推流画面里是裁掉的,
/// 自动化测试用 `tasklist /v` 就能读到每一路的状态
fn update_title(s: &mut State, hwnd: HWND) {
    let names = |state: &[bool; KEYS.len()]| {
        let held: Vec<&str> = KEY_ASCII.iter().zip(state).filter(|(_, b)| **b).map(|(n, _)| *n).collect();
        held.join(",")
    };
    let title = format!(
        "gametest msgs={} key=[{}] async=[{}] raw={} rawsum=({},{}) fg={} act={}{} cursor=({},{})",
        s.msg_count,
        names(&s.key_state),
        names(&s.async_state),
        s.raw_events,
        s.raw_total.0,
        s.raw_total.1,
        u8::from(s.foreground),
        u8::from(s.active),
        u8::from(s.app_active),
        s.host_cursor.0,
        s.host_cursor.1
    );
    if title != s.last_title {
        // WM_SETTEXT 会同步重入 wndproc,这时 STATE 还借着,wndproc 会走默认处理
        let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
        }
        s.last_title = title;
    }
}

fn paint(s: &State, hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        // 双缓冲,60Hz 重画不闪
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old_bmp = SelectObject(mem, bmp.into());
        let old_font = SelectObject(mem, s.font.into());
        let bg = CreateSolidBrush(rgb(24, 26, 32));
        FillRect(mem, &rc, bg);
        let _ = DeleteObject(bg.into());
        SetBkMode(mem, TRANSPARENT);

        let half = w / 2;
        draw_messages(s, mem, RECT { left: 0, top: 0, right: half, bottom: h / 2 }, hwnd);
        draw_keys(s, mem, RECT { left: half, top: 0, right: w, bottom: h / 2 });
        draw_raw(s, mem, RECT { left: 0, top: h / 2, right: half, bottom: h });
        draw_pads(s, mem, RECT { left: half, top: h / 2, right: w, bottom: h });

        // 窗口消息里的鼠标位置画个准星,对照画面上点的位置
        if let Some((x, y)) = s.mouse_pos {
            let brush = CreateSolidBrush(rgb(255, 80, 80));
            FillRect(mem, &RECT { left: x - 10, top: y, right: x + 11, bottom: y + 1 }, brush);
            FillRect(mem, &RECT { left: x, top: y - 10, right: x + 1, bottom: y + 11 }, brush);
            let _ = DeleteObject(brush.into());
        }

        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_font);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 每栏一个带标题的框,返回写字用的游标
struct Panel {
    hdc: HDC,
    x: i32,
    y: i32,
}

impl Panel {
    fn new(hdc: HDC, rc: RECT, title: &str, live: bool) -> Self {
        unsafe {
            let inner = RECT { left: rc.left + 6, top: rc.top + 6, right: rc.right - 6, bottom: rc.bottom - 6 };
            // 有输入时边框变绿,一眼看出哪一层收到了
            let border = CreateSolidBrush(if live { rgb(80, 200, 120) } else { rgb(70, 74, 84) });
            FrameRect(hdc, &inner, border);
            let _ = DeleteObject(border.into());
        }
        let mut p = Panel { hdc, x: rc.left + 18, y: rc.top + 14 };
        p.line_color(title, rgb(255, 210, 90));
        p
    }

    fn line(&mut self, text: &str) {
        self.line_color(text, rgb(220, 224, 230));
    }

    fn line_color(&mut self, text: &str, color: COLORREF) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            SetTextColor(self.hdc, color);
            let _ = TextOutW(self.hdc, self.x, self.y, &wide);
        }
        self.y += LINE;
    }
}

fn draw_messages(s: &State, hdc: HDC, rc: RECT, hwnd: HWND) {
    let mut p = Panel::new(hdc, rc, "① 窗口消息(Phase 1 注入应在这里出现)", s.msg_count > 0);
    p.line(&format!("消息数 {}", s.msg_count));
    let pos = match s.mouse_pos {
        Some((x, y)) => format!("{x}, {y}"),
        None => "-".into(),
    };
    p.line(&format!("鼠标 {} @ {pos}  MK={:#06x}", or_dash(s.mouse_msg), s.mk));
    p.line(&format!("滚轮累计 {}(一格 120)", s.wheel));
    p.line(&format!("键 {}", or_dash(&s.last_key)));
    p.line(&format!("字符 {}", or_dash(&s.typed)));
    p.line(&format!(
        "焦点 {}  激活 {}  应用激活 {}",
        yes(s.focused),
        yes(s.active),
        yes(s.app_active)
    ));
    // 主机光标换到本窗口坐标,注入时它不该跟着动
    let mut host = POINT { x: s.host_cursor.0, y: s.host_cursor.1 };
    unsafe {
        let _ = ScreenToClient(hwnd, &mut host);
    }
    p.line(&format!("主机光标 屏幕 {:?} 窗口内 ({}, {})", s.host_cursor, host.x, host.y));
}

fn draw_keys(s: &State, hdc: HDC, rc: RECT) {
    let live = s.key_state.iter().chain(&s.async_state).any(|&b| b);
    let mut p = Panel::new(hdc, rc, "② 按键查询", live);
    p.line("GetKeyState(线程状态,注入可写)");
    p.line(&format!("  {}", pressed(&s.key_state)));
    p.line("GetAsyncKeyState(系统级,需 Phase 2 hook)");
    p.line(&format!("  {}", pressed(&s.async_state)));
    p.line(&format!("前台窗口是我 {}", yes(s.foreground)));
}

fn draw_raw(s: &State, hdc: HDC, rc: RECT) {
    let mut p = Panel::new(hdc, rc, "③ Raw Input(需 Phase 2 hook)", s.raw_events > 0);
    p.line(&format!("事件数 {}", s.raw_events));
    p.line(&format!("相对位移 本次 {:?} 累计 {:?}", s.raw_last, s.raw_total));
    let names = ["左", "右", "中"];
    let held: Vec<&str> = names.iter().zip(s.raw_buttons).filter(|(_, b)| *b).map(|(n, _)| *n).collect();
    p.line(&format!("按钮 {}", if held.is_empty() { "-".into() } else { held.join(" ") }));
    p.line(&format!("按住的键 vk {}", if s.raw_keys.is_empty() { "-".into() } else { s.raw_keys.join(" ") }));
    p.line_color("主机本地的真鼠标、真键盘也会出现在这里", rgb(140, 146, 158));
}

fn draw_pads(s: &State, hdc: HDC, rc: RECT) {
    let live = s.pads.iter().flatten().any(|st| {
        let g = st.Gamepad;
        g.wButtons.0 != 0 || g.bLeftTrigger > 30 || g.bRightTrigger > 30
    });
    let mut p = Panel::new(hdc, rc, "④ XInput 手柄(需 Phase 2 hook)", live);
    let mut any = false;
    for (i, st) in s.pads.iter().enumerate() {
        let Some(st) = st else { continue };
        any = true;
        let g = st.Gamepad;
        let buttons: Vec<&str> =
            PAD_BUTTONS.iter().filter(|(bit, _)| g.wButtons.0 & bit != 0).map(|(_, n)| *n).collect();
        p.line(&format!("#{i} 包号 {}", st.dwPacketNumber));
        p.line(&format!("  按钮 {}", if buttons.is_empty() { "-".into() } else { buttons.join(" ") }));
        p.line(&format!(
            "  左摇杆 ({}, {}) 右摇杆 ({}, {}) 扳机 {} / {}",
            g.sThumbLX, g.sThumbLY, g.sThumbRX, g.sThumbRY, g.bLeftTrigger, g.bRightTrigger
        ));
    }
    if !any {
        p.line("没有手柄");
    }
}

fn pressed(state: &[bool; KEYS.len()]) -> String {
    let names: Vec<&str> = KEYS.iter().zip(state).filter(|(_, b)| **b).map(|((n, _), _)| *n).collect();
    if names.is_empty() { "-".into() } else { names.join(" ") }
}

fn mouse_name(msg: u32) -> &'static str {
    match msg {
        0x0200 => "MOVE",
        0x0201 => "LDOWN",
        0x0202 => "LUP",
        0x0203 => "LDBLCLK",
        0x0204 => "RDOWN",
        0x0205 => "RUP",
        0x0206 => "RDBLCLK",
        0x0207 => "MDOWN",
        0x0208 => "MUP",
        0x0209 => "MDBLCLK",
        0x020B => "XDOWN",
        0x020C => "XUP",
        0x020D => "XDBLCLK",
        0x020E => "HWHEEL",
        _ => "?",
    }
}

/// 按扫描码取键名,看 lParam 里的扫描码和扩展位对不对
fn key_name(lparam: u32) -> String {
    let mut buf = [0u16; 32];
    let n = unsafe { GetKeyNameTextW(lparam as i32, &mut buf) };
    if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { "?".into() }
}

fn lparam_point(lparam: LPARAM) -> (i32, i32) {
    let v = lparam.0 as u32;
    ((v & 0xFFFF) as u16 as i16 as i32, (v >> 16) as u16 as i16 as i32)
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

fn yes(b: bool) -> &'static str {
    if b { "是" } else { "否" }
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}

/// `--latency` 模式的当前颜色:左键按着时是白色
static WHITE: AtomicBool = AtomicBool::new(false);

/// `--latency` 模式:整窗黑色,左键按下立刻涂白,抬起涂黑。PLAN 0.8 的延迟测量
/// (web/latency.js)在浏览器里看画面中心什么时候变白
unsafe extern "system" fn latency_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            WHITE.store(msg == WM_LBUTTONDOWN, Ordering::Relaxed);
            // 立刻重画,不等消息队列空下来
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = UpdateWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let v = if WHITE.load(Ordering::Relaxed) { 255 } else { 0 };
                let brush = CreateSolidBrush(rgb(v, v, v));
                FillRect(hdc, &rc, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
