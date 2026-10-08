//! 防止重复启动（单实例）。
//!
//! 机制（全部是标准 Win32 对象，不需要额外进程通信代码）：
//!   * **命名互斥体** `Local\WindowToolsRust.SingleInstance`
//!     第一个实例创建并持有它；后来的实例会发现它已存在，于是：
//!   * 用**命名事件** `Local\WindowToolsRust.ShowWindow` 通知第一个实例
//!     「把主窗口显示出来」，然后自己直接退出。
//!
//! 用 `Local\` 前缀而不是 `Global\`：这样是「每个登录会话一个实例」，
//! 多用户同时登录时互不影响。
//!
//! ⚠️ 一个容易踩的坑：**提权重启前必须先 [`release`] 互斥体**。
//! 因为 `ShellExecuteW("runas")` 是「先创建好新进程再返回」，
//! 如果此时旧进程还占着互斥体，新起来的提权进程会把自己当成第二个实例而立刻退出，
//! 结果就是「点了以管理员身份重启，结果程序反而没了」。

use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject,
};

const MUTEX_NAME: PCWSTR = w!("Local\\WindowToolsRust.SingleInstance");
const SHOW_EVENT_NAME: PCWSTR = w!("Local\\WindowToolsRust.ShowWindow");

/// 保存在静态里（而不是放进 AppState）：句柄是裸指针，放进共享状态会破坏 Send/Sync
static MUTEX: AtomicIsize = AtomicIsize::new(0);
static SHOW_EVENT: AtomicIsize = AtomicIsize::new(0);

fn as_handle(v: isize) -> HANDLE {
    HANDLE(v as *mut core::ffi::c_void)
}

/// 尝试成为唯一实例。返回 `true` 表示可以继续启动。
pub fn acquire() -> bool {
    unsafe {
        match CreateMutexW(None, true, MUTEX_NAME) {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(h);
                    false
                } else {
                    MUTEX.store(h.0 as isize, Relaxed);
                    true
                }
            }
            Err(e) => {
                // ERROR_ACCESS_DENIED 之类：多半是已有实例、而且它是以更高权限运行的。
                // 保守起见按「已经有实例」处理，避免真的跑出两个实例。
                log::warn!("单实例互斥体创建失败，按「已有实例」处理: {e}");
                false
            }
        }
    }
}

/// 释放唯一实例标记。
///
/// **提权重启之前必须调用**，否则新起的提权进程会被误判成第二个实例。
pub fn release() {
    let v = MUTEX.swap(0, Relaxed);
    if v != 0 {
        unsafe {
            let _ = CloseHandle(as_handle(v));
        }
    }
}

fn ensure_event() -> HANDLE {
    let v = SHOW_EVENT.load(Relaxed);
    if v != 0 {
        return as_handle(v);
    }
    unsafe {
        match CreateEventW(None, false, false, SHOW_EVENT_NAME) {
            Ok(h) => {
                SHOW_EVENT.store(h.0 as isize, Relaxed);
                h
            }
            Err(_) => HANDLE::default(),
        }
    }
}

/// 第二个实例调用：请求已经运行的那个实例把主窗口显示出来
pub fn request_show() {
    unsafe {
        // 事件已经由第一个实例创建好了，这里打开的是同一个对象
        if let Ok(h) = CreateEventW(None, false, false, SHOW_EVENT_NAME) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
        }
    }
}

/// 有没有别的实例请求我们显示窗口。
/// 事件是**自动重置**的，读到一次就自动清零，不会重复触发。
pub fn take_show_request() -> bool {
    let h = ensure_event();
    if h.is_invalid() {
        return false;
    }
    unsafe { WaitForSingleObject(h, 0) == WAIT_OBJECT_0 }
}
