//! 输入隔离:和注入目标进程里的 hook DLL 通信(协议见 `hook-abi`)。
//!
//! `attach` 建命名共享内存和就绪事件,把 DLL 注入目标进程,等 DLL 报告钩子已启用。
//! 之后输入线程用 `publish` 写按键、光标、窗口快照,用 `push_raw` 写 Raw Input 事件。
//! `Drop` 时清掉 `FLAG_ACTIVE`,目标进程里的钩子随即回落到真实输入。
//!
//! 只支持 64 位目标:32 位目标里 `LoadLibraryW` 的地址和本进程不同,还没做。

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use hook_abi::{
    FLAG_ACTIVE, FLAG_OVERRIDE_CURSOR, FLAG_OVERRIDE_FOREGROUND, RawEvent, Shared, Snapshot,
    mapping_name, raw_handle, ready_event_name,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
    POINT, RECT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile, VirtualAllocEx,
    VirtualFreeEx,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateRemoteThread, GetExitCodeThread, IsWow64Process, LPTHREAD_START_ROUTINE,
    OpenProcess, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
    PROCESS_VM_READ, PROCESS_VM_WRITE, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowThreadProcessId, PostMessageW, WM_INPUT};
use windows::core::{BOOL, HSTRING, s, w};

const LOAD_TIMEOUT_MS: u32 = 10_000;
const READY_TIMEOUT_MS: u32 = 5_000;

/// 出作用域就 `CloseHandle`
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 和一个目标进程的连接。多个浏览器会话共用一个,调用方用 Mutex 保证只有一个写方
pub struct HookLink {
    hwnd: HWND,
    /// 只为 Drop 时关句柄
    _mapping: Owned,
    view: *mut Shared,
    ready: Owned,
}

// view 指向跨进程共享内存,并发由 hook-abi 的 seqlock 和“单写方”约定保证,
// 单写方由调用方的 Mutex 保证
unsafe impl Send for HookLink {}

/// 接管状态下的基础快照:实例认为自己在前台、有焦点,光标是虚拟的。`cursor` 是屏幕坐标。
/// 按键、手柄由调用方在返回值上继续填
pub fn base_snapshot(top: HWND, focus: HWND, cursor: POINT) -> Snapshot {
    let hwnd_value = |h: HWND| h.0 as usize as u64;
    Snapshot {
        foreground: hwnd_value(top),
        active: hwnd_value(top),
        focus: hwnd_value(focus),
        flags: FLAG_ACTIVE | FLAG_OVERRIDE_CURSOR | FLAG_OVERRIDE_FOREGROUND,
        cursor_x: cursor.x,
        cursor_y: cursor.y,
        ..Default::default()
    }
}

/// 客户区中心的屏幕坐标
pub fn client_center(hwnd: HWND) -> POINT {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let mut pt = POINT { x: rc.right / 2, y: rc.bottom / 2 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
    }
    pt
}

impl HookLink {
    fn shared(&self) -> &Shared {
        unsafe { &*self.view }
    }

    /// 整块写入快照
    pub fn publish(&mut self, snap: &Snapshot) {
        self.shared().write_snapshot(snap);
    }

    /// 写一条 Raw Input 事件,再往目标窗口投一个带伪造句柄的 `WM_INPUT`
    pub fn push_raw(&mut self, ev: RawEvent) {
        let seq = self.shared().push_raw(ev);
        let handle = raw_handle(seq) as isize;
        // wParam 0 = RIM_INPUT(前台收到)
        if let Err(e) = unsafe { PostMessageW(Some(self.hwnd), WM_INPUT, WPARAM(0), LPARAM(handle)) } {
            tracing::debug!("投递 WM_INPUT 失败: {e}");
        }
    }
}

impl Drop for HookLink {
    fn drop(&mut self) {
        // 清掉 FLAG_ACTIVE:目标进程里的钩子回落到真实输入。映射本身随目标进程的视图继续存在
        self.shared().write_snapshot(&Snapshot::default());
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
        }
    }
}

