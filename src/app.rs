//! eframe 应用：把托盘、全局热键、界面、后台音频轮询串在一起。
//!
//! eframe 0.36 的 `App` trait 分两个回调：
//!   * `logic()` —— 不画界面，即使主窗口隐藏时也会被调用（正好用来处理托盘事件）；
//!   * `ui()`    —— 画界面。

use crate::hotkey::{self, Hotkeys};
use crate::state::Shared;
use crate::tray::{self, Tray};
use crate::ui::{self, UiState};
use crate::win::{actions, frame, hwnd, sysmenu, window};
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
    /// 用户是否已经明确要求退出（托盘菜单「退出」/ 关掉「关闭最小化到托盘」时点 X）
    quitting: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, state: Shared) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let cfg = state.lock().config.clone();

        // egui 自带字体不含 CJK，不装系统字体的话中文全是方框
        match crate::fonts::install(&cc.egui_ctx, &cfg.font_file) {
            Some(name) => state.lock().log(format!("已加载中文字体: {name}")),
            None => state
                .lock()
                .log("[警告] 未找到可用的中文字体，界面中文可能显示为方框"),
        }

        let tray = match tray::build() {
            Ok(t) => Some(t),
            Err(e) => {
                state.lock().log(format!("托盘创建失败: {e:#}"));
                None
            }
        };
        // 启动时自己检查一下有没有管理员权限
        if state.lock().elevated() {
            state.lock().log("权限检查: 已以管理员身份运行");
        } else {
            state.lock().log(
                "权限检查: 未以管理员身份运行 —— 无法操作以管理员权限运行的窗口（界面顶部有提权按钮）",
            );
        }

        let hotkeys = match hotkey::build(&cfg) {
            Ok(h) => {
                state.lock().log(format!(
                    "全局热键已注册: {}",
                    hotkey::describe(&cfg)
                ));
                Some(h)
            }
            Err(e) => {
                state.lock().log(format!("全局热键注册失败: {e:#}"));
                None
            }
        };

        let config_path = state.lock().config_path.display().to_string();
        let ui_state = UiState::new(&cfg, config_path);

        Self {
            state,
            tray,
            hotkeys,
            ui_state,
            last_audio_poll: None,
            last_frame_flag: None,
            last_topmost_flag: None,
            quitting: false,
        }
    }

    fn show_main_window(ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    /// 有别的实例启动过 → 它请求我们把窗口显示出来
    fn pump_show_request(&mut self, ctx: &egui::Context) {
        if !self.state.lock().config.single_instance {
            return;
        }
        if crate::win::single_instance::take_show_request() {
            Self::show_main_window(ctx);
            self.state.lock().log("检测到重复启动，已唤出主窗口");
        }
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
                    let msg = report(actions::toggle_frame(&self.state, false));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_TOPMOST => {
                    let msg = report(actions::toggle_topmost(&self.state, false));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_MUTE => {
                    let msg = report(actions::toggle_mute(&self.state, false));
                    self.state.lock().log(format!("[托盘] {msg}"));
                }
                tray::ID_QUIT => {
                    self.quitting = true;
                    self.state.lock().save_config();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
    }

    fn pump_hotkeys(&mut self, ctx: &egui::Context) {
        let Some(hk) = self.hotkeys.as_ref() else { return };
        // 留空的快捷键是 None，不参与匹配
        let ids = [
            hk.pick.map(|k| k.id),
            hk.frame.map(|k| k.id),
            hk.topmost.map(|k| k.id),
            hk.mute.map(|k| k.id),
        ];

        // 快捷键默认作用于「当前前台窗口」（可在设置里改回作用于已拾取的目标）
        let fg = self.state.lock().config.hotkey_foreground;

        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state != HotKeyState::Pressed {
                continue;
            }
            let result = if ids[0] == Some(ev.id) {
                // 拾取按钮本身就是「取光标下的窗口」，不受前台/目标设置影响
                actions::pick_under_cursor(&self.state).map(|t| format!("已选中: {}", t.short_label()))
            } else if ids[1] == Some(ev.id) {
                actions::toggle_frame(&self.state, fg)
            } else if ids[2] == Some(ev.id) {
                actions::toggle_topmost(&self.state, fg)
            } else if ids[3] == Some(ev.id) {
                actions::toggle_mute(&self.state, fg)
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

    /// 界面改了快捷键后，在这里真正重新注册
    fn apply_pending_hotkeys(&mut self) {
        if !self.state.lock().pending_hotkey_apply {
            return;
        }
        self.state.lock().pending_hotkey_apply = false;
        let cfg = self.state.lock().config.clone();

        let result = match self.hotkeys.as_mut() {
            Some(h) => h.reapply(&cfg),
            None => match hotkey::build(&cfg) {
                Ok(h) => {
                    self.hotkeys = Some(h);
                    Ok(())
                }
                Err(e) => Err(e),
            },
        };

        match result {
            Ok(()) => {
                let msg = format!("全局热键已更新: {}", hotkey::describe(&cfg));
                self.state.lock().log(msg);
            }
            Err(e) => {
                // 注册失败时 reapply 已经把旧的热键恢复回去了
                self.state
                    .lock()
                    .log(format!("全局热键更新失败，已保留原有快捷键: {e:#}"));
            }
        }
        // 让界面上的输入框回到「真正生效的值」
        let live = self.state.lock().config.clone();
        self.ui_state.hotkey_pick = live.hotkey_pick;
        self.ui_state.hotkey_frame = live.hotkey_frame;
        self.ui_state.hotkey_topmost = live.hotkey_topmost;
        self.ui_state.hotkey_mute = live.hotkey_mute;
    }

    /// 只做「把音频工作线程的快照搬到界面状态」这件轻活。
    ///
    /// 真正的设备/会话枚举在工作线程里完成且有缓存（1.5s），
    /// 不会因为界面每帧刷新就反复枚举 COM 对象。
    fn poll_audio(&mut self) {
        let due = self
            .last_audio_poll
            .map(|t| t.elapsed() >= Duration::from_millis(600))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.last_audio_poll = Some(Instant::now());

        // 只有音频快照的版本号变了才整表拷贝（内部已经做了「内容没变不改版本号」）
        let snap = self.state.lock().audio.snapshot();
        if snap.rev != self.ui_state.audio.rev || snap.error != self.ui_state.audio.error {
            self.ui_state.audio = snap;
        }
    }

    /// 日志尾部缓存：只在日志版本号变化时重建，避免每帧克隆几百条字符串。
    fn refresh_log_tail(&mut self) {
        let rev = self.state.lock().log_rev;
        if rev == self.ui_state.log_rev {
            return;
        }
        let (rev, tail) = {
            let st = self.state.lock();
            (
                st.log_rev,
                st.log.iter().rev().take(150).rev().cloned().collect::<Vec<_>>(),
            )
        };
        self.ui_state.log_rev = rev;
        self.ui_state.log_tail = tail;
    }

    /// 点窗口关闭按钮时的处理：默认只隐藏到托盘，让托盘继续常驻。
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        let (close_to_tray, pending_quit) = {
            let st = self.state.lock();
            (st.config.close_to_tray, st.pending_quit)
        };
        if self.quitting || pending_quit || !close_to_tray {
            // 真退出：不再拦截
            self.quitting = true;
            return;
        }
        // 取消这次关闭，改为隐藏窗口；进程与托盘继续活着
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.state
            .lock()
            .log("窗口已隐藏到托盘，程序继续在后台运行（托盘图标右键 → 退出 可结束）");
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
        self.handle_close_request(ctx);
        self.pump_show_request(ctx);
        self.apply_pending_hotkeys();
        self.pump_tray_menu(ctx);
        self.pump_hotkeys(ctx);
        self.pump_tray_icon(ctx);
        self.poll_audio();
        self.refresh_log_tail();
        self.sync_tray_state();

        // 红框跟随的兜底轮询：即使 WinEvent 钩子漏事件，红框也不会卡住不动。
        // 有红框时提高刷新率让跟随更跟手，没有时降到 4~5Hz 省 CPU。
        frame::tick();
        let interval = if frame::is_active() {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(400)
        };
        // 主窗口隐藏时也要保持轮询（托盘 / 钩子回调依赖消息循环持续运转）
        ctx.request_repaint_after(interval);
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
