//! Windows 原生能力封装层。
//!
//! 约定：**只有本模块内部允许出现 unsafe**，对上层暴露安全的函数/类型。
//! 这样 GUI / 托盘 / 业务逻辑代码里不会散落 unsafe 与裸句柄。

pub mod actions;
pub mod audio;
pub mod dpi;
pub mod frame;
pub mod privilege;
pub mod single_instance;
pub mod sysmenu;
pub mod window;

use windows::Win32::Foundation::HWND;

/// 把 isize 形式的窗口句柄还原为 HWND（句柄只在跨线程共享时以 isize 保存）
pub fn hwnd(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

/// 把 HWND 转成可跨线程保存的 isize
pub fn hwnd_i(h: HWND) -> isize {
    h.0 as isize
}
