//! 红框覆盖层 —— **支持同时标记多个窗口**。
//!
//! 设计要点：每个被标记的窗口都有自己独立的覆盖层窗口，互不干扰。
//! （早期版本只有一组全局 `TARGET`/`OVERLAY`，导致「置顶第二个窗口时第一个的红框消失」，
//! 因为「置顶自动加红框」本来就是会作用于多个窗口的功能。）
//!
//! 实现原理（对每个目标窗口各来一份）：
//!   1. 创建一个 `WS_POPUP` + `WS_EX_TOOLWINDOW|NOACTIVATE|TRANSPARENT` 的窗口
//!      （**刻意不带 `WS_EX_TOPMOST`**，见下面的 z-order 说明）；
//!   2. 用 `SetWindowRgn` 挖空中心（`CombineRgn(RGN_DIFF)`），中间透明且鼠标可穿透
//!      （再配合 `WM_NCHITTEST` 返回 `HTTRANSPARENT` 双保险）；
//!   3. 用 `SetWinEventHook(..., WINEVENT_OUTOFCONTEXT)` 跟随目标窗口移动/缩放
//!      （回调在我们自己的进程里执行，**不需要 DLL 注入**）；
//!   4. 主线程每帧调 [`tick`] 做兜底轮询，即使钩子漏事件也不会卡住。
//!
//! ## z-order：覆盖层必须紧贴在自己那个目标窗口的正上方
//!
//! 覆盖层**不能**无条件做成 TOPMOST。否则同时标记多个窗口时，红框会浮在所有窗口之上：
//! 你点击窗口 A 让它盖住窗口 B 时，B 的红框仍然画在 A 的前面 —— 这不符合直觉。
//!
//! 正确做法是维持一条不变式：**每个覆盖层在 z 序里紧挨在自己目标窗口的上方一格**。
//! 这样谁盖住了目标窗口，就自然一起盖住它的红框。
//!
//! 实现上有两个要点：
//!   * **覆盖层的 topmost 状态必须跟目标一致**：Windows 强制「置顶窗口」整体高于
//!     「普通窗口」，普通窗口无法被放到置顶窗口之上。所以目标是 TOPMOST 时覆盖层也得是，
//!     否则红框会跑到目标窗口下面去（看不见）。目标取消置顶后要及时降回普通层。
//!   * **靠 `GW_HWNDPREV` 校验**：每帧检查 `GetWindow(target, GW_HWNDPREV) == overlay`，
//!     不成立就重新插到目标上方。用户点击切换前后顺序后，红框会在下一帧跟过去。
//!
//! 钩子管理：`SetWinEventHook` 只能按**进程**过滤，不能按窗口过滤。
//! 所以这里按「被标记窗口所属的 pid」去重安装钩子 —— 同一个进程的多个窗口共用一个钩子。
//!
//! 线程模型：所有操作都发生在主线程（`App::logic` 与钩子回调），用一个 Mutex 保护状态。

