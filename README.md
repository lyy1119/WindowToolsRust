# WindowToolsRust

使用 Rust 编写的、类似 [WindowTop](https://github.com/BiNGdK/WindowTop) 的 Windows 窗口增强工具。

当前处于**框架阶段**：功能骨架已全部打通、可编译、可运行，细节待真机验证后打磨。

---

## 已实现的功能

| 功能 | 状态 | 说明 |
|---|---|---|
| 拾取窗口 | ✅ | `Ctrl+Alt+P` 拾取光标下的窗口，或从窗口列表选择 |
| 给窗口加红框 | ✅ | 置顶镂空覆盖层 + `SetWinEventHook` 跟随移动/缩放 |
| 窗口置顶 | ✅ | `SetWindowPos(HWND_TOPMOST)` |
| 调整为指定分辨率 | ✅ | 支持「客户区尺寸」与「外框尺寸」两种口径，自动换算 |
| 静音 / 取消静音 | ✅ | 走 WASAPI 音频会话，**按进程**静音 |
| 注入标题栏右键菜单 | ⚠️ 待验证 | `GetSystemMenu + AppendMenuW` 注入；点击靠低层鼠标钩子命中测试 |
| 托盘图标 + 托盘菜单 | ✅ | 用 `tray-icon`，双击托盘图标唤出主窗口 |
| 全局快捷键 | ✅ | `global-hotkey` |
| 配置持久化 | ✅ | `%APPDATA%\WindowToolsRust\config.json` |

## 技术选型

* **Windows 原生能力** → [`windows`](https://crates.io/crates/windows)（windows-rs 0.62）
* **界面** → [`eframe`](https://crates.io/crates/eframe) / egui 0.36（纯 Rust，不写 Win32 界面代码）
* **托盘** → [`tray-icon`](https://crates.io/crates/tray-icon) 0.26
* **全局热键** → [`global-hotkey`](https://crates.io/crates/global-hotkey) 0.8

**所有 `unsafe` 与裸句柄都集中在 `src/win/` 目录**，其余模块（界面、托盘、热键、状态）完全不碰 Win32。

## 目录结构

```
src/
├── main.rs            入口：初始化日志 / 配置 / 钩子，然后进 eframe
├── app.rs             eframe App：串联托盘、热键、界面、后台轮询
├── ui.rs              egui 界面
├── tray.rs            托盘图标与托盘菜单
├── hotkey.rs          全局快捷键
├── config.rs          配置读写 (serde_json)
├── state.rs           全局共享状态 AppState
└── win/               仅此目录允许 unsafe
    ├── mod.rs         句柄转换辅助
    ├── window.rs      窗口枚举 / 信息 / 置顶 / 调整大小
    ├── frame.rs       红框覆盖层（镂空置顶窗口 + WinEvent 跟随）
    ├── audio.rs       按进程静音（WASAPI 音频会话，独立 MTA 线程）
    ├── sysmenu.rs     系统菜单注入 + 点击捕获
    └── actions.rs     业务动作（GUI / 托盘 / 热键 / 系统菜单 的公共入口）
```

## 快捷键

| 快捷键 | 作用 |
|---|---|
| `Ctrl+Alt+P` | 拾取光标下的窗口作为目标 |
| `Ctrl+Alt+F` | 红框开 / 关 |
| `Ctrl+Alt+T` | 置顶 / 取消置顶 |
| `Ctrl+Alt+M` | 静音 / 取消静音 |

## 构建

需要 Rust stable（实测 1.99.0）。

```powershell
# Windows 本机（推荐，MSVC 工具链）
cargo build --release

# 或交叉编译成 GNU 目标
cargo build --release --target x86_64-pc-windows-gnu
```

产物：`target/<target>/release/window-tools-rust.exe`

> Debug 构建会保留控制台窗口并输出日志，方便排查；Release 构建使用
> `windows_subsystem = "windows"`，不显示控制台。

---

## 关键设计说明（为什么这么写）

### 1. 红框：为什么不用注入

用 `SetWindowLongPtr` 给别的进程窗口挂钩子（子类化）是**被系统禁止的**，
所以红框走「自己建一个镂空置顶窗口 + 跟随目标窗口」的路线：

* 覆盖层窗口用 `SetWindowRgn` 挖空中心 → 中间完全透明，且鼠标可穿透（`HTTRANSPARENT`）；
* 用 `SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, WINEVENT_OUTOFCONTEXT)` 跟随目标窗口，
  `WINEVENT_OUTOFCONTEXT` 意味着回调在**我们自己的进程**里执行，不需要往目标进程注入 DLL；
* 目标最小化/隐藏时红框自动隐藏，目标销毁时红框自动关闭。

### 2. 系统菜单：能做与不能做

* ✅ **能给其它进程的窗口加菜单项**：`GetSystemMenu(hwnd, false)` + `AppendMenuW` 对别的进程窗口有效，
  菜单对象由窗口管理器持有。
* ❌ **不能跨进程子类化**：`SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ..)` 对其它进程窗口会失败，
  所以目标进程收到 `WM_SYSCOMMAND` 时我们**收不到通知**。
* ✅ 因此点击捕获改用 **`WH_MOUSE_LL` 低层鼠标钩子**（回调同样在本进程执行）：
  右键按下时记录窗口并注入条目 → 左键按下时用 `GetMenuItemRect` 做命中测试 → 命中我们的条目就执行动作。

> ⚠️ 这套命中测试逻辑在 Linux 上无法验证，**需要真机确认**。
> 如果发现点击不灵敏或误触发，备选方案是「右键时用自绘菜单替换系统菜单」
> （`TrackPopupMenuEx` + `TPM_RETURNCMD`），100% 可控但需要自己重建系统菜单项。

### 3. 静音：为什么是「按进程」

Windows 的声音是**进程级**的音频会话，没有「按窗口」的静音接口。因此：

* 实现方式：枚举默认播放设备上的 `IAudioSessionControl2`，匹配 `GetProcessId`，
  再用 `ISimpleAudioVolume::SetMute`。
* 限制：只有当该进程**正在发声**时才会存在会话；没在放声音时枚举不到，界面会显示
  「未检测到会话」。同一进程的多个窗口会一起被静音。
* 若需要「即使没发声也能静音」，后续可升级为进程回环捕获
  （`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`，要求 Windows 10 2004+）。

### 4. 权限

如果要操作**以管理员身份运行**的目标窗口，本工具也必须以管理员身份运行，
否则 `SetWindowPos` / `GetSystemMenu` 等调用会因完整性级别不足而失败。
目前 manifest 是 `asInvoker`，需要时可改为 `requireAdministrator`。

---

## 已知限制 / TODO

- [ ] 系统菜单点击捕获需真机验证（见上文第 2 点）
- [ ] 红框在跨虚拟桌面、DPI 缩放变化时的表现需验证
- [ ] 静音对 UWP 应用、以及「未发声的进程」尚不支持
- [ ] 还没有 exe 图标与版本资源（需要 `.ico` + `build.rs`）
- [ ] 系统菜单里的「调整分辨率」目前直接用配置里的第一个预设，没有子菜单
- [ ] 未做开机自启、单实例检测
- [ ] 目标窗口被管理员权限进程占用时需要提权提示
