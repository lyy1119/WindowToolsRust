//! 按进程静音：走 Windows Core Audio (WASAPI) 的音频会话接口。
//!
//! ## 为什么是「按进程」
//! Windows 没有「按窗口」的静音接口，声音是**进程级**音频会话，一个进程的所有窗口
//! 共享同一条会话。所以「把某个窗口静音」在系统层面就是「把它所属进程静音」。
//!
//! ## 匹配策略（这是之前「静音无效」最可能的原因）
//! 1. 先按 **PID 精确匹配**；
//! 2. 如果一个都没匹配到，再退回 **按可执行文件名匹配**。
//!
//! 第 2 条很关键：浏览器（Chrome/Edge）、多进程应用里，**窗口所属进程**和
//! **真正持有音频会话的进程**经常不是同一个 PID，但它们的 exe 名字相同。
//! 只按 PID 匹配时这种情况会「找不到会话」。
//!
//! ## 其它已处理的坑
//! * **遍历所有活动的播放设备**，而不只是默认设备 —— 应用可能输出到别的声卡/耳机。
//! * 跳过已过期的会话（`AudioSessionStateExpired`）。
//! * COM 全部放在独立工作线程（MTA），并使用「扫描结果缓存 + 定时刷新」，
//!   避免每次读状态都做一次全量设备/会话枚举。
//!
//! ## 已知限制
//! * 只有当该进程**正在发声**时才会存在音频会话；没在播放时枚举不到，
//!   界面会显示「未检测到会话」。
//! * UWP / 商店应用的会话可能归属系统进程，暂不支持。

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// 扫描结果多久算过期
const CACHE_TTL: Duration = Duration::from_millis(1500);
/// 没有界面交互时的后台自动刷新间隔
const IDLE_RESCAN: Duration = Duration::from_millis(2500);

/// 一条音频会话
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSessionInfo {
    pub pid: u32,
    pub exe: String,
    pub muted: bool,
    /// 所属播放设备的序号（用于区分多声卡）
    pub device_index: u32,
    /// AudioSessionState 原始值：0=Inactive, 1=Active, 2=Expired
    pub state: i32,
}

/// 匹配到目标的方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchKind {
    #[default]
    None,
    /// 按进程 id 精确匹配
    Pid,
    /// 按可执行文件名匹配（浏览器这类多进程应用）
    Exe,
}

impl MatchKind {
    pub fn label(self) -> &'static str {
        match self {
            MatchKind::None => "未匹配",
            MatchKind::Pid => "按 PID 匹配",
            MatchKind::Exe => "按进程名匹配",
        }
    }
}

/// 供界面只读展示的音频状态快照
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioSnapshot {
    pub pid: u32,
    pub exe: String,
    /// 匹配到的会话数量
    pub matched: usize,
    /// 匹配到的会话是否全部静音（None = 没有匹配到任何会话）
    pub muted: Option<bool>,
    pub matched_by: MatchKind,
    /// 系统上所有可见的音频会话（调试用，界面上可以展开看）
    pub all: Vec<AudioSessionInfo>,
    pub error: Option<String>,
    /// 内容版本号：只有内容真的变了才 +1。
    /// 界面是每帧读这个快照的，靠它避免无谓的整表克隆。
    pub rev: u64,
}

impl AudioSnapshot {
    /// 给界面用的一句话状态
    pub fn summary(&self) -> String {
        if let Some(e) = &self.error {
            return format!("音频: {e}");
        }
        if self.pid == 0 {
            return "音频: 未选中窗口".into();
        }
        match self.matched {
            0 => format!(
                "音频: 未找到属于 {} 的音频会话（该进程当前没有在发声？系统共 {} 个会话）",
                if self.exe.is_empty() {
                    format!("PID {}", self.pid)
                } else {
                    self.exe.clone()
                },
                self.all.len()
            ),
            n => format!(
                "音频: {} 个会话（{}），当前{}",
                n,
                self.matched_by.label(),
                if self.muted.unwrap_or(false) {
                    "已静音"
                } else {
                    "未静音"
                }
            ),
        }
    }
}

enum Cmd {
    /// 换目标窗口
    Focus { pid: u32, exe: String },
    /// 设置静音（作用于当前目标）
    SetMute(bool),
    /// 强制重新扫描
    Rescan,
}