/// 让目标进程 `LoadLibraryW` 这个 DLL,返回远程线程的退出码(模块句柄的低 32 位)。
/// 只有同位宽的进程里 kernel32 才在同一个地址,所以调用方要先排除 32 位目标
fn inject_dll(process: HANDLE, dll: &Path) -> Result<u32> {
    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    let bytes = wide.len() * 2;
    unsafe {
        let remote = VirtualAllocEx(process, None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            bail!("在目标进程里分配内存失败: {}", windows::core::Error::from_thread());
        }
        if let Err(e) = WriteProcessMemory(process, remote, wide.as_ptr().cast(), bytes, None) {
            let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
            return Err(e).context("写入 DLL 路径失败");
        }
        let kernel32 = GetModuleHandleW(w!("kernel32.dll"))?;
        let load = GetProcAddress(kernel32, s!("LoadLibraryW")).ok_or_else(|| anyhow!("找不到 LoadLibraryW"))?;
        let start: LPTHREAD_START_ROUTINE = Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut c_void) -> u32,
        >(load));
        let thread = CreateRemoteThread(process, None, 0, start, Some(remote.cast_const()), 0, None)
            .context("CreateRemoteThread 失败(目标进程权限更高,或是受保护进程)")?;
        let thread = Owned(thread);
        if WaitForSingleObject(thread.0, LOAD_TIMEOUT_MS) != WAIT_OBJECT_0 {
            // 线程可能还在读路径,这块内存不释放
            bail!("目标进程 {LOAD_TIMEOUT_MS} 毫秒内没有加载完 DLL");
        }
        let mut code = 0u32;
        GetExitCodeThread(thread.0, &mut code)?;
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        Ok(code)
    }
}

/// 给 `hwnd` 所在的进程装上输入隔离。任何一步失败都返回错误,调用方退回无隔离模式;
/// 已经建好的资源随 `HookLink` / `Owned` 的 Drop 清理
pub fn attach(hwnd: HWND, dll: &Path) -> Result<HookLink> {
    if !dll.is_file() {
        bail!("找不到 {},先 cargo build -p hook", dll.display());
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        bail!("取不到窗口所属的进程");
    }
    let access = PROCESS_CREATE_THREAD
        | PROCESS_QUERY_INFORMATION
        | PROCESS_VM_OPERATION
        | PROCESS_VM_READ
        | PROCESS_VM_WRITE;
    let process = Owned(unsafe { OpenProcess(access, false, pid) }.context("打开目标进程失败(权限不够?)")?);
    let mut wow64 = BOOL(0);
    unsafe { IsWow64Process(process.0, &mut wow64)? };
    if wow64.as_bool() {
        bail!("目标是 32 位进程,输入隔离暂只支持 64 位");
    }

    let mapping = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            size_of::<Shared>() as u32,
            &HSTRING::from(mapping_name(pid)),
        )?
    };
    let mapping = Owned(mapping);
    // 同名映射已存在:目标进程已经被别的 agent 或上次运行装过钩子,不能再装
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        bail!("进程 {pid} 已经装过输入隔离,请先关掉它再重新启动");
    }
    let ready = Owned(unsafe { CreateEventW(None, true, false, &HSTRING::from(ready_event_name(pid)))? });
    let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_ALL_ACCESS, 0, 0, size_of::<Shared>()) };
    if view.Value.is_null() {
        bail!("映射共享内存失败: {}", windows::core::Error::from_thread());
    }
    let mut link = HookLink { hwnd, _mapping: mapping, view: view.Value.cast(), ready };
    link.shared().init();
    link.publish(&base_snapshot(hwnd, hwnd, client_center(hwnd)));

    let code = inject_dll(process.0, dll)?;
    if unsafe { WaitForSingleObject(link.ready.0, READY_TIMEOUT_MS) } != WAIT_OBJECT_0 {
        bail!(
            "钩子 {READY_TIMEOUT_MS} 毫秒内没有装好(LoadLibraryW 返回 {code:#x})。\
             常见原因:目标机缺 VC++ 运行库,或被安全软件拦截"
        );
    }
    Ok(link)
}
