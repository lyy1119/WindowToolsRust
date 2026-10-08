//! 配置持久化。存到 %APPDATA%\WindowToolsRust\config.json。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolutionPreset {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

impl ResolutionPreset {
    pub fn new(name: &str, width: u32, height: u32) -> Self {
        Self { name: name.to_string(), width, height }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 红框颜色 RGB
    pub frame_color: [u8; 3],
    /// 红框粗细（像素，逻辑像素）
    pub frame_thickness: i32,
    /// 调整分辨率时，目标尺寸按“客户区”还是“整个窗口（含边框标题栏）”计算
    pub resize_client_area: bool,
    /// 分辨率调整前，若窗口处于最大化则先还原
    pub restore_before_resize: bool,
    /// 是否往目标窗口的标题栏右键系统菜单里注入条目
    pub inject_system_menu: bool,
    /// 是否在选中目标窗口时自动显示红框
    pub auto_frame_on_pick: bool,
    /// 常用分辨率预设
    pub presets: Vec<ResolutionPreset>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            frame_color: [229, 40, 40],
            frame_thickness: 3,
            resize_client_area: true,
            restore_before_resize: true,
            inject_system_menu: true,
            auto_frame_on_pick: true,
            presets: vec![
                ResolutionPreset::new("1920 x 1080", 1920, 1080),
                ResolutionPreset::new("1600 x 900", 1600, 900),
                ResolutionPreset::new("1280 x 720", 1280, 720),
                ResolutionPreset::new("1024 x 768", 1024, 768),
                ResolutionPreset::new("800 x 600", 800, 600),
            ],
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("配置文件解析失败（{e}），使用默认配置");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("WindowToolsRust").join("config.json")
}
