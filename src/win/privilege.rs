//! 管理员权限（UAC 提权）检测与重启提权。
//!
//! 为什么需要：如果要操作的**目标窗口属于以管理员身份运行的进程**（任务管理器、
//! 各种带 UAC 提权的工具），而本程序只是普通权限，那么 `SetWindowPos`、
//! `GetSystemMenu`、`OpenProcess` 等调用都会因为「完整性级别不够」而失败，
//! 表现就是「某些窗口怎么点都没反应」。

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 当前进程是否已经以管理员身份运行
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

pub fn current_exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// 以管理员身份重新启动自己（会弹 UAC）。成功后调用方应当退出当前进程。
pub fn restart_as_admin() -> Result<()> {
    if is_elevated() {
        bail!("当前已经是管理员权限，无需重启");
    }
    let exe = current_exe_path().context("无法获取自身可执行文件路径")?;
    let mut wide: Vec<u16> = exe.as_os_str().to_string_lossy().encode_utf16().collect();
    wide.push(0);

    unsafe {
        let result = ShellExecuteW(
            None,
            windows::core::w!("runas"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // ShellExecuteW 的返回值大于 32 才算成功
        if result.0 as isize <= 32 {
            bail!(
                "请求提权失败（ShellExecuteW 返回 {}），可能是你在 UAC 弹窗里点了「否」",
                result.0 as isize
            );
        }
    }
    Ok(())
}