use anyhow::{bail, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::OnceLock;
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
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowRect, IsIconic, IsWindow,
    IsWindowVisible, RegisterClassW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE, GetWindow, GW_HWNDPREV, HTTRANSPARENT,
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::win::{hwnd, hwnd_i, window};

const OVERLAY_CLASS: PCWSTR = w!("WindowToolsRust.FrameOverlay");
const UNSET: i32 = i32::MIN;
/// 修正 z-order 的最小间隔（毫秒）。
///
/// 只在「`GW_HWNDPREV` 校验发现顺序不对」时才动手，所以正常情况下零开销；
/// 加这个下限是为了万一某次 SetWindowPos 没生效，不至于每帧反复重试。
const Z_FIX_MS: i32 = 60;

/// 单个被标记窗口的覆盖层
struct Frame {
    /// 覆盖层窗口句柄
    overlay: isize,
    /// 上次应用的几何，用于变化检测（没变就不调 SetWindowPos）
    last_x: i32,
    last_y: i32,
    last_w: i32,
    last_h: i32,
    /// 覆盖层当前是否可见
    shown: bool,
    /// 覆盖层当前的 topmost 状态（None = 还没同步过），用来跟随目标窗口所在的 z 序带
    topmost: Option<bool>,
    /// 上次修正 z-order 的 GetTickCount
    last_z_tick: i32,
}

impl Frame {
    fn new(overlay: isize) -> Self {
        Self {
            overlay,
            last_x: UNSET,
            last_y: UNSET,
            last_w: UNSET,
            last_h: UNSET,
            shown: false,
            topmost: None,
            last_z_tick: UNSET,
        }
    }

    /// 几何缓存失效（换样式/重新显示时用）
    fn invalidate(&mut self) {
        self.last_x = UNSET;
        self.last_y = UNSET;
        self.last_w = UNSET;
        self.last_h = UNSET;
        self.last_z_tick = UNSET;
    }
}

#[derive(Default)]
struct FrameState {
    /// key = 目标窗口句柄，value = 该窗口的覆盖层
    frames: HashMap<isize, Frame>,
    /// pid -> WinEvent 钩子句柄（同一个进程的多个窗口共用一个钩子）
    hooks: HashMap<u32, isize>,
    /// 当前红框颜色（RGB）
    rgb: [u8; 3],
    /// 当前边框粗细
    thickness: i32,
    /// 窗口类是否已注册
    class_ok: bool,
}

static STATE: OnceLock<Mutex<FrameState>> = OnceLock::new();

/// 画笔（所有覆盖层共用）。
///
/// 单独用原子量存、**不放进 `FrameState`**：因为 `WM_PAINT` 里要用它，
/// 而 `ShowWindow` / `DestroyWindow` 等调用可能同步派发窗口消息到 `overlay_proc`。
/// 如果在 `WM_PAINT` 里 `lock()`，一旦这层调用发生在持锁区域内就会死锁
/// （`parking_lot::Mutex` 不可重入）。
static BRUSH: AtomicIsize = AtomicIsize::new(0);

fn state() -> &'static Mutex<FrameState> {
    STATE.get_or_init(|| {
        Mutex::new(FrameState {
            frames: HashMap::new(),
            hooks: HashMap::new(),
            rgb: [229, 40, 40],
            thickness: 3,
            class_ok: false,
        })
    })
}

fn colorref_from(rgb: [u8; 3]) -> COLORREF {
    COLORREF(rgb[0] as u32 | ((rgb[1] as u32) << 8) | ((rgb[2] as u32) << 16))
}

// ---------------------------------------------------------------- 公开 API

/// 注册覆盖层窗口类。启动时调一次即可（不预先创建窗口，覆盖层是按需为每个目标创建的）。
pub fn init() {
    let mut st = state().lock();
    if let Err(e) = ensure_class(&mut st) {
        log::warn!("红框覆盖层窗口类注册失败: {e:#}");
    }
}

/// 当前是否有任意窗口被标记
pub fn has_any() -> bool {
    !state().lock().frames.is_empty()
}

/// 当前被标记的窗口数量
pub fn count() -> usize {
    state().lock().frames.len()
}

/// 某个窗口当前是否被标记
pub fn is_framed(target: HWND) -> bool {
    state().lock().frames.contains_key(&hwnd_i(target))
}

/// 给 `target` 加上红框（已经加过就只更新样式）。
///
/// 注意**不会**影响其它已经标记的窗口。
pub fn show(target: HWND, rgb: [u8; 3], thickness: i32) -> Result<()> {
    if !unsafe { IsWindow(Some(target)).as_bool() } {
        bail!("目标窗口已失效");
    }
    let mut st = state().lock();
    apply_style(&mut st, rgb, thickness);

    let key = hwnd_i(target);
    if !st.frames.contains_key(&key) {
        ensure_class(&mut st)?;
        let overlay = unsafe { create_overlay() }?;
        st.frames.insert(key, Frame::new(hwnd_i(overlay)));
    }
    sync_hooks(&mut st);
    refresh_entry(&mut st, key);
    Ok(())
}

/// 只更新样式（颜色/粗细），不新增也不移除任何红框。
pub fn restyle(rgb: [u8; 3], thickness: i32) {
    let mut st = state().lock();
    apply_style(&mut st, rgb, thickness);
}

/// 移除某个窗口的红框。窗口不存在时也安全。
pub fn remove(target: HWND) {
    let mut st = state().lock();
    let key = hwnd_i(target);
    if st.frames.contains_key(&key) {
        destroy_entry(&mut st, key);
        sync_hooks(&mut st);
    }
}

/// 移除所有红框。
pub fn remove_all() {
    let mut st = state().lock();
    let keys: Vec<isize> = st.frames.keys().copied().collect();
    for k in keys {
        destroy_entry(&mut st, k);
    }
    sync_hooks(&mut st);
}

