//! 全局热键（走 global-hotkey）。
//!
//! 拾取窗口需要「把鼠标移到目标窗口上再按一下」，所以必须用全局热键，
//! 而且这样也不会和游戏 / 全屏应用的按键冲突太多。

use anyhow::Result;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
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

pub fn build() -> Result<Hotkeys> {
    let manager = GlobalHotKeyManager::new()?;
    let mods = Some(Modifiers::CONTROL | Modifiers::ALT);

    let pick = HotKey::new(mods, Code::KeyP);
    let frame = HotKey::new(mods, Code::KeyF);
    let topmost = HotKey::new(mods, Code::KeyT);
    let mute = HotKey::new(mods, Code::KeyM);

    let manager = {
        manager.register(pick)?;
        manager.register(frame)?;
        manager.register(topmost)?;
        manager.register(mute)?;
        manager
    };

    Ok(Hotkeys { manager, pick, frame, topmost, mute })
}

impl Hotkeys {
    /// 快捷键说明，用于界面展示
    pub fn describe(&self) -> [(&'static str, String); 4] {
        [
            ("拾取光标下的窗口", self.pick.to_string()),
            ("红框开关", self.frame.to_string()),
            ("置顶开关", self.topmost.to_string()),
            ("静音开关", self.mute.to_string()),
        ]
    }
}
