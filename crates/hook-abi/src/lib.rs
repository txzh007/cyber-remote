//! agent 与注入 DLL 之间的共享内存布局(`#[repr(C)]`,纯 POD)。
//!
//! 一个目标进程一块命名文件映射 `Local\cyber_remote_hook_{pid}`。agent 是写方,
//! hook 是读方;两边都按本文件的布局解释这块内存,所以这个 crate 不依赖 windows,
//! 32 位和 64 位 DLL 都用同一份定义(字段全用定长整数,布局与位宽无关)。
//!
//! 同步:快照块(按键、光标、窗口、手柄)用 seqlock(`seq` 为奇数表示正在写);
//! Raw Input 用环形缓冲,agent 递增 `raw_head`,把绝对序号编进伪造的 `HRAWINPUT`
//! 一起 `PostMessage(WM_INPUT)`,hook 用序号对 `RAW_RING` 取模读回对应事件。

#![cfg_attr(not(test), no_std)]

extern crate alloc;
use alloc::format;
use alloc::string::String;

/// 映射头部的校验字,防止读到未初始化或别的进程的内存
pub const MAGIC: u32 = 0x4359_4248; // "CYBH"
/// 布局版本,agent 与 DLL 不一致时 DLL 拒绝挂钩
pub const ABI_VERSION: u32 = 1;
/// Raw Input 环形缓冲槽位数,必须是 2 的幂
pub const RAW_RING: usize = 256;

/// 伪造 `HRAWINPUT` 的高位标记:`(RAW_HANDLE_TAG | 绝对序号的低位)`。
/// 系统的真句柄理论上也可能落进这个区间,所以 hook 还要求序号在环里对得上
/// (`Shared::raw_from_handle`),两者都满足才当伪造句柄处理
pub const RAW_HANDLE_TAG: usize = 0xCB00_0000;
pub const RAW_HANDLE_MASK: usize = 0x00FF_FFFF;

/// 绝对序号 → 伪造的 `HRAWINPUT`(放进 `WM_INPUT` 的 lParam)
pub const fn raw_handle(seq: u32) -> usize {
    RAW_HANDLE_TAG | (seq as usize & RAW_HANDLE_MASK)
}

/// 伪造的 `HRAWINPUT` → 序号低 24 位;不带标记的是系统真句柄
pub const fn raw_handle_seq(handle: usize) -> Option<u32> {
    if handle & !RAW_HANDLE_MASK == RAW_HANDLE_TAG {
        Some((handle & RAW_HANDLE_MASK) as u32)
    } else {
        None
    }
}

/// 要模拟的 XInput 槽位(只做手柄 0,其余槽位 hook 一律报未接)
pub const PAD_SLOT: u32 = 0;

pub const RAW_KIND_NONE: u32 = 0;
pub const RAW_KIND_MOUSE: u32 = 1;
pub const RAW_KIND_KEYBOARD: u32 = 2;

/// 命名文件映射的名字,按目标进程 PID 区分(多开隔离)
pub fn mapping_name(pid: u32) -> String {
    format!("Local\\cyber_remote_hook_{pid}")
}

/// 就绪事件的名字。agent 注入前创建,DLL 把钩子全部启用后 `SetEvent`,
/// agent 等到它才认为该实例已经隔离
pub fn ready_event_name(pid: u32) -> String {
    format!("Local\\cyber_remote_hook_ready_{pid}")
}

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering, fence};

/// Snapshot.flags 的位
pub const FLAG_OVERRIDE_CURSOR: u32 = 1 << 0;
pub const FLAG_OVERRIDE_FOREGROUND: u32 = 1 << 1;
pub const FLAG_PAD_CONNECTED: u32 = 1 << 2;
/// 总开关:agent 在线且接管输入。没置这一位(agent 退出时清掉)时,按键、光标、窗口和
/// 手柄的钩子全部回落到真实 API,Raw Input 的钩子也放行真实事件。
pub const FLAG_ACTIVE: u32 = 1 << 3;

/// 手柄状态,布局同 `XINPUT_GAMEPAD`(不依赖 windows crate)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Pad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// 一次 Raw Input 事件。布局与位宽无关;hook 读出后按本进程位宽拼 `RAWINPUT`。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RawEvent {
    pub kind: u32,
    /// 绝对序号,用于确认槽位没被覆写
    pub seq: u32,
    pub mouse_last_x: i32,
    pub mouse_last_y: i32,
    /// `RI_MOUSE_*` 按钮标志
    pub mouse_flags: u16,
    /// 滚轮增量(放进 usButtonData)
    pub mouse_button_data: u16,
    pub kbd_vkey: u16,
    /// `RI_KEY_MAKE`(0)/`RI_KEY_BREAK`(1)等
    pub kbd_flags: u16,
    pub kbd_scan: u16,
    pub _pad: u16,
}

