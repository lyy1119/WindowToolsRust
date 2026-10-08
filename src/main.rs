// WindowToolsRust —— 一个类似 windowTop 的 Windows 窗口增强工具
//
// 设计原则：
//   * 需要用 Windows 原生能力的部分（窗口操作、红框、系统菜单、音频）走 `windows` crate；
//   * 界面 / 托盘 / 热键走纯 Rust 生态（egui / tray-icon / global-hotkey），尽量少碰 unsafe；
//   * 所有 Win32 调用集中在本 crate 的 `win` 模块里，其它模块只面对安全封装。
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod app;
mod config;
mod hotkey;
mod state;
mod tray;
mod ui;
mod win;

use parking_lot::Mutex;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config_path = config::default_config_path();
    let cfg = config::Config::load(&config_path);
    let state = Arc::new(Mutex::new(state::AppState::new(cfg, config_path)));
    state.lock().log("WindowToolsRust 启动");

    // 系统菜单命令监听：低层鼠标钩子，必须在有消息循环的线程（主线程）上安装。
    // 这里先装钩子，稍后 eframe/winit 的事件循环会在同一线程上泵消息。
    if let Err(e) = win::sysmenu::install_command_watcher(state.clone()) {
        state.lock().log(format!("[警告] 系统菜单命令监听安装失败: {e:#}"));
    }

    // 预创建红框覆盖层窗口，避免首次标记时闪一下
    win::frame::init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([820.0, 600.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("WindowToolsRust"),
        ..Default::default()
    };

    let app_state = state.clone();
    eframe::run_native(
        "WindowToolsRust",
        options,
        Box::new(move |cc| {
            let app = app::App::new(cc, app_state);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe 启动失败: {e}"))?;

    state.lock().save_config();
    Ok(())
}
