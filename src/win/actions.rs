//! 业务动作：把「选中窗口 / 红框 / 置顶 / 分辨率 / 静音」这些操作串起来。
//!
//! 所有入口（GUI 按钮、托盘菜单、全局热键、系统菜单注入项）最终都调用这里，
//! 保证行为一致、日志一致。

use crate::state::{Shared, TargetWindow};
use crate::win::{frame, hwnd, sysmenu, window};
use anyhow::{bail, Result};
use windows::Win32::Foundation::HWND;

/// 当前目标窗口的句柄；没有选中或已失效则返回 Err
fn require_target(state: &Shared) -> Result<(HWND, TargetWindow)> {
    let st = state.lock();
    let t = match &st.target {
        Some(t) => t.clone(),
        None => bail!("还没有选中目标窗口（用 Ctrl+Alt+P 或托盘菜单拾取）"),
    };
    drop(st);
    let h = hwnd(t.hwnd);
    if !window::is_window(h) {
        state.lock().target = None;
        bail!("目标窗口已关闭，请重新拾取");
    }
    Ok((h, t))
}

/// 选中一个窗口作为目标：记录信息、按需注入系统菜单、按需显示红框
pub fn set_target(state: &Shared, h: HWND, source: &str) -> Result<TargetWindow> {
    if !window::is_window(h) {
        bail!("无效的窗口句柄");
    }
    let info = window::describe(h);
    if info.title.is_empty() {
        bail!("该窗口没有标题，无法作为目标");
    }

    let (inject, auto_frame, rgb, thickness) = {
        let st = state.lock();
        (
            st.config.inject_system_menu,
            st.config.auto_frame_on_pick,
            st.config.frame_color,
            st.config.frame_thickness,
        )
    };

    // 目标换了：把旧窗口的系统菜单清理干净
    let old = state.lock().menu_injected;
    if let Some(old_hwnd) = old {
        if old_hwnd != info.hwnd {
            let _ = sysmenu::remove(hwnd(old_hwnd));
        }
    }

    {
        let mut st = state.lock();
        st.target = Some(info.clone());
        st.menu_injected = None;
        st.log(format!("[{source}] 已选中窗口: {}", info.short_label()));
    }

    if inject {
        match sysmenu::inject(h) {
            Ok(()) => {
                state.lock().menu_injected = Some(info.hwnd);
                state.lock().log("已往该窗口的标题栏右键菜单注入条目");
            }
            Err(e) => state.lock().log(format!("注入系统菜单失败: {e:#}")),
        }
    }

    if auto_frame {
        if let Err(e) = frame::show(h, rgb, thickness) {
            state.lock().log(format!("显示红框失败: {e:#}"));
        } else {
            state.lock().frame_on = true;
        }
    }

    state.lock().audio.refresh(info.pid);
    Ok(info)
}

/// 拾取光标下方的窗口
pub fn pick_under_cursor(state: &Shared) -> Result<TargetWindow> {
    let h = match window::root_window_at_cursor() {
        Some(h) => h,
        None => bail!("光标下方没有可用窗口"),
    };
    if !window::is_eligible(h) {
        bail!("光标下方的窗口不支持操作（无标题栏或为工具窗口）");
    }
    set_target(state, h, "拾取")
}

pub fn pick_foreground(state: &Shared) -> Result<TargetWindow> {
    let h = match window::foreground_window() {
        Some(h) => h,
        None => bail!("没有前台窗口"),
    };
    set_target(state, h, "前台窗口")
}

/// 红框开 / 关
pub fn toggle_frame(state: &Shared) -> Result<String> {
    if frame::is_active() {
        frame::hide();
        state.lock().frame_on = false;
        return Ok("已关闭红框".into());
    }
    let (h, _) = require_target(state)?;
    let (rgb, th) = {
        let st = state.lock();
        (st.config.frame_color, st.config.frame_thickness)
    };
    frame::show(h, rgb, th)?;
    state.lock().frame_on = true;
    Ok("已显示红框".into())
}

pub fn toggle_topmost(state: &Shared) -> Result<String> {
    let (h, info) = require_target(state)?;
    let on = window::toggle_topmost(h)?;
    let msg = format!(
        "{} 已{}",
        info.short_label(),
        if on { "置顶" } else { "取消置顶" }
    );
    state.lock().log(msg.clone());
    Ok(msg)
}

pub fn toggle_mute(state: &Shared) -> Result<String> {
    let (_h, info) = require_target(state)?;
    let snap = state.lock().audio.snapshot();
    let current = snap.muted.unwrap_or(false);
    let next = !current;
    state.lock().audio.set_mute(info.pid, next);
    let msg = format!(
        "已请求{}进程 {} (PID {})",
        if next { "静音" } else { "取消静音" },
        info.short_label(),
        info.pid
    );
    state.lock().log(msg.clone());
    Ok(msg)
}

/// 把目标窗口调整到指定分辨率
pub fn resize_to(state: &Shared, w: u32, h: u32) -> Result<String> {
    let (hw, info) = require_target(state)?;
    let (client_area, restore) = {
        let st = state.lock();
        (st.config.resize_client_area, st.config.restore_before_resize)
    };
    if !window::is_resizable(hw) {
        state.lock().log("该窗口不可调整大小（无 WS_THICKFRAME）");
    }
    let (tw, th) = window::resize(hw, w, h, client_area, restore)?;
    let msg = format!(
        "{} 已调整为 {}x{}（{}，实际外框 {}x{}）",
        info.short_label(),
        w,
        h,
        if client_area { "客户区" } else { "外框" },
        tw,
        th
    );
    state.lock().log(msg.clone());
    Ok(msg)
}

/// 系统菜单注入项被点击时的入口。`h` 是被点击的那个窗口（未必是当前 target）。
pub fn dispatch_menu_command(cmd: u32, h: HWND, state: &Shared) {
    // 点击哪个窗口就作用于哪个窗口
    if let Err(e) = set_target(state, h, &format!("系统菜单: {}", sysmenu::command_label(cmd))) {
        state.lock().log(format!("系统菜单命令失败: {e:#}"));
        return;
    }
    let result: Result<String> = match cmd {
        sysmenu::CMD_FRAME => toggle_frame(state),
        sysmenu::CMD_TOPMOST => toggle_topmost(state),
        sysmenu::CMD_MUTE => toggle_mute(state),
        sysmenu::CMD_RESIZE => {
            // 系统菜单里没有输入框，用配置里第一个预设
            let preset = state.lock().config.presets.first().cloned();
            match preset {
                Some(p) => resize_to(state, p.width, p.height),
                None => Err(anyhow::anyhow!("没有配置分辨率预设")),
            }
        }
        _ => return,
    };
    match result {
        Ok(msg) => state.lock().log(format!("[系统菜单] {msg}")),
        Err(e) => state.lock().log(format!("[系统菜单] 执行失败: {e:#}")),
    }
}
