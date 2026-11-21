//! 业务动作：把「选中窗口 / 红框 / 置顶 / 分辨率 / 静音」这些操作串起来。
//!
//! 所有入口（GUI 按钮、托盘菜单、全局热键、系统菜单注入项）最终都调用这里，
//! 保证行为一致、日志一致。

use crate::state::{Shared, TargetWindow};
use crate::win::{frame, hwnd, hwnd_i, sysmenu, window};
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

/// 在当前前台窗口和已拾取目标之间选一个作为「这次动作的对象」。
///
/// * `prefer_foreground = true`（快捷键默认走这条）：
///   直接用**当前前台窗口** —— 这就是 WindowTop 的行为，也是「我盯着哪个窗口就操作哪个」
///   最自然的语义。自绘标题栏的程序（Firefox、PotPlayer 皮肤模式）没有系统菜单，
///   只能靠快捷键，所以这条路径必须能用。
/// * 前台窗口是本程序自己的窗口、或不是可操作窗口时，自动退回已拾取的目标。
fn resolve_target(state: &Shared, prefer_foreground: bool) -> Result<(HWND, TargetWindow)> {
    if prefer_foreground {
        if let Some(fg) = window::foreground_window() {
            // is_eligible 会排除本程序自己的窗口（主界面、托盘菜单的宿主窗口）
            if window::is_eligible(fg) {
                let info = sync_target_quiet(state, fg)?;
                return Ok((fg, info));
            }
        }
    }
    require_target(state)
}

/// 把某个窗口静默地同步成「当前目标」。
/// 已经是目标时直接返回缓存，不做任何系统调用。
fn sync_target_quiet(state: &Shared, h: HWND) -> Result<TargetWindow> {
    {
        let st = state.lock();
        if let Some(t) = &st.target {
            if t.hwnd == hwnd_i(h) && window::is_window(h) {
                return Ok(t.clone());
            }
        }
    }
    set_target(state, h, "快捷键", false)
}

