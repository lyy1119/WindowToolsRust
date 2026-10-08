//! 窗口枚举、信息查询、置顶、按分辨率调整大小。

use crate::state::TargetWindow;
use crate::win::hwnd_i;
use anyhow::{bail, Result};
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    IsZoomed, SetWindowPos, ShowWindow, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GWL_STYLE,
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SWP_NOSIZE,
    SW_RESTORE, WS_EX_TOPMOST, WS_THICKFRAME,
};

pub fn title(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    let n = n.clamp(0, buf.len() as i32) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    let n = n.clamp(0, buf.len() as i32) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn pid(h: HWND) -> u32 {
    let mut p = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
    p
}

/// 进程的可执行文件名（如 `chrome.exe`）。取不到时返回 None
/// （例如目标进程以更高权限运行，我们没权限查询它）。
pub fn process_exe_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        Some(
            full.rsplit(['\\', '/'])
                .next()
                .unwrap_or(full.as_str())
                .to_string(),
        )
    }
}

pub fn is_window(h: HWND) -> bool {
    unsafe { IsWindow(Some(h)).as_bool() }
}

pub fn is_visible(h: HWND) -> bool {
    unsafe { IsWindowVisible(h).as_bool() }
}

pub fn is_iconic(h: HWND) -> bool {
    unsafe { IsIconic(h).as_bool() }
}

pub fn is_zoomed(h: HWND) -> bool {
    unsafe { IsZoomed(h).as_bool() }
}

pub fn rect(h: HWND) -> Result<RECT> {
    let mut rc = RECT::default();
    unsafe { GetWindowRect(h, &mut rc)? };
    Ok(rc)
}

pub fn client_size(h: HWND) -> Result<(i32, i32)> {
    let mut rc = RECT::default();
    unsafe { GetClientRect(h, &mut rc)? };
    Ok((rc.right - rc.left, rc.bottom - rc.top))
}

pub fn is_topmost(h: HWND) -> bool {
    let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
    ex & WS_EX_TOPMOST.0 != 0
}

pub fn is_resizable(h: HWND) -> bool {
    let style = unsafe { GetWindowLongPtrW(h, GWL_STYLE) } as u32;
    style & WS_THICKFRAME.0 != 0
}

/// 置顶 / 取消置顶，返回操作后是否处于置顶状态
pub fn set_topmost(h: HWND, on: bool) -> Result<bool> {
    if !is_window(h) {
        bail!("目标窗口已失效");
    }
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        SetWindowPos(
            h,
            Some(after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
    }
    Ok(is_topmost(h))
}

pub fn toggle_topmost(h: HWND) -> Result<bool> {
    set_topmost(h, !is_topmost(h))
}

/// 把窗口调整到指定尺寸。
///
/// * `client_area = true` 时，(w, h) 指客户区尺寸（不含标题栏与边框），会换算成外框尺寸；
/// * `client_area = false` 时，(w, h) 指整个窗口外框尺寸。
pub fn resize(
    target: HWND,
    width: u32,
    height: u32,
    client_area: bool,
    restore_first: bool,
) -> Result<(i32, i32)> {
    if !is_window(target) {
        bail!("目标窗口已失效");
    }
    if restore_first && (is_iconic(target) || is_zoomed(target)) {
        unsafe { let _ = ShowWindow(target, SW_RESTORE); };
    }
    let (mut tw, mut th) = (width as i32, height as i32);
    if client_area {
        let outer = rect(target)?;
        let (cw, ch) = client_size(target)?;
        let dw = (outer.right - outer.left) - cw;
        let dh = (outer.bottom - outer.top) - ch;
        tw += dw;
        th += dh;
    }
    tw = tw.max(80);
    th = th.max(60);
    unsafe {
        SetWindowPos(
            target,
            None,
            0,
            0,
            tw,
            th,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    Ok((tw, th))
}

/// 光标下最上层窗口的顶层窗口（用于“拾取窗口”）
pub fn root_window_at_cursor() -> Option<HWND> {
    let mut pt = POINT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt).ok()? };
    let under = unsafe { WindowFromPoint(pt) };
    if under.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(under, GA_ROOT) };
    let root = if root.0.is_null() { under } else { root };
    if !is_visible(root) {
        return None;
    }
    Some(root)
}

pub fn foreground_window() -> Option<HWND> {
    let h = unsafe { GetForegroundWindow() };
    if h.0.is_null() {
        None
    } else {
        Some(h)
    }
}

/// shell 自己的窗口（桌面、任务栏、缩略图等），不是用户想操作的对象
const SHELL_CLASSES: [&str; 6] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "TaskListThumbnailWnd",
    "ForegroundStaging",
];

pub fn is_own_process(h: HWND) -> bool {
    pid(h) == std::process::id()
}

/// 是否是一个「值得操作」的顶层窗口。
///
/// **注意：这里刻意不要求必须有 WS_CAPTION。**
/// 之前要求「必须有标题栏」，导致 PotPlayer 的皮肤模式、各种自绘标题栏的程序
/// （用 WS_POPUP / 无 WS_CAPTION 自己画一条标题栏）被直接判为「不支持操作」，
/// 快捷键和拾取都用不了。现在的判定是：
///   * 必须是可见的顶层窗口（root == 自己）；
///   * 不是本程序自己的窗口，也不是 shell 的桌面/任务栏；
///   * 不是「工具窗口/不可激活窗口」——但如果它有标题栏就仍然放行
///     （有些正常窗口会误设这些风格）；
///   * 尺寸不能太小（过滤掉工具条、浮动面板之类）。
pub fn is_eligible(h: HWND) -> bool {
    if !is_window(h) || !is_visible(h) {
        return false;
    }
    if unsafe { GetAncestor(h, GA_ROOT) }.0 != h.0 {
        return false; // 只接受顶层窗口
    }
    if is_own_process(h) {
        return false;
    }
    if SHELL_CLASSES.contains(&class_name(h).as_str()) {
        return false;
    }

    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    let style = unsafe { GetWindowLongPtrW(h, GWL_STYLE) } as u32;
    let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
    let has_caption = style & WS_CAPTION == WS_CAPTION;
    if !has_caption && ex & (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0 {
        return false;
    }

    let Ok(rc) = rect(h) else { return false };
    (rc.right - rc.left) >= 80 && (rc.bottom - rc.top) >= 60
}

pub fn describe(h: HWND) -> TargetWindow {
    let process_id = pid(h);
    TargetWindow {
        hwnd: hwnd_i(h),
        title: title(h),
        class: class_name(h),
        pid: process_id,
        exe: process_exe_name(process_id).unwrap_or_default(),
    }
}

/// 枚举当前所有顶层、可见、带标题栏的窗口（按标题排序）
pub fn enumerate() -> Vec<TargetWindow> {
    let mut out: Vec<TargetWindow> = Vec::new();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<TargetWindow> as isize),
        );
    }
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    out
}

unsafe extern "system" fn enum_proc(h: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<TargetWindow>);
    if is_eligible(h) {
        out.push(describe(h));
    }
    BOOL(1) // 继续枚举
}
