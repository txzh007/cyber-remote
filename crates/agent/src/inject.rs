//! 把浏览器输入翻译成窗口消息,PostMessage 给目标窗口。
//!
//! 不用 SendInput:主机光标和前台窗口都不受影响。修饰键和鼠标按钮的状态
//! 用 AttachThreadInput + SetKeyboardState 写进目标线程,应用里的
//! GetKeyState(快捷键、Shift 选区)才读得到。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use protocol::{Input, MouseButton};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, GetUpdateRect, InvalidateRect, MapWindowPoints};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyboardState, MAPVK_VK_TO_VSC, MapVirtualKeyW,
    SetKeyboardState,
    VK_CONTROL, VK_LBUTTON, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MBUTTON, VK_MENU, VK_RBUTTON,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_SHIFT, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_DBLCLKS, CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx,
    GCL_STYLE, GUITHREADINFO, GetClassLongPtrW, GetClientRect, GetGUIThreadInfo, GetSystemMetrics,
    GetWindowThreadProcessId, IsChild, IsWindow, PostMessageW, SM_CXDOUBLECLK, SM_CYDOUBLECLK,
    GetForegroundWindow, WM_ACTIVATE, WM_ACTIVATEAPP, WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_SETFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDBLCLK,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDBLCLK, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
};

use hook_abi::{RAW_KIND_KEYBOARD, RAW_KIND_MOUSE, RawEvent};

use crate::capture::Pipeline;
use crate::hooklink::base_snapshot;

// MK_* 在 SystemServices 里,为几个常量不值得多开一个 feature
const MK_LBUTTON: usize = 0x0001;
const MK_RBUTTON: usize = 0x0002;
const MK_SHIFT: usize = 0x0004;
const MK_CONTROL: usize = 0x0008;
const MK_MBUTTON: usize = 0x0010;
const MK_XBUTTON1: usize = 0x0020;
const MK_XBUTTON2: usize = 0x0040;

const SYNC_TIMEOUT: Duration = Duration::from_millis(100);

/// `WM_SETFOCUS` 的 wParam:传绝对窗口句柄(高 16 位为 0xFFFF 时句柄才算相对),
/// 这样 64 位下超过 0xFFFF 的句柄也能被正确解释
const HWND_ABSOLUTE: usize = 0xFFFF_0000;

/// 这些键的消息要带扩展键标志(lParam 第 24 位)
const EXTENDED_KEYS: &[&str] = &[
    "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Insert", "Delete", "Home", "End",
    "PageUp", "PageDown", "NumpadEnter", "NumpadDivide", "ControlRight", "AltRight",
    "ContextMenu", "NumLock", "PrintScreen",
];

/// 每个浏览器会话一个输入线程,事件按到达顺序处理。发送端全部丢弃后线程松开所有按键并退出。
pub fn spawn(pipeline: Pipeline) -> std::io::Result<mpsc::Sender<Input>> {
    let (tx, rx) = mpsc::channel::<Input>();
    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            let mut injector = Injector::new(pipeline);
            for ev in rx {
                injector.handle(ev);
            }
            injector.release_all();
            tracing::info!("输入线程退出");
        })?;
    Ok(tx)
}

struct Injector {
    pipeline: Pipeline,
    /// 已按下的键:DOM code → 虚拟键码
    keys: HashMap<String, u16>,
    buttons: HashSet<MouseButton>,
    /// 按住鼠标期间,移动和抬起都发给按下时的窗口(模拟鼠标捕获)
    pressed_in: Option<HWND>,
    last_seq: Option<u32>,
    last_pos: (f32, f32),
    /// 上一次鼠标移动的客户区位置,相邻两次的差就是伪造的 Raw Input 相对位移
    raw_prev: Option<POINT>,
    /// Pointer Lock 下的虚拟光标:客户区像素坐标,只由浏览器的相对增量累加得到。
    /// 目标程序通常会按帧把窗口消息里的位置改回中心,所以不能从消息反推
    virtual_pt: Option<POINT>,
    last_down: Option<(MouseButton, HWND, POINT, Instant)>,
    /// 最近一次点击的窗口:目标线程没有焦点窗口时(应用不在前台)键盘发给它
    clicked: Option<HWND>,
    /// 这次会话已经给目标补发过激活消息。模拟"应用被切到前台",见 `activate`
    activated: bool,
    attachment: Attachment,
}

