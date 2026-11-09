//! 各个被钩 API 的替身。
//!
//! 每个钩子一个 `Hook<F>`:装钩子时 MinHook 返回的蹦床(调用原函数的入口)存进去,
//! 替身在不需要覆盖时用它回落到真实 API。

mod cursor;
mod keys;
mod pad;
mod raw;
mod window;

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicPtr, Ordering};

use minhook::MinHook;

pub use pad::install_loaded_xinput;

/// 一个被钩函数的原始入口。`F` 是该函数的 `unsafe extern "system" fn` 类型
pub struct Hook<F> {
    original: AtomicPtr<c_void>,
    _ty: PhantomData<F>,
}

impl<F: Copy> Hook<F> {
    pub const fn new() -> Self {
        // 下面两处 transmute_copy 按指针宽度搬运,F 必须就是个函数指针
        const { assert!(size_of::<F>() == size_of::<*mut c_void>()) };
        Self { original: AtomicPtr::new(core::ptr::null_mut()), _ty: PhantomData }
    }

    /// 给 `module` 里的 `name` 创建钩子(只创建,启用由 `install::enable_all` 统一做)。
    /// `detour` 的类型由 `F` 约束,签名对不上编译不过
    pub fn install(&self, module: &str, name: &str, detour: F) -> Result<(), String> {
        let detour_ptr = unsafe { core::mem::transmute_copy::<F, *mut c_void>(&detour) };
        let original = unsafe { MinHook::create_hook_api(module, name, detour_ptr) }
            .map_err(|e| format!("{module}!{name}: {e}"))?;
        self.original.store(original, Ordering::Release);
        Ok(())
    }

    /// 原函数。没装上时是 None,替身此时按“不覆盖”的缺省值返回
    pub fn original(&self) -> Option<F> {
        let p = self.original.load(Ordering::Acquire);
        (!p.is_null()).then(|| unsafe { core::mem::transmute_copy::<*mut c_void, F>(&p) })
    }
}

/// 装 user32 里的全部钩子(只创建不启用)。任何一个失败就整体失败:
/// 只装一半会让一部分输入绕过虚拟状态,不如不装
pub fn install_user32() -> Result<(), String> {
    keys::install()?;
    cursor::install()?;
    window::install()?;
    raw::install()?;
    Ok(())
}