/// 音频服务句柄：命令异步投递给工作线程，界面只读快照。
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

    /// 目标窗口变了
    pub fn focus(&self, pid: u32, exe: String) {
        let _ = self.tx.send(Cmd::Focus { pid, exe });
    }

    /// 设置静音（异步）
    pub fn set_mute(&self, mute: bool) {
        let _ = self.tx.send(Cmd::SetMute(mute));
    }

    pub fn rescan(&self) {
        let _ = self.tx.send(Cmd::Rescan);
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.snap.lock().clone()
    }

    /// 只取轻量字段，避免每帧克隆整个会话列表
    pub fn is_muted(&self) -> Option<bool> {
        self.snap.lock().muted
    }
}

struct Worker {
    snap: Arc<Mutex<AudioSnapshot>>,
    pid: u32,
    exe: String,
    cache: Vec<AudioSessionInfo>,
    cached_at: Option<Instant>,
    /// pid -> exe 名字缓存，避免反复 OpenProcess
    name_cache: HashMap<u32, String>,
}

impl Worker {
    /// 确保每个会话都带上了 exe 名字
    fn fill_names(&mut self) {
        for i in 0..self.cache.len() {
            let pid = self.cache[i].pid;
            if self.cache[i].exe.is_empty() {
                if let Some(name) = self.name_cache.get(&pid) {
                    self.cache[i].exe = name.clone();
                    continue;
                }
                if let Some(name) = crate::win::window::process_exe_name(pid) {
                    self.name_cache.insert(pid, name.clone());
                    self.cache[i].exe = name;
                }
            }
        }
    }

    fn scan(&mut self) {
        match collect_all() {
            Ok(mut list) => {
                std::mem::swap(&mut self.cache, &mut list);
                self.fill_names();
                self.cached_at = Some(Instant::now());
                self.set_error(None);
            }
            Err(e) => {
                self.cache.clear();
                self.cached_at = Some(Instant::now());
                self.set_error(Some(format!("{e:#}")));
            }
        }
    }

    /// 只在错误信息变化时更新快照（并让版本号 +1）
    fn set_error(&self, msg: Option<String>) {
        let mut snap = self.snap.lock();
        if snap.error != msg {
            snap.error = msg;
            snap.rev = snap.rev.wrapping_add(1);
        }
    }

    fn ensure_fresh(&mut self, force: bool) {
        let stale = match self.cached_at {
            None => true,
            Some(t) => t.elapsed() >= CACHE_TTL,
        };
        if force || stale {
            self.scan();
        }
    }

    /// 按当前 pid/exe 选出目标的会话下标
    fn matched_indices(&self) -> (Vec<usize>, MatchKind) {
        if self.pid == 0 && self.exe.is_empty() {
            return (Vec::new(), MatchKind::None);
        }
        let by_pid: Vec<usize> = self
            .cache
            .iter()
            .enumerate()
            .filter(|(_, s)| self.pid != 0 && s.pid == self.pid)
            .map(|(i, _)| i)
            .collect();
        if !by_pid.is_empty() {
            return (by_pid, MatchKind::Pid);
        }
        if !self.exe.is_empty() {
            let by_exe: Vec<usize> = self
                .cache
                .iter()
                .enumerate()
                .filter(|(_, s)| s.exe.eq_ignore_ascii_case(&self.exe))
                .map(|(i, _)| i)
                .collect();
            if !by_exe.is_empty() {
                return (by_exe, MatchKind::Exe);
            }
        }
        (Vec::new(), MatchKind::None)
    }

    /// 重新计算快照里跟「当前目标」相关的部分
    fn publish(&self) {
        let (idx, kind) = self.matched_indices();
        let mut muted: Option<bool> = None;
        let mut matched = 0usize;
        for i in idx {
            matched += 1;
            let m = self.cache[i].muted;
            muted = Some(muted.unwrap_or(true) && m);
        }
        let mut snap = self.snap.lock();
        // 内容没变就什么都不做 —— 连克隆都省掉。
        // 界面每帧都会读这个快照，这里是之前主要的无谓分配点。
        if snap.pid == self.pid
            && snap.exe == self.exe
            && snap.matched == matched
            && snap.muted == muted
            && snap.matched_by == kind
            && snap.all == self.cache
        {
            return;
        }
        snap.pid = self.pid;
        snap.exe = self.exe.clone();
        snap.matched = matched;
        snap.muted = muted;
        snap.matched_by = kind;
        snap.all = self.cache.clone();
        snap.rev = snap.rev.wrapping_add(1);
    }

