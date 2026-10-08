//! 托盘图标与托盘菜单（走 tray-icon，纯安全 Rust，不直接碰 Shell_NotifyIcon）。

use anyhow::Result;
use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub const ID_SHOW: &str = "wt.show";
pub const ID_PICK: &str = "wt.pick";
pub const ID_FRAME: &str = "wt.frame";
pub const ID_TOPMOST: &str = "wt.topmost";
pub const ID_MUTE: &str = "wt.mute";
pub const ID_QUIT: &str = "wt.quit";

pub struct Tray {
    /// 必须持有，drop 掉托盘图标就消失了（只用于 RAII，不直接读取）
    #[allow(dead_code)]
    pub icon: TrayIcon,
    pub frame_item: CheckMenuItem,
    pub topmost_item: CheckMenuItem,
}

pub fn build() -> Result<Tray> {
    let menu = Menu::new();

    let show = MenuItem::with_id(ID_SHOW, "显示主窗口", true, None);
    let pick = MenuItem::with_id(ID_PICK, "拾取光标下的窗口", true, None);
    let frame = CheckMenuItem::with_id(ID_FRAME, "标记红框", true, false, None);
    let topmost = CheckMenuItem::with_id(ID_TOPMOST, "窗口置顶", true, false, None);
    let mute = MenuItem::with_id(ID_MUTE, "静音 / 取消静音", true, None);
    let quit = MenuItem::with_id(ID_QUIT, "退出", true, None);

    menu.append_items(&[
        &show,
        &pick,
        &PredefinedMenuItem::separator(),
        &frame,
        &topmost,
        &mute,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;

    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("WindowToolsRust")
        .with_icon(make_icon())
        .build()?;

    Ok(Tray {
        icon,
        frame_item: frame,
        topmost_item: topmost,
    })
}

/// 生成一个 32x32 的图标：红色边框 + 深色内芯（和工具本身的「红框」呼应）
fn make_icon() -> Icon {
    const S: u32 = 32;
    const BORDER: u32 = 4;
    let mut rgba = vec![0u8; (S * S * 4) as usize];
    for y in 0..S {
        for x in 0..S {
            let i = ((y * S + x) * 4) as usize;
            let on_border =
                x < BORDER || y < BORDER || x >= S - BORDER || y >= S - BORDER;
            if on_border {
                rgba[i] = 0xE5;
                rgba[i + 1] = 0x28;
                rgba[i + 2] = 0x28;
                rgba[i + 3] = 0xFF;
            } else {
                // 中间全透明，形成一个「框」
                rgba[i + 3] = 0x00;
            }
        }
    }
    Icon::from_rgba(rgba, S, S).expect("内置图标数据一定是合法的")
}
