//! eframe 应用：把托盘、全局热键、界面、后台音频轮询串在一起。
//!
//! eframe 0.36 的 `App` trait 分两个回调：
//!   * `logic()` —— 不画界面，即使主窗口隐藏时也会被调用（正好用来处理托盘事件）；
//!   * `ui()`    —— 画界面。

use crate::hotkey::{self, Hotkeys};
use crate::state::Shared;
use crate::tray::{self, Tray};
use crate::ui::{self, UiState};
use crate::win::{actions, hwnd, sysmenu, window};
use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use std::time::{Duration, Instant};
use tray_icon::menu::MenuEvent;
use tray_icon::TrayIconEvent;

pub struct App {
    state: Shared,
    tray: Option<Tray>,
    hotkeys: Option<Hotkeys>,
    ui_state: UiState,
    last_audio_poll: Option<Instant>,
    last_frame_flag: Option<bool>,
    last_topmost_flag: Option<bool>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, state: Shared) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let tray = match tray::build() {
            Ok(t) => Some(t),
            Err(e) => {
                state.lock().log(format!("托盘创建失败: {e:#}"));
                None
            }
        };
        let hotkeys = match hotkey::build() {
            Ok(h) => {
                state.lock().log("全局热键已注册: Ctrl+Alt+P/F/T/M");
                Some(h)
            }
            Err(e) => {
                state.lock().log(format!("全局热键注册失败: {e:#}"));
                None
            }
        };

        let ui_state = {
            let cfg = state.lock().config.clone();
            UiState::new(&cfg, hotkeys.as_ref())
        };

        Self {
            state,
            tray,
            hotkeys,
            ui_state,
            last_audio_poll: None,
            last_frame_flag: None,
            last_topmost_flag: None,
        }
    }

    fn show_main_window(ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn pump_tray_menu(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let id = ev.id.as_ref().to_string();
            match id.as_str() {
                tray::ID_SHOW => {
                    Self::show_main_window(ctx);
                }
                tray::ID_PICK => {
                    let msg = report(actions::pick_under_cursor(&self.state)
                        .map(|t| format!("已选中: {}", t.short_label())));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_FRAME => {
                    let msg = report(actions::toggle_frame(&self.state));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_TOPMOST => {
                    let msg = report(actions::toggle_topmost(&self.state));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_MUTE => {
                    let msg = report(actions::toggle_mute(&self.state));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_QUIT => {
                    self.state.lock().save_config();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
    }

    fn pump_hotkeys(&mut self, ctx: &egui::Context) {
        let Some(hk) = self.hotkeys.as_ref() else { return };
        let (pick, frame_hk, topmost, mute) = (hk.pick.id, hk.frame.id, hk.topmost.id, hk.mute.id);

        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state != HotKeyState::Pressed {
                continue;
            }
            let result = if ev.id == pick {
                actions::pick_under_cursor(&self.state).map(|t| format!("已选中: {}", t.short_label()))
            } else if ev.id == frame_hk {
                actions::toggle_frame(&self.state)
            } else if ev.id == topmost {
                actions::toggle_topmost(&self.state)
            } else if ev.id == mute {
                actions::toggle_mute(&self.state)
            } else {
                continue;
            };
            let msg = report(result);
            self.state.lock().log(format!("[热键] {msg}"));
            ctx.request_repaint();
        }
    }

    fn pump_tray_icon(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = ev {
                Self::show_main_window(ctx);
            }
        }
    }

    /// 周期性刷新目标进程的静音状态
    fn poll_audio(&mut self) {
        let due = self
            .last_audio_poll
            .map(|t| t.elapsed() >= Duration::from_millis(800))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_audio_poll = Some(Instant::now());

        let mut st = self.state.lock();
        if let Some(pid) = st.target_pid() {
            st.audio.refresh(pid);
        }
        let snap = st.audio.snapshot();
        st.audio_snapshot = snap;
    }

    /// 托盘勾选状态跟随实际状态
    fn sync_tray_state(&mut self) {
        let Some(tray) = self.tray.as_ref() else { return };
        let (frame_on, topmost) = {
            let st = self.state.lock();
            let topmost = st
                .target
                .as_ref()
                .map(|t| window::is_topmost(hwnd(t.hwnd)))
                .unwrap_or(false);
            (st.frame_on, topmost)
        };
        if self.last_frame_flag != Some(frame_on) {
            tray.frame_item.set_checked(frame_on);
            self.last_frame_flag = Some(frame_on);
        }
        if self.last_topmost_flag != Some(topmost) {
            tray.topmost_item.set_checked(topmost);
            self.last_topmost_flag = Some(topmost);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_tray_menu(ctx);
        self.pump_hotkeys(ctx);
        self.pump_tray_icon(ctx);
        self.poll_audio();
        self.sync_tray_state();
        // 主窗口隐藏时也要保持轮询（托盘 / 钩子回调依赖消息循环持续运转）
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(ui, &self.state, &mut self.ui_state);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.state.lock().save_config();
        sysmenu::uninstall_command_watcher();
    }
}

fn report(r: anyhow::Result<String>) -> String {
    match r {
        Ok(msg) => msg,
        Err(e) => format!("失败: {e:#}"),
    }
}
