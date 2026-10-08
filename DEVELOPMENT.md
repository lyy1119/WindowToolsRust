# WindowToolsRust 开发说明

> 本文档面向**后续维护者**（人类和 AI 同等重要）。
> 目标是：读完本文档 + 抽查少量源码，就能安全、正确地继续开发这个项目。
>
> 建议阅读顺序：**第 1 章（总体设计）→ 第 2 章（逐文件精读）→ 第 3 章（技术专题）→ 第 4 章（修改指引）**。
> 只想改一个小功能的话，直接跳到第 4 章看对应的「改动配方」。

## 目录

1. [总体设计](#1-总体设计)
2. [逐文件说明](#2-逐文件说明)
3. [技术专题：为什么这么做](#3-技术专题为什么这么做)
4. [常见修改指引（改动配方）](#4-常见修改指引改动配方)
5. [构建、验证与发布](#5-构建验证与发布)
6. [已知限制与后续方向](#6-已知限制与后续方向)

---

# 1. 总体设计

## 1.1 技术选型与边界

| 层面 | 选型 | 理由 |
|---|---|---|
| Windows 原生能力 | `windows` (windows-rs) 0.62 | 官方绑定，API 返回 `Result`/`BOOL` 而不是裸错误码，比 `winapi` 少踩坑；类型安全（`HWND`/`HMENU` 是独立类型，不会互相传错） |
| GUI | `eframe` / `egui` 0.36 | 纯 Rust、单依赖树、不需要写 Win32 界面代码，也不需要 .NET/WebView 运行时 |
| 托盘 | `tray-icon` 0.26 | 对 `Shell_NotifyIcon` 的安全封装，菜单用 `muda` |
| 全局热键 | `global-hotkey` 0.8 | 同上，且支持字符串解析热键 |
| 配置 | `serde` + `serde_json` | 结构简单、人工可读可改 |

**刻意不做的事**：

* **不注入目标进程**（不写 DLL、不改别人的 `WndProc`）。原因见 [3.1](#31-跨进程操作的限制这是理解整个项目的钥匙)。
* **不做全局键盘/鼠标钩子以外的系统级 hook**。全项目只用一个 `WH_MOUSE_LL` 低层鼠标钩子。

## 1.2 线程模型

只有**两个线程**，理解这一点对读懂代码很关键。

### 主线程（同时也是 winit / eframe 的事件循环线程）

它承担了几乎所有工作，原因是：**下面这些 Win32 对象都依赖「安装它们的线程持续泵消息」**，
而主线程由 winit 的事件循环天然满足这个条件：

| 对象 | 依赖消息循环的原因 |
|---|---|
| 红框覆盖层窗口 | 需要接收 `WM_PAINT` / `WM_NCHITTEST` |
| `WH_MOUSE_LL` 低层鼠标钩子 | 钩子回调被投递到安装线程的消息队列 |
| `SetWinEventHook(..., WINEVENT_OUTOFCONTEXT)` | 同上 |
| 托盘图标 | `tray-icon` 内部创建隐藏窗口接收 shell 消息 |
| 全局热键 | `global-hotkey` 内部创建消息窗口接收 `WM_HOTKEY` |
| 单实例命名事件 | 不需要窗口，但轮询在主线程做 |

主线程上跑的代码：

* `main()` → `eframe::run_native` → `App::logic()` / `App::ui()`（每帧各一次）
* 上述所有钩子/窗口的回调函数（`overlay_proc` / `mouse_proc` / `win_event_proc`）

### 音频工作线程（`wt-audio`）

只有 `win::audio` 用它，见 [2.13](#213-srcwinaudiors--按进程静音477-行)。原因：

* COM 在 **MTA** 下使用，避免和主线程可能存在的 STA（winit 会为拖放做 `OleInitialize`）互相干扰；
* 枚举音频设备/会话是一串 COM 调用，放到后台不会卡界面。

它通过 `std::sync::mpsc` 接收命令、通过 `Arc<Mutex<AudioSnapshot>>` 回传结果，**不直接访问 `AppState`**。

## 1.3 数据流：四个入口全部收敛到 `win::actions`

理解这个收敛结构，基本就掌握了项目的骨架。

```text
                       ┌──────────────────────┐
   界面按钮 ───────────▶│                      │
   托盘菜单 ───────────▶│   win::actions::*    │──▶ win::window / frame / audio / sysmenu / ...
   全局热键 ───────────▶│  （唯一的动作实现）   │         （真正的 Win32 调用）
   系统菜单注入项 ─────▶│                      │
                       └──────────┬───────────┘
                                  │ 读写
                                  ▼
                       Shared = Arc<Mutex<AppState>>
                                  ▲
                                  │ 每帧读取 + 轮询事件
                       ┌──────────┴───────────┐
                       │  App::logic / App::ui │
                       └──────────────────────┘
```

**为什么要收敛**：同一件事（比如「置顶」）有四个触发入口。如果每个入口各写一遍，
就会出现「界面按钮能置顶、托盘菜单不能」这类不一致。所以所有入口只负责**解析出目标窗口**，
然后调用同一个 `actions::` 函数。

## 1.4 每帧心跳

`App::logic()` 每帧执行一轮「泵事件 + 轮询」，这是整个程序的心跳：

```text
handle_close_request   ← 拦截关窗，改为隐藏到托盘
pump_show_request      ← 另一个实例请求唤出窗口？
apply_pending_hotkeys  ← 界面上改了快捷键，重新注册
pump_tray_menu         ← 托盘菜单点击
pump_hotkeys           ← 全局热键按下
pump_tray_icon         ← 托盘图标双击
poll_audio             ← 把音频线程的快照搬过来（带版本号，没变不搬）
refresh_log_tail       ← 日志尾部缓存（带版本号，没变不重建）
sync_tray_state        ← 托盘勾选状态跟随实际状态
frame::tick()          ← 红框跟随的兜底轮询
ctx.request_repaint_after(interval)
```

`interval` 是自适应的：

| 情况 | 间隔 | 理由 |
|---|---|---|
| 有红框在跟随 | 33 ms（约 30 Hz） | 拖动窗口时要跟手 |
| 空闲 | 400 ms | 只是轮询事件，没必要高频重绘 |

**注意**：`logic()` 在**主窗口隐藏时也会被调用**（eframe 0.36 的特性），
所以「关闭到托盘」之后托盘/热键依然工作。这是整个常驻逻辑成立的前提。

## 1.5 状态共享与锁的纪律

全局状态是 `Shared = Arc<parking_lot::Mutex<AppState>>`（定义在 `state.rs`）。

> ⚠️ **最重要的一条纪律：绝不在持有锁的情况下调用会再次加锁的函数。**
>
> `parking_lot::Mutex` **不可重入**，违反这条会直接死锁。
> 正确写法是先 `clone` 出需要的数据、`drop` 掉 guard，再去调 `actions::`：
>
> ```rust
> // ✅ 正确
> let (rgb, th) = { let st = state.lock(); (st.config.frame_color, st.config.frame_thickness) };
> frame::show(h, rgb, th)?;
>
> // ❌ 错误：show() 内部可能再 lock
> frame::show(h, state.lock().config.frame_color, ...)?;
> ```
>
> `ui.rs::snapshot()` 就是为了这个目的存在的：**一次性把界面这一帧要用到的数据快照出来**，
> 之后画界面全程不再加锁。

## 1.6 句柄的存储约定

Win32 句柄（`HWND` / `HANDLE` / `HMENU`）在 windows-rs 里是**裸指针的 newtype**，
既不是 `Send` 也不是 `Sync`。而 `Shared` 会被放进 `static OnceCell`（`sysmenu.rs` 里），
这要求 `AppState: Send + Sync`。

因此约定：

* **需要跨调用/跨线程长期保存的句柄，一律存成 `isize`**，用 `win::hwnd()` / `win::hwnd_i()` 互转；
* 只在单个函数内使用的句柄，直接用 `HWND` 类型，不必转换；
* 放进 `static` 的句柄用 `AtomicIsize`（例如 `frame.rs`、`sysmenu.rs`、`single_instance.rs`）。

## 1.7 错误处理约定

| 场景 | 做法 |
|---|---|
| `win::` 里真正的操作（`SetWindowPos` 等） | 返回 `anyhow::Result<T>`；失败信息带中文上下文，方便直接展示给用户 |
| 纯查询（`is_topmost` / `rect` / `class_name`） | 返回 `bool` / `Option<T>` / 默认值，**不把「查不到」当致命错误** |
| 面向用户的操作失败 | 写进 `AppState::log`（界面可见）+ 界面 toast，**不 panic** |
| 可选功能失败（托盘、热键、钩子） | 记日志后继续运行，功能降级而不是崩溃 |

整个项目**没有 `unwrap()` 处理外部输入**；仅有的几个 `expect` 是「内置常量不可能失败」的场景
（例如 `tray.rs` 里自造的 RGBA 图标数据、`audio.rs` 里启动工作线程）。

## 1.8 日志

`AppState::log(msg)` 做三件事：

1. 加 `[HH:MM:SS]` 前缀后写 `log::info!`（Debug 构建时输出到控制台）；
2. 追加到内存环形缓冲 `log: Vec<String>`，**上限 300 条**，超出时丢掉最早的 100 条；
3. `log_rev += 1`（版本号），让界面知道「日志变了，该重建缓存了」。

时间戳用 `SystemTime` 自己算 `HH:MM:SS`，**刻意不引入 `chrono`** —— 只为一行日志不值得多一个依赖。

## 1.9 版本与平台门槛

* `Cargo.toml` 里 `edition = "2021"`，`version = "1.0.0"`。
* `main.rs` 顶部：

  ```rust
  #![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]
  ```

  即 **Release 构建不显示控制台窗口，Debug 构建保留控制台**方便看日志 —— 这是刻意设计的调试体验。
* `windows` 的 features 在 `Cargo.toml` 里按需开启；**加新 API 时经常需要补一个 feature**，
  例如 `IMMDevice::Activate` 需要同时开 `Win32_System_Com_StructuredStorage` 和 `Win32_System_Variant`
  （windows-rs 会按参数类型把方法 cfg 掉，不开就报「no method found」，很容易卡住）。

---

# 2. 逐文件说明

每个文件按「设计目的 → 数据结构 → 函数 → 注意与坑」组织。

## 2.1 `src/main.rs` — 进程入口（72 行）

### 设计目的

按**严格顺序**完成启动前必须做的事，然后把控制权交给 eframe。
顺序在这里非常关键，注释里也写了原因。

### 启动顺序（每一步都不能随意挪动）

| 顺序 | 动作 | 为什么必须是这个位置 |
|---|---|---|
| 1 | `win::dpi::enable_per_monitor_v2()` | **必须在任何窗口创建之前**。否则进程被标记为 DPI-unaware，之后所有窗口坐标都会被系统虚拟化缩放，红框位置会算错 |
| 2 | `env_logger` 初始化 | 之后的日志才有输出 |
| 3 | 读配置 `Config::load` | 后面的单实例开关来自配置 |
| 4 | 单实例 `single_instance::acquire()` | 抢不到就直接 `request_show()` + `return`。放在创建任何窗口/钩子**之前**，避免第二个实例做无用功 |
| 5 | 构造 `Shared`（`AppState::new`） | 内部会 `AudioService::spawn()`（起音频线程）和 `privilege::is_elevated()`（权限自检） |
| 6 | `sysmenu::install_command_watcher(state)` | 低层鼠标钩子必须在**有消息循环的线程**上安装。此刻 winit 还没开始泵消息，但钩子回调只会在消息循环启动后被投递，所以这里装是安全的 |
| 7 | `frame::init()` | 预创建红框覆盖层窗口，避免第一次标记时闪一下 |
| 8 | `eframe::run_native(...)` | 阻塞在这一行，内部跑事件循环 |
| 9 | `state.lock().save_config()` | 正常退出后保存配置 |

### 函数

#### `fn main() -> anyhow::Result<()>`

* **返回**：只在 `eframe::run_native` 失败时返回 `Err`；正常退出返回 `Ok(())`。
* **副作用**：创建互斥体、钩子、窗口；阻塞运行 GUI；退出时写配置文件。
* **注意**：单实例抢失败时直接 `return Ok(())`，这是**正常的成功退出**，不是错误。

## 2.2 `src/app.rs` — eframe 应用主体（344 行）

### 设计目的

把「事件来源」（托盘、热键、另一个实例、窗口关闭请求）和「数据来源」（音频线程、日志）
统一收敛成一个每帧一次的泵（pump），并驱动界面渲染。**这里只做编排，不实现任何业务动作**，
业务动作全部在 `win::actions`。

### 数据结构

```rust
pub struct App {
    state: Shared,                       // 全局状态
    tray: Option<Tray>,                  // 托盘；创建失败时为 None，功能降级
    hotkeys: Option<Hotkeys>,            // 全局热键；注册失败时为 None
    ui_state: UiState,                   // 界面的草稿状态（输入框内容等）
    last_audio_poll: Option<Instant>,    // 上次拉取音频快照的时间，节流到 600ms
    last_frame_flag: Option<bool>,       // 上次同步给托盘的「红框」勾选值
    last_topmost_flag: Option<bool>,     // 上次同步给托盘的「置顶」勾选值
    quitting: bool,                      // 用户是否已明确要求退出
}
```

> `last_*_flag` 存在的原因是：托盘勾选状态每次 `set_checked()` 都会触发一次重绘/保存，
> 所以只在值**变化**时才调用。`None` 表示「还没同步过」。

### 函数

#### `pub fn new(cc: &eframe::CreationContext<'_>, state: Shared) -> Self`

* **参数**：`cc` 提供 `egui_ctx`；`state` 是已经构造好的全局状态。
* **逻辑（顺序有讲究）**：
  1. `set_visuals(Visuals::dark())`；
  2. `let cfg = state.lock().config.clone()` —— **先 clone 出配置**（后面多次要用，且不能持锁）；
  3. **安装中文字体** `fonts::install(&cc.egui_ctx, &cfg.font_file)`，成功/失败都记日志；
  4. 权限自检结果写日志（读取 `state.elevated()`）；
  5. `hotkey::build(&cfg)` 注册全局热键，失败则 `hotkeys = None` 并记日志；
  6. `tray::build()` 创建托盘，失败则 `tray = None` 并记日志；
  7. `UiState::new(&cfg, config_path)`。
* **返回**：构造好的 `App`。
* **注意**：**托盘和热键必须在这个函数（主线程）里创建**，它们依赖主线程的消息循环。

#### `fn show_main_window(ctx: &egui::Context)`（关联函数，无 self）

发送两个 viewport 命令：`Visible(true)` + `Focus`，再 `request_repaint()`。
用于「双击托盘」和「另一个实例请求唤出窗口」。

#### `fn pump_show_request(&mut self, ctx)`

* 若 `config.single_instance` 关闭则直接返回；
* 调用 `win::single_instance::take_show_request()`（自动重置事件，读一次即清零）；
* 为真时唤出主窗口并记日志「检测到重复启动，已唤出主窗口」。

#### `fn pump_tray_menu(&mut self, ctx)`

从 `MenuEvent::receiver()`（muda 的跨线程 channel）里 drain 所有事件，按 id 分派：

| id 常量 | 动作 |
|---|---|
| `ID_SHOW` | 唤出主窗口 |
| `ID_PICK` | `actions::pick_under_cursor`（作用于光标下的窗口） |
| `ID_FRAME` | `actions::toggle_frame(&state, false)` |
| `ID_TOPMOST` | `actions::toggle_topmost(&state, false)` |
| `ID_MUTE` | `actions::toggle_mute(&state, false)` |
| `ID_QUIT` | `quitting = true` → 保存配置 → 发 `ViewportCommand::Close` |

> **为什么托盘传 `false`（不用前台语义）**：用户点托盘菜单时，前台窗口已经变成托盘宿主窗口了，
> 用「当前前台窗口」会操作错对象。托盘和界面按钮始终作用于**已拾取的目标**。

#### `fn pump_hotkeys(&mut self, ctx)`

1. 取 `ids = [pick, frame, topmost, mute]` 的 `Option<u32>`（留空的快捷键是 `None`，不参与匹配）；
2. 读一次 `config.hotkey_foreground` 决定作用对象（前台 vs 已拾取）；
3. drain `GlobalHotKeyEvent::receiver()`，只处理 `HotKeyState::Pressed`（忽略 Released，避免触发两次）；
4. 分派到 `actions::`，把结果写日志并 `request_repaint()`。

> 拾取键（`ids[0]`）**不受 `hotkey_foreground` 影响** —— 它本来就是「取光标下的窗口」。

#### `fn pump_tray_icon(&mut self, ctx)`

只处理 `TrayIconEvent::DoubleClick` → 唤出主窗口。

#### `fn apply_pending_hotkeys(&mut self)`

界面改快捷键后，间接通过 `AppState.pending_hotkey_apply` 触发的实际注册动作。

* 读并清除 `pending_hotkey_apply`；
* 已有 `Hotkeys` 时调 `hotkeys.reapply(&cfg)`（带回滚），没有则尝试 `hotkey::build(&cfg)`；
* 成功/失败都记日志；
* **最后把界面输入框的内容同步回「真正生效的值」**（注册失败回滚后，输入框要显示旧值）。

#### `fn poll_audio(&mut self)`

节流到 600 ms：

```rust
let snap = self.state.lock().audio.snapshot();
if snap.rev != self.ui_state.audio.rev || snap.error != self.ui_state.audio.error {
    self.ui_state.audio = snap;
}
```

> **只做「搬运」，不做枚举。** 真正的设备/会话枚举在音频线程里、且有 1.5 s 缓存。
> 这里的版本号比较是为了避免无谓的深拷贝（`AudioSnapshot` 里含 `Vec<AudioSessionInfo>`）。

#### `fn refresh_log_tail(&mut self)`

日志版本号没变就返回；变了才取最后 150 条克隆进 `ui_state.log_tail`。
（早期版本每帧克隆 200 条字符串，是明确的性能浪费点。）

#### `fn handle_close_request(&mut self, ctx)`

点窗口关闭按钮时的拦截逻辑：

```text
if !close_requested                → return
if quitting || pending_quit || !config.close_to_tray
    → quitting = true; 放行（真正退出）
else
    → CancelClose + Visible(false)，记日志「窗口已隐藏到托盘…」
```

* **必须在同一帧内发送 `CancelClose`**，eframe 才会取消这次关闭（它检查本次 pass 的
  `viewport_output.commands` 里有没有 `CancelClose`）。
* `pending_quit` 是给「以管理员身份重启」用的：那条路径要真正退出，不能被「关闭到托盘」吃掉。

#### `fn sync_tray_state(&mut self)`

读实际状态（`frame_on` + 目标窗口的 `is_topmost`），与 `last_frame_flag` / `last_topmost_flag`
比较，只在变化时 `set_checked()`。

#### `impl eframe::App for App`

* `fn logic(&mut self, ctx, _frame)` —— 见 [1.4 每帧心跳](#14-每帧心跳)。
* `fn ui(&mut self, ui, _frame)` —— 只有一行：`ui::draw(ui, &self.state, &mut self.ui_state)`。
* `fn on_exit(&mut self, _gl)` —— 保存配置 + `sysmenu::uninstall_command_watcher()`。

#### `fn report(r: anyhow::Result<String>) -> String`

把 `Result<String>` 压成一行可展示文本：成功取原串，失败变成 `"失败: {e:#}"`。
**只用于界面/日志展示，不吞错误语义。**

> 注意 eframe 0.36 的 `App` trait 是 `logic()` + `ui()`，**不是**旧版的 `update()`。
> 从网上抄示例代码时特别容易在这里踩坑。

## 2.3 `src/state.rs` — 全局共享状态（106 行）

### 设计目的

定义「整个程序在运行期间需要共享的所有可变数据」以及访问它的类型别名。
**这个文件不依赖除 `config` / `win::audio` / `win::privilege` 之外的任何模块**，处于依赖树的底层。

### 数据结构

```rust
pub type Shared = Arc<Mutex<AppState>>;   // parking_lot::Mutex
```

#### `pub struct TargetWindow`

「当前操作的窗口」的快照。用值类型保存而不是每次重新查询，是为了避免在持锁状态下调系统调用。

| 字段 | 类型 | 说明 |
|---|---|---|
| `hwnd` | `isize` | 窗口句柄。**存 `isize` 而不是 `HWND`**，因为 `HWND` 是裸指针、不满足 `Send + Sync`（见 [1.6](#16-句柄的存储约定)） |
| `title` | `String` | 窗口标题（`GetWindowTextW`） |
| `class` | `String` | 窗口类名（`GetClassNameW`） |
| `pid` | `u32` | 所属进程 id |
| `exe` | `String` | 进程可执行文件名（如 `chrome.exe`）。用于音频会话的「按进程名回退匹配」；取不到时为空串 |

* `pub fn short_label(&self) -> String` —— 界面/日志用的短标签：标题为空时退化成 `类名 (0x句柄)`。

#### `pub struct AppState`

| 字段 | 类型 | 说明 |
|---|---|---|
| `config` | `Config` | 当前配置（界面里的「草稿值」在 `UiState` 里，点保存才写回这里） |
| `config_path` | `PathBuf` | 配置文件绝对路径，界面底部会显示 |
| `target` | `Option<TargetWindow>` | 当前目标窗口，`None` = 还没拾取 |
| `frame_on` | `bool` | 红框是否显示中。**缓存值**（真相在 `frame::is_active()`），用于界面按钮文案和托盘勾选 |
| `audio` | `AudioService` | 音频服务句柄（内部持有命令 channel + 快照），**可 Clone、廉价** |
| `menu_injected` | `Option<isize>` | 已经注入过系统菜单条目的窗口句柄。换目标时用它来清理旧窗口的菜单 |
| `frame_auto_for_topmost` | `Option<isize>` | **因为「置顶时自动标记红框」而加上的红框属于哪个窗口**。只记录自动加的那种，取消置顶时只移除它，不会误删用户手动标的红框 |
| `pending_hotkey_apply` | `bool` | 界面改了快捷键 → 置位 → `App::apply_pending_hotkeys` 在下一帧真正重新注册 |
| `pending_quit` | `bool` | 界面要求**真正退出**（目前只有「以管理员身份重启」用）。有这个标记时 `handle_close_request` 不再拦截关闭 |
| `log` | `Vec<String>` | 内存日志环形缓冲，上限 300 条 |
| `log_rev` | `u64` | 日志版本号，每次 `log()` 自增。界面据此判断要不要重建日志缓存 |
| `elevated` | `bool` | 启动时自检到的管理员权限状态，界面顶部据此显示提示 |

#### `fn timestamp() -> String`（模块私有）

用 `SystemTime::now()` 距 UNIX_EPOCH 的秒数算 `HH:MM:SS`（UTC）。
**刻意不引入 `chrono`** —— 只为日志前缀不值得多一个依赖。

### 函数

#### `pub fn new(config: Config, config_path: PathBuf) -> Self`

* **副作用**：调用 `AudioService::spawn()`（**会启动音频工作线程**）和 `privilege::is_elevated()`（系统调用）。
* 其余字段都给「空」初值：`target = None`、`frame_on = false`、两个 pending 标记为 `false`。

#### `pub fn log(&mut self, msg: impl Into<String>)`

加时间戳 → `log::info!` → push 进 `log` → `log_rev += 1` → 超过 300 条时 `drain(..100)`。
用 `impl Into<String>` 是为了让调用点可以同时传 `&str` 和 `String`。

#### `pub fn save_config(&mut self)`

调 `config.save(&config_path)`；失败时递归调用 `log()` 记录错误（不会再触发保存，无递归风险）。

#### `pub fn elevated(&self) -> bool`

只读访问器（避免 `state.lock().elevated` 这种字段直读，便于将来加逻辑）。

## 2.4 `src/config.rs` — 配置的读写（122 行）

### 设计目的

把「哪些设置可以持久化」集中到一处，并保证**向后兼容**：老配置文件缺少新字段时不会解析失败。

### 数据结构

#### `pub struct ResolutionPreset`

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | `String` | 预设名（可为空，界面里会只显示 `宽 x 高`） |
| `width` | `u32` | 宽（客户区或外框，取决于 `Config::resize_client_area`） |
| `height` | `u32` | 高 |

* `pub fn new(name: &str, width: u32, height: u32) -> Self`

#### `pub struct Config`（标注了 `#[serde(default)]`）

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `frame_color` | `[u8; 3]` | `[229, 40, 40]` | 红框颜色 RGB（`#E52828`） |
| `frame_thickness` | `i32` | `3` | 红框粗细，使用时再 `clamp(1, 32)` |
| `resize_client_area` | `bool` | `true` | 调整分辨率时按「客户区」还是「整个外框」算 |
| `restore_before_resize` | `bool` | `true` | 调整前若窗口最大化/最小化，先 `SW_RESTORE` |
| `inject_system_menu` | `bool` | `true` | 是否往别的窗口注入标题栏右键菜单条目 |
| `auto_frame_on_pick` | `bool` | `true` | 拾取到窗口后自动画红框 |
| `auto_frame_on_topmost` | `bool` | `true` | 置顶时自动画红框（取消置顶会自动移除「自动加的那个」） |
| `close_to_tray` | `bool` | `true` | 关闭窗口 = 隐藏到托盘，而不是退出 |
| `single_instance` | `bool` | `true` | 防止重复启动 |
| `hotkey_foreground` | `bool` | `true` | 快捷键作用于当前前台窗口（`false` = 只作用于已拾取目标） |
| `font_file` | `String` | `""` | 中文字体文件（绝对路径或 `%WINDIR%\Fonts` 下的文件名）。空 = 自动挑选 |
| `hotkey_pick` | `String` | `"Ctrl+Alt+P"` | 拾取窗口。**空串 = 不注册** |
| `hotkey_frame` | `String` | `"Ctrl+Alt+F"` | 红框开关 |
| `hotkey_topmost` | `String` | `"Ctrl+Alt+T"` | 置顶开关 |
| `hotkey_mute` | `String` | `"Ctrl+Alt+M"` | 静音开关 |
| `presets` | `Vec<ResolutionPreset>` | 5 个常见分辨率 | 会出现在系统菜单子菜单里 |

### 函数

#### `impl Default for Config`

给出上面所有默认值。注意默认 `presets` 是 1080P / 1600×900 / 720P / 1024×768 / 800×600。

#### `pub fn load(path: &Path) -> Self`

* 文件不存在 → 返回 `Config::default()`（首次运行）；
* 文件存在但 JSON 坏了 → 记 `log::warn!` 后返回默认值（**不崩、不覆盖用户文件**）；
* 解析成功 → 反序列化。**因为有 `#[serde(default)]`，老配置缺新字段时会自动补默认值。**

#### `pub fn save(&self, path: &Path) -> anyhow::Result<()>`

先 `create_dir_all(parent)`（目录不存在时自动建），再 `to_string_pretty` 写入。
用 pretty 格式是为了让用户能直接手改。

#### `pub fn default_config_path() -> PathBuf`

```text
%USERPROFILE%\WindowToolsSetting\config.json
```

`USERPROFILE` 缺失时退回 `%APPDATA%`，再缺失退到临时目录。
（**注意目录名是 `WindowToolsSetting`，没有复数 s**，这是需求里指定的。）

## 2.5 `src/ui.rs` — egui 界面（629 行，最大的文件）

### 设计目的

画界面 + 收集用户操作。**只做「显示」和「触发动作」，不含任何 Windows 调用**（全部经 `win::actions`）。

### 数据结构

#### `pub struct UiState`

「界面草稿状态」——用户正在编辑但还没保存的值都放这里。字段与 `Config` 基本一一对应，
外加一些纯界面用的字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `windows` | `Vec<TargetWindow>` | 窗口枚举结果缓存，**点「刷新窗口列表」才重新枚举** |
| `pick_idx` | `usize` | 下拉列表里选中项的下标 |
| `presets` | `Vec<ResolutionPreset>` | **可编辑的预设副本**，点「保存设置」才写回配置 |
| `new_preset_name` / `new_preset_w` / `new_preset_h` | `String` / `u32` / `u32` | 「新增预设」那一行的输入 |
| `custom_w` / `custom_h` | `u32` | 「自定义尺寸」的输入 |
| `color` | `[u8; 3]` | 红框颜色草稿 |
| `thickness` | `i32` | 红框粗细草稿（Slider 范围 1..=16） |
| `resize_client_area` / `restore_before_resize` | `bool` | 两个调整尺寸的选项 |
| `auto_frame_on_pick` / `auto_frame_on_topmost` | `bool` | 两个自动红框开关 |
| `close_to_tray` / `single_instance` / `hotkey_foreground` | `bool` | 三个行为开关 |
| `font_file` | `String` | 字体文件草稿 |
| `inject_system_menu` | `bool` | 菜单注入开关 |
| `hotkey_pick` / `hotkey_frame` / `hotkey_topmost` / `hotkey_mute` | `String` | 四个快捷键输入框内容 |
| `log_tail` | `Vec<String>` | 日志尾部缓存（由 `App::refresh_log_tail` 填充） |
| `log_rev` | `u64` | 上面这份缓存对应的日志版本号，初值 `u64::MAX` 保证第一次一定刷新 |
| `audio` | `AudioSnapshot` | 音频快照缓存（由 `App::poll_audio` 按版本号填充） |
| `config_path` | `String` | 配置文件路径，启动时读一次就固定，不用每帧 `display().to_string()` |

#### `struct Snapshot`（模块私有）

界面一帧要用到的、来自 `AppState` 的只读数据快照。

| 字段 | 类型 | 说明 |
|---|---|---|
| `target` | `Option<TargetWindow>` | 当前目标窗口 |
| `frame_on` | `bool` | 红框开关状态 |
| `topmost` | `bool` | 目标窗口当前是否置顶（**每帧实查**，因为窗口状态可能被别的程序改） |
| `resizable` | `bool` | 目标窗口是否有 `WS_THICKFRAME`（不可调整大小时界面会提示） |
| `elevated` | `bool` | 是否已提权 |

> **设计要点**：`log_tail` / `audio` / `config_path` 这些「会变但很大」的数据**故意不放 Snapshot**，
> 而是缓存在 `UiState` 里按版本号更新，避免每帧深拷贝。

### 函数

#### `impl UiState`

* `pub fn new(cfg: &Config, config_path: String) -> Self` —— 从配置初始化所有草稿字段。
* `pub fn apply_to(&self, cfg: &mut Config)` —— 把草稿写回配置。字符串字段会 `trim()`。

#### `fn snapshot(state: &Shared) -> Snapshot`

一次加锁取出这一帧需要的所有数据，**取完立刻释放锁**。
`topmost` / `resizable` 是对目标窗口的实时查询（会调 Win32，但很轻）。

#### `pub fn draw(ui: &mut egui::Ui, state: &Shared, s: &mut UiState)`

只有一件事：把整个界面套进竖向 `ScrollArea`（`auto_shrink([false, false])`），
内部调用 `draw_inner`。这样窗口再矮也能滚到全部设置。

#### `fn draw_inner(ui, state, s)`

按顺序画这些区块（**顺序即界面顺序，改布局改这里**）：

| 顺序 | 区块 | 关键交互 |
|---|---|---|
| 1 | 标题 + 一句话说明 | — |
| 2 | 权限提示 | 未提权时显示橙色提示 + 「以管理员身份重启」按钮（调 `actions::restart_elevated`，成功后置 `pending_quit` 再发 `Close`） |
| 3 | 目标窗口 | `Grid` 展示 标题/类名/PID/HWND/可调整大小；「拾取光标下的窗口」「拾取前台窗口」「刷新窗口列表」三个按钮；有列表时显示 `ComboBox` |
| 4 | 窗口操作 | 三个按钮（红框 / 置顶 / 静音），**都传 `prefer_foreground = false`**（作用于已拾取目标）；下面是音频状态行 + 「刷新音频信息」+ 可折叠的「所有音频会话」表格 |
| 5 | 分辨率预设 | 可编辑表格（名称/宽/高 + 「应用到当前窗口」+ 「删除」）、「新增预设」行、「自定义尺寸」行 + 两个选项 checkbox |
| 6 | 设置 | `Grid`，见 README 的设置项表 |
| 7 | 保存区 | 「保存设置」（写回配置 + 置 `pending_hotkey_apply` + 重新注入系统菜单）、「重新注入系统菜单」、「移除系统菜单注入」；底部显示配置文件路径 |
| 8 | 全局快捷键 | 四个输入框 + 「应用快捷键」/「恢复默认快捷键」 |
| 9 | toast | 上一动作的结果提示 |
| 10 | 运行日志 | 竖向 `ScrollArea`，`max_height(160)`，`stick_to_bottom(true)`，读 `s.log_tail` |

#### `fn report(r: anyhow::Result<String>) -> String`

与 `app.rs` 里同名函数作用相同（两处各自私有，避免为一个 5 行函数搞公共模块）。

### 注意与坑

* **界面里任何地方都不能在持锁时调 `actions::`** —— 先 clone 再调用（见 [1.5](#15-状态共享与锁的纪律)）。
* 预设表格用**下标循环** `for i in 0..s.presets.len()` 而不是迭代器，
  这样可以在循环里可变借用 `s.presets[i]`，同时把「删除」「应用」收集到临时变量，循环结束后再执行
  （否则会和迭代器借用冲突）。
* 界面的「红框」按钮文案依赖 `snap.frame_on`，而真相在 `frame::is_active()`。
  两者在正常路径下一致，异常路径（如目标窗口被外部销毁）时会由 `frame.rs` 内部 `hide()` 修正状态。

## 2.6 `src/tray.rs` — 托盘图标与菜单（78 行）

### 设计目的

用 `tray-icon` 创建托盘图标和菜单。**不直接碰 `Shell_NotifyIcon`**。

### 数据结构

```rust
pub const ID_SHOW:    &str = "wt.show";
pub const ID_PICK:    &str = "wt.pick";
pub const ID_FRAME:   &str = "wt.frame";
pub const ID_TOPMOST: &str = "wt.topmost";
pub const ID_MUTE:    &str = "wt.mute";
pub const ID_QUIT:    &str = "wt.quit";

pub struct Tray {
    pub icon: TrayIcon,                 // 必须一直持有，drop 掉图标就消失了
    pub frame_item: CheckMenuItem,      // 需要保留引用来同步勾选状态
    pub topmost_item: CheckMenuItem,
}
```

### 函数

#### `pub fn build() -> Result<Tray>`

创建菜单项（顺带用 `PredefinedMenuItem::separator()` 插两条分隔线），
构建 `TrayIconBuilder`（菜单 + tooltip + 图标），返回 `Tray`。

> `menu` 本身不需要保存 —— 它被 `TrayIcon` 接管了所有权。但两个 `CheckMenuItem`
> 必须留着，`App::sync_tray_state` 要拿它们调 `set_checked()`。

#### `fn make_icon() -> Icon`（模块私有）

**在代码里生成**一个 32×32 的 RGBA 图标（红色边框 + 全透明中心，呼应「红框」这个主题），
避免引入图片资源文件和 `image` 依赖。

## 2.7 `src/hotkey.rs` — 全局快捷键（148 行）

### 设计目的

包装 `global-hotkey`，把「配置里的字符串」翻译成热键注册，并处理**留空 = 不注册**和**注册失败回滚**。

### 数据结构

```rust
pub struct Hotkeys {
    #[allow(dead_code)]
    pub manager: GlobalHotKeyManager,   // 必须一直持有，drop 掉注册就失效
    pub pick:    Option<HotKey>,        // None = 用户留空，不注册
    pub frame:   Option<HotKey>,
    pub topmost: Option<HotKey>,
    pub mute:    Option<HotKey>,
}
```

`Option<HotKey>` 是关键设计：**「留空」和「解析失败」是两回事** ——
留空是合法的「不要这个快捷键」，解析失败才报错。

### 函数

#### `pub fn parse(text: &str, which: &str) -> Result<Option<HotKey>>`

* `trim()` 后为空 → `Ok(None)`；
* 否则 `text.parse::<HotKey>()`，失败时把 `which`（哪个功能）和正确格式示例拼进错误信息。

#### `fn check_duplicates(keys: &[(&str, Option<HotKey>)]) -> Result<()>`

两两比较 `mods` 和 `key`，重复就报错（**留空的跳过**）。
比较 `mods`/`key` 而不是 `id`，因为这个比较不依赖 `HotKey::new` 的 id 生成规则。

#### `fn parse_all(cfg: &Config) -> Result<[(&'static str, Option<HotKey>); 4]>`

依次解析四个，然后查重。返回「显示名 + 热键」的数组，显示名用于错误信息。

#### `fn register(manager, name, key: Option<HotKey>) -> Result<Option<HotKey>>`

`None` 直接返回；`Some` 则 `manager.register(k)`，失败时把「可能被别的程序占用」写进提示。

#### `fn register_all(manager, cfg) -> Result<[Option<HotKey>; 4]>`

`parse_all` + 逐个 `register`。

#### `pub fn build(cfg: &Config) -> Result<Hotkeys>`

创建 `GlobalHotKeyManager` 并注册全部。**必须在主线程调用**（内部要消息循环）。

#### `pub fn describe(cfg: &Config) -> String`

给日志用的一行摘要，留空的显示成「（未设置）」。**注意它读配置而不是读 `Hotkeys`** ——
这样即使注册失败也能打印出「用户想设成什么」。

#### `impl Hotkeys::reapply(&mut self, cfg: &Config) -> Result<()>`

**带回滚的重新注册**，这是这个文件最需要小心的地方：

1. 先把当前四个热键全部 `unregister`；
2. 用新配置 `register_all`；
3. 成功 → 更新 `self` 的四个字段；
4. 失败 → **把旧的热键逐个重新注册回去**，然后返回错误。

> 没有第 4 步的话，用户填一个被占用的组合就会「旧的注销了、新的没注册上」，
> 结果一个快捷键都用不了。这是个很容易漏、后果又很烦人的坑。

## 2.8 `src/fonts.rs` — 中文字体加载（70 行）

### 设计目的

egui 自带字体（Ubuntu-Light / Hack）**不含 CJK 字形**，中文会渲染成方框（tofu）。
这里从系统字体目录挑一个中文字体，作为**回退字体**挂进 egui 字体表。

### 数据结构

```rust
const CANDIDATES: &[(&str, u32)] = &[
    ("msyh.ttc", 0),   // 微软雅黑
    ("msyh.ttf", 0),
    ("msyhl.ttc", 0),  // 微软雅黑 Light
    ("Deng.ttf", 0),   // 等线
    ("simhei.ttf", 0), // 黑体
    ("simsun.ttc", 0), // 宋体
    ("msjh.ttc", 0),   // 微软正黑（繁体系统）
    ("mingliu.ttc", 0),
];
```

二元组是 `(文件名, ttc 内的 face 索引)`。`.ttc` 是字体集合，需要 `index` 指定用哪一个 face。

#### `fn font_dir() -> PathBuf`

`%WINDIR%\Fonts`，`WINDIR` 读不到时兜底 `C:\Windows`。

### 函数

#### `pub fn install(ctx: &Context, override_file: &str) -> Option<String>`

1. 组装候选列表：**用户指定的 `override_file` 排最前**（绝对路径直接用，否则当作 `%WINDIR%\Fonts` 下的文件名），
   后面接 `CANDIDATES`；
2. 逐个尝试 `std::fs::read`，读不到的跳过；
3. 第一个读到的：`FontData::from_owned(bytes)`，设置 `index`，
   插进 `fonts.font_data["cjk"]`；
4. 把它**追加到 `Proportional` 和 `Monospace` 两个 family 的末尾**（作为回退，
   拉丁字符仍用 egui 自带字体，中文回退到系统字体）；
5. `ctx.set_fonts(fonts)`，返回实际使用的字体文件名。

* **返回**：`Some(文件名)` 表示成功；`None` 表示一个候选都没读到（调用方会记警告日志）。
* **注意**：字体文件（微软雅黑约 20 MB）会**常驻内存**，因为 egui 要按需光栅化新字形。
  这是本程序内存占用的大头，`font_file` 配置项就是为此准备的。

---

## 2.9 ~ 2.17 `src/win/` —— Windows 原生能力层（模块群）

> **整个项目只有这个目录允许出现 `unsafe`。** 其它模块（界面、托盘、热键、状态）完全不碰 Win32。
> 这条约定让「哪里可能出内存/句柄问题」的范围被限制在这 9 个文件里。

## 2.9 `src/win/mod.rs` — 模块声明与句柄转换（25 行）

### 设计目的

`win` 子模块的门面。只放两件事：模块声明、句柄与 `isize` 的互转。

### 函数

#### `pub fn hwnd(v: isize) -> HWND`

`HWND(v as *mut core::ffi::c_void)`。**调用方必须自己保证 `v` 是有效句柄**，
所以只应该在「刚从 `hwnd_i` 存下来、还没被销毁」的场景使用。

#### `pub fn hwnd_i(h: HWND) -> isize`

`h.0 as isize`。用于把句柄存进 `AppState` / `AtomicIsize`（见 [1.6](#16-句柄的存储约定)）。

## 2.10 `src/win/dpi.rs` — DPI 感知（17 行）

### 设计目的

一个函数，但**必须在创建任何窗口之前调用**，否则整个红框功能都会算错位置。

### 函数

#### `pub fn enable_per_monitor_v2() -> bool`

调 `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)`。

* **返回**：成功 `true`；失败一般意味着 DPI 感知已被 manifest 或 winit 设置过，**可以安全忽略**。
* **为什么必须最早调用**：进程若是 DPI-unaware，Windows 会对 `GetWindowRect` /
  `SetWindowPos` 的坐标做「虚拟化」缩放（按主显示器的缩放比例换算）。
  在 125% / 150% 缩放或混合 DPI 多显示器下，算出来的红框位置会整体偏移，甚至看起来像「不跟随窗口」。
* **选了 V2 而不是 V1**：Per-Monitor-V2 才能正确处理跨显示器拖动时的 DPI 切换。

## 2.11 `src/win/window.rs` — 窗口查询与基本操作（282 行）

### 设计目的

项目里最基础的一层：**只做「窗口」这一概念的查询和操作**，不涉及红框/菜单/音频。
被 `frame` / `sysmenu` / `actions` 广泛调用。

### 函数分类

#### 信息查询（失败时返回默认值，不返回 Result）

| 函数 | 实现要点 |
|---|---|
| `pub fn title(h: HWND) -> String` | `GetWindowTextW` 到 `[u16; 512]`，`from_utf16_lossy`。返回的字符数做 `clamp` 防越界 |
| `pub fn class_name(h: HWND) -> String` | `GetClassNameW` 到 `[u16; 256]`，同上 |
| `pub fn pid(h: HWND) -> u32` | `GetWindowThreadProcessId(h, Some(&mut p))`，查不到返回 0 |
| `pub fn process_exe_name(pid: u32) -> Option<String>` | `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW`；成功后 `CloseHandle`（**这句千万别漏，否则句柄泄漏**），再从全路径取最后一段作为 `xxx.exe`。目标进程权限更高时返回 `None` |
| `pub fn is_window` / `is_visible` / `is_iconic` / `is_zoomed` | 对应 Win32 的 `BOOL` → `.as_bool()` |
| `pub fn rect(h) -> Result<RECT>` | `GetWindowRect`。**注意这是含不可见缩放边框的外框**，画红框时不要用它（见 `frame::visible_frame_rect`） |
| `pub fn client_size(h) -> Result<(i32, i32)>` | `GetClientRect` 后算宽高 |
| `pub fn is_topmost(h) -> bool` | 读 `GWL_EXSTYLE`，看 `WS_EX_TOPMOST` 位 |
| `pub fn is_resizable(h) -> bool` | 读 `GWL_STYLE`，看 `WS_THICKFRAME` 位 |

#### 窗口操作

##### `pub fn set_topmost(h: HWND, on: bool) -> Result<bool>`

* 先 `is_window` 检查，失效则 `bail!("目标窗口已失效")`；
* `SetWindowPos(h, Some(HWND_TOPMOST | HWND_NOTOPMOST), 0,0,0,0, SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE)`；
* **返回操作后的真实状态**（重新 `is_topmost` 查询，而不是假设成功），调用方据此写日志。

##### `pub fn toggle_topmost(h: HWND) -> Result<bool>`

`set_topmost(h, !is_topmost(h))`。注意「先读后写」之间有极小的竞态，
但对于用户手动操作完全够用。

##### `pub fn resize(target: HWND, width: u32, height: u32, client_area: bool, restore_first: bool) -> Result<(i32, i32)>`

* **参数**：`client_area` 为 `true` 时 `width/height` 指客户区尺寸，否则指整个外框；
  `restore_first` 为 `true` 时，若窗口处于最小化或最大化状态先 `ShowWindow(SW_RESTORE)`。
* **返回**：实际设置的外框尺寸 `(tw, th)`，用于日志/界面回显。
* **客户区换算逻辑**：`dw = 外框宽 - 客户区宽`，`dh` 同理，然后 `tw = width + dw`。
  用「实测差值」而不是 `AdjustWindowRectExForDpi`，因为它天然适配不同的边框样式和主题。
* **下界**：`tw.max(80)`、`th.max(60)`，避免把窗口压成 0。
* **注意**：对没有 `WS_THICKFRAME` 的窗口调用不会报错，但系统不会改变尺寸
  （`actions::resize_to` 会先探测并记一条提示日志）。

#### 拾取与判定

##### `pub fn root_window_at_cursor() -> Option<HWND>`

`GetCursorPos` → `WindowFromPoint` → `GetAncestor(GA_ROOT)`。
取不到根时退回 `under`；结果不可见则返回 `None`。

> `WindowFromPoint` 返回的是**最深处**的那个窗口（可能是子窗口），
> 所以必须再取顶层祖先。这一步对 Firefox 这类「内容是子窗口」的程序是必需的。

##### `pub fn foreground_window() -> Option<HWND>`

`GetForegroundWindow`，空句柄时返回 `None`。快捷键的「前台语义」用它。

##### `pub fn is_own_process(h: HWND) -> bool`

`pid(h) == std::process::id()`。用于排除「操作自己」的情况。

##### `pub fn is_eligible(h: HWND) -> bool`

判定「这值不值得作为操作对象」。**这是本项目改动风险最高、也最容易被误改的函数之一**，
完整规则：

```text
1. is_window && is_visible                 —— 必须存在且可见
2. GetAncestor(GA_ROOT) == h               —— 必须是顶层窗口（不接受子窗口）
3. !is_own_process(h)                      —— 不能是本程序自己的窗口
4. class_name 不在 SHELL_CLASSES 里         —— 排除桌面/任务栏等 shell 窗口
5. !(无标题栏 && (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE))  —— 排除工具窗口
6. 外框尺寸 >= 80 x 60                      —— 排除工具条/浮动面板
```

* **第 5 条是关键**：早期版本要求「必须有 `WS_CAPTION`（标题栏）」，
  结果 PotPlayer 皮肤模式、各种自绘标题栏的程序（用 `WS_POPUP` 自己画标题栏）
  被直接判为「不支持操作」，拾取和快捷键全都用不了。
  **现在只在「没有标题栏」且「带工具窗口风格」时才排除。**
* `SHELL_CLASSES`：`Progman`（桌面）、`WorkerW`（桌面壁纸层）、`Shell_TrayWnd`（任务栏）、
  `Shell_SecondaryTrayWnd`（副屏任务栏）、`TaskListThumbnailWnd`、`ForegroundStaging`。

#### 描述与枚举

##### `pub fn describe(h: HWND) -> TargetWindow`

把窗口信息打包成 `TargetWindow`（含 `exe`，会调一次 `OpenProcess`）。

##### `pub fn enumerate() -> Vec<TargetWindow>`

`EnumWindows` + 回调 `enum_proc`，只收集 `is_eligible` 的窗口，最后按标题小写排序。
**只在用户点「刷新窗口列表」时调用**，不会自己定期跑。

##### `unsafe extern "system" fn enum_proc(h: HWND, lparam: LPARAM) -> BOOL`

把 `lparam` 还原成 `&mut Vec<TargetWindow>` 后 push。返回 `BOOL(1)` 表示继续枚举。
（`lparam` 里塞的是 `&mut Vec` 的裸指针，这是 `EnumWindows` 的经典用法，
安全性由「只在 `enumerate` 的调用栈内有效」保证。）

## 2.12 `src/win/frame.rs` — 红框覆盖层（396 行）

### 设计目的

给任意目标窗口套一个红色边框。**不注入目标进程**，方式是自己创建一个
「只有边框、中间镂空」的置顶窗口，并让它跟随目标窗口。

### 实现原理（4 步）

1. 注册一个窗口类 `WindowToolsRust.FrameOverlay`，创建一个 `WS_POPUP` 窗口，
   带 `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT`；
2. 用 `SetWindowRgn` 把窗口中心挖空（`CombineRgn(RGN_DIFF)`）→ 中间完全透明、
   **鼠标可以穿透**（再配合 `WM_NCHITTEST` 返回 `HTTRANSPARENT` 双保险）；
3. 用 `SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, ..., WINEVENT_OUTOFCONTEXT)`
   跟随目标窗口的移动/缩放。`WINEVENT_OUTOFCONTEXT` 表示**回调在我们自己的进程里执行**，
   不需要 DLL 注入；
4. 主线程每帧再调一次 `tick()` 做**兜底轮询**，即使钩子漏事件红框也不会卡住。

### 静态状态（全部用原子量，因为钩子回调是 `extern "system"` 无法捕获环境）

| 名称 | 类型 | 说明 |
|---|---|---|
| `OVERLAY` | `AtomicIsize` | 覆盖层窗口句柄（0 = 还没创建） |
| `TARGET` | `AtomicIsize` | 当前被标记的目标窗口句柄（0 = 没有） |
| `HOOK` | `AtomicIsize` | `SetWinEventHook` 返回的钩子句柄 |
| `THICKNESS` | `AtomicI32` | 边框粗细 |
| `BRUSH` | `AtomicIsize` | 画边框用的 `HBRUSH`，换色时重建 |
| `UPDATING` | `AtomicBool` | **重入保护**：防止 `SetWindowPos` 触发的 LOCATIONCHANGE 回调再次进入更新逻辑 |
| `SHOWN` | `AtomicBool` | 覆盖层当前是否可见（避免重复 `ShowWindow`） |
| `LAST_X/Y/W/H` | `AtomicI32` | 上次应用的几何，**用来做变化检测**：几何没变就不调 `SetWindowPos`，大幅减少无谓的系统调用和重绘 |
| `LAST_Z_TICK` | `AtomicI32` | 上次「重申置顶」的 `GetTickCount` |
| `COLOR` | `AtomicI32` | 当前红框颜色（RGB 打包） |

常量：`OVERLAY_CLASS`（类名）、`UNSET = i32::MIN`（几何缓存未初始化）、`Z_REASSERT_MS = 1000`。

### 函数

#### `pub fn init()`

预创建覆盖层窗口，避免第一次显示时有可见延迟。失败只记 `log::warn!`。

#### `pub fn is_active() -> bool` / `pub fn active_target() -> Option<isize>`

`TARGET != 0` / 返回 `TARGET`。前者被 `App::logic` 用来决定刷新率；
后者被 `actions::toggle_frame_for` 用来判断「是不是在给同一个窗口切换红框」。

#### `pub fn show(target: HWND, rgb: [u8; 3], thickness: i32) -> Result<()>`

1. 校验目标窗口有效；
2. `set_color(rgb)`（换色时重建画刷并删掉旧的）、`THICKNESS.store(clamp(1,32))`；
3. `ensure_overlay()` 保证覆盖层存在；
4. **`unhook()` 先卸掉旧钩子** —— 钩子是按目标 pid 过滤的，换目标必须重装；
5. `SetWinEventHook(EVENT_OBJECT_DESTROY .. EVENT_OBJECT_LOCATIONCHANGE, pid, 0,
   WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)`；失败只 warn（还有轮询兜底）；
6. `TARGET.store(...)`、`reset_cache()`、`InvalidateRect`、`refresh()`。

#### `pub fn hide()`

卸钩子、`TARGET = 0`、清缓存、`ShowWindow(SW_HIDE)`、`SHOWN = false`。

#### `fn reset_cache()`

把 `LAST_X/Y/W/H` 和 `LAST_Z_TICK` 全部设成 `UNSET`，让下一次 `refresh` 强制应用一次几何。

#### `pub fn refresh()`

带重入保护的入口：`UPDATING.swap(true)` 成功才执行 `refresh_inner()`，结束后复位。

#### `pub fn tick()`

`is_active()` 时调 `refresh()`。由 `App::logic` 每帧调用，是「兜底跟随」的来源。

#### `unsafe fn refresh_inner()`（核心）

```text
1. TARGET / OVERLAY 任一为 0 → 直接返回
2. 目标窗口已销毁 → hide() 并返回
3. 目标不可见或最小化 → 隐藏覆盖层并返回（只在 SHOWN 为真时才真的调 ShowWindow）
4. visible_frame_rect(target) 取可见帧边界
5. 按 thickness 向外扩：(x, y, w, h)
6. 尺寸变了 → apply_region() 重做镂空区域 + 更新 LAST_W/H
7. 位置/尺寸变了、或还没显示 → SetWindowPos(HWND_TOPMOST, x, y, w, h,
   SWP_NOACTIVATE|SWP_SHOWWINDOW) 并更新缓存，然后 return
8. 几何没变 → 距上次「重申置顶」超过 1 秒时，用
   SetWindowPos(HWND_TOPMOST, 0,0,0,0, SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE) 把 z-order 顶回去
```

* **第 8 步是必需的**：目标窗口自己变成 TOPMOST 时会被排到置顶层最上方，
  我们的覆盖层就会排在它下面。定期重申一次保证红框始终压在窗口上面。
* **第 7 步的「几何没变就不动」很重要**：目标窗口静止时每秒最多只有一次系统调用，
  否则 30 Hz 的 `SetWindowPos` 会造成明显的 CPU 占用和闪烁。

#### `unsafe fn visible_frame_rect(target: HWND) -> Option<RECT>`

**这是红框能正确跟随的关键函数。**

* 先试 `DwmGetWindowAttribute(target, DWMWA_EXTENDED_FRAME_BOUNDS, ...)`，
  拿到的是**用户肉眼看到的**窗口边缘（物理像素）；
* 取不到（返回错误或矩形无效）时退回 `GetWindowRect`。

> **为什么不能用 `GetWindowRect`**：它对 DWM 合成窗口返回的是**含一圈不可见缩放边框**的外框，
> 比可见边缘大（普通窗口每边约 7~8 px）。对着它向外扩 3 px 画框，
> 红框实际落在目标窗口**自身非客户区的绘制范围内**；目标窗口一旦置顶排到我们上面，
> 红框就被它自己的边框画掉了 —— 表现出来就是「红框不见了 / 不跟随」。

#### `fn apply_region(overlay: HWND, w: i32, h: i32, th: i32)`

`CreateRectRgn(0,0,w,h)` + `CreateRectRgn(th,th,w-th,h-th)`，
`CombineRgn(RGN_DIFF)` 得到「回」字形区域，`SetWindowRgn` 应用。
**`SetWindowRgn` 成功后区域归系统所有，不要再自己删 outer**；只删 inner。

#### `fn set_color(rgb: [u8; 3])`

颜色没变且画刷存在时直接返回；否则删掉旧画刷、`CreateSolidBrush` 建新的。
（画刷只在 `WM_PAINT` 里用，删旧的是安全的。）

#### `fn unhook()`

`UnhookWinEvent` 并清零 `HOOK`。

#### `unsafe fn ensure_overlay() -> Result<HWND>`

覆盖层已存在且有效则直接返回。否则 `RegisterClassW`（已注册时返回 0，忽略）+
`CreateWindowExW`。**类背景刷设为 NULL**，背景完全由 `WM_PAINT` 自己画 ——
这样换色重建画刷时不会留下悬空句柄被 `DefWindowProc` 使用。

#### `unsafe extern "system" fn overlay_proc(...)`（覆盖层窗口过程）

| 消息 | 处理 |
|---|---|
| `WM_NCHITTEST` | 返回 `HTTRANSPARENT` —— 鼠标完全穿透，点红框等于点下面的窗口 |
| `WM_ERASEBKGND` | 返回 1（已处理，不擦背景，避免闪烁） |
| `WM_PAINT` | `BeginPaint` → `GetClientRect` → `FillRect(BRUSH)` → `EndPaint` |
| 其它 | `DefWindowProcW` |

#### `unsafe extern "system" fn win_event_proc(...)`（WinEvent 回调）

1. 只要 `id_object == OBJID_WINDOW(0)` 且 `id_child == CHILDID_SELF(0)` 的事件（窗口对象本身）；
2. 句柄必须等于 `TARGET`；
3. `EVENT_OBJECT_LOCATIONCHANGE` → `refresh()`；
   `EVENT_OBJECT_DESTROY` → `hide()` 并记日志。

> 这个回调在**主线程**执行（out-of-context 钩子的回调被投递到安装线程的消息队列），
> 所以它可以直接读写那些原子量，不需要额外同步。

## 2.13 `src/win/audio.rs` — 按进程静音（477 行）

### 设计目的

实现「把某个窗口的声音静音」。因为 **Windows 没有按窗口的静音接口** ——
音频会话是**进程级**的，所以实际上是「把该窗口所属进程静音」。

### 为什么单独开线程

* COM 用 **MTA**（`COINIT_MULTITHREADED`），避免与主线程可能存在的 STA
  （winit 为拖放做过 `OleInitialize`）互相干扰；
* 枚举设备/会话是一串 COM 调用，放后台不卡界面。

### 数据结构

#### `pub struct AudioSessionInfo`（一条音频会话）

| 字段 | 类型 | 说明 |
|---|---|---|
| `pid` | `u32` | 拥有该会话的进程 id |
| `exe` | `String` | 进程名，用于「按进程名回退匹配」 |
| `muted` | `bool` | 是否已静音 |
| `device_index` | `u32` | 属于第几个播放设备（从 0 开始，界面显示时 +1） |
| `state` | `i32` | `AudioSessionState` 原始值：0=Inactive，1=Active，2=Expired |

#### `pub enum MatchKind`

`None` / `Pid` / `Exe`，带 `label()` 返回中文说明。记录「这次是按什么匹配上的」，界面会显示。

#### `pub struct AudioSnapshot`（给界面读的只读快照）

| 字段 | 类型 | 说明 |
|---|---|---|
| `pid` / `exe` | `u32` / `String` | 当前关注的进程 |
| `matched` | `usize` | 匹配到的会话数 |
| `muted` | `Option<bool>` | 匹配到的会话是否**全部**静音；`None` = 没匹配到任何会话 |
| `matched_by` | `MatchKind` | 匹配方式 |
| `all` | `Vec<AudioSessionInfo>` | 系统上**所有**可见会话（界面里可展开查看，排查静音问题全靠它） |
| `error` | `Option<String>` | 最近一次失败原因 |
| `rev` | `u64` | **内容版本号**，只有内容真的变了才 +1（见下） |

* `pub fn summary(&self) -> String` —— 给界面的一行中文状态描述（区分「未选中」「未找到会话」「N 个会话已静音」等）。

#### `enum Cmd`（命令，模块私有）

`Focus { pid, exe }`（换目标窗口）/ `SetMute(bool)` / `Rescan`。

#### `pub struct AudioService`（可 Clone 的句柄）

| 字段 | 类型 | 说明 |
|---|---|---|
| `tx` | `Sender<Cmd>` | 向工作线程发命令 |
| `snap` | `Arc<Mutex<AudioSnapshot>>` | 与工作线程共享的快照 |

#### `struct Worker`（工作线程内部状态，模块私有）

| 字段 | 类型 | 说明 |
|---|---|---|
| `snap` | `Arc<Mutex<AudioSnapshot>>` | 回传用 |
| `pid` / `exe` | `u32` / `String` | 当前关注的进程 |
| `cache` | `Vec<AudioSessionInfo>` | 上次扫描结果 |
| `cached_at` | `Option<Instant>` | 上次扫描时间，用来判断缓存是否过期 |
| `name_cache` | `HashMap<u32, String>` | pid → 进程名缓存，避免反复 `OpenProcess` |

常量：`CACHE_TTL = 1500ms`（缓存多久算过期）、`IDLE_RESCAN = 2500ms`（后台自动刷新间隔）。

### 匹配策略（**这一段是本模块的核心，修静音问题必看**）

```text
1. 先按 PID 精确匹配：session.pid == target.pid   → MatchKind::Pid
2. 一个都没匹配到时，回退按进程名匹配：
   session.exe.eq_ignore_ascii_case(target.exe)   → MatchKind::Exe
3. 都没有 → MatchKind::None
```

> **第 2 条是必需的**：浏览器（Chrome/Edge）是多进程架构，**窗口所属进程**和
> **真正持有音频会话的进程**经常不是同一个 PID，但它们的 exe 名字相同。
> 只按 PID 匹配时这种情况会「找不到会话」，用户看到的就是「静音无效」。

### 函数

#### `impl AudioService`

| 函数 | 说明 |
|---|---|
| `pub fn spawn() -> Self` | 建 channel + 共享快照，`std::thread::Builder` 起名为 `wt-audio` 的线程跑 `worker()` |
| `pub fn focus(&self, pid: u32, exe: String)` | 发 `Cmd::Focus`（换目标窗口时由 `actions::set_target` 调用） |
| `pub fn set_mute(&self, mute: bool)` | 发 `Cmd::SetMute`，作用于当前 focus 的目标 |
| `pub fn rescan(&self)` | 发 `Cmd::Rescan`（界面上的「刷新音频信息」按钮） |
| `pub fn snapshot(&self) -> AudioSnapshot` | 克隆当前快照（`App::poll_audio` 按 `rev` 决定要不要用） |
| `pub fn is_muted(&self) -> Option<bool>` | 只取 `muted` 一个字段，避免为了判断方向做整表克隆 |

#### `impl Worker`

| 函数 | 说明 |
|---|---|
| `fn fill_names(&mut self)` | 给 `cache` 里 `exe` 为空的条目补进程名，优先用 `name_cache` |
| `fn scan(&mut self)` | 调 `collect_all()`；成功则整体换入 `cache` + `fill_names()` + 刷新 `cached_at` + 清错误；失败则清空缓存并记录错误 |
| `fn set_error(&self, msg: Option<String>)` | **只在错误文本变化时**更新快照并 `rev += 1` |
| `fn ensure_fresh(&mut self, force: bool)` | `force` 或缓存超过 `CACHE_TTL` 时重新 `scan()` |
| `fn matched_indices(&self) -> (Vec<usize>, MatchKind)` | 按上面的两级策略在 `cache` 里找匹配项，返回下标和匹配方式 |
| `fn publish(&self)` | 把 `pid`/`exe`/`matched`/`muted`/`matched_by`/`all` 写进快照。**先逐字段比较，全都没变就直接返回**（连克隆都不做），变了才写入并 `rev += 1` |
| `fn apply_mute(&mut self, mute: bool) -> Result<usize>` | 强制刷新一次 → 算匹配 → 调 `set_mute_match`。返回实际设置了几个会话 |

#### `fn worker(rx: Receiver<Cmd>, snap: Arc<Mutex<AudioSnapshot>>)`

工作线程主循环：

* `CoInitializeEx(None, COINIT_MULTITHREADED)`（**必须**，否则后面的 COM 调用会 `CO_E_NOTINITIALIZED`）；
* `recv_timeout(IDLE_RESCAN)`：
  * `Focus` —— pid/exe 有变化才重扫并 `publish()`；
  * `SetMute` —— `apply_mute`；返回 0 时记录「未找到该窗口进程的音频会话（可能它当前没有在发声）」，
    成功则清错误并重新扫描发布；
  * `Rescan` —— 强制重扫 + 发布；
  * 超时 —— 有目标时做一次轻量刷新（走缓存）并发布；
  * channel 断开 —— 退出循环；
* 退出时 `CoUninitialize()`。

#### `fn collect_all() -> Result<Vec<AudioSessionInfo>>`（freestanding）

```text
CoCreateInstance(MMDeviceEnumerator)
  → EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)   ← 遍历【所有】活动播放设备
    → 对每个设备 Activate::<IAudioSessionManager2>(CLSCTX_ALL)
      → GetSessionEnumerator → GetCount / GetSession(i)
        → GetState()，跳过 Expired
        → cast::<IAudioSessionControl2>() → GetProcessId()
        → cast::<ISimpleAudioVolume>() → GetMute()
```

* **为什么要遍历所有设备**：只看默认设备时，输出到别的声卡/耳机的应用根本枚举不到，
  表现同样是「静音无效」。
* 单个会话失败（会话过期、cast 失败）就 `continue`，不影响其它会话。
* `exe` 字段这里留空，由 `Worker::fill_names()` 补 —— 因为枚举阶段拿 PID 更快，
  而且进程名有缓存。

#### `unsafe fn set_mute_match(target_pid: u32, target_exe: &str, kind: MatchKind, mute: bool) -> Result<usize>`

**重新枚举一遍**（不复用缓存里的 COM 对象，避免用到已失效的会话），
按**传入的 `kind`（与界面显示完全一致的策略）**匹配，对命中的会话调
`vol.SetMute(mute, std::ptr::null())`，返回命中数量。

> **为什么强调「与界面显示一致的策略」**：如果显示用 PID 匹配、设置却同时按进程名匹配，
> 就会出现「界面说匹配了 3 个、实际静音了 8 个」这种难以理解的行为。

### 已知限制

* 只有进程**正在发声**时才存在音频会话，没在播放时枚举不到（界面会提示）；
* UWP / 商店应用的会话可能归属系统进程，暂不支持。

## 2.14 `src/win/sysmenu.rs` — 系统菜单注入与点击捕获（359 行）

### 设计目的

往**其它进程窗口**的标题栏右键（系统）菜单里追加自定义条目，并捕获点击。

### 关键技术事实（决定了这里为什么这么绕）

| 事实 | 结论 |
|---|---|
| `GetSystemMenu(hwnd, false)` + `AppendMenuW` 对**其它进程**的窗口是**有效**的（菜单对象由窗口管理器持有） | 「注入条目」不需要注入 DLL，可以做 |
| **跨进程子类化被系统禁止**：`SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ..)` 对别的进程窗口会失败 | 目标进程收到 `WM_SYSCOMMAND` 时我们**收不到通知** |
| `WH_MOUSE_LL` 低层鼠标钩子的回调**在本进程执行** | 用它 + `GetMenuItemRect` 命中测试来捕获点击 |
| 自绘标题栏的程序没有系统菜单（`GetSystemMenu` 返回 NULL） | 这类窗口**无法注入**，只能靠快捷键/托盘/界面 |

### 命令 id 约定

| 常量 | 值 | 含义 |
|---|---|---|
| `CMD_FRAME` | `0x8101` | 标记红框 |
| `CMD_TOPMOST` | `0x8102` | 置顶 / 取消置顶 |
| `CMD_MUTE` | `0x8103` | 静音 / 取消静音 |
| `CMD_RESIZE_EMPTY` | `0x8104` | 没有配置预设时的灰色占位项 |
| `CMD_PRESET_BASE` | `0x8200` | 预设区间的起点：`0x8200 + 预设下标` |

其它常量：`FIXED_CMDS`（前四个的数组）、`ARM_TIMEOUT_MS = 10_000`（菜单「待命中」状态的最长存活时间）。

### 静态状态

| 名称 | 类型 | 说明 |
|---|---|---|
| `HOOK` | `AtomicIsize` | 鼠标钩子句柄 |
| `ARMED` | `AtomicIsize` | 右键按下时记录的窗口句柄（接下来弹出的菜单属于它）；0 = 未待命 |
| `ARMED_AT` | `AtomicI32` | `ARMED` 的时间戳（`GetTickCount`），用于超时失效 |
| `STATE` | `OnceCell<Shared>` | 全局状态。钩子回调是 `extern "system"` 无法捕获环境，只能用静态量 |

### 菜单结构

```text
──────────────                       ← MF_SEPARATOR
WindowTools: 标记红框                 ← CMD_FRAME
WindowTools: 置顶 / 取消置顶          ← CMD_TOPMOST
WindowTools: 静音 / 取消静音          ← CMD_MUTE
WindowTools: 调整到指定分辨率  ▸      ← MF_POPUP，子菜单里是各预设（CMD_PRESET_BASE + i）
```

### 函数

#### 命令 id 工具

* `pub fn preset_cmd(index: usize) -> u32` —— `CMD_PRESET_BASE + index`
* `pub fn preset_index(cmd: u32) -> Option<usize>` —— 落在 `[BASE, BASE+0x100)` 时返回下标
* `fn is_fixed_cmd(id: u32) -> bool`
* `pub fn command_label(cmd: u32) -> String` —— 日志用的中文名

#### `pub fn has_system_menu(h: HWND) -> bool`

`GetSystemMenu(h, false)` 的返回值非空即真。
**自绘标题栏的程序返回 `false`**，`actions::set_target` 用它来给出友好提示而不是报错。

#### `pub fn inject(h: HWND, presets: &[ResolutionPreset]) -> Result<()>`

1. 取系统菜单，空则 `bail!("窗口没有系统菜单")`；
2. **先 `remove_injected` 清掉旧条目**（保证幂等，多次注入不会堆叠）；
3. 追加分隔符 + 三个固定项；
4. 预设为空 → 追加一个带 `MF_GRAYED` 的灰色占位项；
   否则 `CreatePopupMenu()` 建子菜单、逐条 `AppendMenuW`，
   再用 **`MF_POPUP`** 把子菜单挂到主菜单上（`MF_POPUP` 时 `uIDNewItem` 必须是子菜单句柄）。

#### `pub fn remove(h: HWND) -> Result<()>`

只调 `remove_injected`。

#### `fn to_wide(s: &str) -> Vec<u16>`

String → 带结尾 NUL 的 UTF-16，用于把中文预设名传进 `AppendMenuW`。

#### `unsafe fn menu_item_info(menu, pos, mask) -> Option<MENUITEMINFOW>`

`GetMenuItemInfoW` 的薄封装，失败返回 `None`。三个调用点都靠它少写样板。

#### `unsafe fn is_our_popup(sub: HMENU) -> bool`

判断一个子菜单是不是我们建的：遍历它的子项，只要有一个 id 落在预设区间就是。
（因为用 `MF_POPUP` 添加时，父项自己的 `wID` 是子菜单句柄值，没法直接识别。）

#### `unsafe fn remove_injected(menu: HMENU)`

遍历菜单项，收集「id 是固定命令」「id 是预设」「子菜单是我们的」三种位置，
再额外把紧挨着第一个我们条目**之前的分隔符**也加进来，
最后**按下标从大到小**逐个 `RemoveMenu(MF_BYPOSITION)`（从后往前删才不会打乱前面的下标）。

#### `pub fn install_command_watcher(shared: Shared) -> Result<()>`

把 `shared` 存进 `STATE`，然后 `SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0)`。
**必须在有消息循环的线程（主线程）安装。**

#### `pub fn uninstall_command_watcher()`

`UnhookWindowsHookEx`，在 `App::on_exit` 里调用。

#### `unsafe extern "system" fn mouse_proc(code, wparam, lparam) -> LRESULT`（核心）

```text
if code < 0 → 直接 CallNextHookEx（钩子约定：负值必须放行）

1. 超时检查：ARMED 非 0 且距今超过 ARM_TIMEOUT_MS → 清除 ARMED
   （避免用很久以前的菜单做命中测试）

2. WM_RBUTTONDOWN / WM_RBUTTONUP：
   取光标处的顶层窗口；若 is_eligible：
     - ARMED = 该窗口、ARMED_AT = now
     - 若【该窗口有系统菜单】且配置开启了注入 → 趁机 inject()
     （菜单马上就要弹出来了，此刻注入刚好来得及）

3. WM_LBUTTONDOWN：
   若 ARMED != 0 → hit_test(ARMED, 光标位置)
     - 命中 → 清除 ARMED + actions::dispatch_menu_command(cmd, 窗口, state)
     - 未命中 → 【不清除 ARMED】
       因为用户可能先点开子菜单、再点子菜单里的预设项，中间会有多次左键按下

4. CallNextHookEx(None, code, wparam, lparam)   ← 必须调用，否则会影响其它钩子
```

#### `fn top_level_at(pt: POINT) -> Option<HWND>`

`WindowFromPoint` + `GetAncestor(GA_ROOT)`。

#### `unsafe fn hit_test(h: HWND, pt: POINT) -> Option<u32>`

遍历系统菜单项：

* 是固定命令 → 用 `hit_at` 测这一项；
* 是子菜单且 `is_our_popup` → 遍历子菜单的每一项用 `hit_at` 测。
  对子菜单项同样用 `GetMenuItemRect(Some(h), 子菜单句柄, i, &rect)` 取矩形。

#### `unsafe fn hit_at(h, menu, pos, pt) -> Option<u32>`

取该项的 id 和屏幕矩形，光标落在矩形内就返回 id。
**只有菜单真正弹出时 `GetMenuItemRect` 才会返回有效的屏幕坐标** ——
这也是为什么未命中时不能急着清除 `ARMED` 的另一个原因。

#### `fn contains(rc: &RECT, pt: POINT) -> bool`

左闭右开的点-矩形判定。

### 注意与坑

* 这套「鼠标钩子 + 矩形命中」的方案是**在没有 DLL 注入的前提下能做的最优解**，
  但它依赖 `GetMenuItemRect` 在菜单显示期间返回有效坐标 —— 已尽量做容错，
  如果实测发现子菜单项不灵敏，备选方案是「右键时用自绘菜单替换系统菜单」
  （`TrackPopupMenuEx` + `TPM_RETURNCMD`，100% 可控但要自己重建标准菜单项）。

## 2.15 `src/win/privilege.rs` — 管理员权限（70 行）

### 设计目的

启动时自检是否已提权，并提供「以管理员身份重启」。

### 函数

#### `pub fn is_elevated() -> bool`

`OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)` 拿自身令牌，
再 `GetTokenInformation(token, TokenElevation, ...)` 读 `TOKEN_ELEVATION.TokenIsElevated`。
**用完 `CloseHandle(token)`**。任何一步失败都返回 `false`（保守判定为「未提权」）。

#### `pub fn current_exe_path() -> Option<PathBuf>`

`std::env::current_exe()` 的包装。

#### `pub fn restart_as_admin() -> Result<()>`

* 已经提权则 `bail!`；
* `ShellExecuteW(None, "runas", 自身路径, None, None, SW_SHOWNORMAL)`；
* **返回值大于 32 才算成功**（这是 `ShellExecute` 系列的历史约定，小于等于 32 是错误码），
  否则返回带「可能是你在 UAC 弹窗里点了否」提示的错误。

> ⚠️ **调用方需要先释放单实例互斥体**，见 `actions::restart_elevated` 的说明。

## 2.16 `src/win/single_instance.rs` — 单实例（107 行）

### 设计目的

防止重复启动。用两个标准 Win32 命名对象，不需要额外写进程通信代码。

| 对象 | 名称 | 作用 |
|---|---|---|
| 命名互斥体 | `Local\WindowToolsRust.SingleInstance` | 第一个实例创建并持有 |
| 命名事件 | `Local\WindowToolsRust.ShowWindow` | 后启动的实例用它通知已有实例唤出窗口 |

用 `Local\` 而不是 `Global\`：**每个登录会话一个实例**，多用户同时登录互不影响。

### 静态状态

`MUTEX` / `SHOW_EVENT`（都是 `AtomicIsize`）。
**为什么不放进 `AppState`**：句柄是裸指针、不满足 `Send + Sync`（见 [1.6](#16-句柄的存储约定)）。

### 函数

#### `pub fn acquire() -> bool`

`CreateMutexW(None, true, MUTEX_NAME)`：

* 成功且 `GetLastError() != ERROR_ALREADY_EXISTS` → 存下句柄，返回 `true`；
* 返回 `ERROR_ALREADY_EXISTS` → `CloseHandle` 后返回 `false`；
* **创建直接失败**（典型是 `ERROR_ACCESS_DENIED`：已有实例且它以更高权限运行）→ 记 warn 后
  也返回 `false`。宁可少开也不多开。

> `GetLastError()` 必须**紧接**在 `CreateMutexW` 之后读，中间不能插别的 API 调用。
> 这里代码就是紧挨着的。

#### `pub fn release()`

关闭并清零互斥体句柄。**提权重启前必须调用**（见下）。

#### `fn ensure_event() -> HANDLE`

懒创建本进程持有的通知事件（自动重置），存进 `SHOW_EVENT`。

#### `pub fn request_show()`

第二个实例调用：`CreateEventW` 打开同名事件（已由第一个实例创建）→ `SetEvent` → `CloseHandle`。

#### `pub fn take_show_request() -> bool`

`WaitForSingleObject(事件, 0) == WAIT_OBJECT_0`。
事件是**自动重置**的，读到一次就自动清零，不会重复触发。

### ⚠️ 必须注意的执行顺序

**提权重启前必须先 `release()`。** 原因：

`ShellExecuteW("runas")` 是**「新进程已经创建好了才返回」**的。如果那时旧进程还占着互斥体，
新起来的提权进程会把自己判成「第二个实例」，通知一下旧进程就退出了；旧进程收到通知后也在退出。

**结果就是：点了「以管理员身份重启」，程序反而彻底没了。**

正确顺序（实现在 `actions::restart_elevated`）：

```text
1. release()                      ← 先放掉互斥体
2. privilege::restart_as_admin()  ← 再提权启动
3. 提权失败 → 重新 acquire() 拿回互斥体，继续正常运行
```

## 2.17 `src/win/actions.rs` — 业务动作（363 行）

### 设计目的

**所有入口（界面按钮 / 托盘菜单 / 全局热键 / 系统菜单注入项）的公共实现。**
这一个文件决定了「同一件事在不同入口的行为必须一致」。

### 目标窗口的解析

这是本文件最需要理解的三个函数，它们的层次关系是：

```text
require_target(state)                     ← 最严格：必须有已拾取的目标
resolve_target(state, prefer_foreground)  ← 快捷键用：前台窗口优先，失败退回上面那个
sync_target_quiet(state, h)               ← 把某个窗口静默地设为当前目标
```

#### `fn require_target(state: &Shared) -> Result<(HWND, TargetWindow)>`

* 没拾取过 → `bail!("还没有选中目标窗口（用 Ctrl+Alt+P 或托盘菜单拾取）")`；
* 句柄已失效 → 清空 `state.target` 并 `bail!("目标窗口已关闭，请重新拾取")`。

#### `fn resolve_target(state: &Shared, prefer_foreground: bool) -> Result<(HWND, TargetWindow)>`

* `prefer_foreground = true`（快捷键默认）：
  取 `foreground_window()`，若 `is_eligible`（**会排除本程序自己的窗口**）则
  `sync_target_quiet` 后返回；
* 否则（前台窗口不可用，或调用方要求作用于已拾取目标）→ `require_target`。

> **为什么快捷键要用前台语义**：用户的心智模型是「我盯着哪个窗口就操作哪个」。
> 早期版本只作用于已拾取目标，导致「焦点切到 Firefox 按快捷键没反应」——
> 那不是句柄 bug，而是这里的目标选择逻辑。

#### `fn sync_target_quiet(state: &Shared, h: HWND) -> Result<TargetWindow>`

* 如果已经是当前目标**且窗口仍然有效** → 直接返回缓存的 `TargetWindow`，**不做任何系统调用**；
* 否则调 `set_target(state, h, "快捷键", false)`（不画红框）。

### 公开动作

| 函数 | 说明 |
|---|---|
| `pub fn set_target(state, h, source, want_frame) -> Result<TargetWindow>` | 见下 |
| `pub fn pick_under_cursor(state) -> Result<TargetWindow>` | 取光标下窗口 → `is_eligible` 检查 → `set_target(..., config.auto_frame_on_pick)` |
| `pub fn pick_foreground(state) -> Result<TargetWindow>` | 同上，目标是前台窗口 |
| `pub fn toggle_frame_for(state, h) -> Result<String>` | **对指定窗口**切换红框：若红框正在这个窗口上就关掉，否则开到这个窗口上（不会因为别的窗口有红框就把它收掉）。手动操作会清空 `frame_auto_for_topmost` |
| `pub fn toggle_frame(state, prefer_foreground) -> Result<String>` | 前台语义 → 转给 `toggle_frame_for`；否则「有红框就关，没有就开在当前目标上」 |
| `pub fn toggle_topmost(state, prefer_foreground) -> Result<String>` | 见下 |
| `pub fn toggle_mute(state, prefer_foreground) -> Result<String>` | 读 `audio.is_muted()` 决定方向 → `audio.set_mute(!current)` |
| `pub fn resize_to(state, w, h) -> Result<String>` | 读 `resize_client_area` / `restore_before_resize` → `window::resize`；不可调整大小时先记一条提示 |
| `pub fn resize_to_preset(state, index) -> Result<String>` | 取 `config.presets[index]` 后转给 `resize_to` |
| `pub fn restart_elevated(state) -> Result<()>` | **先 `single_instance::release()` 再提权**，失败时把互斥体拿回来（见 [2.16](#216-srcwinsingle_instancers--单实例107-行)） |
| `pub fn dispatch_menu_command(cmd: u32, h: HWND, state: &Shared)` | 系统菜单项被点击的入口，见下 |

#### `pub fn set_target(state: &Shared, h: HWND, source: &str, want_frame: bool) -> Result<TargetWindow>`

这是「选中窗口」的唯一实现，逻辑顺序：

1. 校验句柄、标题非空（空标题的窗口不作为目标）；
2. 一次性 clone 出 `inject` / `presets` / `rgb` / `thickness`（**避免持锁调用**）；
3. 目标变了 → 清理旧窗口的系统菜单注入（`sysmenu::remove`）；
4. 写入 `target`，**若窗口确实换了就清空 `frame_auto_for_topmost`**（那条记录失效了），记日志；
5. `inject` 开启时：
   * 窗口**没有系统菜单** → 记一条友好提示（自绘标题栏的程序，不是错误）；
   * 有 → `sysmenu::inject(h, &presets)`，成功则记 `menu_injected`；
6. `want_frame` 为真 → `frame::show(...)` 并置 `frame_on`；
7. `audio.focus(pid, exe)` —— 让音频线程开始关注这个进程。

> **`want_frame` 这个参数存在的原因**：早期版本一律按配置画红框，导致「从系统菜单点静音也冒出红框」。
> 现在只有**用户主动拾取/从列表选择**才画，系统菜单命令一律传 `false`。

#### `pub fn dispatch_menu_command(cmd: u32, h: HWND, state: &Shared)`

1. `set_target(state, h, "系统菜单: {命令名}", false)` —— **以被点击的那个窗口为对象**，
   并且**不画红框**；
2. 按 cmd 分派：`CMD_FRAME → toggle_frame_for`、`CMD_TOPMOST → toggle_topmost(state, false)`、
   `CMD_MUTE → toggle_mute(state, false)`、`CMD_RESIZE_EMPTY → 提示先配置预设`、
   `0x8200+i → resize_to_preset(state, i)`；
3. 结果写日志（前缀 `[系统菜单]`）。

---

# 3. 技术专题：为什么这么做

这一章记录那些「看起来可以更简单、但实际上只能这么写」的设计决策。
**改代码前先看这一章，能避免把已经踩过的坑再踩一遍。**

## 3.1 跨进程操作的限制（这是理解整个项目的钥匙）

Windows 对「操作别人的窗口」有明确的边界，整个项目的架构就是围绕这些边界长出来的。

### ✅ 能做（不需要注入 DLL）

| 能力 | API |
|---|---|
| 移动/缩放/置顶窗口 | `SetWindowPos` |
| 改别人的**系统菜单** | `GetSystemMenu` + `AppendMenuW` / `RemoveMenu`（菜单对象由窗口管理器持有，跨进程有效） |
| 读窗口信息 | `GetWindowTextW` / `GetClassNameW` / `GetWindowRect` / `DwmGetWindowAttribute` |
| 读进程信息 | `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW` |
| 监视窗口事件 | `SetWinEventHook(..., WINEVENT_OUTOFCONTEXT)` —— **回调在我们自己的进程执行** |
| 低层输入 | `WH_MOUSE_LL` / `WH_KEYBOARD_LL` —— **回调也在我们自己的进程执行** |
| 控制别人的音量 | Core Audio 会话（`IAudioSessionControl2` / `ISimpleAudioVolume`） |

### ❌ 不能做

| 想做的事 | 为什么不行 |
|---|---|
| 跨进程子类化（改别人的 `WndProc`） | `SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ..)` 对非本进程窗口会失败；`SetWindowSubclass` 也只支持同进程 |
| 直接拿到别人窗口的 `WM_SYSCOMMAND` 通知 | 上一条的直接后果 —— 所以我们**收不到**自定义菜单项的点击 |
| 用「全局非低层钩子」（如 `WH_CBT`） | 这类钩子的钩子过程必须位于 DLL 中并被注入所有进程 |
| 往**没有系统菜单**的窗口加菜单项 | `GetSystemMenu` 返回 NULL，根本没有菜单对象可以改 |

### 由此推出的三个核心设计

1. **红框** → 自己创建一个独立的覆盖层窗口（[2.12](#212-srcwinframers--红框覆盖层396-行)）；
2. **菜单点击捕获** → `WH_MOUSE_LL` + `GetMenuItemRect` 命中测试（[2.14](#214-srcwinsysmenurs--系统菜单注入与点击捕获359-行)）；
3. **自绘标题栏的程序** → 只能靠快捷键 / 托盘 / 界面按钮，这是系统限制不是 bug。

## 3.2 红框跟随的四个坑

「红框不跟随窗口」是最容易被报告的问题，它有四个彼此独立的成因，**每一个都需要单独处理**：

| # | 坑 | 症状 | 处理 |
|---|---|---|---|
| 1 | 用 `GetWindowRect` 当边界 | 红框落在目标窗口自身的非客户区绘制范围内，目标窗口一旦置顶排到上层，红框被它自己的边框画掉 → **看起来像红框不动了/消失了** | 改用 `DWMWA_EXTENDED_FRAME_BOUNDS`（可见帧边界）再向外扩 |
| 2 | z-order 被抢 | 目标窗口自己变成 TOPMOST 后排到置顶层最上方，压住覆盖层 | 几何变化时重新插入 `HWND_TOPMOST`；另加每秒一次兜底重申 |
| 3 | 进程是 DPI-unaware | 125%/150% 缩放或混合 DPI 多显示器下坐标被虚拟化缩放，红框整体偏移 | `main()` 第一行调 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` |
| 4 | 只靠 WinEvent 钩子 | 钩子漏事件时红框卡住不动 | 主线程每帧 `frame::tick()` 兜底轮询（有红框时 33 ms 一次） |

**排查顺序建议**：先看日志确认钩子是否装上了（`SetWinEventHook 失败` 会有 warn），
再确认 DPI 设置，最后才怀疑坐标来源。

## 3.3 静音为什么是「按进程」，以及两级匹配

* Windows **没有**「按窗口静音」的接口。音频会话（audio session）的粒度就是进程，
  一个进程的所有窗口共享同一条会话。所以「把某个窗口静音」在系统层面只能是「把它所属进程静音」。
* **两级匹配**（先 PID、失败再按进程名）是必需的：浏览器等多进程应用里，
  窗口进程和真正持有音频会话的进程经常不是同一个 PID，但 exe 名字相同。
* **遍历所有播放设备**也是必需的：只看默认设备时，输出到别的声卡/耳机的应用枚举不到会话，
  症状同样是「静音无效」。
* **设置时用与显示一致的匹配策略**：否则会出现「界面说匹配 3 个、实际静音 8 个」。

## 3.4 提权重启与单实例的顺序问题

这是一个典型的「两个功能各自都对、组合起来就出错」的例子：

```text
ShellExecuteW("runas") 的行为 = 新进程已经创建好了才返回
                     ↓
新进程启动时，旧进程仍然活着、仍然持有单实例互斥体
                     ↓
新进程把自己判成「第二个实例」→ 通知旧进程 → 自己退出
旧进程收到通知 → 也退出
                     ↓
结果：点了「以管理员身份重启」，程序彻底没了
```

**唯一正确的顺序**（`actions::restart_elevated`）：

```text
release()  →  restart_as_admin()  →  失败则 acquire() 拿回来
```

## 3.5 性能与内存

### 基线构成（约 90~100 MB，属于正常，不是泄漏）

| 组成 | 大致占用 |
|---|---|
| OpenGL 上下文 + 显卡驱动 | 30~50 MB |
| winit / egui / eframe 运行时 | 20~30 MB |
| **中文字体文件**（微软雅黑约 20 MB） | ~20 MB |
| 本程序自身的数据 | 少量 |

中文字体是**常驻**的，因为 egui 需要按需光栅化新字形（界面上会出现任意窗口标题）。
想降内存可以用配置项 `font_file` 指定更小的字体（如 `Deng.ttf` / `simhei.ttf`）。

### 已经消除的浪费（修改时注意别退回去）

| 浪费点 | 现在的做法 |
|---|---|
| 每帧克隆 200 条日志字符串 | `log_rev` 版本号 + `UiState::log_tail` 缓存 |
| 每帧克隆整个音频会话表 | `AudioSnapshot::rev` 版本号，内容没变连克隆都不做 |
| 每 800 ms 全量枚举音频设备/会话 | 焦点变化才扫 + 1.5 s 缓存 + 2.5 s 后台刷新 |
| 空闲时以 5 Hz 重绘整个界面 | 空闲 400 ms；有红框才 33 ms |
| 每秒 30 次 `SetWindowPos` | 几何变化检测（`LAST_X/Y/W/H`），没变就不调 |

### 两条通用原则

1. **不要在每帧路径上做「可能很大」的克隆** —— 用版本号 + 缓存，或者用 `Snapshot` 一次性取。
2. **不要在每帧路径上做系统调用**，除非它能被「变化检测」挡住（例如 `frame::refresh_inner` 的几何缓存）。

## 3.6 eframe / egui 0.36 的两个易踩点

1. **`App` trait 是 `logic()` + `ui()`，不是旧版的 `update()`。**
   从网上抄示例代码时极易在这里编译报错。`ui()` 收到的是一棵现成的 `Ui`（不需要自己建 `CentralPanel`）。
2. **主窗口隐藏时 `logic()` 仍会被调用**（eframe 会走「只跑逻辑不跑界面」的路径）。
   这正是「关闭到托盘后托盘和热键依然工作」的前提。
   如果你在 `logic()` 里依赖 `ui()` 的副作用，隐藏时就会行为不一致。

---

# 4. 常见修改指引（改动配方）

## 4.1 加一个「设置开关」

以「加一个 `foo_enabled` 开关」为例，需要改 **3 个文件**：

1. **`src/config.rs`**
   * 在 `Config` 里加 `pub foo_enabled: bool,`
   * 在 `impl Default for Config` 里给默认值
   * （不用管反序列化兼容 —— `#[serde(default)]` 已处理）
2. **`src/ui.rs`**
   * `UiState` 加同名字段
   * `UiState::new()` 里从 `cfg` 初始化
   * `UiState::apply_to()` 里写回 `cfg`
   * 在「设置」`Grid` 里加一行控件（`ui.checkbox(&mut s.foo_enabled, "")`）
3. **使用它的地方**（`state.rs` / `app.rs` / `win/actions.rs` …）

> ⚠️ 三处**都要改**，漏掉 `apply_to` 是最常见的错误 —— 表现是「勾了但没保存」。

## 4.2 加一个全局快捷键

`hotkey.rs` 里几个数组是**定长 `[_; 4]`**，加第 5 个要同步改 **4 处**，
漏一处就会编译报错（还好编译器会拦住）：

1. `src/config.rs`：加 `pub hotkey_xxx: String` + 默认值；
2. `src/hotkey.rs`：
   * `Hotkeys` 结构体加 `pub xxx: Option<HotKey>`；
   * `parse_all` 的数组加一项，返回值类型改成 `[_; 5]`；
   * `register_all` 的返回类型和 `Ok([...])` 加一项；
   * `build` 里的 `let [a, b, c, d] = ...` 解构改成 5 个；
   * `reapply` 里的 `old` 数组和 `Ok([...])` 解构加一项；
   * `describe` 加一项；
3. `src/app.rs`：`pump_hotkeys` 的 `ids` 数组加一项，并补一个 `else if` 分支；
4. `src/ui.rs`：`UiState` 字段 + `new` + `apply_to` + 快捷键 `Grid` 加一行输入框。

## 4.3 加一个「窗口操作」（例：设置窗口透明度）

1. **`src/win/window.rs`** 加底层实现，返回 `Result<...>`：
   例如用 `SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ...)` + `SetLayeredWindowAttributes`；
2. **`src/win/actions.rs`** 加一个 `pub fn set_opacity(state: &Shared, prefer_foreground: bool) -> Result<String>`，
   用 `resolve_target` 拿目标、执行、写日志、返回给界面看的字符串；
3. **接线**（按需要挑）：
   * 界面按钮 → `src/ui.rs` 的「窗口操作」那一行；
   * 托盘 → `src/tray.rs` 加 `MenuId` 常量 + 菜单项，`src/app.rs` 的 `pump_tray_menu` 加分支；
   * 热键 → 见 [4.2](#42-加一个全局快捷键)；
   * 系统菜单 → 见 [4.4](#44-给系统菜单加一条命令)。

> **原则：动作实现只写在 `actions.rs` 一处，各个入口只负责「解析目标 + 调用」。**

## 4.4 给系统菜单加一条命令

1. `src/win/sysmenu.rs`：
   * 加一个常量 `pub const CMD_XXX: u32 = 0x8105;`（**别和预设区间 `0x8200+` 撞**）；
   * `FIXED_CMDS` 数组加进去（注意数组长度也要改）；
   * `inject()` 里 `AppendMenuW` 追加一项；
   * `command_label()` 加一个分支；
2. `src/win/actions.rs` 的 `dispatch_menu_command` 里加一个 `match` 分支。

## 4.5 改红框的外观

* **颜色/粗细**：改 `Config` 的默认值即可；运行时由 `actions::toggle_frame_for` 传给 `frame::show`，
  再由 `set_color()` 重建画刷、`THICKNESS` 影响 `apply_region` 的镂空尺寸。
* **想改成虚线/渐变等复杂外观**：需要放弃「区域 + 纯色画刷」的方案，
  改用 `WS_EX_LAYERED` + `UpdateLayeredWindow` 做逐像素 alpha 绘制 ——
  改动集中在 `frame.rs` 的 `overlay_proc` / `ensure_overlay` / `apply_region`。

## 4.6 排错指南（症状 → 排查方向）

| 症状 | 先看哪里 |
|---|---|
| 快捷键没反应 | 界面日志有没有对应那一行？没有 → 热键没注册上（被占用，`App::new` 时会记日志）；有但提示「还没有选中目标窗口」→ 前台窗口不可操作且没拾取过目标 |
| 操作了错误的窗口 | `hotkey_foreground` 配置；托盘/界面按钮始终作用于已拾取目标，这是设计行为 |
| 红框不跟随 | 依次查：`SetWinEventHook` 是否 warn 失败 → DPI 是否是 PMv2 → 红框是否被目标窗口压住（[3.2](#32-红框跟随的四个坑)） |
| 红框位置整体偏移 | `enable_per_monitor_v2()` 是否在 `main()` 最开头 |
| 置顶无效 | 目标窗口是否以管理员权限运行（界面顶部权限提示） |
| 静音无效 | 展开界面里的「所有音频会话」面板：有没有目标进程的会话？`summary()` 显示的是「按 PID」还是「按进程名」还是「未找到」 |
| 标题栏右键没有我们的条目 | 该窗口是否**有**系统菜单（自绘标题栏的程序没有，日志会提示） |
| 菜单点击不生效 | `GetMenuItemRect` 需要菜单**正在显示**；子菜单项命中是最脆弱的一环 |
| 托盘图标不见了 | `App::new` 里的「托盘创建失败」日志 |
| 关窗后程序还在 | 这是 `close_to_tray` 的设计行为，用托盘「退出」结束 |
| 点提权重启后程序没了 | 检查 `restart_elevated` 是否仍然先 `release()`（[3.4](#34-提权重启与单实例的顺序问题)） |
| 内存偏高 | 换更小的 `font_file`（[3.5](#35-性能与内存)） |

---

# 5. 构建、验证与发布

## 5.1 本机编译（Windows）

```powershell
cargo build --release          # target\release\window-tools-rust.exe
cargo run                      # Debug：带控制台，能看 env_logger 输出
```

## 5.2 交叉编译（Linux → Windows）

本项目开发阶段是在 Linux 上用交叉编译产出 exe、再在 Windows 真机上验证的。

```bash
# 1. 安装工具链
rustup target add x86_64-pc-windows-gnu x86_64-pc-windows-msvc
apt-get install -y mingw-w64

# 2. cargo 全局配置（$CARGO_HOME/config.toml）
#    [target.x86_64-pc-windows-gnu]
#    linker = "x86_64-w64-mingw32-gcc"
#    ar     = "x86_64-w64-mingw32-ar"

# 3. 验证 + 构建
cargo check --target x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-msvc   # msvc 只能 check，Linux 上无法链接
cargo build --release --target x86_64-pc-windows-gnu
```

**产物自检**（确认是真正的 64 位 Windows 可执行文件）：

```bash
x86_64-w64-mingw32-objdump -p target/x86_64-pc-windows-gnu/release/window-tools-rust.exe | grep "DLL Name"
# 期望：只有系统 DLL（KERNEL32/user32/comctl32/dwmapi/ole32…），
#      不应出现 libgcc_s_*.dll / libwinpthread-1.dll / libstdc++-6.dll
```

PE 头应为 `MZ` + `PE\0\0`，machine = `0x8664`，subsystem = 2（GUI）/ 3（Console，Debug 构建）。

## 5.3 平台限制（重要）

**Linux 上无法运行和测试这个程序**，只能做编译期验证。任何涉及 Win32 行为的改动
都必须**在 Windows 真机上验证**，尤其：

* 系统菜单注入与点击命中；
* 红框跟随、置顶后的 z-order；
* 音频会话匹配；
* 单实例与提权重启。

## 5.4 提交约定

* **提交里不允许出现任何凭据**：token、密码、代理地址、个人邮箱等。
  不要用带 token 的 URL 作为 `git remote`，需要时临时在命令行里用完即弃，
  并在提交前 `grep` 一遍。
* 提交信息用中文，说明「改了什么 + 为什么」。
* 每次提交前跑一遍两个 target 的 `cargo check`，保持**零警告**。

## 5.5 v1.0.0 发布清单（尚未执行）

* [ ] 全部功能在 Windows 真机上回归通过
* [ ] `Cargo.toml` version 已是 `1.0.0`
* [ ] README / DEVELOPMENT 与代码一致
* [ ] 准备 exe 图标与版本资源（`.ico` + `build.rs`，目前缺失）
* [ ] 打 tag `v1.0.0` 并创建 Release（**需要明确指示后再做**）

---

# 6. 已知限制与后续方向

## 6.1 已知限制

| 限制 | 原因 | 可能的改进方向 |
|---|---|---|
| 静音是「按进程」而非「按窗口」 | Windows 没有按窗口的静音接口 | 进程回环捕获 `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`（Win10 2004+），复杂度高一个量级 |
| 进程没发声时枚举不到音频会话 | 没有播放就没有会话对象 | 同上；或维护一份「上次见过的会话」做提示 |
| UWP / 商店应用静音不支持 | 会话可能归属系统进程 | 需要按窗口句柄反查 AppContainer 进程 |
| 自绘标题栏窗口无法注入右键菜单 | 系统限制（无系统菜单对象） | 加一个「快捷键在光标处弹出自家菜单」，不依赖系统菜单 |
| 子菜单项点击命中可能不灵敏 | 依赖菜单显示期间 `GetMenuItemRect` 返回有效坐标 | 改为 `TrackPopupMenuEx` + `TPM_RETURNCMD` 自绘替换菜单 |
| 无 exe 图标 / 版本资源 | 未实现 | `.ico` + `build.rs`（GNU 目标用 `windres`，MSVC 用 `rc.exe`） |
| 无开机自启、无单实例外的多开管理 | 未实现 | 注册表 `Run` 项或任务计划 |

## 6.2 后续可以做的功能（参照 WindowTop）

* 窗口透明度 / 点击穿透（`WS_EX_LAYERED` + `SetLayeredWindowAttributes`）
* 窗口缩略图预览（`DwmRegisterThumbnail`）
* 智能暗色模式（`DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`）
* 窗口锚点 / 快速定位（把窗口吸附到屏幕某个区域）
* 记住「每个进程上次的尺寸/位置」

## 6.3 修改代码时的三条红线

1. **`unsafe` 只能出现在 `src/win/`。**
2. **不要在持锁时调用会加锁的函数**（`parking_lot::Mutex` 不可重入，会死锁）。
3. **动作实现只写在 `actions.rs` 一处**，入口只负责解析目标。

---
*文档版本：与 v1.0.0 源码同步。改动代码时请顺手更新对应章节。*
