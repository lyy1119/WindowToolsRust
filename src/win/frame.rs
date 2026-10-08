//! 给目标窗口套一个红框。
//!
//! 实现方式（不注入、不 hook 目标进程）：
//!   1. 自己创建一个「只有边框、中间镂空」的置顶窗口；
//!   2. 用 `SetWindowRgn` 挖空中心，所以中间区域完全透明且鼠标可穿透；
//!   3. 用 `SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE)` 跟随目标窗口的移动/缩放，
//!      `WINEVENT_OUTOFCONTEXT` 表示回调在我们的进程里执行，**不需要 DLL 注入**；
//!   4. 另外由界面线程每帧调用 [`tick`] 做兜底轮询 —— 即使事件钩子漏事件，
//!      红框也不会「卡住不动」。
//!
//! 关于「目标窗口置顶后红框跟不上」的两个坑（都已处理）：
//!   * **坐标来源**：`GetWindowRect` 返回的是含「不可见缩放边框」的外框，比肉眼看到的
//!     窗口边缘大一圈。对着它画框，红框会落在目标窗口**自身非客户区的绘制范围内**；
//!     目标窗口一旦置顶并排到我们上面，红框就会被它自己的边框画掉（看起来就是红框
//!     不动了/消失了）。改用 `DWMWA_EXTENDED_FRAME_BOUNDS`（可见帧边界）再向外扩即可。
//!   * **z-order**：目标窗口自己变成 TOPMOST 后会被排到置顶层最上方。所以除了几何变化
//!     时重新插入 TOPMOST，还会周期性兜底重申一次，保证红框始终压在窗口上面。

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering::Relaxed};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateRectRgn, CreateSolidBrush, DeleteObject, EndPaint, FillRect,
    InvalidateRect, SetWindowRgn, HBRUSH, HGDIOBJ, HRGN, PAINTSTRUCT, RGN_DIFF,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, GetWindowRect, IsIconic, IsWindow,
    IsWindowVisible, RegisterClassW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE, HTTRANSPARENT, HWND_TOPMOST, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::win::{hwnd, hwnd_i, window};

const OVERLAY_CLASS: PCWSTR = w!("WindowToolsRust.FrameOverlay");
const UNSET: i32 = i32::MIN;
/// z-order 兜底重申的最小间隔（毫秒）
const Z_REASSERT_MS: i32 = 1000;

static OVERLAY: AtomicIsize = AtomicIsize::new(0);
static TARGET: AtomicIsize = AtomicIsize::new(0);
static HOOK: AtomicIsize = AtomicIsize::new(0);
static THICKNESS: AtomicI32 = AtomicI32::new(3);
static BRUSH: AtomicIsize = AtomicIsize::new(0);
/// 防止 SetWindowPos 触发的 LOCATIONCHANGE 回调里再次 SetWindowPos 造成递归
static UPDATING: AtomicBool = AtomicBool::new(false);
static SHOWN: AtomicBool = AtomicBool::new(false);
static LAST_X: AtomicI32 = AtomicI32::new(UNSET);
static LAST_Y: AtomicI32 = AtomicI32::new(UNSET);
static LAST_W: AtomicI32 = AtomicI32::new(UNSET);
static LAST_H: AtomicI32 = AtomicI32::new(UNSET);
static LAST_Z_TICK: AtomicI32 = AtomicI32::new(UNSET);
/// 红框颜色（RGB 打包）
static COLOR: AtomicI32 = AtomicI32::new(0x00E5_2828);

fn colorref_from(rgb: [u8; 3]) -> COLORREF {
    COLORREF(rgb[0] as u32 | ((rgb[1] as u32) << 8) | ((rgb[2] as u32) << 16))
}

fn pack_rgb(rgb: [u8; 3]) -> i32 {
    (rgb[0] as i32) | ((rgb[1] as i32) << 8) | ((rgb[2] as i32) << 16)
}

/// 预先创建覆盖层窗口，避免第一次显示时有可见延迟
pub fn init() {
    if let Err(e) = unsafe { ensure_overlay() } {
        log::warn!("红框覆盖层初始化失败: {e:#}");
    }
}

/// 是否正在标记某个窗口
pub fn is_active() -> bool {
    TARGET.load(Relaxed) != 0
}

