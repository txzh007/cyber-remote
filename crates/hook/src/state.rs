//! 打开 agent 创建的共享内存,读写映射进本进程。
//!
//! 输入相关的字段 hook 只读不写:快照走 seqlock,Raw Input 走环形缓冲(见 `hook-abi`)。
//! 映射要可写只是因为声音环 `Shared::audio` 是 hook 写、agent 读,除它之外一个字节也不许写。

use std::sync::OnceLock;

use hook_abi::{ABI_VERSION, FLAG_ACTIVE, Shared, Snapshot, mapping_name};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, OpenFileMappingW};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::HSTRING;

/// 视图在进程存活期间一直有效(DLL 不卸载),所以当 `'static` 用
static SHARED: OnceLock<&'static Shared> = OnceLock::new();

/// 打开并校验共享内存。装钩子之前必须成功,否则一个钩子都不装
pub fn open() -> Result<(), String> {
    let name = HSTRING::from(mapping_name(unsafe { GetCurrentProcessId() }));
    unsafe {
        let access = FILE_MAP_READ | FILE_MAP_WRITE;
        let handle = OpenFileMappingW(access.0, false, &name)
            .map_err(|e| format!("打开共享内存 {name} 失败: {e}"))?;
        // 按 Shared 的完整大小映射;映射比它小就直接失败,不会读越界
        let view = MapViewOfFile(handle, access, 0, 0, size_of::<Shared>());
        // 视图自己撑着底层对象,句柄用完就关
        let _ = CloseHandle(handle);
        if view.Value.is_null() {
            return Err(format!("映射共享内存失败: {}", windows::core::Error::from_thread()));
        }
        let shared: &'static Shared = &*(view.Value as *const Shared);
        if !shared.valid() {
            return Err(format!("共享内存校验失败(本 DLL 的 ABI 版本是 {ABI_VERSION})"));
        }
        let _ = SHARED.set(shared);
    }
    Ok(())
}

/// `open` 成功之后才有值
pub fn shared() -> Option<&'static Shared> {
    SHARED.get().copied()
}

/// 一致的快照。agent 崩在写到一半时读不到,返回 None,调用方回退到真实 API。
/// agent 没置 `FLAG_ACTIVE`(已退出,或还没开始接管)时同样返回 None
pub fn snapshot() -> Option<Snapshot> {
    shared()?.read_snapshot().filter(|s| s.flags & FLAG_ACTIVE != 0)
}

/// agent 处于接管状态时的共享内存。Raw Input 的钩子用它决定是拦截还是放行真实事件
pub fn active_shared() -> Option<&'static Shared> {
    snapshot().and(shared())
}