impl Injector {
    fn new(pipeline: Pipeline) -> Self {
        Self {
            pipeline,
            keys: HashMap::new(),
            buttons: HashSet::new(),
            pressed_in: None,
            last_seq: None,
            last_pos: (0.5, 0.5),
            raw_prev: None,
            virtual_pt: None,
            last_down: None,
            clicked: None,
            activated: false,
            attachment: Attachment::default(),
        }
    }

    /// 浏览器第一次操作时,给目标补发一遍"被切到前台"的消息。
    ///
    /// 很多游戏靠 `WM_ACTIVATEAPP` / `WM_ACTIVATE` / `WM_SETFOCUS` 判断自己有没有在前台,
    /// 没收到就暂停或降帧。窗口本来就在前台时不必补:消息本来就发过了,再发一遍反而会
    /// 让应用以为要重画或重置状态。只在这次会话还没发过的时候发。
    fn activate(&mut self, top: HWND) {
        if self.activated {
            return;
        }
        self.activated = true;
        if unsafe { GetForegroundWindow() } == top {
            return;
        }
        let target = self.key_target(top);
        // 先 APP 级再窗口级,和系统真实切前台时的顺序一致。焦点只发给真正接收键盘的那个窗口
        post(top, WM_ACTIVATEAPP, 1, 0);
        // 低位 WA_ACTIVE(1),高位为 0 表示不是鼠标点击激活
        post(top, WM_ACTIVATE, 1, 0);
        // WM_SETFOCUS 的 wParam 是"正在失去焦点的窗口"。顶层窗口上的 wParam 传绝对句柄,
        // 焦点窗口上的传 0(自己原先没有焦点)
        if target != top {
            post(top, WM_SETFOCUS, HWND_ABSOLUTE, 0);
            post(target, WM_SETFOCUS, 0, 0);
        } else {
            post(top, WM_SETFOCUS, HWND_ABSOLUTE, 0);
        }
        tracing::debug!("补发激活消息给 {top:?},焦点 {target:?}");
    }

    fn key_target(&self, top: HWND) -> HWND {
        focus_window(top)
            .or(self.clicked.filter(|&h| is_window(h)))
            .unwrap_or(top)
    }

    fn handle(&mut self, ev: Input) {
        if ev == Input::ReleaseAll {
            self.release_all();
            return;
        }
        let Some(top) = self.window() else { return };
        self.activate(top);
        match ev {
            Input::MouseMove { seq, x, y, dx, dy, rel } => {
                // 不可靠通道可能乱序,晚到的旧位置直接丢弃
                if self.last_seq.is_some_and(|last| seq.wrapping_sub(last) as i32 <= 0) {
                    return;
                }
                self.last_seq = Some(seq);
                self.mouse_move(top, x, y, dx, dy, rel);
            }
            Input::MouseButton { button, down, x, y } => self.mouse_button(top, button, down, x, y),
            Input::Wheel { dx, dy, x, y } => self.wheel(top, dx, dy, x, y),
            Input::Key { code, vk, down, repeat } => self.key(top, &code, vk, down, repeat),
            Input::Text { text } => self.text(top, &text),
            Input::ReleaseAll => unreachable!(),
        }
    }

    fn window(&self) -> Option<HWND> {
        self.pipeline.window().filter(|&h| is_window(h))
    }

    /// 把按键、光标、窗口状态写进输入隔离的共享内存。没启用输入隔离时什么也不做。
    /// 必须在投递对应的窗口消息之前调用:目标处理消息时再查状态,读到的才是新的
    fn publish(&self, top: HWND) {
        // 有虚拟光标(Pointer Lock)时以它为准,否则按归一化位置换算
        let client = self.virtual_pt.unwrap_or_else(|| {
            let (x, y) = self.last_pos;
            client_point(top, x, y)
        });
        self.pipeline.with_hook(|link| {
            let mut cursor = client;
            unsafe {
                let _ = ClientToScreen(top, &mut cursor);
            }
            let mut snap = base_snapshot(top, focus_window(top).unwrap_or(top), cursor);
            // 通用键码(VK_SHIFT)和左右键码(VK_LSHIFT)都要置位,应用查哪个都对
            for (code, &vk) in &self.keys {
                for v in [Some(vk), side_vk(code)].into_iter().flatten() {
                    if let Some(k) = snap.keys.get_mut(v as usize) {
                        *k = 0x80;
                    }
                }
            }
            for &b in &self.buttons {
                snap.keys[button_vk(b) as usize] = 0x80;
            }
            link.publish(&snap);
        });
    }