/// 当前被标记的窗口句柄
pub fn active_target() -> Option<isize> {
    match TARGET.load(Relaxed) {
        0 => None,
        v => Some(v),
    }
}

/// 给 `target` 画红框
pub fn show(target: HWND, rgb: [u8; 3], thickness: i32) -> Result<()> {
    if !unsafe { IsWindow(Some(target)).as_bool() } {
        bail!("目标窗口已失效");
    }
    unsafe {
        set_color(rgb);
        THICKNESS.store(thickness.clamp(1, 32), Relaxed);
        ensure_overlay()?;

        // 先卸掉旧的跟随钩子，再装新的（换目标时必须重装，因为钩子按 pid 过滤）
        unhook();
        let pid = window::pid(target);
        let hook = SetWinEventHook(
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(win_event_proc),
            pid,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        if hook.0.is_null() {
            log::warn!("SetWinEventHook 失败，红框将只依赖轮询跟随");
        }
        HOOK.store(hook.0 as isize, Relaxed);

        TARGET.store(hwnd_i(target), Relaxed);
        // 目标换了，几何缓存全部失效
        reset_cache();
        let overlay = hwnd(OVERLAY.load(Relaxed));
        let _ = InvalidateRect(Some(overlay), None, true);
        refresh();
    }
    Ok(())
}

/// 关闭红框
pub fn hide() {
    unsafe {
        unhook();
        TARGET.store(0, Relaxed);
        reset_cache();
        let o = OVERLAY.load(Relaxed);
        if o != 0 {
            let _ = ShowWindow(hwnd(o), SW_HIDE);
        }
        SHOWN.store(false, Relaxed);
    }
}

fn reset_cache() {
    LAST_X.store(UNSET, Relaxed);
    LAST_Y.store(UNSET, Relaxed);
    LAST_W.store(UNSET, Relaxed);
    LAST_H.store(UNSET, Relaxed);
    LAST_Z_TICK.store(UNSET, Relaxed);
}

/// 重算覆盖层几何。由 WinEvent 回调触发，也由界面线程每帧调用做兜底。
pub fn refresh() {
    if UPDATING.swap(true, Relaxed) {
        return; // 正在更新，避免重入
    }
    unsafe { refresh_inner() };
    UPDATING.store(false, Relaxed);
}

/// 界面线程每帧调用：跟随 + 置顶兜底 + 最小化/关闭检测
pub fn tick() {
    if !is_active() {
        return;
    }
    refresh();
}

unsafe fn refresh_inner() {
    let t = TARGET.load(Relaxed);
    let o = OVERLAY.load(Relaxed);
    if t == 0 || o == 0 {
        return;
    }
    let target = hwnd(t);
    let overlay = hwnd(o);
    if !IsWindow(Some(target)).as_bool() {
        hide();
        return;
    }

    // 目标最小化 / 隐藏时把红框也藏起来
    if !IsWindowVisible(target).as_bool() || IsIconic(target).as_bool() {
        if SHOWN.swap(false, Relaxed) {
            let _ = ShowWindow(overlay, SW_HIDE);
        }
        return;
    }

    let Some(rc) = visible_frame_rect(target) else {
        return;
    };
    let th = THICKNESS.load(Relaxed).max(1);
    let x = rc.left - th;
    let y = rc.top - th;
    let w = (rc.right - rc.left) + th * 2;
    let h = (rc.bottom - rc.top) + th * 2;
    if w <= th * 2 || h <= th * 2 {
        return;
    }

    let size_changed = LAST_W.load(Relaxed) != w || LAST_H.load(Relaxed) != h;
    let moved = LAST_X.load(Relaxed) != x || LAST_Y.load(Relaxed) != y;

    if size_changed {
        apply_region(overlay, w, h, th);
        LAST_W.store(w, Relaxed);
        LAST_H.store(h, Relaxed);
    }

    if moved || size_changed || !SHOWN.load(Relaxed) {
        LAST_X.store(x, Relaxed);
        LAST_Y.store(y, Relaxed);
        let _ = SetWindowPos(
            overlay,
            Some(HWND_TOPMOST),
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        SHOWN.store(true, Relaxed);
        return;
    }

    // 几何没变：只做 z-order 兜底，保证目标窗口自己置顶后红框仍压在它上面
    let now = GetTickCount() as i32;
    let last = LAST_Z_TICK.load(Relaxed);
    if last == UNSET || now.wrapping_sub(last) >= Z_REASSERT_MS {
        LAST_Z_TICK.store(now, Relaxed);
        let _ = SetWindowPos(
            overlay,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 目标窗口**可见**的帧边界（物理像素）。
///
/// `GetWindowRect` 对 DWM 合成窗口会包含一圈不可见的缩放边框，比肉眼看到的窗口大；
/// `DWMWA_EXTENDED_FRAME_BOUNDS` 才是真正可见的边缘。取不到时退回 `GetWindowRect`。
unsafe fn visible_frame_rect(target: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    let ok = DwmGetWindowAttribute(
        target,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut rc as *mut RECT as *mut core::ffi::c_void,
        std::mem::size_of::<RECT>() as u32,
    );
    if ok.is_ok() && rc.right > rc.left && rc.bottom > rc.top {
        return Some(rc);
    }
    let mut fallback = RECT::default();
    if GetWindowRect(target, &mut fallback).is_ok() {
        Some(fallback)
    } else {
        None
    }
}

fn apply_region(overlay: HWND, w: i32, h: i32, th: i32) {
    unsafe {
        let outer: HRGN = CreateRectRgn(0, 0, w, h);
        let inner: HRGN = CreateRectRgn(th, th, w - th, h - th);
        if outer.0.is_null() || inner.0.is_null() {
            return;
        }
        CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
        // SetWindowRgn 成功后区域归系统所有，不要再自己删 outer
        let _ = SetWindowRgn(overlay, Some(outer), true);
        let _ = DeleteObject(HGDIOBJ(inner.0));
    }
}

fn set_color(rgb: [u8; 3]) {
    let packed = pack_rgb(rgb);
    if COLOR.swap(packed, Relaxed) == packed && BRUSH.load(Relaxed) != 0 {
        return;
    }
    unsafe {
        let old = BRUSH.swap(0, Relaxed);
        if old != 0 {
            let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
        }
        let brush = CreateSolidBrush(colorref_from(rgb));
        BRUSH.store(brush.0 as isize, Relaxed);
    }
}

fn unhook() {
    let h = HOOK.swap(0, Relaxed);
    if h != 0 {
        unsafe {
            let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut core::ffi::c_void));
        }
    }
}

unsafe fn ensure_overlay() -> Result<HWND> {
    let existing = OVERLAY.load(Relaxed);
    if existing != 0 && IsWindow(Some(hwnd(existing))).as_bool() {
        return Ok(hwnd(existing));
    }
    let hmodule = GetModuleHandleW(None)?;
    let hinstance = HINSTANCE(hmodule.0);
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: Default::default(),
        hCursor: Default::default(),
        // NULL 背景刷：背景完全由 WM_PAINT 自己画，换色时不会留下悬空句柄
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: OVERLAY_CLASS,
    };
    // 已注册过会返回 0，忽略即可
    RegisterClassW(&class);

    let ex_style: WINDOW_EX_STYLE =
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
    let style: WINDOW_STYLE = WS_POPUP;
    let overlay = CreateWindowExW(
        ex_style,
        OVERLAY_CLASS,
        w!(""),
        style,
        0,
        0,
        1,
        1,
        None,
        None,
        Some(hinstance),
        None,
    )?;
    OVERLAY.store(hwnd_i(overlay), Relaxed);
    Ok(overlay)
}

unsafe extern "system" fn overlay_proc(
    h: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // 鼠标完全穿透：点红框等于点下面的窗口
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(h, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            let brush = HBRUSH(BRUSH.load(Relaxed) as *mut core::ffi::c_void);
            if !brush.0.is_null() {
                FillRect(hdc, &rc, brush);
            }
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wparam, lparam),
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    h: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_child != 0 || id_object != 0 {
        return; // 只关心窗口对象本身 (OBJID_WINDOW == 0, CHILDID_SELF == 0)
    }
    if hwnd_i(h) != TARGET.load(Relaxed) {
        return;
    }
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => refresh(),
        EVENT_OBJECT_DESTROY => {
            hide();
            log::info!("目标窗口已销毁，红框自动关闭");
        }
        _ => {}
    }
}
