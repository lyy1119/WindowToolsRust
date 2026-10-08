# WindowToolsRust

使用 Rust 编写的、类似 [WindowTop](https://github.com/BiNGdK/WindowTop) 的 Windows 窗口增强工具。

当前处于**框架阶段**：功能骨架已全部打通、可编译、可运行，细节待真机验证后打磨。

---

## 已实现的功能

| 功能 | 状态 | 说明 |
|---|---|---|
| 拾取窗口 | ✅ | `Ctrl+Alt+P` 拾取光标下的窗口，或从窗口列表选择 |
| 快捷键作用于前台窗口 | ✅ | 快捷键默认直接操作**当前活动窗口**，无需先拾取 |
| 给窗口加红框 | ✅ | 置顶镂空覆盖层 + `SetWinEventHook` + 轮询兜底跟随 |
| 窗口置顶 | ✅ | `SetWindowPos(HWND_TOPMOST)` |
| 调整为指定分辨率 | ✅ | 预设可在界面里增删改；支持「客户区/外框」两种口径 |
| 静音 / 取消静音 | ⚠️ 待复测 | 走 WASAPI 音频会话，**按进程**静音；PID 匹配不上时回退按进程名匹配 |
| 注入标题栏右键菜单 | ⚠️ 待验证 | `GetSystemMenu + AppendMenuW` 注入，含分辨率预设子菜单 |
| 托盘图标 + 托盘菜单 | ✅ | 用 `tray-icon`，双击托盘唤出窗口；**关窗口默认只隐藏到托盘，进程常驻** |
| 全局快捷键 | ✅ | `global-hotkey`，**留空 = 不注册该快捷键** |
| 中文字体 | ✅ | 自动加载系统 CJK 字体，避免界面方框字（可在配置里指定字体文件） |
| 管理员权限自检 | ✅ | 启动时检查，未提权会在界面顶部提示并给「以管理员身份重启」按钮 |
| 防止重复启动 | ✅ | 命名互斥体；重复启动只会唤出已有实例的窗口然后自己退出 |
| 界面滚动 | ✅ | 整个界面套竖向 `ScrollArea`，窗口再矮也能滚到全部设置 |
| 配置持久化 | ✅ | `%USERPROFILE%\WindowToolsSetting\config.json` |

### 管理员权限

启动时会用 `OpenProcessToken` + `GetTokenInformation(TokenElevation)` 自检一次。

为什么重要：如果要操作的目标窗口属于**以管理员身份运行的进程**（任务管理器、各种
带 UAC 提权的工具），而本程序只是普通权限，那么 `SetWindowPos`、`GetSystemMenu`、
`OpenProcess` 等调用都会因为「完整性级别不够」而失败 —— 表现就是「某些窗口怎么点
都没反应」。未提权时界面顶部会给出提示和「以管理员身份重启」按钮
（走 `ShellExecuteW("runas")`，会弹 UAC）。

### 防止重复启动（单实例）

用两个标准 Win32 命名对象实现，不需要额外的进程通信代码：

* **命名互斥体** `Local\WindowToolsRust.SingleInstance` —— 第一个实例创建并持有；
* **命名事件** `Local\WindowToolsRust.ShowWindow` —— 后来的实例用它通知第一个实例
  「把主窗口显示出来」，然后自己直接退出。

用 `Local\` 而不是 `Global\`：这样是「每个登录会话一个实例」，多用户同时登录互不影响。
可以在设置里关掉（`single_instance`）。

> ⚠️ 实现上有个容易踩的坑：**提权重启前必须先释放互斥体**。
> `ShellExecuteW("runas")` 是「新进程已经创建好了才返回」的，
> 如果那时旧进程还占着互斥体，新起的提权进程会把自己当成第二个实例而立刻退出 ——
> 表现就是「点了以管理员身份重启，结果程序反而彻底没了」。
> 所以 `actions::restart_elevated` 里先 `release()` 再提权，提权失败还会把互斥体拿回来。

### 性能与内存

`eframe/egui` + OpenGL 上下文 + 一个 20 MB 左右的中文字体文件，
**空载 90~100 MB 属于正常基线，不是泄漏**。内存在这区间小幅波动是分配器
（Windows 堆 / arena）的正常行为。

在上一版基础上清掉了这些真实浪费：

| 浪费点 | 处理 |
|---|---|
| 每帧克隆 200 条日志字符串 | 加日志版本号 `log_rev`，只在变化时重建尾部缓存 |
| 每帧克隆整个音频会话表 | 音频快照加内容版本号 `rev`，内容没变连克隆都不做 |
| 每 800 ms 触发一次全量音频设备/会话枚举 | 改为焦点变化才枚举 + 1.5 s 结果缓存 + 2.5 s 后台刷新 |
| 无红框时仍以 5 Hz 重绘整个界面 | 空闲刷新率降到 2.5 Hz；有红框时仍是 30 Hz（跟随需要） |
| 中文 CJK 字体约 20 MB 常驻 | 可在配置里用 `font_file` 指定更小的字体 |

`font_file` 留空时按优先级自动挑选：微软雅黑 → 等线 → 黑体 → 宋体 → 微软正黑。
想进一步降内存，可以填一个体积更小的字体文件名（例如 `Deng.ttf`），重启生效。

### 两个自动标记红框的开关

| 开关 | 默认 | 行为 |
|---|---|---|
| 拾取时自动标记红框 | 开 | `Ctrl+Alt+P` 拾取到窗口后顺手画上红框 |
| **置顶时自动标记红框** | 开 | 把窗口置顶时自动画红框（一眼看出它被钉在最上面）；**取消置顶时会自动移除这个自动加上的红框** |

注意区分「自动加的红框」和「手动标的红框」：只有前者会在取消置顶时被自动移除，
手动标的红框不会被误删。

## 技术选型

* **Windows 原生能力** → [`windows`](https://crates.io/crates/windows)（windows-rs 0.62）
* **界面** → [`eframe`](https://crates.io/crates/eframe) / egui 0.36（纯 Rust，不写 Win32 界面代码）
* **托盘** → [`tray-icon`](https://crates.io/crates/tray-icon) 0.26
* **全局热键** → [`global-hotkey`](https://crates.io/crates/global-hotkey) 0.8

**所有 `unsafe` 与裸句柄都集中在 `src/win/` 目录**，其余模块（界面、托盘、热键、状态）完全不碰 Win32。

## 目录结构

```
src/
├── main.rs            入口：DPI 感知 / 日志 / 配置 / 钩子，然后进 eframe
├── app.rs             eframe App：串联托盘、热键、界面、红框轮询
├── ui.rs              egui 界面（含分辨率预设编辑器）
├── tray.rs            托盘图标与托盘菜单
├── hotkey.rs          全局快捷键
├── fonts.rs           加载系统 CJK 字体（否则中文是方框）
├── config.rs          配置读写 (serde_json)
├── state.rs           全局共享状态 AppState
└── win/               仅此目录允许 unsafe
    ├── mod.rs         句柄转换辅助
    ├── dpi.rs         Per-Monitor-V2 DPI 感知
    ├── window.rs      窗口枚举 / 信息 / 置顶 / 调整大小
    ├── frame.rs       红框覆盖层（镂空置顶窗口 + 事件跟随 + 轮询兜底）
    ├── audio.rs       按进程静音（WASAPI 音频会话，独立 MTA 线程）
    ├── sysmenu.rs     系统菜单注入（含预设子菜单）+ 点击捕获
    └── actions.rs     业务动作（GUI / 托盘 / 热键 / 系统菜单 的公共入口）
```

## 快捷键

默认值如下，**全部可以在界面里改**（「全局快捷键」一节，改完点「应用快捷键」）：

| 快捷键 | 作用 |
|---|---|
| `Ctrl+Alt+P` | 拾取光标下的窗口作为目标 |
| `Ctrl+Alt+F` | 红框开 / 关 |
| `Ctrl+Alt+T` | 置顶 / 取消置顶 |
| `Ctrl+Alt+M` | 静音 / 取消静音 |

### 快捷键作用于哪个窗口

默认**作用于当前前台窗口**（和 WindowTop 一致）：切到目标窗口直接按 `Ctrl+Alt+T`
就能置顶，不需要先拾取。可以在设置里改成「只作用于界面里拾取的目标窗口」。

当当前前台窗口不可操作（比如焦点在本程序自己的界面上）时，会自动退回已拾取的目标。

托盘菜单和界面按钮**始终作用于已拾取的目标**，因为打开托盘菜单时前台窗口已经变成
托盘宿主窗口了。

### 自绘标题栏的程序（Firefox、PotPlayer 皮肤模式等）

这类程序**没有 Windows 系统菜单**（`GetSystemMenu` 返回 NULL），所以：

* 往标题栏右键菜单里注入条目是**做不到的** —— 这是系统层面的限制，
  不是本程序的 bug，任何工具都绕不过去（除非注入 DLL）；
* 但**快捷键、托盘菜单、界面按钮都照常可用**，因为置顶 (`SetWindowPos`)、
  红框（独立覆盖层）、静音（音频会话）都不依赖系统菜单。

程序会在选中这类窗口时给出提示：「该窗口没有系统菜单……请直接用快捷键或界面按钮操作」。

判定「可操作窗口」时**刻意不要求有标题栏**：早期版本要求 `WS_CAPTION`，导致
PotPlayer 皮肤模式这类自绘标题栏的窗口被直接判为「不支持操作」，拾取和快捷键都用不了。

### 快捷键格式

修饰键在前、主键在最后，例如 `Ctrl+Alt+T`；修饰键可用 `Ctrl`/`Control`、
`Alt`、`Shift`、`Super`（Win 键），主键可直接写 `T` 或写成 `KeyT`。
**留空表示不注册该快捷键**（区别于「解析失败」，留空是合法状态，也不会参与重复检查）。

重新注册时会先注销旧的、注册新的；如果新快捷键被别的程序占用，会**自动回滚**到旧快捷键，
不会出现「改坏了就一个都用不了」。

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
* 界面线程每帧再调用一次 `frame::tick()` 做**兜底轮询**，即使钩子漏事件也不会卡住；
* 目标最小化/隐藏时红框自动隐藏，目标销毁时红框自动关闭。

#### 「目标窗口置顶后红框跟不上」的两个根因

1. **坐标来源错了。** `GetWindowRect` 对 DWM 合成窗口返回的是**含一圈不可见缩放边框**的
   外框，比肉眼看到的窗口边缘大。对着它向外扩 3px 画框，红框实际落在目标窗口
   **自身非客户区的绘制范围内**；目标窗口一旦置顶排到我们上面，红框就被它自己的
   边框画掉了（表现出来就是红框不动了 / 消失了）。
   → 改用 `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` 取**可见帧边界**，
   取不到时退回 `GetWindowRect`。
2. **z-order 被抢。** 目标窗口自己变成 TOPMOST 后会被排到置顶层最上方。
   → 几何变化时用 `SWP_...|HWND_TOPMOST` 重新插入，另外每 1 秒兜底重申一次置顶。

#### DPI

必须在创建任何窗口之前调用 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`。
否则进程是 DPI-unaware，Windows 会虚拟化缩放 `GetWindowRect` / `SetWindowPos` 的坐标，
在 125% / 150% 缩放或混合 DPI 多显示器下红框位置会整体算错。

### 2. 系统菜单：能做与不能做

* ✅ **能给其它进程的窗口加菜单项**：`GetSystemMenu(hwnd, false)` + `AppendMenuW` 对别的进程窗口有效，
  菜单对象由窗口管理器持有。
* ❌ **不能跨进程子类化**：`SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ..)` 对其它进程窗口会失败，
  所以目标进程收到 `WM_SYSCOMMAND` 时我们**收不到通知**。
* ✅ 因此点击捕获改用 **`WH_MOUSE_LL` 低层鼠标钩子**（回调同样在本进程执行）：
  右键按下时记录窗口并注入条目 → 左键按下时用 `GetMenuItemRect` 做命中测试 → 命中我们的条目就执行动作。
* 菜单结构（`▸` 是子菜单）：

  ```text
  ──────────────────────────────
  WindowTools: 标记红框
  WindowTools: 置顶 / 取消置顶
  WindowTools: 静音 / 取消静音
  WindowTools: 调整到指定分辨率 ▸ ┌ 1920 x 1080（1080P）┐
                                  │ 1280 x 720（720P）  │
                                  └ …                   ┘
  ```

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

- [ ] 静音待复测：已改为「遍历所有播放设备 + 先按 PID、失败再按进程名匹配」，
      且界面里新增了「查看系统上的所有音频会话」面板，若仍无效请把该面板内容发出来
- [ ] 系统菜单点击捕获、尤其是**子菜单**里的预设项命中测试，需真机验证
- [ ] 自绘标题栏的窗口（Firefox / PotPlayer 皮肤模式）无法注入右键菜单，
      这是系统限制；如果希望这类窗口也有「右键出菜单」的体验，
      可以后续加一个「用快捷键在光标处弹出我们自己的操作菜单」
- [ ] 红框在跨虚拟桌面、DPI 缩放变化时的表现需验证
- [ ] 静音对 UWP 应用、以及「未发声的进程」尚不支持
- [ ] 还没有 exe 图标与版本资源（需要 `.ico` + `build.rs`）
- [ ] 未做开机自启、单实例检测
- [ ] 目标窗口被管理员权限进程占用时需要提权提示

## 关于 WindowTop 的实现方式

`WindowTop/WindowTop-App` 这个仓库**是闭源的**，里面只有 README 和 LICENSE，
没有源码（1534 stars，只发布编译好的安装包）。所以无法直接对照它的实现。

从公开信息和技术常识推断：WindowTop 是 .NET/WPF 程序，它的「置顶」就是
`SetWindowPos(HWND_TOPMOST, …)`；它的「边框」属于同一类做法 —— 一个透明的、
置顶的、鼠标穿透的独立窗口盖在目标窗口外面，**并不是**什么「往目标窗口里注入」的原生技巧
（跨进程子类化被系统禁止，任何工具都绕不过去，除非注入 DLL）。

真正决定成败的不是用不用原生 API，而是这几个细节：

1. 用 `DWMWA_EXTENDED_FRAME_BOUNDS` 而不是 `GetWindowRect` 取边界；
2. 覆盖层与目标窗口都在置顶层时的 z-order 维护；
3. 进程的 DPI 感知级别；
4. 跟随用「事件钩子 + 定时轮询」双保险。

上面四条本项目都已处理（见「关键设计说明」）。