    /// 伪造一条鼠标 Raw Input:相对位移、`RI_MOUSE_*` 按钮标志、滚轮量
    fn raw_mouse(&self, dx: i32, dy: i32, flags: u16, data: u16) {
        self.pipeline.with_hook(|link| {
            link.push_raw(RawEvent {
                kind: RAW_KIND_MOUSE,
                mouse_last_x: dx,
                mouse_last_y: dy,
                mouse_flags: flags,
                mouse_button_data: data,
                ..Default::default()
            });
        });
    }

    /// 伪造一条键盘 Raw Input,扫描码和扩展位取自窗口消息的 lParam
    fn raw_key(&self, vk: u16, down: bool, lparam: isize) {
        let l = lparam as u32;
        // RI_KEY_BREAK = 1,RI_KEY_E0 = 2
        let flags = u16::from(!down) | if l & (1 << 24) != 0 { 2 } else { 0 };
        self.pipeline.with_hook(|link| {
            link.push_raw(RawEvent {
                kind: RAW_KIND_KEYBOARD,
                kbd_vkey: vk,
                kbd_flags: flags,
                kbd_scan: ((l >> 16) & 0xFF) as u16,
                ..Default::default()
            });
        });
    }
}
impl Injector {
    /// `rel` 为真表示 Pointer Lock:窗口消息里的位置由虚拟光标累加浏览器给的相对增量得到,
/// Raw Input 直接用这个增量。为假时按绝对位置移动,和以前一样。
    ///
    /// 相对模式下 `x`/`y` 只是浏览器侧的近似位置,按下、滚轮等消息仍用它换算,
    /// 但窗口消息的坐标一律取虚拟光标,免得被目标程序自己回中的 `SetCursorPos` 带偏
    fn mouse_move(&mut self, top: HWND, x: f32, y: f32, dx: i32, dy: i32, rel: bool) {
        self.last_pos = (x, y);
        let cur = if rel {
            // 第一次收到相对事件时从客户区中心起步
            let base = self.virtual_pt.unwrap_or_else(|| {
                let mut rc = RECT::default();
                unsafe {
                    let _ = GetClientRect(top, &mut rc);
                }
                POINT { x: rc.right / 2, y: rc.bottom / 2 }
            });
            POINT { x: base.x.saturating_add(dx), y: base.y.saturating_add(dy) }
        } else {
            client_point(top, x, y)
        };
        if rel {
            self.virtual_pt = Some(cur);
        }
        // 输入隔离的状态先写好,目标处理消息时读到的就是新位置
        self.publish(top);
        if rel {
            if dx != 0 || dy != 0 {
                self.raw_mouse(dx, dy, 0, 0);
            }
        } else if let Some(prev) = self.raw_prev.replace(cur) {
            let (mdx, mdy) = (cur.x - prev.x, cur.y - prev.y);
            if mdx != 0 || mdy != 0 {
                self.raw_mouse(mdx, mdy, 0, 0);
            }
        }
        let (target, pt) = self.mouse_target_at(top, cur);
        post(target, WM_MOUSEMOVE, self.mk_flags(), point_lparam(pt));
    }

    fn mouse_button(&mut self, top: HWND, button: MouseButton, down: bool, x: f32, y: f32) {
        self.last_pos = (x, y);
        if down == self.buttons.contains(&button) {
            return; // 重复的按下/抬起,忽略
        }
        let (target, pt) = self.mouse_target(top, x, y);
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        self.attachment.set_key_state(target, &[(button_vk(button), down)]);
        self.publish(top);
        self.raw_mouse(0, 0, raw_button_flag(button, down), 0);

        let (down_msg, up_msg, dbl_msg) = button_msgs(button);
        let xbutton = match button {
            MouseButton::Back => 1usize << 16,
            MouseButton::Forward => 2usize << 16,
            _ => 0,
        };
        let msg = if down {
            self.clicked = Some(target);
            let dbl = self.is_double_click(button, target, pt);
            self.last_down = (!dbl).then_some((button, target, pt, Instant::now()));
            if dbl { dbl_msg } else { down_msg }
        } else {
            up_msg
        };
        post(target, msg, self.mk_flags() | xbutton, point_lparam(pt));
        sync(target); // 按钮状态同样不随消息排队

        // 按下时锁定目标窗口,全部松开后解除
        self.pressed_in = if self.buttons.is_empty() { None } else { Some(target) };
    }