    fn apply_mute(&mut self, mute: bool) -> Result<usize> {
        self.ensure_fresh(true);
        let (idx, kind) = self.matched_indices();
        if idx.is_empty() {
            return Ok(0);
        }
        // 重新枚举一次、取新的会话对象来设置，避免用到缓存里已经失效的对象。
        // 匹配策略与上面完全一致，不会凭空多静音别的进程。
        unsafe { set_mute_match(self.pid, &self.exe, kind, mute) }
    }
}

fn worker(rx: Receiver<Cmd>, snap: Arc<Mutex<AudioSnapshot>>) {
    unsafe {
        // MTA：与 UI 线程（可能是 STA）互不干扰
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let mut w = Worker {
        snap,
        pid: 0,
        exe: String::new(),
        cache: Vec::new(),
        cached_at: None,
        name_cache: HashMap::new(),
    };

    loop {
        match rx.recv_timeout(IDLE_RESCAN) {
            Ok(Cmd::Focus { pid, exe }) => {
                if w.pid != pid || w.exe != exe {
                    w.pid = pid;
                    w.exe = exe;
                    w.ensure_fresh(true);
                    w.publish();
                }
            }
            Ok(Cmd::SetMute(mute)) => match w.apply_mute(mute) {
                Ok(0) => {
                    w.set_error(Some(
                        "未找到该窗口进程的音频会话（可能它当前没有在发声）".to_string(),
                    ));
                    w.publish();
                }
                Ok(_) => {
                    w.set_error(None);
                    w.ensure_fresh(true);
                    w.publish();
                }
                Err(e) => {
                    w.set_error(Some(format!("{e:#}")));
                    w.publish();
                }
            },
            Ok(Cmd::Rescan) => {
                w.ensure_fresh(true);
                w.publish();
            }
            Err(RecvTimeoutError::Timeout) => {
                if w.pid != 0 {
                    w.ensure_fresh(false);
                    w.publish();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    unsafe { CoUninitialize() };
}

/// 枚举**所有活动播放设备**上的全部音频会话
fn collect_all() -> Result<Vec<AudioSessionInfo>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("创建音频设备枚举器失败")?;
        let devices = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .context("枚举播放设备失败")?;
        let device_count = devices.GetCount().unwrap_or(0);

        let mut out = Vec::new();
        for d in 0..device_count {
            let Ok(device) = devices.Item(d) else { continue };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(list) = manager.GetSessionEnumerator() else {
                continue;
            };
            let count = list.GetCount().unwrap_or(0);
            for i in 0..count {
                // 单个会话可能已失效，失败就跳过
                let Ok(ctl) = list.GetSession(i) else { continue };
                let state = ctl.GetState().map(|s| s.0).unwrap_or(0);
                if state == AudioSessionStateExpired.0 {
                    continue;
                }
                let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = ctl2.GetProcessId().unwrap_or(0);
                let muted = match ctl2.cast::<ISimpleAudioVolume>() {
                    Ok(vol) => vol.GetMute().map(|b| b.as_bool()).unwrap_or(false),
                    Err(_) => false,
                };
                out.push(AudioSessionInfo {
                    pid,
                    exe: String::new(),
                    muted,
                    device_index: d,
                    state,
                });
            }
        }
        Ok(out)
    }
}

/// 重新枚举一遍，按**与界面显示一致的策略**给匹配到的会话设置静音。
unsafe fn set_mute_match(
    target_pid: u32,
    target_exe: &str,
    kind: MatchKind,
    mute: bool,
) -> Result<usize> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        .context("创建音频设备枚举器失败")?;
    let devices = enumerator
        .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        .context("枚举播放设备失败")?;
    let device_count = devices.GetCount().unwrap_or(0);

    let mut applied = 0usize;
    for d in 0..device_count {
        let Ok(device) = devices.Item(d) else { continue };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            continue;
        };
        let Ok(list) = manager.GetSessionEnumerator() else {
            continue;
        };
        let count = list.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(ctl) = list.GetSession(i) else { continue };
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let Ok(pid) = ctl2.GetProcessId() else { continue };
            let hit = match kind {
                MatchKind::Pid => pid == target_pid,
                MatchKind::Exe => {
                    !target_exe.is_empty()
                        && pid != 0
                        && crate::win::window::process_exe_name(pid)
                            .is_some_and(|n| n.eq_ignore_ascii_case(target_exe))
                }
                MatchKind::None => false,
            };
            if !hit {
                continue;
            }
            let Ok(vol) = ctl2.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            vol.SetMute(mute, std::ptr::null())
                .context("SetMute 调用失败")?;
            applied += 1;
        }
    }
    Ok(applied)
}
