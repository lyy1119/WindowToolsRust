//! 中文字体。
//!
//! egui 内置字体（Ubuntu-Light / Hack）**不含 CJK 字形**，所以界面上的中文会渲染成
//! 方框（tofu）。这里从系统字体目录挑一个中文字体挂到 egui 的字体表里作为回退字体，
//! 拉丁字符仍用 egui 自带字体，中文字符回退到系统字体。

use eframe::egui::{Context, FontData, FontDefinitions, FontFamily};
use std::path::PathBuf;
use std::sync::Arc;

/// 候选字体（文件名, ttc 内的 face 索引），按优先级排列
const CANDIDATES: &[(&str, u32)] = &[
    ("msyh.ttc", 0),   // 微软雅黑
    ("msyh.ttf", 0),   // 微软雅黑（旧版单文件）
    ("msyhl.ttc", 0),  // 微软雅黑 Light
    ("Deng.ttf", 0),   // 等线
    ("simhei.ttf", 0), // 黑体
    ("simsun.ttc", 0), // 宋体
    ("msjh.ttc", 0),   // 微软正黑（繁体系统）
    ("mingliu.ttc", 0),
];

fn font_dir() -> PathBuf {
    let windir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    windir.join("Fonts")
}

/// 安装中文字体，返回实际使用的字体文件名。
///
/// `override_file` 非空时优先用它（绝对路径，或者 `%WINDIR%\Fonts` 下的文件名）。
pub fn install(ctx: &Context, override_file: &str) -> Option<String> {
    let dir = font_dir();

    // 候选列表：用户指定的排最前，然后是内置优先级
    let mut candidates: Vec<(PathBuf, u32, String)> = Vec::new();
    let custom = override_file.trim();
    if !custom.is_empty() {
        let raw = PathBuf::from(custom);
        let path = if raw.is_absolute() { raw } else { dir.join(&raw) };
        candidates.push((path, 0, custom.to_string()));
    }
    for (name, index) in CANDIDATES {
        candidates.push((dir.join(name), *index, (*name).to_string()));
    }

    for (path, index, label) in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let mut data = FontData::from_owned(bytes);
        data.index = index;

        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
        // 作为「回退字体」追加到末尾：优先用 egui 自带字体渲染拉丁字符，
        // 遇到中文字形时再回退到系统字体。
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return Some(label);
    }
    None
}
