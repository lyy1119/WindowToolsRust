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
//! 菜单结构：
//! ```text
//! ──────────────
//! WindowTools: 标记红框
//! WindowTools: 置顶 / 取消置顶
//! WindowTools: 静音 / 取消静音
//! WindowTools: 调整到指定分辨率  ▸  ┌ 1920 x 1080 ┐
//!                                   │ 1280 x 720  │
//!                                   └ …           ┘
//! ```
//!
//! ⚠️ 命中测试（尤其是子菜单项）在 Linux 上无法验证，需要真机确认。

use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering::Relaxed};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, GetAncestor, GetMenuItemCount, GetMenuItemInfoW,
    GetMenuItemRect, GetSystemMenu, HMENU, RemoveMenu, SetWindowsHookExW, UnhookWindowsHookEx,
    WindowFromPoint, GA_ROOT, HHOOK, MENUITEMINFOW, MF_BYPOSITION, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_SUBMENU, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_LBUTTONDOWN, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use crate::config::ResolutionPreset;
use crate::state::Shared;
use crate::win::{actions, hwnd, hwnd_i, window};

pub const CMD_FRAME: u32 = 0x8101;
pub const CMD_TOPMOST: u32 = 0x8102;
pub const CMD_MUTE: u32 = 0x8103;
pub const CMD_RESIZE_EMPTY: u32 = 0x8104;
/// 预设项的命令 id：0x8200 + 预设下标
pub const CMD_PRESET_BASE: u32 = 0x8200;

const FIXED_CMDS: [u32; 4] = [CMD_FRAME, CMD_TOPMOST, CMD_MUTE, CMD_RESIZE_EMPTY];
/// 一次「右键 -> 菜单 -> 点击」的最大存活时间，超时后不再用旧菜单做命中测试
const ARM_TIMEOUT_MS: u32 = 10_000;

static HOOK: AtomicIsize = AtomicIsize::new(0);
/// 右键按下时记录下来的窗口：接下来弹出的菜单属于它
static ARMED: AtomicIsize = AtomicIsize::new(0);
static ARMED_AT: AtomicI32 = AtomicI32::new(0);
static STATE: OnceCell<Shared> = OnceCell::new();

pub fn preset_cmd(index: usize) -> u32 {
    CMD_PRESET_BASE + index as u32
}

pub fn preset_index(cmd: u32) -> Option<usize> {
    if (CMD_PRESET_BASE..CMD_PRESET_BASE + 0x100).contains(&cmd) {
        Some((cmd - CMD_PRESET_BASE) as usize)
    } else {
        None
    }
}

fn is_fixed_cmd(id: u32) -> bool {
    FIXED_CMDS.contains(&id)
}

/// 往窗口的系统菜单里追加我们的条目（幂等：先清掉旧条目再追加）
pub fn inject(h: HWND, presets: &[ResolutionPreset]) -> Result<()> {
    unsafe {
        let menu = GetSystemMenu(h, false);
        if menu.0.is_null() {
            bail!("窗口没有系统菜单");
        }
        remove_injected(menu);

        AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())?;
        AppendMenuW(menu, MF_STRING, CMD_FRAME as usize, w!("WindowTools: 标记红框"))?;
        AppendMenuW(
            menu,
            MF_STRING,
            CMD_TOPMOST as usize,
            w!("WindowTools: 置顶 / 取消置顶"),
        )?;
        AppendMenuW(
            menu,
            MF_STRING,
            CMD_MUTE as usize,
            w!("WindowTools: 静音 / 取消静音"),
        )?;

        if presets.is_empty() {
            // 没有预设时给一个灰色占位项，避免用户以为功能不见了
            AppendMenuW(
                menu,
                MF_STRING | MF_GRAYED,
                CMD_RESIZE_EMPTY as usize,
                w!("WindowTools: 调整到指定分辨率（未配置预设）"),
            )?;
        } else {
            let sub: HMENU = CreatePopupMenu()?;
            for (i, p) in presets.iter().enumerate() {
                let label = format!("{} x {}", p.width, p.height);
                let label = if p.name.trim().is_empty() {
                    label
                } else {
                    format!("{}（{}）", label, p.name)
                };
                let wide = to_wide(&label);
                AppendMenuW(sub, MF_STRING, preset_cmd(i) as usize, PCWSTR(wide.as_ptr()))?;
            }
            // MF_POPUP 时 uIDNewItem 必须是子菜单句柄
            AppendMenuW(
                menu,
                MF_POPUP,
                sub.0 as usize,
                w!("WindowTools: 调整到指定分辨率"),
            )?;
        }
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

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn menu_item_info(menu: HMENU, pos: u32, mask: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_MASK) -> Option<MENUITEMINFOW> {
    let mut mii = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: mask,
        ..Default::default()
    };
    GetMenuItemInfoW(menu, pos, true, &mut mii).ok().map(|_| mii)
}

