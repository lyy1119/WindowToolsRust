//! 全局应用状态。所有模块共享一个 `Shared`（Arc<Mutex<AppState>>）。

use crate::config::Config;
use crate::win::audio::AudioService;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Shared = Arc<Mutex<AppState>>;

/// 被选中的目标窗口
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TargetWindow {
    pub hwnd: isize,
    pub title: String,
    pub class: String,
    pub pid: u32,
    /// 进程可执行文件名，用于按进程名匹配音频会话
    pub exe: String,
}

impl TargetWindow {
    pub fn short_label(&self) -> String {
        if self.title.is_empty() {
            format!("{} (0x{:X})", self.class, self.hwnd)
        } else {
            self.title.clone()
        }
    }
}

pub struct AppState {
    pub config: Config,
    pub config_path: PathBuf,
    pub target: Option<TargetWindow>,
    pub frame_on: bool,
    pub audio: AudioService,
    /// 当前已经注入过系统菜单的窗口 hwnd
    pub menu_injected: Option<isize>,
    /// 因为「置顶时自动标记红框」而加上的红框属于哪个窗口。
    /// 只记录自动加上的那种，取消置顶时才会自动移除，不会误删用户手动标的红框。
    pub frame_auto_for_topmost: Option<isize>,
    /// 界面改了快捷键后置位，由 App 在下一帧真正重新注册
    pub pending_hotkey_apply: bool,
    /// 界面要求真正退出（例如点了「以管理员身份重启」）。
    /// 有这个标记时就不再拦截窗口关闭，否则会被「关闭最小化到托盘」吃掉。
    pub pending_quit: bool,
    pub log: Vec<String>,
    /// 日志版本号：每次写日志 +1。
    /// 界面据此判断「日志有没有变」，避免每帧都克隆一遍日志（之前的内存/CPU 浪费点）。
    pub log_rev: u64,
    /// 启动时检测到的管理员权限状态
    pub elevated: bool,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            target: None,
            frame_on: false,
            audio: AudioService::spawn(),
            menu_injected: None,
            frame_auto_for_topmost: None,
            pending_hotkey_apply: false,
            pending_quit: false,
            log: Vec::new(),
            log_rev: 0,
            // 启动时自己检查一次是否有管理员权限
            elevated: crate::win::privilege::is_elevated(),
        }
    }

    pub fn log(&mut self, msg: impl Into<String>) {
        let line = format!("[{}] {}", timestamp(), msg.into());
        log::info!("{line}");
        self.log.push(line);
        self.log_rev = self.log_rev.wrapping_add(1);
        if self.log.len() > 300 {
            self.log.drain(..100);
        }
    }

    pub fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.config_path) {
            self.log(format!("配置保存失败: {e:#}"));
        }
    }

    /// 当前是否以管理员身份运行
    pub fn elevated(&self) -> bool {
        self.elevated
    }
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 只做 HH:MM:SS，避免引入 chrono 依赖（按 UTC 计时，够用于日志排序）
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}
