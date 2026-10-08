//! 全局热键（走 global-hotkey）。
//!
//! 拾取窗口需要「把鼠标移到目标窗口上再按一下」，所以必须用全局热键。
//!
//! 字符串格式由 `global-hotkey` 解析：
//!   * 修饰键：`Ctrl` / `Control`、`Alt`、`Shift`、`Super`（Win 键）
//!   * 主键：字母/数字直接写（`T`、`5`），也接受 `KeyT`、`Digit5` 这种写法
//!   * 顺序必须是「修饰键在前，主键在最后」，例如 `Ctrl+Alt+T`
//!
//! **留空字符串表示不注册这个快捷键**（区别于「解析失败」）。

use crate::config::Config;
use anyhow::{anyhow, Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::GlobalHotKeyManager;

pub struct Hotkeys {
    /// 必须一直持有，drop 掉注册就失效了（只用于 RAII，不直接读取）
    #[allow(dead_code)]
    pub manager: GlobalHotKeyManager,
    pub pick: Option<HotKey>,
    pub frame: Option<HotKey>,
    pub topmost: Option<HotKey>,
    pub mute: Option<HotKey>,
}

/// 解析快捷键字符串。
///
/// * 留空 → `Ok(None)`，表示该功能不绑定快捷键；
/// * 非空但解析不了 → 报错（带上下文中文提示）。
pub fn parse(text: &str, which: &str) -> Result<Option<HotKey>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<HotKey>()
        .map(Some)
        .map_err(|e| anyhow!("「{which}」的快捷键 \"{t}\" 无法解析：{e}（正确格式例如 Ctrl+Alt+T，留空则不注册）"))
}

/// 检查已启用的快捷键有没有互相重复（留空的不参与）
fn check_duplicates(keys: &[(&str, Option<HotKey>)]) -> Result<()> {
    for i in 0..keys.len() {
        let Some(a) = keys[i].1 else { continue };
        for (name_b, b) in keys.iter().skip(i + 1) {
            let Some(b) = b else { continue };
            if a.mods == b.mods && a.key == b.key {
                return Err(anyhow!(
                    "「{}」和「{}」设置了相同的快捷键，请改成不同的",
                    keys[i].0,
                    name_b
                ));
            }
        }
    }
    Ok(())
}

fn parse_all(cfg: &Config) -> Result<[(&'static str, Option<HotKey>); 4]> {
    let keys = [
        ("拾取窗口", parse(&cfg.hotkey_pick, "拾取窗口")?),
        ("红框开关", parse(&cfg.hotkey_frame, "红框开关")?),
        ("置顶开关", parse(&cfg.hotkey_topmost, "置顶开关")?),
        ("静音开关", parse(&cfg.hotkey_mute, "静音开关")?),
    ];
    check_duplicates(&keys)?;
    Ok(keys)
}

fn register(
    manager: &GlobalHotKeyManager,
    name: &str,
    key: Option<HotKey>,
) -> Result<Option<HotKey>> {
    match key {
        None => Ok(None), // 留空 = 不注册
        Some(k) => {
            manager
                .register(k)
                .map_err(|e| anyhow!("注册「{name}」快捷键 {k} 失败（可能已被其它程序占用）：{e}"))?;
            Ok(Some(k))
        }
    }
}

fn register_all(manager: &GlobalHotKeyManager, cfg: &Config) -> Result<[Option<HotKey>; 4]> {
    let keys = parse_all(cfg)?;
    Ok([
        register(manager, keys[0].0, keys[0].1)?,
        register(manager, keys[1].0, keys[1].1)?,
        register(manager, keys[2].0, keys[2].1)?,
        register(manager, keys[3].0, keys[3].1)?,
    ])
}

pub fn build(cfg: &Config) -> Result<Hotkeys> {
    let manager = GlobalHotKeyManager::new().context("创建全局热键管理器失败")?;
    let [pick, frame, topmost, mute] = register_all(&manager, cfg)?;
    Ok(Hotkeys { manager, pick, frame, topmost, mute })
}

/// 界面/日志里展示当前生效的快捷键
pub fn describe(cfg: &Config) -> String {
    let one = |s: &str| {
        if s.trim().is_empty() {
            "（未设置）".to_string()
        } else {
            s.trim().to_string()
        }
    };
    format!(
        "拾取 {} / 红框 {} / 置顶 {} / 静音 {}",
        one(&cfg.hotkey_pick),
        one(&cfg.hotkey_frame),
        one(&cfg.hotkey_topmost),
        one(&cfg.hotkey_mute)
    )
}

impl Hotkeys {
    /// 用新配置重新注册。
    ///
    /// 先注销旧的再注册新的；如果新的注册失败（例如被别的程序占用），
    /// 会尽量把旧的热键恢复回去，避免「改坏了就一个快捷键都没有」。
    pub fn reapply(&mut self, cfg: &Config) -> Result<()> {
        let old = [self.pick, self.frame, self.topmost, self.mute];
        for k in old.iter().flatten() {
            let _ = self.manager.unregister(*k);
        }

        match register_all(&self.manager, cfg) {
            Ok([pick, frame, topmost, mute]) => {
                self.pick = pick;
                self.frame = frame;
                self.topmost = topmost;
                self.mute = mute;
                Ok(())
            }
            Err(e) => {
                // 回滚到旧的热键
                for k in old.iter().flatten() {
                    let _ = self.manager.register(*k);
                }
                Err(e)
            }
        }
    }
}
