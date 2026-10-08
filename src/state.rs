//! 全局应用状态。所有模块共享一个 `Shared`（Arc<Mutex<AppState>>）。

use crate::config::Config;
use crate::win::audio::{AudioService, AudioSnapshot};
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
    pub audio_snapshot: AudioSnapshot,
    /// 当前已经注入过系统菜单的窗口 hwnd
    pub menu_injected: Option<isize>,
    pub log: Vec<String>,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            target: None,
            frame_on: false,
            audio: AudioService::spawn(),
            audio_snapshot: AudioSnapshot::default(),
            menu_injected: None,
            log: Vec::new(),
        }
    }

    pub fn log(&mut self, msg: impl Into<String>) {
        let line = format!("[{}] {}", timestamp(), msg.into());
        log::info!("{line}");
        self.log.push(line);
        if self.log.len() > 500 {
            self.log.drain(..100);
        }
    }

    pub fn save_config(&mut self) {
        if let Err(e) = self.config.save(&self.config_path) {
            self.log(format!("配置保存失败: {e:#}"));
        }
    }

    /// 目标窗口的进程 id（没有目标时返回 None）
    pub fn target_pid(&self) -> Option<u32> {
        self.target.as_ref().map(|t| t.pid)
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
