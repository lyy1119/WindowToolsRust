//! 把自定义条目注入到**其它进程窗口**的标题栏右键（系统）菜单里，并捕获点击。
//!
//! 关键技术事实（决定了实现方式）：
//!   * `GetSystemMenu(hwnd, false)` + `AppendMenuW` 对**其它进程**的窗口是有效的，
//!     菜单对象由窗口管理器持有，所以「注入条目」这一步不需要注入 DLL。
//!   * 但是**跨进程子类化是被系统禁止的**：`SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ..)`
//!     对其它进程的窗口会失败，所以点击后目标进程收到 `WM_SYSCOMMAND` 我们无法收到通知。
//!   * 因此点击捕获走 **WH_MOUSE_LL 低层鼠标钩子**（该钩子的回调在本进程执行，
//!     同样不需要注入 DLL）：右键按下时记录窗口并注入菜单，左键按下时用
//!     `GetMenuItemRect` 做命中测试，落在我们的条目上就执行动作。
//!
//! ⚠️ 这套命中测试逻辑在 Linux 上无法验证，需要真机确认；如果发现不灵，
//!    备选方案见 README（改为「自绘替换菜单」）。

use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, GetAncestor, GetMenuItemCount, GetMenuItemInfoW, GetMenuItemRect,
    GetSystemMenu, RemoveMenu, SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, GA_ROOT,
    HHOOK, MENUITEMINFOW, MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MFT_SEPARATOR, MIIM_FTYPE,
    MIIM_ID, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use crate::state::Shared;
use crate::win::{actions, hwnd, hwnd_i, window};

pub const CMD_FRAME: u32 = 0x8101;
pub const CMD_TOPMOST: u32 = 0x8102;
pub const CMD_RESIZE: u32 = 0x8103;
pub const CMD_MUTE: u32 = 0x8104;

const ALL_CMDS: [u32; 4] = [CMD_FRAME, CMD_TOPMOST, CMD_RESIZE, CMD_MUTE];

static HOOK: AtomicIsize = AtomicIsize::new(0);
/// 右键按下时记录下来的窗口：接下来弹出的菜单属于它
static ARMED: AtomicIsize = AtomicIsize::new(0);
static STATE: OnceCell<Shared> = OnceCell::new();

/// 往窗口的系统菜单里追加我们的条目（幂等：先清掉旧条目再追加）
pub fn inject(h: HWND) -> Result<()> {
    unsafe {
        let menu = GetSystemMenu(h, false);
        if menu.0.is_null() {
            bail!("窗口没有系统菜单");
        }
        remove_injected(menu);
        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(menu, MF_STRING, CMD_FRAME as usize, w!("WindowTools: 标记红框"))?;
        AppendMenuW(menu, MF_STRING, CMD_TOPMOST as usize, w!("WindowTools: 置顶 / 取消置顶"))?;
        AppendMenuW(menu, MF_STRING, CMD_RESIZE as usize, w!("WindowTools: 调整到指定分辨率"))?;
        AppendMenuW(menu, MF_STRING, CMD_MUTE as usize, w!("WindowTools: 静音 / 取消静音"))?;
    }
    Ok(())
}

/// 从窗口的系统菜单里移除我们的条目
pub fn remove(h: HWND) -> Result<()> {
    unsafe {
        let menu = GetSystemMenu(h, false);
        if menu.0.is_null() {
            bail!("窗口没有系统菜单");
        }
        remove_injected(menu);
    }
    Ok(())
}

unsafe fn remove_injected(menu: windows::Win32::UI::WindowsAndMessaging::HMENU) {
    let count = GetMenuItemCount(Some(menu));
    let mut positions: Vec<u32> = Vec::new();
    let mut first_ours: Option<u32> = None;
    for pos in 0..count.max(0) {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_FTYPE,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, pos as u32, true, &mut mii).is_ok() {
            if ALL_CMDS.contains(&mii.wID) {
                positions.push(pos as u32);
                first_ours = Some(first_ours.map_or(pos as u32, |f| f.min(pos as u32)));
            }
        }
    }
    // 顺带把紧挨在我们条目之前的分隔符一起删掉
    if let Some(first) = first_ours {
        if first > 0 {
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            if GetMenuItemInfoW(menu, first - 1, true, &mut mii).is_ok()
                && mii.fType == MFT_SEPARATOR
            {
                positions.push(first - 1);
            }
        }
    }
    positions.sort_unstable();
    positions.dedup();
    for pos in positions.into_iter().rev() {
        let _ = RemoveMenu(menu, pos, MF_BYPOSITION);
    }
}

/// 安装低层鼠标钩子，用来捕获系统菜单里我们注入的条目被点击
pub fn install_command_watcher(shared: Shared) -> Result<()> {
    let _ = STATE.set(shared);
    if HOOK.load(Relaxed) != 0 {
        return Ok(());
    }
    unsafe {
        let hook: HHOOK = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)?;
        HOOK.store(hook.0 as isize, Relaxed);
    }
    Ok(())
}

pub fn uninstall_command_watcher() {
    let h = HOOK.swap(0, Relaxed);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut core::ffi::c_void));
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        match wparam.0 as u32 {
            WM_RBUTTONDOWN | WM_RBUTTONUP => {
                if let Some(h) = top_level_at(info.pt) {
                    if window::is_eligible(h) {
                        ARMED.store(hwnd_i(h), Relaxed);
                        // 菜单即将弹出，趁机把条目注入进去
                        let enabled = STATE
                            .get()
                            .map(|s| s.lock().config.inject_system_menu)
                            .unwrap_or(false);
                        if enabled {
                            if let Err(e) = inject(h) {
                                log::debug!("注入系统菜单失败: {e:#}");
                            }
                        }
                    }
                }
            }
            WM_LBUTTONDOWN => {
                let armed = ARMED.swap(0, Relaxed);
                if armed != 0 {
                    if let Some(cmd) = hit_test(hwnd(armed), info.pt) {
                        if let Some(state) = STATE.get() {
                            actions::dispatch_menu_command(cmd, hwnd(armed), state);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn top_level_at(pt: POINT) -> Option<HWND> {
    unsafe {
        let under = WindowFromPoint(pt);
        if under.0.is_null() {
            return None;
        }
        let root = GetAncestor(under, GA_ROOT);
        Some(if root.0.is_null() { under } else { root })
    }
}

/// 光标是否落在我们注入的某个菜单条目上
unsafe fn hit_test(h: HWND, pt: POINT) -> Option<u32> {
    let menu = GetSystemMenu(h, false);
    if menu.0.is_null() {
        return None;
    }
    let count = GetMenuItemCount(Some(menu));
    for pos in 0..count.max(0) {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, pos as u32, true, &mut mii).is_err() {
            continue;
        }
        if !ALL_CMDS.contains(&mii.wID) {
            continue;
        }
        let mut rc = RECT::default();
        // 只有菜单真正弹出时 GetMenuItemRect 才会给出有效的屏幕坐标
        if GetMenuItemRect(Some(h), menu, pos as u32, &mut rc).is_ok() && contains(&rc, pt) {
            return Some(mii.wID);
        }
    }
    None
}

fn contains(rc: &RECT, pt: POINT) -> bool {
    pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
}

pub fn command_label(cmd: u32) -> &'static str {
    match cmd {
        CMD_FRAME => "标记红框",
        CMD_TOPMOST => "置顶",
        CMD_RESIZE => "调整分辨率",
        CMD_MUTE => "静音",
        _ => "未知命令",
    }
}