    fn wheel(&mut self, top: HWND, dx: i32, dy: i32, x: f32, y: f32) {
        self.last_pos = (x, y);
        let (target, _) = self.mouse_target(top, x, y);
        // 滚轮消息的坐标是屏幕坐标
        let mut screen = client_point(top, x, y);
        unsafe {
            let _ = ClientToScreen(top, &mut screen);
        }
        let mk = self.mk_flags();
        self.publish(top);
        // RI_MOUSE_WHEEL = 0x400,RI_MOUSE_HWHEEL = 0x800
        for (delta, msg, raw_flag) in [(dy, WM_MOUSEWHEEL, 0x400u16), (dx, WM_MOUSEHWHEEL, 0x800u16)] {
            if delta != 0 {
                self.raw_mouse(0, 0, raw_flag, delta as i16 as u16);
                let wparam = ((delta as i16 as u16 as usize) << 16) | mk;
                post(target, msg, wparam, point_lparam(screen));
            }
        }
    }

    fn key(&mut self, top: HWND, code: &str, vk: u16, down: bool, repeat: bool) {
        if vk == 0 || vk > 0xFE {
            return;
        }
        if !down && !self.keys.contains_key(code) {
            return; // 没发过按下的键不发抬起
        }
        let target = self.key_target(top);
        tracing::debug!("键 {code} vk={vk:#x} down={down} → 窗口 {target:?}(焦点 {:?})", focus_window(top));
        if down {
            self.keys.insert(code.to_owned(), vk);
        } else {
            self.keys.remove(code);
        }
        let mut state = vec![(vk, down)];
        if let Some(side) = side_vk(code) {
            state.push((side, down));
        }
        self.attachment.set_key_state(target, &state);

        let alt_held = self.keys.values().any(|&v| v == VK_MENU.0);
        let mut lparam = key_lparam(code, vk, down, repeat);
        if alt_held {
            lparam |= 1 << 29; // 上下文码:Alt 按着
        }
        self.publish(top);
        self.raw_key(vk, down, lparam);
        let msg = match (down, alt_held || vk == VK_MENU.0) {
            (true, true) => WM_SYSKEYDOWN,
            (true, false) => WM_KEYDOWN,
            // 单独松开 Alt 用 WM_KEYUP:WM_SYSKEYUP 会让 DefWindowProc 进入菜单模式,
            // 而菜单栏在画面里被裁掉了
            (false, _) if vk == VK_MENU.0 => WM_KEYUP,
            (false, true) => WM_SYSKEYUP,
            (false, false) => WM_KEYUP,
        };
        post(target, msg, vk as usize, lparam);
        sync(target);
    }

    /// 文字直接发 WM_CHAR,不经过键盘布局,中文输入法上屏的内容也走这里
    fn text(&mut self, top: HWND, text: &str) {
        let target = self.key_target(top);
        for unit in text.encode_utf16() {
            post(target, WM_CHAR, unit as usize, 1);
        }
    }

    fn release_all(&mut self) {
        let Some(top) = self.window() else {
            self.keys.clear();
            self.buttons.clear();
            self.pressed_in = None;
            return;
        };
        let (x, y) = self.last_pos;
        for button in self.buttons.clone() {
            self.mouse_button(top, button, false, x, y);
        }
        let keys: Vec<(String, u16)> = self.keys.iter().map(|(c, v)| (c.clone(), *v)).collect();
        for (code, vk) in keys {
            self.key(top, &code, vk, false, false);
        }
        if !self.keys.is_empty() || !self.buttons.is_empty() {
            tracing::warn!("释放按键后仍有残留状态");
        }
        self.pressed_in = None;
        self.last_down = None;
        self.attachment.detach();
    }

    /// 鼠标消息发给哪个窗口、用该窗口的客户区坐标
    fn mouse_target(&self, top: HWND, x: f32, y: f32) -> (HWND, POINT) {
        self.mouse_target_at(top, client_point(top, x, y))
    }