/// seqlock 保护的那块状态:按键、光标、窗口、手柄。agent 整块写,hook 整块读,
/// 所以这里的字段都是普通 POD(不是原子),一致性由 `Shared::seq` 保证。
///
/// 字段按对齐从大到小排:四个 `u64` 在最前,32 位和 64 位编译出的布局一致
/// (`u64` 在两个目标上都是 8 字节对齐,不会因位宽差异插入不同的填充)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Snapshot {
    /// 下面四个是 `HWND` 的数值(u64 容纳 64 位句柄),0 表示不覆盖
    pub foreground: u64,
    pub active: u64,
    pub focus: u64,
    pub capture: u64,
    /// `FLAG_*` 的位或
    pub flags: u32,
    /// 伪造的光标位置(屏幕坐标),`GetCursorPos` 返回它
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub pad: Pad,
    /// `GetAsyncKeyState`/`GetKeyState`/`GetKeyboardState` 用:高位 0x80 表示按下
    pub keys: [u8; 256],
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            foreground: 0,
            active: 0,
            focus: 0,
            capture: 0,
            flags: 0,
            cursor_x: 0,
            cursor_y: 0,
            pad: Pad::default(),
            keys: [0; 256],
        }
    }
}

/// 整块共享内存的布局。agent 创建命名映射(OS 清零),调 `init` 写入校验字;
/// hook 映射进来先 `valid()` 校验,再按 seqlock 读快照、按环形缓冲读 Raw Input。
///
/// 这块内存跨进程共享,另一边随时可能改写,所以非原子字段都放在 `UnsafeCell` 里
/// (`repr(transparent)`,不改布局),只通过 volatile 读写访问。
#[repr(C)]
pub struct Shared {
    magic: AtomicU32,
    version: AtomicU32,
    /// seqlock:奇数表示 agent 正在写快照
    seq: AtomicU32,
    /// 已写入的 Raw Input 事件总数,下一个事件的绝对序号
    raw_head: AtomicU32,
    snapshot: UnsafeCell<Snapshot>,
    raw: UnsafeCell<[RawEvent; RAW_RING]>,
}

// 并发访问由 seqlock 和环形缓冲的序号校验约束,见各方法
unsafe impl Sync for Shared {}

/// 读快照最多自旋这么多次,免得 agent 崩了之后 hook 卡死
const READ_SPINS: u32 = 1024;

impl Shared {
    /// 在刚创建、清零的映射上写入校验字。最后写 magic 当作“就绪”标志。
    pub fn init(&self) {
        self.version.store(ABI_VERSION, Ordering::Relaxed);
        self.magic.store(MAGIC, Ordering::Release);
    }

    /// 校验字对得上才信这块内存
    pub fn valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == MAGIC
            && self.version.load(Ordering::Relaxed) == ABI_VERSION
    }

    /// Raw Input 已写入的事件总数(下一个事件的序号)
    pub fn raw_head(&self) -> u32 {
        self.raw_head.load(Ordering::Acquire)
    }

    /// agent:整块写入快照(seqlock 写方)。只允许一个写方。
    pub fn write_snapshot(&self, snap: &Snapshot) {
        let s = self.seq.load(Ordering::Relaxed);
        // 置奇数,宣告开始写
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        unsafe { core::ptr::write_volatile(self.snapshot.get(), *snap) };
        // 置偶数,写完
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// hook:读一份一致的快照(seqlock 读方)。agent 没在写、读期间没被覆写才成功。
    pub fn read_snapshot(&self) -> Option<Snapshot> {
        for _ in 0..READ_SPINS {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let snap = unsafe { core::ptr::read_volatile(self.snapshot.get()) };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some(snap);
            }
        }
        None
    }

    /// agent:写一个 Raw Input 事件,返回它的绝对序号(编进伪造的 `HRAWINPUT`)。只允许一个写方。
    pub fn push_raw(&self, mut ev: RawEvent) -> u32 {
        let seq = self.raw_head.load(Ordering::Relaxed);
        ev.seq = seq;
        unsafe { core::ptr::write_volatile(self.slot(seq), ev) };
        // 事件内容先落地,再公布 head
        self.raw_head.store(seq.wrapping_add(1), Ordering::Release);
        seq
    }

    /// hook:按绝对序号读回 Raw Input 事件;还没写到、已被覆写或为空则返回 None
    pub fn raw_at(&self, seq: u32) -> Option<RawEvent> {
        let head = self.raw_head.load(Ordering::Acquire);
        // 只认 [head - RAW_RING, head) 区间内的序号
        let age = head.wrapping_sub(seq);
        if age == 0 || age as usize > RAW_RING {
            return None;
        }
        let ev = unsafe { core::ptr::read_volatile(self.slot(seq)) };
        fence(Ordering::Acquire);
        // 读的过程中槽位可能被新事件覆写,事件自带的序号对得上才算数
        (ev.seq == seq && ev.kind != RAW_KIND_NONE).then_some(ev)
    }

    /// hook:伪造的 `HRAWINPUT` → 事件。句柄只带序号低 24 位,按当前 head 还原成
    /// 最近的那个绝对序号;不是我们的句柄返回 None
    pub fn raw_from_handle(&self, handle: usize) -> Option<RawEvent> {
        let low = raw_handle_seq(handle)?;
        let head = self.raw_head.load(Ordering::Acquire);
        // head-1 往回数,低位对得上的第一个序号
        let back = head.wrapping_sub(1).wrapping_sub(low) & RAW_HANDLE_MASK as u32;
        self.raw_at(head.wrapping_sub(1).wrapping_sub(back))
    }

    fn slot(&self, seq: u32) -> *mut RawEvent {
        let slot = (seq as usize) & (RAW_RING - 1);
        unsafe { (self.raw.get() as *mut RawEvent).add(slot) }
    }
}