/// 在「有框 / 无框」之间切换**这一个**窗口，返回切换后是否处于有框状态。
pub fn toggle(target: HWND, rgb: [u8; 3], thickness: i32) -> Result<bool> {
    if is_framed(target) {
        remove(target);
        Ok(false)
    } else {
        show(target, rgb, thickness)?;
        Ok(true)
    }
}

/// 主线程每帧调用：清理已销毁的目标 + 刷新所有红框几何。
pub fn tick() {
    let mut st = state().lock();
    if st.frames.is_empty() {
        return;
    }

    // 清理目标窗口已经消失的项
    let dead: Vec<isize> = st
        .frames
        .keys()
        .copied()
        .filter(|k| !unsafe { IsWindow(Some(hwnd(*k))).as_bool() })
        .collect();
    if !dead.is_empty() {
        for k in dead {
            destroy_entry(&mut st, k);
        }
        sync_hooks(&mut st);
    }

    let keys: Vec<isize> = st.frames.keys().copied().collect();
    for k in keys {
        refresh_entry(&mut st, k);
    }
}

// ---------------------------------------------------------------- 内部实现

fn ensure_class(st: &mut FrameState) -> Result<()> {
    if st.class_ok {
        return Ok(());
    }
    unsafe {
        let hmodule = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: HINSTANCE(hmodule.0),
            hIcon: Default::default(),
            hCursor: Default::default(),
            // NULL 背景刷：背景完全由 WM_PAINT 自己画，换色重建画笔时不会留下悬空句柄
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: OVERLAY_CLASS,
        };
        // 已注册过会返回 0，忽略即可
        RegisterClassW(&class);
    }
    st.class_ok = true;
    Ok(())
}

/// 更新全局样式（颜色/粗细）。粗细变化会让所有缓存的几何失效，需要重算镂空区域。
fn apply_style(st: &mut FrameState, rgb: [u8; 3], thickness: i32) {
    let th = thickness.clamp(1, 32);

    if BRUSH.load(Relaxed) == 0 || st.rgb != rgb {
        unsafe {
            let new = CreateSolidBrush(colorref_from(rgb)).0 as isize;
            let old = BRUSH.swap(new, Relaxed);
            if old != 0 {
                let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
            }
        }
        st.rgb = rgb;
        // 颜色变了，所有覆盖层都要重绘
        for f in st.frames.values() {
            if unsafe { IsWindow(Some(hwnd(f.overlay))).as_bool() } {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd(f.overlay)), None, true);
                }
            }
        }
    }

    if st.thickness != th {
        st.thickness = th;
        for f in st.frames.values_mut() {
            f.invalidate();
        }
    }
}

fn destroy_entry(st: &mut FrameState, key: isize) {
    if let Some(f) = st.frames.remove(&key) {
        unsafe {
            let o = hwnd(f.overlay);
            if IsWindow(Some(o)).as_bool() {
                let _ = DestroyWindow(o);
            }
        }
    }
}

/// 让「已安装的钩子」与「当前被标记窗口的 pid 集合」保持一致
fn sync_hooks(st: &mut FrameState) {
    let mut need: Vec<u32> = st
        .frames
        .keys()
        .map(|k| window::pid(hwnd(*k)))
        .filter(|p| *p != 0)
        .collect();
    need.sort_unstable();
    need.dedup();

    // 卸掉多余的
    let stale: Vec<u32> = st
        .hooks
        .keys()
        .copied()
        .filter(|p| !need.contains(p))
        .collect();
    for p in stale {
        if let Some(h) = st.hooks.remove(&p) {
            unsafe {
                let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut core::ffi::c_void));
            }
        }
    }

    // 装上缺的
    for p in need {
        if st.hooks.contains_key(&p) {
            continue;
        }
        unsafe {
            let hook = SetWinEventHook(
                EVENT_OBJECT_DESTROY,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(win_event_proc),
                p,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.0.is_null() {
                log::warn!("SetWinEventHook 失败（pid={p}），该窗口的红框将只依赖轮询跟随");
            } else {
                st.hooks.insert(p, hook.0 as isize);
            }
        }
    }
}

