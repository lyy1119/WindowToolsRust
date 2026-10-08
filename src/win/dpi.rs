//! DPI 感知设置。
//!
//! 这个必须在**任何窗口创建之前**调用，否则进程会被标记成 DPI-unaware，
//! Windows 会对 `GetWindowRect` / `SetWindowPos` 的坐标做「虚拟化」缩放：
//! 在 125% / 150% 缩放或多显示器混合 DPI 下，算出来的红框位置会整体偏移，
//! 甚至看起来像「不跟随窗口」。

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// 开启 Per-Monitor-V2 DPI 感知。返回是否成功。
///
/// 失败一般意味着 DPI 感知已经被别处（manifest 或 winit）设置过，可以直接忽略。
pub fn enable_per_monitor_v2() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok() }
}