/// 这个子菜单是不是我们创建的（用第一个子项的命令 id 判断）
unsafe fn is_our_popup(sub: HMENU) -> bool {
    if sub.0.is_null() {
        return false;
    }
    let n = GetMenuItemCount(Some(sub));
    for i in 0..n.max(0) {
        if let Some(mii) = menu_item_info(sub, i as u32, MIIM_ID) {
            if preset_index(mii.wID).is_some() {
                return true;
            }
        }
    }
    false
}

unsafe fn remove_injected(menu: HMENU) {
    let count = GetMenuItemCount(Some(menu));
    let mut positions: Vec<u32> = Vec::new();
    let mut first_ours: Option<u32> = None;

    for pos in 0..count.max(0) {
        let Some(mii) = menu_item_info(menu, pos as u32, MIIM_ID | MIIM_SUBMENU | MIIM_FTYPE) else {
            continue;
        };
        let ours = is_fixed_cmd(mii.wID) || preset_index(mii.wID).is_some() || is_our_popup(mii.hSubMenu);
        if ours {
            positions.push(pos as u32);
            first_ours = Some(first_ours.map_or(pos as u32, |f| f.min(pos as u32)));
        }
    }

    // 顺带把紧挨在我们条目之前的分隔符一起删掉
    if let Some(first) = first_ours {
        if first > 0 {
            if let Some(mii) = menu_item_info(menu, first - 1, MIIM_FTYPE) {
                if mii.fType == MFT_SEPARATOR {
                    positions.push(first - 1);
                }
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
        let msg = wparam.0 as u32;

        // 超时后放弃旧的「待命中」状态，避免用很久以前的菜单做命中测试
        let now = GetTickCount();
        if ARMED.load(Relaxed) != 0 {
            let armed_at = ARMED_AT.load(Relaxed) as u32;
            if now.wrapping_sub(armed_at) > ARM_TIMEOUT_MS {
                ARMED.store(0, Relaxed);
            }
        }

        match msg {
            WM_RBUTTONDOWN | WM_RBUTTONUP => {
                if let Some(h) = top_level_at(info.pt) {
                    if window::is_eligible(h) {
                        ARMED.store(hwnd_i(h), Relaxed);
                        ARMED_AT.store(now as i32, Relaxed);
                        // 菜单即将弹出，趁机把条目注入进去
                        if let Some(state) = STATE.get() {
                            let (enabled, presets) = {
                                let st = state.lock();
                                (st.config.inject_system_menu, st.config.presets.clone())
                            };
                            if enabled {
                                if let Err(e) = inject(h, &presets) {
                                    log::debug!("注入系统菜单失败: {e:#}");
                                }
                            }
                        }
                    }
                }
            }
            WM_LBUTTONDOWN => {
                let armed = ARMED.load(Relaxed);
                if armed != 0 {
                    // 注意：命中失败**不清除** ARMED —— 用户可能先点开子菜单，
                    // 再点子菜单里的预设项，中间会有多次左键按下。
                    if let Some(cmd) = hit_test(hwnd(armed), info.pt) {
                        ARMED.store(0, Relaxed);
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

/// 光标是否落在我们注入的某个菜单条目（含子菜单里的预设项）上
unsafe fn hit_test(h: HWND, pt: POINT) -> Option<u32> {
    let menu = GetSystemMenu(h, false);
    if menu.0.is_null() {
        return None;
    }
    let count = GetMenuItemCount(Some(menu));
    for pos in 0..count.max(0) {
        let Some(mii) = menu_item_info(menu, pos as u32, MIIM_ID | MIIM_SUBMENU) else {
            continue;
        };
        if is_fixed_cmd(mii.wID) && mii.wID != CMD_RESIZE_EMPTY {
            if let Some(cmd) = hit_at(h, menu, pos as u32, pt) {
                return Some(cmd);
            }
            continue;
        }
        // 子菜单（预设列表）
        if !mii.hSubMenu.0.is_null() && is_our_popup(mii.hSubMenu) {
            let n = GetMenuItemCount(Some(mii.hSubMenu));
            for i in 0..n.max(0) {
                if let Some(cmd) = hit_at(h, mii.hSubMenu, i as u32, pt) {
                    return Some(cmd);
                }
            }
        }
    }
    None
}

/// 单独一项的命中测试：只有菜单真正弹出时 GetMenuItemRect 才会返回有效屏幕坐标
unsafe fn hit_at(h: HWND, menu: HMENU, pos: u32, pt: POINT) -> Option<u32> {
    let id = menu_item_info(menu, pos, MIIM_ID)?.wID;
    let mut rc = RECT::default();
    if GetMenuItemRect(Some(h), menu, pos, &mut rc).is_ok() && contains(&rc, pt) {
        return Some(id);
    }
    None
}

fn contains(rc: &RECT, pt: POINT) -> bool {
    pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom
}

pub fn command_label(cmd: u32) -> String {
    match cmd {
        CMD_FRAME => "标记红框".into(),
        CMD_TOPMOST => "置顶".into(),
        CMD_MUTE => "静音".into(),
        _ => match preset_index(cmd) {
            Some(i) => format!("调整分辨率（预设 #{i}）"),
            None => "未知命令".into(),
        },
    }
}
