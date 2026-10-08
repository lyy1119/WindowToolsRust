//! 按进程静音：走 Windows Core Audio (WASAPI) 的音频会话接口。
//!
//! 为什么用会话而不是直接改窗口：声音是**进程级**的，一个进程的所有窗口共享同一条
//! 音频会话，所以“把某个窗口静音”在系统层面实际是“把该窗口所属进程静音”。
//!
//! 已知限制（框架阶段先记录，后续可按需增强）：
//!   * 只有当该进程**正在发声**时才会存在音频会话；没在播放时枚举不到会话，
//!     此时无法设置静音（界面会显示“无音频会话”）。
//!   * UWP / 商店应用的部分会话可能归属系统进程，需要额外处理。
//!   * 同一进程的多个窗口会一起被静音（详见 README 的“已知限制”）。
//!
//! COM 相关调用全部放在独立的工作线程里（MTA），避免和 UI 线程的 COM 状态互相干扰。

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};

/// 某个进程的音频状态快照，供 UI 只读展示
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioSnapshot {
    pub pid: u32,
    /// 命中的音频会话数量
    pub session_count: usize,
    /// 全部会话是否都处于静音（None = 未知 / 未采集）
    pub muted: Option<bool>,
    pub error: Option<String>,
}

enum Cmd {
    Refresh(u32),
    SetMute(u32, bool),
}

/// 音频服务句柄：命令是异步投递给工作线程的，UI 只读快照。
#[derive(Clone)]
pub struct AudioService {
    tx: Sender<Cmd>,
    snap: Arc<Mutex<AudioSnapshot>>,
}

impl AudioService {
    pub fn spawn() -> Self {
        let (tx, rx) = channel::<Cmd>();
        let snap = Arc::new(Mutex::new(AudioSnapshot::default()));
        let worker_snap = snap.clone();
        std::thread::Builder::new()
            .name("wt-audio".into())
            .spawn(move || worker(rx, worker_snap))
            .expect("无法启动音频工作线程");
        Self { tx, snap }
    }

    /// 请求刷新指定进程的静音状态
    pub fn refresh(&self, pid: u32) {
        let _ = self.tx.send(Cmd::Refresh(pid));
    }

    /// 设置静音（异步）
    pub fn set_mute(&self, pid: u32, mute: bool) {
        let _ = self.tx.send(Cmd::SetMute(pid, mute));
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.snap.lock().clone()
    }
}

fn worker(rx: Receiver<Cmd>, snap: Arc<Mutex<AudioSnapshot>>) {
    unsafe {
        // MTA：与 UI 线程（可能是 STA）互不干扰
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut current_pid = 0u32;
    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(Cmd::Refresh(pid)) => {
                current_pid = pid;
                refresh(&snap, pid);
            }
            Ok(Cmd::SetMute(pid, mute)) => {
                current_pid = pid;
                match apply_mute(pid, mute) {
                    Ok(n) => {
                        refresh(&snap, pid);
                        let mut s = snap.lock();
                        if n == 0 {
                            s.error = Some("未找到该进程的音频会话（可能当前没有在发声）".into());
                        }
                    }
                    Err(e) => {
                        snap.lock().error = Some(format!("{e:#}"));
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if current_pid != 0 {
                    refresh(&snap, current_pid);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    unsafe { CoUninitialize() };
}

fn refresh(snap: &Arc<Mutex<AudioSnapshot>>, pid: u32) {
    let result = collect_sessions(pid).and_then(|sessions| {
        let mut muted = None;
        for s in &sessions {
            let vol: ISimpleAudioVolume = s.cast()?;
            let m = unsafe { vol.GetMute() }?.as_bool();
            muted = Some(muted.unwrap_or(true) && m);
        }
        Ok((sessions.len(), muted))
    });
    let mut s = snap.lock();
    s.pid = pid;
    match result {
        Ok((count, muted)) => {
            s.session_count = count;
            s.muted = muted;
            s.error = None;
        }
        Err(e) => {
            s.session_count = 0;
            s.muted = None;
            s.error = Some(format!("{e:#}"));
        }
    }
}

fn apply_mute(pid: u32, mute: bool) -> Result<usize> {
    let sessions = collect_sessions(pid)?;
    for s in &sessions {
        let vol: ISimpleAudioVolume = s.cast()?;
        unsafe { vol.SetMute(mute, std::ptr::null())? };
    }
    Ok(sessions.len())
}

/// 枚举默认播放设备上属于 `pid` 的所有音频会话
fn collect_sessions(pid: u32) -> Result<Vec<IAudioSessionControl2>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("创建音频设备枚举器失败")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .context("获取默认播放设备失败")?;
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .context("激活音频会话管理器失败")?;
        let list = manager.GetSessionEnumerator().context("枚举音频会话失败")?;
        let count = list.GetCount().unwrap_or(0);

        let mut out = Vec::new();
        for i in 0..count {
            // 单个会话可能已过期，失败就跳过
            let Ok(ctl) = list.GetSession(i) else { continue };
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
            match ctl2.GetProcessId() {
                Ok(p) if p == pid => out.push(ctl2),
                _ => {}
            }
        }
        Ok(out)
    }
}