// 环形槽位数必须是 2 的幂,取模才能用位与
const _: () = assert!(RAW_RING.is_power_of_two());

// 布局不含指针,32 位和 64 位进程必须看到完全一致的大小、对齐和字段偏移。
// 任一目标上这些断言不成立就是布局跟位宽相关了,编译期就拦住。
const _: () = assert!(core::mem::size_of::<Pad>() == 12);
const _: () = assert!(core::mem::size_of::<RawEvent>() == 28);
const _: () = assert!(core::mem::align_of::<RawEvent>() == 4);
const _: () = assert!(core::mem::size_of::<Snapshot>() == 312);
const _: () = assert!(core::mem::align_of::<Snapshot>() == 8);
const _: () = assert!(core::mem::offset_of!(Snapshot, flags) == 32);
const _: () = assert!(core::mem::offset_of!(Snapshot, pad) == 44);
const _: () = assert!(core::mem::offset_of!(Snapshot, keys) == 56);
const _: () = assert!(core::mem::size_of::<Shared>() == 16 + 312 + 28 * RAW_RING);
const _: () = assert!(core::mem::align_of::<Shared>() == 8);
const _: () = assert!(core::mem::offset_of!(Shared, snapshot) == 16);
const _: () = assert!(core::mem::offset_of!(Shared, raw) == 328);

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed() -> Box<Shared> {
        // 映射由 OS 清零,测试里同样从全零开始
        unsafe { Box::new(core::mem::zeroed()) }
    }

    #[test]
    fn init_marks_valid() {
        let s = zeroed();
        assert!(!s.valid());
        s.init();
        assert!(s.valid());
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = zeroed();
        s.init();
        let mut snap = Snapshot { cursor_x: 12, foreground: 0xABCD, ..Default::default() };
        snap.keys[0x41] = 0x80;
        s.write_snapshot(&snap);
        let got = s.read_snapshot().unwrap();
        assert_eq!((got.cursor_x, got.foreground, got.keys[0x41]), (12, 0xABCD, 0x80));
    }

    #[test]
    fn raw_ring_by_handle() {
        let s = zeroed();
        s.init();
        let ev = |x| RawEvent { kind: RAW_KIND_MOUSE, mouse_last_x: x, ..Default::default() };
        let first = s.push_raw(ev(1));
        assert_eq!(s.raw_from_handle(raw_handle(first)).unwrap().mouse_last_x, 1);
        // 还没写到的序号不认
        assert!(s.raw_from_handle(raw_handle(first + 1)).is_none());
        // 灌满一圈后最早的被覆写
        for i in 0..RAW_RING as i32 {
            s.push_raw(ev(i + 2));
        }
        assert!(s.raw_at(first).is_none());
        let last = s.raw_head() - 1;
        assert_eq!(s.raw_from_handle(raw_handle(last)).unwrap().mouse_last_x, RAW_RING as i32 + 1);
        // 不带标记的句柄是系统真句柄
        assert!(s.raw_from_handle(0x1234_5678).is_none());
    }
}