/// 刷新单个窗口的红框几何
fn refresh_entry(st: &mut FrameState, key: isize) {
    let th = st.thickness.max(1);
    let target = hwnd(key);
    let Some(f) = st.frames.get_mut(&key) else {
        return;
    };
    let overlay = hwnd(f.overlay);

    unsafe {
        if !IsWindow(Some(overlay)).as_bool() {
            return;
        }
        // 目标最小化 / 隐藏 / 已销毁时把红框藏起来
        if !IsWindow(Some(target)).as_bool()
            || !IsWindowVisible(target).as_bool()
            || IsIconic(target).as_bool()
        {
            if f.shown {
                let _ = ShowWindow(overlay, SW_HIDE);
                f.shown = false;
            }
            return;
        }

        let Some(rc) = visible_frame_rect(target) else {
            return;
        };
        let x = rc.left - th;
        let y = rc.top - th;
        let w = (rc.right - rc.left) + th * 2;
        let h = (rc.bottom - rc.top) + th * 2;
        if w <= th * 2 || h <= th * 2 {
            return;
        }

        let size_changed = f.last_w != w || f.last_h != h;
        let moved = f.last_x != x || f.last_y != y;

        if size_changed {
            apply_region(overlay, w, h, th);
            f.last_w = w;
            f.last_h = h;
        }

        // (1) 先让覆盖层待在**和目标同一个 z 序带**里：
        //     目标是置顶窗口 -> 覆盖层也必须置顶；目标变回普通窗口 -> 覆盖层也要降下来。
        //     否则 Windows 会强制把普通窗口排在所有置顶窗口之下，红框就跑到目标下面去了。
        let target_topmost = window::is_topmost(target);
        let mut need_reinsert = false;
        if f.topmost != Some(target_topmost) {
            let band = if target_topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(
                overlay,
                Some(band),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            f.topmost = Some(target_topmost);
            need_reinsert = true;
        }

        // (2) 位置/尺寸变了、或刚切换过 z 序带 -> 重新插到目标窗口的正上方一格
        if moved || size_changed || !f.shown || need_reinsert {
            f.last_x = x;
            f.last_y = y;
            let _ = SetWindowPos(
                overlay,
                Some(target),
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            f.shown = true;
            f.last_z_tick = GetTickCount() as i32;
            return;
        }

        // (3) 几何没变时校验 z-order：覆盖层必须紧挨在目标上方
        //     （GW_HWNDPREV = 该窗口在 z 序里的前一个窗口，也就是压在它上面的那个）。
        //     用户点击切换前后顺序后，这里会在下一帧把红框挪回去，
        //     于是「谁盖住目标窗口，就一起盖住它的红框」。
        let prev = GetWindow(target, GW_HWNDPREV).unwrap_or_default();
        if prev.0 != overlay.0 {
            let now = GetTickCount() as i32;
            if f.last_z_tick == UNSET || now.wrapping_sub(f.last_z_tick) >= Z_FIX_MS {
                f.last_z_tick = now;
                let _ = SetWindowPos(
                    overlay,
                    Some(target),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
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

/// 把覆盖层挖成「回」字形：外框减去内框
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

unsafe fn create_overlay() -> Result<HWND> {
    let hmodule = GetModuleHandleW(None)?;
    // 注意：**不要**带 WS_EX_TOPMOST。置顶与否由 refresh_entry 按目标窗口的状态动态决定，
    // 否则同时标记多个窗口时所有红框都会浮在最上层，遮挡关系错乱。
    let ex_style: WINDOW_EX_STYLE =
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
    let style: WINDOW_STYLE = WS_POPUP;
    Ok(CreateWindowExW(
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
        Some(HINSTANCE(hmodule.0)),
        None,
    )?)
}

// ---------------------------------------------------------------- 窗口过程 / 钩子

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
            // 画笔从原子量读，**不要在这里加锁**（见 BRUSH 的注释）
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
    // 只关心窗口对象本身（OBJID_WINDOW == 0, CHILDID_SELF == 0）
    if id_child != 0 || id_object != 0 {
        return;
    }
    let key = hwnd_i(h);
    {
        // 钩子是按进程装的，同进程其它窗口的事件也会进来，先快速过滤
        let st = state().lock();
        if !st.frames.contains_key(&key) {
            return;
        }
    }
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => {
            let mut st = state().lock();
            refresh_entry(&mut st, key);
        }
        EVENT_OBJECT_DESTROY => {
            log::info!("被标记的窗口已销毁，红框自动移除");
            let mut st = state().lock();
            destroy_entry(&mut st, key);
            sync_hooks(&mut st);
        }
        _ => {}
    }
}
