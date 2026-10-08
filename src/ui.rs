//! egui 界面。刻意只做「显示 + 触发动作」，所有 Windows 操作都通过 `win::actions`。

use crate::config::{Config, ResolutionPreset};
use crate::hotkey::Hotkeys;
use crate::state::{Shared, TargetWindow};
use crate::win::{actions, frame, sysmenu, window};
use eframe::egui;

pub struct UiState {
    /// 窗口枚举结果（点「刷新」才重新枚举）
    pub windows: Vec<TargetWindow>,
    pub pick_idx: usize,
    /// 可编辑的分辨率预设（点「保存设置」才写回配置，并刷新右键菜单子菜单）
    pub presets: Vec<ResolutionPreset>,
    pub new_preset_name: String,
    pub new_preset_w: u32,
    pub new_preset_h: u32,
    pub custom_w: u32,
    pub custom_h: u32,
    pub color: [u8; 3],
    pub thickness: i32,
    pub resize_client_area: bool,
    pub restore_before_resize: bool,
    pub auto_frame_on_pick: bool,
    pub inject_system_menu: bool,
    pub hotkey_help: Vec<(String, String)>,
}

impl UiState {
    pub fn new(cfg: &Config, hotkeys: Option<&Hotkeys>) -> Self {
        Self {
            windows: Vec::new(),
            pick_idx: 0,
            presets: cfg.presets.clone(),
            new_preset_name: String::new(),
            new_preset_w: 1280,
            new_preset_h: 720,
            custom_w: 1280,
            custom_h: 720,
            color: cfg.frame_color,
            thickness: cfg.frame_thickness,
            resize_client_area: cfg.resize_client_area,
            restore_before_resize: cfg.restore_before_resize,
            auto_frame_on_pick: cfg.auto_frame_on_pick,
            inject_system_menu: cfg.inject_system_menu,
            hotkey_help: hotkeys
                .map(|h| {
                    h.describe()
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// 把界面上的设置写回配置
    pub fn apply_to(&self, cfg: &mut Config) {
        cfg.frame_color = self.color;
        cfg.frame_thickness = self.thickness;
        cfg.resize_client_area = self.resize_client_area;
        cfg.restore_before_resize = self.restore_before_resize;
        cfg.auto_frame_on_pick = self.auto_frame_on_pick;
        cfg.inject_system_menu = self.inject_system_menu;
        cfg.presets = self.presets.clone();
    }
}

/// 一次快照，避免在画界面的过程中反复加锁
struct Snapshot {
    target: Option<TargetWindow>,
    frame_on: bool,
    audio: crate::win::audio::AudioSnapshot,
    log_tail: Vec<String>,
    topmost: bool,
    resizable: bool,
    config_path: String,
}

fn snapshot(state: &Shared) -> Snapshot {
    let st = state.lock();
    let target = st.target.clone();
    let topmost = target
        .as_ref()
        .map(|t| window::is_topmost(crate::win::hwnd(t.hwnd)))
        .unwrap_or(false);
    let resizable = target
        .as_ref()
        .map(|t| window::is_resizable(crate::win::hwnd(t.hwnd)))
        .unwrap_or(false);
    let log_tail = st.log.iter().rev().take(200).rev().cloned().collect();
    Snapshot {
        target,
        frame_on: st.frame_on,
        audio: st.audio_snapshot.clone(),
        log_tail,
        topmost,
        resizable,
        config_path: st.config_path.display().to_string(),
    }
}

pub fn draw(ui: &mut egui::Ui, state: &Shared, s: &mut UiState) {
    let snap = snapshot(state);
    let mut toast: Option<String> = None;

    ui.heading("WindowToolsRust");
    ui.label(
        egui::RichText::new("窗口增强工具：红框标记 / 置顶 / 指定分辨率 / 按进程静音")
            .small()
            .weak(),
    );
    ui.separator();

    // ---------- 目标窗口 ----------
    ui.heading("目标窗口");
    match &snap.target {
        None => {
            ui.label("尚未选中窗口。把鼠标移到目标窗口上，按 Ctrl+Alt+P 或点下面的按钮。");
        }
        Some(t) => {
            egui::Grid::new("target_grid")
                .num_columns(2)
                .spacing([12.0, 2.0])
                .show(ui, |ui| {
                    ui.label("标题");
                    ui.monospace(t.short_label());
                    ui.end_row();
                    ui.label("类名");
                    ui.monospace(&t.class);
                    ui.end_row();
                    ui.label("PID");
                    ui.monospace(t.pid.to_string());
                    ui.end_row();
                    ui.label("HWND");
                    ui.monospace(format!("0x{:X}", t.hwnd));
                    ui.end_row();
                    ui.label("可调整大小");
                    ui.monospace(if snap.resizable { "是" } else { "否（无 WS_THICKFRAME）" });
                    ui.end_row();
                });
        }
    }

    ui.horizontal(|ui| {
        if ui.button("🎯 拾取光标下的窗口").clicked() {
            toast = Some(report(
                actions::pick_under_cursor(state).map(|t| format!("已选中: {}", t.short_label())),
            ));
        }
        if ui.button("拾取前台窗口").clicked() {
            toast = Some(report(
                actions::pick_foreground(state).map(|t| format!("已选中: {}", t.short_label())),
            ));
        }
        if ui.button("刷新窗口列表").clicked() {
            s.windows = window::enumerate();
            s.pick_idx = 0;
        }
    });

    if !s.windows.is_empty() {
        let mut chosen: Option<usize> = None;
        let current = s
            .windows
            .get(s.pick_idx)
            .map(|w| w.short_label())
            .unwrap_or_else(|| "-".into());
        egui::ComboBox::from_label("从列表选择")
            .selected_text(current)
            .width(420.0)
            .show_ui(ui, |ui| {
                for (i, w) in s.windows.iter().enumerate() {
                    let label = format!("{}  [{}]", w.short_label(), w.class);
                    if ui.selectable_label(s.pick_idx == i, label).clicked() {
                        chosen = Some(i);
                    }
                }
            });
        if let Some(i) = chosen {
            s.pick_idx = i;
            let h = s.windows[i].hwnd;
            let want_frame = state.lock().config.auto_frame_on_pick;
            let r = actions::set_target(state, crate::win::hwnd(h), "列表选择", want_frame)
                .map(|t| format!("已选中: {}", t.short_label()));
            toast = Some(report(r));
        }
    }

    ui.separator();

    // ---------- 窗口操作 ----------
    ui.heading("窗口操作");
    ui.horizontal(|ui| {
        let frame_label = if snap.frame_on { "🔴 关闭红框" } else { "🔴 标记红框" };
        if ui.button(frame_label).clicked() {
            toast = Some(report(actions::toggle_frame(state)));
        }

        let top_label = if snap.topmost { "取消置顶" } else { "置顶" };
        if ui.button(top_label).clicked() {
            toast = Some(report(actions::toggle_topmost(state)));
        }

        let mute_label = match snap.audio.muted {
            Some(true) => "🔇 取消静音",
            _ => "🔇 静音",
        };
        if ui.button(mute_label).clicked() {
            toast = Some(report(actions::toggle_mute(state)));
        }
    });

    // 静音状态说明
    match (&snap.audio.error, snap.audio.muted, snap.audio.session_count) {
        (Some(e), _, _) => {
            ui.label(egui::RichText::new(format!("音频: {e}")).small().weak());
        }
        (None, Some(m), n) if n > 0 => {
            ui.label(
                egui::RichText::new(format!(
                    "音频会话 {n} 个，当前{}",
                    if m { "已静音" } else { "未静音" }
                ))
                .small()
                .weak(),
            );
        }
        _ => {
            ui.label(egui::RichText::new("音频: 未检测到会话").small().weak());
        }
    }

    ui.separator();

    // ---------- 分辨率预设（可编辑） ----------
    ui.heading("分辨率预设");
    ui.label(
        egui::RichText::new(
            "这些预设会出现在标题栏右键菜单的「WindowTools: 调整到指定分辨率」子菜单里。改完记得点「保存设置」。",
        )
        .small()
        .weak(),
    );

    let mut to_delete: Option<usize> = None;
    let mut apply_idx: Option<usize> = None;
    egui::Grid::new("preset_grid")
        .num_columns(5)
        .spacing([8.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("名称").strong());
            ui.label(egui::RichText::new("宽").strong());
            ui.label(egui::RichText::new("高").strong());
            ui.label("");
            ui.label("");
            ui.end_row();

            for i in 0..s.presets.len() {
                ui.add(
                    egui::TextEdit::singleline(&mut s.presets[i].name)
                        .desired_width(130.0)
                        .hint_text("可选"),
                );
                ui.add(egui::DragValue::new(&mut s.presets[i].width).range(80..=16384).speed(4));
                ui.add(egui::DragValue::new(&mut s.presets[i].height).range(60..=16384).speed(4));
                if ui.button("应用到当前窗口").clicked() {
                    apply_idx = Some(i);
                }
                if ui.button("🗑 删除").clicked() {
                    to_delete = Some(i);
                }
                ui.end_row();
            }
        });

    ui.horizontal(|ui| {
        ui.label("新增预设");
        ui.add(
            egui::TextEdit::singleline(&mut s.new_preset_name)
                .desired_width(130.0)
                .hint_text("名称（可空）"),
        );
        ui.add(egui::DragValue::new(&mut s.new_preset_w).range(80..=16384).speed(4));
        ui.label("x");
        ui.add(egui::DragValue::new(&mut s.new_preset_h).range(60..=16384).speed(4));
        if ui.button("➕ 添加").clicked() {
            s.presets.push(ResolutionPreset {
                name: s.new_preset_name.trim().to_string(),
                width: s.new_preset_w,
                height: s.new_preset_h,
            });
            s.new_preset_name.clear();
        }
    });

    if let Some(i) = apply_idx {
        if let Some(p) = s.presets.get(i).cloned() {
            toast = Some(report(actions::resize_to(state, p.width, p.height)));
        }
    }
    if let Some(i) = to_delete {
        if i < s.presets.len() {
            s.presets.remove(i);
        }
    }

    ui.horizontal(|ui| {
        ui.label("自定义尺寸");
        ui.add(egui::DragValue::new(&mut s.custom_w).range(80..=16384).speed(4));
        ui.label("x");
        ui.add(egui::DragValue::new(&mut s.custom_h).range(60..=16384).speed(4));
        if ui.button("应用").clicked() {
            toast = Some(report(actions::resize_to(state, s.custom_w, s.custom_h)));
        }
        ui.checkbox(&mut s.resize_client_area, "按客户区计算");
        ui.checkbox(&mut s.restore_before_resize, "先还原最大化");
    });

    ui.separator();

    // ---------- 设置 ----------
    ui.heading("设置");
    egui::Grid::new("settings_grid")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            ui.label("红框颜色");
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(&mut s.color);
                if ui.button("立即应用").clicked() {
                    let (rgb, th) = (s.color, s.thickness);
                    {
                        let mut st = state.lock();
                        st.config.frame_color = rgb;
                        st.config.frame_thickness = th;
                    }
                    if let Some(t) = &snap.target {
                        toast = Some(report(
                            frame::show(crate::win::hwnd(t.hwnd), rgb, th)
                                .map(|_| "红框样式已更新".to_string()),
                        ));
                        state.lock().frame_on = true;
                    }
                }
            });
            ui.end_row();

            ui.label("红框粗细");
            ui.add(egui::Slider::new(&mut s.thickness, 1..=16).suffix(" px"));
            ui.end_row();

            ui.label("拾取时自动标记红框");
            ui.checkbox(&mut s.auto_frame_on_pick, "");
            ui.end_row();

            ui.label("注入标题栏右键菜单");
            ui.checkbox(&mut s.inject_system_menu, "");
            ui.end_row();
        });

    ui.horizontal(|ui| {
        if ui.button("💾 保存设置").clicked() {
            s.apply_to(&mut state.lock().config);
            state.lock().save_config();
            // 预设变了，顺手把右键菜单里的子菜单刷新一遍
            if let Some(t) = &snap.target {
                let presets = state.lock().config.presets.clone();
                let _ = sysmenu::inject(crate::win::hwnd(t.hwnd), &presets);
            }
            toast = Some("设置已保存".into());
        }
        if ui.button("重新注入系统菜单").clicked() {
            if let Some(t) = &snap.target {
                let presets = state.lock().config.presets.clone();
                let r = sysmenu::inject(crate::win::hwnd(t.hwnd), &presets)
                    .map(|_| "已重新注入系统菜单条目".to_string());
                toast = Some(report(r));
            }
        }
        if ui.button("移除系统菜单注入").clicked() {
            if let Some(t) = &snap.target {
                let r = sysmenu::remove(crate::win::hwnd(t.hwnd))
                    .map(|_| "已移除系统菜单条目".to_string());
                toast = Some(report(r));
            }
        }
    });

    ui.label(
        egui::RichText::new(format!("配置文件: {}", snap.config_path))
            .small()
            .weak(),
    );

    if !s.hotkey_help.is_empty() {
        ui.separator();
        ui.heading("全局快捷键");
        egui::Grid::new("hotkey_grid")
            .num_columns(2)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for (name, key) in &s.hotkey_help {
                    ui.label(name);
                    ui.monospace(key);
                    ui.end_row();
                }
            });
    }

    if !toast.as_deref().unwrap_or("").is_empty() {
        let msg = toast.clone().unwrap_or_default();
        ui.separator();
        ui.label(egui::RichText::new(msg).strong());
    }

    // ---------- 日志 ----------
    ui.separator();
    ui.heading("运行日志");
    egui::ScrollArea::vertical()
        .max_height(160.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for line in &snap.log_tail {
                ui.monospace(line);
            }
        });
}

/// 统一的动作结果 → 界面提示 + 日志
fn report(r: anyhow::Result<String>) -> String {
    match r {
        Ok(msg) => msg,
        Err(e) => format!("失败: {e:#}"),
    }
}