/// 选中一个窗口作为目标。
///
/// `want_frame` 决定是否顺手标记红框。**只有「用户主动拾取/选择」时才该为 true**，
/// 从系统菜单触发的命令不应该顺手画红框（比如点「静音」不该冒出红框）。
pub fn set_target(
    state: &Shared,
    h: HWND,
    source: &str,
    want_frame: bool,
) -> Result<TargetWindow> {
    if !window::is_window(h) {
        bail!("无效的窗口句柄");
    }
    let info = window::describe(h);
    if info.title.is_empty() {
        bail!("该窗口没有标题，无法作为目标");
    }

    let (inject, presets, rgb, thickness) = {
        let st = state.lock();
        (
            st.config.inject_system_menu,
            st.config.presets.clone(),
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
        // 注意：换目标**不会**清空 frame_auto_for_topmost，
        // 因为置顶可以同时作用于多个窗口，它们的自动红框记录必须各自保留。
        st.log(format!("[{source}] 已选中窗口: {}", info.short_label()));
    }

    if inject {
        if !sysmenu::has_system_menu(h) {
            // Firefox、PotPlayer 皮肤模式这类自绘标题栏的程序根本没有系统菜单，
            // 注入是不可能成功的 —— 这不是错误，跳过即可，用快捷键操作。
            state.lock().log(
                "该窗口没有系统菜单（自绘标题栏的程序，如 Firefox / PotPlayer 皮肤模式），\
                 无法注入右键菜单条目；请直接用快捷键或界面按钮操作",
            );
        } else {
            match sysmenu::inject(h, &presets) {
                Ok(()) => {
                    let mut st = state.lock();
                    st.menu_injected = Some(info.hwnd);
                    st.log("已往该窗口的标题栏右键菜单注入条目");
                }
                Err(e) => state.lock().log(format!("注入系统菜单失败: {e:#}")),
            }
        }
    }

    if want_frame {
        if let Err(e) = frame::show(h, rgb, thickness) {
            state.lock().log(format!("显示红框失败: {e:#}"));
        }
    }

    state.lock().audio.focus(info.pid, info.exe.clone());
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
    let frame = state.lock().config.auto_frame_on_pick;
    set_target(state, h, "拾取", frame)
}

pub fn pick_foreground(state: &Shared) -> Result<TargetWindow> {
    let h = match window::foreground_window() {
        Some(h) => h,
        None => bail!("没有前台窗口"),
    };
    let frame = state.lock().config.auto_frame_on_pick;
    set_target(state, h, "前台窗口", frame)
}

/// 切换**指定窗口**的红框（系统菜单「标记红框」用）。
///
/// 只影响这一个窗口，其它窗口的红框保持原样。
/// 手动切换会把它从「置顶自动加的红框」记录里移除 —— 从此它算用户自己标的。
pub fn toggle_frame_for(state: &Shared, h: HWND) -> Result<String> {
    let (rgb, th) = {
        let st = state.lock();
        (st.config.frame_color, st.config.frame_thickness)
    };
    let on = frame::toggle(h, rgb, th)?;
    {
        let mut st = state.lock();
        st.frame_auto_for_topmost.remove(&hwnd_i(h));
    }
    let title = window::title(h);
    let msg = if on {
        format!("已标记 {title} 的红框")
    } else {
        format!("已关闭 {title} 的红框")
    };
    state.lock().log(msg.clone());
    Ok(msg)
}

/// 切换红框（GUI / 托盘 / 热键用）。
///
/// 作用于 `resolve_target` 解析出的那个窗口，**只切换这一个**；
/// 别的窗口上已有的红框不会被收掉。
pub fn toggle_frame(state: &Shared, prefer_foreground: bool) -> Result<String> {
    let (h, _) = resolve_target(state, prefer_foreground)?;
    toggle_frame_for(state, h)
}

/// 清除所有窗口的红框。
pub fn clear_all_frames(state: &Shared) -> Result<String> {
    let n = frame::count();
    frame::remove_all();
    state.lock().frame_auto_for_topmost.clear();
    let msg = if n == 0 {
        "当前没有红框".to_string()
    } else {
        format!("已清除 {n} 个窗口的红框")
    };
    state.lock().log(msg.clone());
    Ok(msg)
}

pub fn toggle_topmost(state: &Shared, prefer_foreground: bool) -> Result<String> {
    let (h, info) = resolve_target(state, prefer_foreground)?;
    let on = window::toggle_topmost(h)?;

    // 「置顶时自动标记红框」：让人一眼看出这个窗口被钉在最上面了。
    // 每个窗口**各自**记录「我这个红框是置顶时自动加的」，
    // 取消置顶时只移除自己那个，不会动其它窗口的红框，也不碰用户手动标的红框。
    let key = hwnd_i(h);
    // 清掉已经不再对应任何红框的旧记录（窗口销毁 / 句柄被复用的极端情况）
    prune_auto_records(state);
    let mut suffix = String::new();
    if on {
        let (auto, rgb, th) = {
            let st = state.lock();
            (
                st.config.auto_frame_on_topmost,
                st.config.frame_color,
                st.config.frame_thickness,
            )
        };
        if auto && !frame::is_framed(h) {
            match frame::show(h, rgb, th) {
                Ok(()) => {
                    state.lock().frame_auto_for_topmost.insert(key);
                    suffix = "，并已自动标记红框".into();
                }
                Err(e) => state.lock().log(format!("自动标记红框失败: {e:#}")),
            }
        }
    } else {
        let was_auto = {
            let mut st = state.lock();
            st.frame_auto_for_topmost.remove(&key)
        };
        if was_auto {
            frame::remove(h);
            suffix = "，并已移除自动标记的红框".into();
        }
    }

    let msg = format!(
        "{}{}",
        format_args!(
            "{} 已{}",
            info.short_label(),
            if on { "置顶" } else { "取消置顶" }
        ),
        suffix
    );
    state.lock().log(msg.clone());
    Ok(msg)
}

pub fn toggle_mute(state: &Shared, prefer_foreground: bool) -> Result<String> {
    let (_h, info) = resolve_target(state, prefer_foreground)?;
    let current = state.lock().audio.is_muted().unwrap_or(false);
    let next = !current;
    state.lock().audio.set_mute(next);
    let msg = format!(
        "已请求{} {} 的声音（PID {}）",
        if next { "静音" } else { "取消静音" },
        if info.exe.is_empty() {
            info.short_label()
        } else {
            info.exe.clone()
        },
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

/// 按预设下标调整分辨率
pub fn resize_to_preset(state: &Shared, index: usize) -> Result<String> {
    let preset = {
        let st = state.lock();
        st.config.presets.get(index).cloned()
    };
    let Some(p) = preset else {
        bail!("预设 #{index} 不存在");
    };
    resize_to(state, p.width, p.height)
}

/// 以管理员身份重启自己。
///
/// ⚠️ 顺序很重要：**必须先放掉单实例互斥体再 ShellExecuteW("runas")**。
/// `ShellExecuteW` 是「新进程已经创建好了才返回」的，如果那时我们还占着互斥体，
/// 新起来的提权进程会把自己当成第二个实例而立刻退出 —— 表现就是
/// 「点了以管理员身份重启，结果程序反而彻底没了」。
pub fn restart_elevated(state: &Shared) -> Result<()> {
    let single_instance_on = state.lock().config.single_instance;
    if single_instance_on {
        crate::win::single_instance::release();
    }
    match crate::win::privilege::restart_as_admin() {
        Ok(()) => Ok(()),
        Err(e) => {
            // 提权没成功（例如用户在 UAC 弹窗上点了「否」），把互斥体拿回来继续跑
            if single_instance_on {
                let _ = crate::win::single_instance::acquire();
            }
            Err(e)
        }
    }
}

/// 清理 `frame_auto_for_topmost` 里「记录的窗口已经没有红框」的过期条目。
///
/// 锁顺序：先 `state` 再 `frame`。frame 模块内部从不反向索取 `state`，因此不会死锁。
fn prune_auto_records(state: &Shared) {
    let mut st = state.lock();
    let stale: Vec<isize> = st
        .frame_auto_for_topmost
        .iter()
        .copied()
        .filter(|k| !frame::is_framed(hwnd(*k)))
        .collect();
    for k in stale {
        st.frame_auto_for_topmost.remove(&k);
    }
}

/// 系统菜单注入项被点击时的入口。`h` 是被点击的那个窗口（未必是当前 target）。
///
/// 注意这里 **不会** 顺手标记红框 —— 点「静音」不应该冒出红框来。
pub fn dispatch_menu_command(cmd: u32, h: HWND, state: &Shared) {
    let label = sysmenu::command_label(cmd);
    if let Err(e) = set_target(state, h, &format!("系统菜单: {label}"), false) {
        state.lock().log(format!("系统菜单命令失败: {e:#}"));
        return;
    }

    let result: Result<String> = match cmd {
        sysmenu::CMD_FRAME => toggle_frame_for(state, h),
        sysmenu::CMD_TOPMOST => toggle_topmost(state, false),
        sysmenu::CMD_MUTE => toggle_mute(state, false),
        sysmenu::CMD_RESIZE_EMPTY => Err(anyhow::anyhow!("还没有配置分辨率预设，请先在界面里添加")),
        other => match sysmenu::preset_index(other) {
            Some(i) => resize_to_preset(state, i),
            None => Err(anyhow::anyhow!("未知的菜单命令 0x{other:X}")),
        },
    };

    match result {
        Ok(msg) => state.lock().log(format!("[系统菜单] {msg}")),
        Err(e) => state.lock().log(format!("[系统菜单] 执行失败: {e:#}")),
    }
}
