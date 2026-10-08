//! 全局热键（走 global-hotkey）。
//!
//! 拾取窗口需要「把鼠标移到目标窗口上再按一下」，所以必须用全局热键。
//! 快捷键现在可以在界面里改，字符串格式由 `global-hotkey` 解析：
//!   * 修饰键：`Ctrl` / `Control`、`Alt`、`Shift`、`Super`（Win 键）
//!   * 主键：字母/数字直接写（`T`、`5`），也接受 `KeyT`、`Digit5` 这种写法
//!   * 顺序必须是「修饰键在前，主键在最后」，例如 `Ctrl+Alt+T`

use crate::config::Config;
use anyhow::{anyhow, Context, Result};
use global_hotkey::hotkey::HotKey;
use global_hotkey::GlobalHotKeyManager;

pub struct Hotkeys {
    /// 必须一直持有，drop 掉注册就失效了（只用于 RAII，不直接读取）
    #[allow(dead_code)]
    pub manager: GlobalHotKeyManager,
    pub pick: HotKey,
    pub frame: HotKey,
    pub topmost: HotKey,
    pub mute: HotKey,
}

/// 解析快捷键字符串，出错时给出带上下文的中文提示
pub fn parse(text: &str, which: &str) -> Result<HotKey> {
    let t = text.trim();
    if t.is_empty() {
        return Err(anyhow!("「{which}」的快捷键不能为空"));
    }
    t.parse::<HotKey>()
        .map_err(|e| anyhow!("「{which}」的快捷键 \"{t}\" 无法解析：{e}（正确格式例如 Ctrl+Alt+T）"))
}

/// 检查四个快捷键是否两两重复
fn check_duplicates(keys: &[(&str, HotKey)]) -> Result<()> {
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            if keys[i].1.mods == keys[j].1.mods && keys[i].1.key == keys[j].1.key {
                return Err(anyhow!(
                    "「{}」和「{}」设置了相同的快捷键，请改成不同的",
                    keys[i].0,
                    keys[j].0
                ));
            }
        }
    }
    Ok(())
}

fn parse_all(cfg: &Config) -> Result<[(&'static str, HotKey); 4]> {
    let keys = [
        ("拾取窗口", parse(&cfg.hotkey_pick, "拾取窗口")?),
        ("红框开关", parse(&cfg.hotkey_frame, "红框开关")?),
        ("置顶开关", parse(&cfg.hotkey_topmost, "置顶开关")?),
        ("静音开关", parse(&cfg.hotkey_mute, "静音开关")?),
    ];
    check_duplicates(&keys)?;
    Ok(keys)
}

fn register_all(manager: &GlobalHotKeyManager, cfg: &Config) -> Result<[HotKey; 4]> {
    let keys = parse_all(cfg)?;
    let mut out = [keys[0].1, keys[1].1, keys[2].1, keys[3].1];
    for (i, (name, key)) in keys.iter().enumerate() {
        manager
            .register(*key)
            .map_err(|e| anyhow!("注册「{name}」快捷键 {key} 失败（可能已被其它程序占用）：{e}"))?;
        out[i] = *key;
    }
    Ok(out)
}

pub fn build(cfg: &Config) -> Result<Hotkeys> {
    let manager = GlobalHotKeyManager::new().context("创建全局热键管理器失败")?;
    let [pick, frame, topmost, mute] = register_all(&manager, cfg)?;
    Ok(Hotkeys { manager, pick, frame, topmost, mute })
}

impl Hotkeys {
    /// 用新配置重新注册。
    ///
    /// 先注销旧的再注册新的；如果新的注册失败（例如被别的程序占用），
    /// 会尽量把旧的热键恢复回去，避免「改坏了就一个快捷键都没有」。
    pub fn reapply(&mut self, cfg: &Config) -> Result<()> {
        let old = [self.pick, self.frame, self.topmost, self.mute];
        let _ = self.manager.unregister_all(&old);

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
                for k in old {
                    let _ = self.manager.register(k);
                }
                Err(e)
            }
        }
    }
}