    /// 同上,但输入是顶层窗口客户区的像素坐标
    fn mouse_target_at(&self, top: HWND, pt: POINT) -> (HWND, POINT) {
        if let Some(h) = self.pressed_in.filter(|&h| is_window(h)) {
            return (h, map_point(top, h, pt));
        }
        // 子窗口命中测试只在坐标落在客户区内时可信;Pointer Lock 下虚拟光标可能已经
        // 累积到客户区外,那时直接发给顶层窗口
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(top, &mut rc);
        }
        if pt.x < 0 || pt.y < 0 || pt.x >= rc.right || pt.y >= rc.bottom {
            return (top, pt);
        }
        child_at(top, pt)
    }

    fn mk_flags(&self) -> usize {
        let mut mk = 0;
        for b in &self.buttons {
            mk |= match b {
                MouseButton::Left => MK_LBUTTON,
                MouseButton::Right => MK_RBUTTON,
                MouseButton::Middle => MK_MBUTTON,
                MouseButton::Back => MK_XBUTTON1,
                MouseButton::Forward => MK_XBUTTON2,
            };
        }
        if self.keys.values().any(|&v| v == VK_SHIFT.0) {
            mk |= MK_SHIFT;
        }
        if self.keys.values().any(|&v| v == VK_CONTROL.0) {
            mk |= MK_CONTROL;
        }
        mk
    }

    fn is_double_click(&self, button: MouseButton, target: HWND, pt: POINT) -> bool {
        let Some((b, h, p, at)) = self.last_down else { return false };
        let class_style = unsafe { GetClassLongPtrW(target, GCL_STYLE) } as u32;
        if class_style & CS_DBLCLKS.0 == 0 || b != button || h != target {
            return false;
        }
        let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK)) };
        at.elapsed() <= Duration::from_millis(unsafe { GetDoubleClickTime() } as u64)
            && (pt.x - p.x).abs() <= cx / 2
            && (pt.y - p.y).abs() <= cy / 2
    }
}

fn is_window(h: HWND) -> bool {
    unsafe { IsWindow(Some(h)) }.as_bool()
}

fn post(h: HWND, msg: u32, wparam: usize, lparam: isize) {
    if let Err(e) = unsafe { PostMessageW(Some(h), msg, WPARAM(wparam), LPARAM(lparam)) } {
        tracing::debug!("PostMessage {msg:#x} 失败: {e}");
    }
}

/// 等目标线程处理完已投递的消息(至多 100ms)。
///
/// 键盘状态是我们这边直接改的,不随消息排队;TranslateMessage 生成的 WM_CHAR
/// 也排在队尾。不等的话,Ctrl 可能在 A 被处理前就松开,回车也会落到后面的文字之后。
/// WM_PAINT 只在投递队列清空后才生成,所以弄脏 1 像素、等它被画掉就是一道屏障。
fn sync(h: HWND) {
    let rc = RECT { left: 0, top: 0, right: 1, bottom: 1 };
    unsafe {
        if !InvalidateRect(Some(h), Some(&rc), false).as_bool() {
            return;
        }
        let deadline = Instant::now() + SYNC_TIMEOUT;
        while GetUpdateRect(h, None, false).as_bool() {
            if Instant::now() >= deadline {
                tracing::debug!("等待目标窗口处理输入超时");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn point_lparam(pt: POINT) -> isize {
    (((pt.y as i16 as u16 as u32) << 16) | (pt.x as i16 as u16 as u32)) as i32 as isize
}

/// 归一化坐标 → 顶层窗口客户区坐标。画面就是客户区(宽高最多差 1 像素的取偶)
fn client_point(top: HWND, x: f32, y: f32) -> POINT {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(top, &mut rc);
    }
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    POINT {
        x: ((x.clamp(0.0, 1.0) * w as f32) as i32).min(w - 1).max(0),
        y: ((y.clamp(0.0, 1.0) * h as f32) as i32).min(h - 1).max(0),
    }
}

fn map_point(from: HWND, to: HWND, pt: POINT) -> POINT {
    let mut pts = [pt];
    unsafe { MapWindowPoints(Some(from), Some(to), &mut pts) };
    pts[0]
}

/// 逐层找坐标下最深的可见子窗口(记事本里是 Edit 控件和状态栏)
fn child_at(top: HWND, pt: POINT) -> (HWND, POINT) {
    let (mut hwnd, mut p) = (top, pt);
    loop {
        let child = unsafe {
            ChildWindowFromPointEx(hwnd, p, CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT)
        };
        if child.is_invalid() || child == hwnd {
            return (hwnd, p);
        }
        p = map_point(hwnd, child, p);
        hwnd = child;
    }
}

/// 目标线程在这个窗口树里的焦点窗口
fn focus_window(top: HWND) -> Option<HWND> {
    let tid = unsafe { GetWindowThreadProcessId(top, None) };
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(tid, &mut info) }.ok()?;
    let f = info.hwndFocus;
    (!f.is_invalid() && (f == top || unsafe { IsChild(top, f) }.as_bool())).then_some(f)
}

/// 输入线程挂到目标 GUI 线程上,共享它的键盘状态。
///
/// 必须一直挂着:实测解除挂接时写进去的状态会丢,应用里 GetKeyState 读不到 Ctrl。
/// 松开全部按键或会话结束时才解除。
#[derive(Default)]
struct Attachment {
    tid: Option<u32>,
}

impl Attachment {
    fn ensure(&mut self, target: HWND) -> bool {
        let me = unsafe { GetCurrentThreadId() };
        let tid = unsafe { GetWindowThreadProcessId(target, None) };
        if tid == 0 || tid == me {
            return false;
        }
        if self.tid == Some(tid) {
            return true;
        }
        self.detach();
        if !unsafe { AttachThreadInput(me, tid, true) }.as_bool() {
            tracing::debug!("AttachThreadInput 失败,修饰键状态不同步");
            return false;
        }
        self.tid = Some(tid);
        true
    }

    fn detach(&mut self) {
        if let Some(tid) = self.tid.take() {
            unsafe {
                let _ = AttachThreadInput(GetCurrentThreadId(), tid, false);
            }
        }
    }

    /// 把按键/按钮状态写进目标线程的键盘状态,应用里 GetKeyState 才能读到修饰键
    fn set_key_state(&mut self, target: HWND, changes: &[(u16, bool)]) {
        if !self.ensure(target) {
            return;
        }
        let mut state = [0u8; 256];
        unsafe {
            if GetKeyboardState(&mut state).is_ok() {
                for &(vk, down) in changes {
                    let s = &mut state[vk as usize];
                    *s = if down { *s | 0x80 } else { *s & !0x80 };
                }
                let _ = SetKeyboardState(&state);
            }
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.detach();
    }
}

fn button_vk(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => VK_LBUTTON.0,
        MouseButton::Right => VK_RBUTTON.0,
        MouseButton::Middle => VK_MBUTTON.0,
        MouseButton::Back => VK_XBUTTON1.0,
        MouseButton::Forward => VK_XBUTTON2.0,
    }
}

/// Raw Input 的鼠标按钮标志(`RI_MOUSE_*`):左键按下 0x1、抬起 0x2,右键 0x4/0x8,
/// 中键 0x10/0x20,侧键 4 是 0x40/0x80,侧键 5 是 0x100/0x200;抬起恰好是按下左移一位
fn raw_button_flag(b: MouseButton, down: bool) -> u16 {
    let press = match b {
        MouseButton::Left => 0x1,
        MouseButton::Right => 0x4,
        MouseButton::Middle => 0x10,
        MouseButton::Back => 0x40,
        MouseButton::Forward => 0x100,
    };
    if down { press } else { press << 1 }
}

fn button_msgs(b: MouseButton) -> (u32, u32, u32) {
    match b {
        MouseButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, WM_LBUTTONDBLCLK),
        MouseButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_RBUTTONDBLCLK),
        MouseButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MBUTTONDBLCLK),
        MouseButton::Back | MouseButton::Forward => (WM_XBUTTONDOWN, WM_XBUTTONUP, WM_XBUTTONDBLCLK),
    }
}

/// 左右修饰键各自的虚拟键码,GetKeyState(VK_LSHIFT) 之类要用
fn side_vk(code: &str) -> Option<u16> {
    Some(match code {
        "ShiftLeft" => VK_LSHIFT.0,
        "ShiftRight" => VK_RSHIFT.0,
        "ControlLeft" => VK_LCONTROL.0,
        "ControlRight" => VK_RCONTROL.0,
        "AltLeft" => VK_LMENU.0,
        "AltRight" => VK_RMENU.0,
        _ => return None,
    })
}

/// WM_KEYDOWN/UP 的 lParam:重复次数、扫描码、扩展键、先前状态、转换状态
fn key_lparam(code: &str, vk: u16, down: bool, repeat: bool) -> isize {
    let scan = if code == "ShiftRight" {
        0x36
    } else {
        unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) }
    };
    let mut l: u32 = 1 | ((scan & 0xff) << 16);
    if EXTENDED_KEYS.contains(&code) {
        l |= 1 << 24;
    }
    if !down {
        l |= (1 << 30) | (1 << 31);
    } else if repeat {
        l |= 1 << 30;
    }
    l as i32 as isize
}
