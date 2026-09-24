# 技术文档 — `opss.customizable-floating-window` 实现详解

> A customizable floating window for MicYou status monitoring and control.
>
> 本文档记录：需求核对 → 可行性调研结论 → 架构决策 → 各模块实现 → 关键坑与解法
> → 资源利用设计 → 验证记录 → 打包发布。对应 MicYou 宿主 `master`
> （Host API v2，ABI v1）与 Slint **1.18.1**。

---

## 1. 需求与核对清单

| 需求 | 结论 | 实现位置 |
|---|---|---|
| Native 插件，Slint 1.18 启动时读取 `.slint` 绘制悬浮窗 | ✅ | helper 进程 `slint-interpreter 1.18` 运行时编译 |
| 用户可自定义 `.slint`；panel.html 切换可用 `.slint` | ✅ | `themes/` 目录 + 面板列表/编辑器 + 热替换 |
| 展示 `audio.state` 数据与本次连接时长 | ✅ | tick 拉取快照；`device_connected/disconnected` 事件 + streaming 回退 |
| 悬浮窗不占任务栏 | ✅（Wayland 除外，合成器策略） | helper 平台 FFI（§5.4） |
| 可互动：拖动 / 左键 / 右键菜单，**交互在 .slint 中定义** | ✅ | TouchArea/ContextMenuArea/WindowMoveArea + 回调契约（§6） |
| 可互动快速切静音的透明背景音量环（v1 样式） | ✅ | `themes/ring.slint`（v1 移植，§6.2）+ `control.intercept` |
| 三桌面平台 | ✅ | CI 三平台矩阵；helper 单进程模型解决 macOS 主线程约束（§3.1） |

## 2. 可行性调研（动手前的核对）

逐项在 Slint 1.18 源码/文档核实（仓库 `slint-ui/slint`，tag 区间 1.18.0/1.18.1）：

| 能力 | 结论 | 证据 |
|---|---|---|
| 透明无边框置顶窗 | ✅ | `Window` 属性 `background/no-frame/always-on-top`（官方 Window 参考） |
| 运行时编译用户 `.slint` | ✅ | `slint-interpreter::Compiler::build_from_source()`；`CompilationResult::components()`；`ComponentDefinition::{properties,callbacks,create}`；`ComponentInstance::{set_property,set_callback,invoke}` |
| 音量环圆弧（圆头线帽、动态扫过） | ✅（需 helper 供路径串） | `Path` 有 `commands/stroke/stroke-width/stroke-line-cap/viewbox-*`；Slint 表达式无 number→string，故弧路径由 Rust 侧生成写入字符串属性 |
| 左键单击切静音 / 点击≠拖动 | ✅ | `TouchArea.pointer-event/moved` + `pressed-x/y`、`mouse-x/y`；阈值逻辑写在 `.slint` |
| 拖动移窗 | ✅ 两条路 | 自定义：回调 → `slint::Window::set_position`；系统级：1.18 新增 `WindowMoveArea`（winit `drag_window()`） |
| 右键菜单 | ✅ | `ContextMenuArea + Menu/MenuItem`（原生菜单）与 `PopupWindow` 均可在解释器用（`item_registry` 注册了 ContextMenu/MenuItem，`popup.rs` 存在） |
| 不占任务栏 | ⚠️ Slint 无此属性 | 全源码无 `skip_taskbar`；`Window::window_handle()`（feature `raw-window-handle-06`）提供原生句柄 → 平台后处理（§5.4） |
| 解释器内交互元素 | ✅ | `internal/interpreter/item_registry.rs` 注册 TouchArea/WindowMoveArea/ContextMenu/MenuItem/Path/Timer 等 |
| 跨线程操作 UI | ✅ | `slint::Weak<T>: Send+Sync`（api.rs 1247/1250）+ `upgrade_in_event_loop` |
| macOS 主线程约束 | ⚠️ 架构级 | winit 事件循环必须主线程，而宿主 Tauri 占主线程 → **helper 子进程**（§3.1） |
| Slint 1.18 软件渲染器元素旋转 | ❌ 静默忽略 | `internal/renderers/software/lib.rs:3355 current_transform()` 仅平移 → 主题改用 Path 线段（§7.1） |

**结论：方案成立，无需中止。** 两个工程化前提（helper 子进程、任务栏平台 FFI）
均有干净解法，且交互逻辑仍 100% 由 `.slint` 定义。

## 3. 总体架构（round-5：双模 UI 运行时）

**Windows / Linux**：Slint 事件循环跑在**插件进程内自建 UI 线程**（回退到用户验证过的
“初版”架构）：宿主分发线程 ↔ UI 线程之间用两条无界通道传 `Cmd`/`Ev`；33ms UI 定时器
drain 通道（用户参考实现的模式），无需 `invoke_from_event_loop` 跳转。
**macOS**：winit/Slint 事件循环必须在进程主线程（被 Tauri 宿主占用）→ UI 跑在
`floating_helper` 子进程，stdin/stdout 承载同一套 `Cmd`/`Ev`（`uiruntime` crate 双端复用）。
zip 中 `bin/` 仅含 macOS helper；Windows/Linux 不需要 helper 二进制。

```text
host threads (init/tick/events) ──Cmd──▶ UI thread (Slint, in-process)   [win/linux]
                             ◀──Ev──
host threads ──stdin Cmd──▶ floating_helper (Slint on main thread)      [macOS]
              ◀──stdout Ev──
```

## 3A. 历史架构（round 1–4，已被取代）

```text
┌────────────── MicYou 宿主进程 (Tauri) ──────────────┐
│  Host API v2 (C ABI)                                │
│   audio_state / set_muted / set_monitoring /        │
│   get_config / set_config / set_interval / …        │
│        ▲                                            │
│        │ micyou_plugin_* (cdylib, 本仓库 plugin/)    │
│        │  · tick 状态机（interval:tick, 100ms）       │
│        │  · 会话计时 · 主题管理 · 重启退避             │
│        │  · 面板 ui:* 动作                            │
└────────┼───────────────────────┬────────────────────┘
         │                       │ stdin/stdout JSON lines (cfw-protocol)
         │                       ▼
│        │        ┌── helper 子进程 (本仓库 helper/) ──┐
│        │        │ main thread = Slint 事件循环        │
│        │        │  slint-interpreter 运行时编译主题   │
│        │        │  契约内省 → 属性/回调绑定            │
│        │        │  平台 FFI：任务栏/位置/屏幕/macos    │
│        │        └───────────────┬────────────────────┘
│        │                        ▼
│        │                 透明悬浮窗 (Slint 软件渲染)
```

### 3.1 为什么 helper 必须是子进程

1. **macOS 主线程铁律**：winit/Slint 事件循环只能跑在进程主线程；插件 cdylib 寄居
   的 Tauri 宿主主线程已被 tao 事件循环占用，进程内开第二 UI 线程在 macOS 直接不可行。
2. **宿主线程契约**：MicYou 文档明令 Host API 只允许在宿主分发线程调用，禁止插件自建
   线程调用。子进程 + 管道 IPC 把这条纪律变成**结构事实**而非自觉：helper 进程里根本
   没有 Host API 符号。
3. **崩溃隔离**：用户 `.slint` 运行时编译 + GPU/驱动渲染栈有任何故障，死的是 helper；
   插件核心按退避重启它，宿主无感（§4.4）。
4. **卸载安全**：cdylib `deinit` 后库被 unload，若 UI 线程还在跑本库代码即崩溃；
   子进程天然无此问题（deinit 只需 kill _child_）。

代价：约 30–60 MB 常驻 RSS（Slint 软件渲染进程）与一次管道跳转（≤ tick 周期）。
`visible=false` 时 helper 完全退出释放内存（隐藏=挂起语义）。

### 3.2 IPC 协议（`protocol/` crate，JSON lines）

plugin → helper（stdin）：`State{…}` / `Theme{path}` / `Visible{show}` /
`Pos{x,y}` / `PosDefault` / `Snapshot{path}` / `Quit`
helper → plugin（stdout）：`Ready{theme,reloaded}` / `ThemeError{message}` /
`Mute` / `Monitoring` / `Menu{action}` / `Hide` / `Moved{x,y}` / `Log{message}` / `Bye`

* serde `tag="cmd"/"ev"` + 忽略未知字段 → 新旧版本互容（前向兼容）。
* stdout 只走协议；诊断一律 stderr（插件侧保留尾 12 行用于崩溃事后日志）。
* 孤儿互防：宿主死 → helper stdin EOF 自退；helper 死 → 读线程 EOF → 插件退避重启。

## 4. 插件核心（`plugin/`，cdylib）

### 4.1 ABI 与版本

* `mpl_plugin_info_t`：`abi_version=1`、`api_version=2`（使用 v2 控制面字段
  `set_muted/get_muted/set_monitoring/get_monitoring`；宿主
  `MIN_SUPPORTED=1..=HOST=2`，v1 老宿主会以错误码 7 干净拒载，而不是越界读表）。
* host 表按值拷贝进 `HOST: Mutex<Option<mpl_host_api_t>>`（init 指针仅调用期有效）。
* v2 追加字段仍声明为 `Option<fn>` 并判空：个别槽位为空的宿主优雅降级。
* **不导出 `micyou_plugin_process`**：本插件 `kind=ui`，永不进实时音频线程。

### 4.2 能力（最小化）

`audio.state`（状态快照）、`control.observe`（get_muted/get_monitoring +
mute_changed 等事件）、`control.intercept`（set_muted/set_monitoring）、
`config.read/config.write`（主题选择/位置/状态镜像）。
不申请 fs.*：主题读写直接用 `plugin_dir()` + 标准 fs 且严格限定自身目录
（文件名白名单校验 `valid_theme_name`：长度/字符集/后缀/防穿越）。

### 4.3 tick 状态机（`interval:tick`，默认 100ms）

单一 interval（payload 标签 `cfw` + interval id 双重过滤，防宿主残留旧定时器串扰）。
每 tick 顺序：

1. `supervise_helper()`：死亡检测（`try_wait` 非阻塞）→ 退避重启（0.5s·2ⁿ，封顶 15s，
   连续 6 次放弃并通知）；健康 ≥60s 清零计数；
2. `handle_helper_events()`：drain 事件队列 →
   `Mute/Monitoring` → 控制面调用（用最近缓存态取反）；`Menu{reload-theme}`；
   `Hide` → 持久化 `visible=false`；`Moved` → 节流写 `windowX/Y`；
   `Ready` → 清 `themeError`、复位退避；`ThemeError` → 写配置 + 去重通知；
3. `audio_state()` → 会话 `observe_streaming`（回退推断）→ 组装 `StatePayload`
   → 推 helper stdin（写线程持有管道，tick 永不阻塞）→ 状态镜像（§4.5）；
4. 每 10 tick：配置轮询（theme/visible/updateMs/windowX/Y 漂移 → 热替换/启停/改周期）；
5. 每 50 tick：`themes/` 扫描 → `themeList` 变化才写配置。

所有 Host API 调用都发生在宿主分发线程（init/deinit/handle_event/handle_message）。
三条自有线程（stdin 写 / stdout 读 / stderr 尾）**只碰管道与队列**。

### 4.4 会话计时（“本次连接时长”）

* 权威源：`device_connected{mode,label}` / `device_disconnected` 事件
  （宿主 TCP 控制通道广播）；
* 回退源：插件在连接建立**之后**才启用时收不到事件 → 用 `audio_state.streaming`
  的 false→true 跳变开场；仅对“推断会话”生效的 30 tick 无串流宽限结束它
  （传输抖动不复零）；权威事件随时接管/升级 label。
* 展示：`session-seconds` + 预格式化 `session-text`（Slint 无 number→string）。

### 4.5 配置键与面板桥

| 键 | 写方 | 用途 |
|---|---|---|
| `theme` `visible` `updateMs` | 面板/宿主表单 | 用户意图（tick 轮询 + `ui:apply-theme` 即时） |
| `windowX/Y`（null=自动右上） | 插件（Moved 节流 1s + deinit 落盘） | 位置持久化 |
| `themeList` `themeError` `helperState` `status` | 插件（变化门控，≥1s） | 面板展示镜像 |
| `themeDraft` `themeDraftName` | 面板 | 编辑器保存/载入中转 |

面板桥动作（topic `ui:<action>`）：`apply-theme` `reload` `restart-helper`
`show` `hide` `reset-pos` `save-theme` `stage-theme` `restore-themes` `snapshot` `log`。

第二轮实测反馈的面板整改：flex 行改为 `flex-wrap` + 提示独立成行（修复按钮/提示被
挤成竖排）；新增「修改主题（推荐方式）」卡片：**优先建议用 AI/LLM 或直接编辑文件**
修改主题，并展示插件 init 时写入的 `themesDir` 配置键（主题目录绝对路径）；
在线编辑器降级标注为「仅快速微调」。

### 4.6 panic 与错误边界

所有 FFI 入口 `catch_unwind(AssertUnwindSafe)` → `MPL_ERR_RUNTIME`；
**release profile 永不 `panic=abort`**（cdylib 在宿主进程内，abort 会带走 MicYou）。

## 5. helper 子进程（`helper/`）

### 5.1 运行时主题编译与契约内省

`Compiler::build_from_source()`（pollster 驱动）→ 首个导出组件 →
`properties()/callbacks()` 收集成员名集合 → **只绑定存在的成员**
（`apply_state/apply_smooth/attach_callbacks` 逐一查表），因此任意子集主题皆可运行。
编译失败：初次加载回退内置 `ring.slint`（`include_str!` 保证总有窗）并报
`ThemeError`；热替换失败则**保留当前窗**只报错。

### 5.2 数据流与平滑

* `State` 命令 → 全量属性即时写入（bool 必须跟手）；
* 33ms UI Timer：EMA 平滑（k=0.3，ε 吸附）→ 仅当可见变化时写
  `smooth-level/level-percent/arc-path/bars-path`（30Hz，字符串重建开销 µs 级）；
  串流且未静音时推进波形相位 +6°/tick（v1 的旋转条动画移到 helper 侧，
  原因见 §7.1）；
* 1s UI Timer：位置轮询（`WindowMoveArea` 系统拖动/WM 移动的唯一感知途径；
  自定义拖动期间抑制；Wayland  dummy (0,0) 过滤）。

### 5.3 自定义拖动协议 v2（点击≠拖动，1:1 跟手）

**v1 协议的反馈环缺陷（实测：移动量≈光标一半 + 跳动）**：主题上报“鼠标相对窗口
坐标 Δ”，helper 用 `P = P0 + Δ` 落位。但窗口一旦移动，相对坐标本身会被窗口位移
抵消（`Δ_n = c_n − u_n`），两种自然解读分别退化为半步（增量式）或 `u = c/2`
（绝对式）——正是用户实测的“一半距离 + 跳动”。

**v2**：主题上报**窗内鼠标绝对坐标**（曾用于 E2E 验证逐像素精确）：
`drag-start(gx,gy)` = 按下时刻窗内坐标（抓取锚点 g）；
`drag-move(mx,my)` = 移动中当前窗内坐标。helper 每事件一步收敛：

```text
C = P_cur + m·scale        // 光标屏幕坐标 = 当前窗位 + 窗内坐标
P_new = C − g·scale        // 锚点不变式：光标始终落在抓取点
```

每事件绝对落位、无累积、无反馈环（E2E 实测逐像素一致）。

**v3（当前默认）**：helper 侧**全局光标跟踪**——彻底移除主题↔helper 的几何耦合：
`drag-start` 时记录 `P0`（窗位）与 `C0`（全局光标，平台 FFI：Win `GetCursorPos` /
X11 `XQueryPointer` / macOS `[NSEvent mouseLocation]`），8ms 定时器持续
`P = P0 + (C − C0)`。全局坐标系与窗口位移无关 → 反馈环**结构性消失**（系统拖动
同级 1:1 手感），且**不占用 WM 抓取**：客户端照常收到全部按钮事件，左键单击/右键
耳返/点击阈值逻辑完全不受影响（E2E：拖动后 `windowX/Y = 968/258` 逐像素精确，
随后单击正确触发 `set_muted`）。全局光标不可用（Wayland）时自动回退 v2 协议
（主题仍发 `drag-move`）。`drag-end` 在点击与拖动两种抬起路径都调用以停跟踪器。

**WindowMoveArea 与点击兼容性调研**：Slint 1.18 的 `WindowMoveArea` 语义即“原生
标题栏”（CHANGELOG：*start an interactive window move, like a native title bar*，
#613）——按下即发起系统拖动并吞掉该 press/release 对，与标题栏不产生 click 同构；
Slint issue 追踪中同类“手势吞点击”问题见 #13118（Flickable 吞 plain click，仍未
完全解决）。因此**没有**“WindowMoveArea 透传左右键”的官方方案；本插件的解法是
v3 全局光标跟踪（不需要 WindowMoveArea 即获系统级手感），pill 等需要系统拖动的
主题仍可用 WindowMoveArea（按钮置于其上层即可点击）。

点击≠拖动改由**移动事件计数**判定：完美跟手下窗内坐标几乎不变，无法再用位移阈值；
`moved` 回调 <2 次 = 单击（切静音），≥2 次 = 拖动（`drag-end` 上报位置持久化）。
xdotool 一次性 warp（无中间 motion）正确判为点击，分步 motion 正确判为拖动。

### 5.4 “不占任务栏”：只碰 winit 不管理的属性（第四轮回归后的最终设计）

窗口装饰/透明由 winit 在**创建期**依据 `.slint` Window item（`no-frame`、
`background`）落实（X11 实测 depth=32 ARGB 证明创建路径正确）；helper 的后处理
**只触碰 winit 内部管理之外的属性**（根因见 §7.8）：

* **Windows**：**用户验证配方**（UI 33ms 定时器内，HWND 缓存后一次性应用）：
  剥 `GWL_STYLE` 的 caption/sysmenu/min/max/thickframe；`GWL_EXSTYLE` 加
  `WS_EX_TOOLWINDOW|WS_EX_TOPMOST`、去 `WS_EX_APPWINDOW`；`SetWindowPos`
  (`FRAMECHANGED|NOMOVE|NOSIZE|NOACTIVATE`)；此后每 30 tick 仅刷新 TOPMOST。
  **绝不调用 `ShowWindow(SW_HIDE/SW_SHOW)`**（破坏 DWM 透明合成表面 + desync
  winit 可见性，见 §7.8）；透明完全依赖 Slint 原生 `background: transparent`。
* **X11**：`_NET_WM_STATE_SKIP_TASKBAR/_NET_WM_STATE_SKIP_PAGER`（WM 管理、
  持久）追加 + `_MOTIF_WM_HINTS` decorations=0（个别 CJK 壳 CSD 兜底）。
* **macOS**：进程级 `NSApplicationActivationPolicyAccessory`（启动时 + show 后
  10×500ms 重复断言，压过 winit 初始化 NSApp 时的 Regular 重置）→ 无 Dock 图标 /
  无 Cmd-Tab；**不动 NSWindow**（styleMask/opaque 等创建期属性交给 winit）。
* **Wayland**：无客户端协议 → no-op + 日志（合成器策略）。

### 5.5 默认位置与多屏

无保存位置时取主屏物理尺寸（Win `GetSystemMetrics` / macOS
`CGDisplayPixelsWide/High` / X11 `XDisplayWidth/Height`）− 窗口尺寸 − 24px 边距
→ 右上角（v1 同款）。保存位置为全局物理坐标，支持负坐标副屏
（`x<0 且 y<0` 保留为“自动”哨兵；面板「位置复位」写 null）。

### 5.6 快照与自测

`Snapshot{path}` → `Window::take_snapshot()` → png crate 写 RGBA PNG
（含 alpha）。`--selftest <prefix>` 无宿主合成三态快照；`--compile-check`
输出契约报告（CI 与主题作者工具）。

### 5.7 WDIS 语音转录接入（WhatdidIsay 广播）

协议（WhatdidIsay README）：宿主消息总线广播二进制帧
`b"WDIS" | start_ms i64 LE | end_ms i64 LE | UTF-8 text`，消费方仅校验 magic。
本插件在 `handle_message` 入口**先于 topic 路由**做 magic 检查（广播 topic 不固定），
解析后经 stdin 发 `Cmd::Wdis{text, hold_ms}` 给 helper（插件线程不做 UI），
helper 在 UI 线程写入可选契约成员 `wdis-text`(string) / `wdis-visible`(bool) 并起
`hold_ms` 单次定时器收回。主题侧动画：

* **ring**：圆下方伸出**宽 = 直径**的文本框（`box-h` animate 260ms 伸出/收回，
  `box-alpha` animate 420ms 渐变消失），窗口高度 `win + box-h` 向下生长（顶边锚定）；
* **pill**：底部下伸面板（`box-h` animate 240ms），`height: 56px + box-h`；
* 配置：`wdisEnabled`（默认开）、`wdisHoldMs`（默认 4000，1–30s），面板可调。

验证：mock-host `MOCK_WDIS` 注入真实二进制帧 → 插件日志收帧 → helper 落属性 →
快照 84×124 / 224×90（窗口确实长高）+ 合成预览图 `dist/shots/wdis-preview.png`。

### 5.8 内置主题升级策略（cfw-bundled 标记）

`themes/` 为用户可编辑目录，插件**绝不覆盖无标记文件**。内置主题首行带
`// cfw-bundled: <rev>` 标记：init 时“缺失→写入；有标记且与内置不同→升级覆盖；
无标记→视为用户财产保留”。用户若魔改内置主题需保留改动：删除标记行或另存新文件名
（面板「恢复内置主题」可随时重置）。该策略源于实测：升级后旧副本若无策略将永远
停留在旧契约（如缺 `wdis-visible`）。

## 6. 主题与 v1 移植

### 6.1 契约（全部可选；面板内含完整表）

属性：`input-level processed-level smooth-level level-percent muted streaming
monitoring sample-rate channels queued-ms session-seconds session-text
device-label device-mode info-text arc-path bars-path`
回调：`mute-toggle() monitoring-toggle() menu-action(string) hide-window()
drag-start() drag-move(float,float) drag-end()`

### 6.2 `ring.slint` ↔ v1 `FloatingMicWindow` 对照

| v1 (Compose Canvas) | ring.slint |
|---|---|
| 36dp 透明无边框置顶圆窗（undecorated/transparent/alwaysOnTop） | 84px `Window{no-frame;always-on-top;background:transparent}` |
| 底环 `drawCircle(alpha .15/.3)` | `Path` 双半弧整圆，alpha 随状态 |
| 电平弧 `drawArc(-90°, 360°·level, cap=Round)` | `Path{commands: arc-path; stroke-line-cap: round}`（helper 归一化 100×100 视图框，r=45） |
| 8 根旋转波形条（2s 相位） | `Path{commands: bars-path}`（helper 30Hz 相位，公式同 v1 `0.3+0.7·sin(…)`） |
| 中心辉光 ∝ level | 圆 `d=28px·lvl`，alpha ∝ lvl |
| 静音斜杠 | 静态对角 `Path "M 33 33 L 67 67"`（软件渲染器无旋转，§7.1） |
| 空闲淡环+中心小圆 | 同 |
| MouseAdapter：5px 阈值拖动/点击切静音 | TouchArea 移动事件计数 + drag v2 三回调（§5.3） |
| —（v1 无菜单） | **右键 = 切换耳返**（用户实测反馈定稿；ring 不再挂右键菜单） |
| — | 耳返开启时外圈绿色细环提示（`monitoring` 属性） |

### 6.3 `pill.slint` / `minimal.slint`

pill：224×56 半透明信息条——状态点（串流呼吸）、电平条（`animate width`）、
`session-text`+设备/格式文本、MIC/MON 按钮、`WindowMoveArea` 系统拖动（Wayland
推荐范式）、右键原生 `ContextMenuArea` 菜单。
minimal：34px 圆点最小契约示例（自定义主题起点模板），交互与 ring 一致
（左键静音/拖动移窗/右键耳返）。

## 7. 关键技术发现与决策记录

### 7.1 Slint 1.18 软件渲染器忽略元素变换 ⚠️

实测（dbg 快照像素级验证）：`transform-rotation/transform-scale` 在
`renderer-software` 下**静默无效**（`current_transform()` 仅平移）。
FemtoVG/Skia 支持变换，但引入 GL/Vulkan 依赖会牺牲“任何机器都能跑”
（无 GPU 的 VM/CI/老驱动）。**决策**：坚持软件渲染器；旋转/斜线几何一律
用 `Path` 线段表达——静态 `commands`（斜杠）或 helper 生成的
`arc-path/bars-path`（动态）。该限制写入面板契约与 README。

### 7.2 `HOST` Mutex 非可重入死锁（mock-host 抓出）

首版 `abi::read_host_string` 在 `with_host` 闭包内再次 `with_host` →
std Mutex 自锁，init 永久挂起。**mock-host E2E 第一轮即暴露**。
修复：先 `with_host(|h| *h)` 按值拷贝表（`mpl_host_api_t: Copy`）释放锁，
再调回调。该 bug 印证了“用真实 ABI 镜像做 E2E”的价值。

### 7.3 macOS 激活策略被 winit 覆盖

winit 建事件循环时把 NSApp 策略重置为 Regular → 单靠启动时设置无效；
show 后重试链内**再断言一次** Accessory（§5.4）。

### 7.4 残留定时器串扰

宿主 disable 插件后其 interval 线程可能仍投 tick（按插件 id 路由）。
双重过滤：payload 标签 `cfw` + `interval id == self.interval_id`，陈旧 tick 直接丢弃。

### 7.5 拖动反馈环（v1 协议半步 + 跳动）

见 §5.3：相对坐标增量在被移动窗口里自抵消，任何“base+Δ”式落位都半步/跳动。
v2 改窗内绝对坐标 + 锚点不变式后逐像素精确（E2E 968/258）。

### 7.6 Windows 任务栏按钮生命周期

任务栏按钮于窗口 show 时创建；事后改 `WS_EX_TOOLWINDOW` 只影响 Alt-Tab/样式，
**按钮不消失**，必须 hide/show 一次。启动期单次 SW_HIDE→SW_SHOWNOACTIVATE
（仅 exstyle 真变化时）为最小代价解。macOS 侧对应问题是 winit 在事件循环初始化时
重置激活策略 → 重复断言 Accessory。

### 7.7 Xvfb 无 WM 的系统拖动

`WindowMoveArea` 依赖 EWMH `_NET_WM_MOVERESIZE`，裸 Xvfb 无 WM → no-op。
E2E 交互断言因此放在 ring（自定义拖动，不依赖 WM）；pill 的 WindowMoveArea
在真实桌面正常。文档注明。

### 7.8 第四轮 Windows 回归根因：与 winit 窗口状态机对抗（经验复盘）

**症状**（用户实测）：热重载后悬浮窗先完整绘制一帧，随后只有互动过的组件重绘、
其余变灰；标题栏重现；任务栏按钮仍在。

**根因**：winit 在 Windows 上以内部 `WindowFlags` 为唯一事实来源，在可见性/装饰/
最小化等变化时通过 `WindowFlags::apply` **重算并覆写** `GWL_STYLE`/`GWL_EXSTYLE`
（含按可见性补 `WS_EX_APPWINDOW`）。第三轮的“修复”在创建后改这些位并做了
`ShowWindow(SW_HIDE/SW_SHOWNOACTIVATE)` 循环：
1. 绕开 winit 的 hide 使其内部可见性状态 desync → 停止整帧重绘，仅 damage 区域
   （互动组件）重绘 → **灰色残帧**；
2. 重新 show 触发 `apply` → 按 winit 内部状态重设装饰与 exstyle → **标题栏重现**、
   我们加的 `WS_EX_TOOLWINDOW` 被清掉 → **任务栏按钮回归**。
   （`DwmEnableBlurBehind` 重复调用与 `GWL_STYLE` 剥帽同样会被 apply 覆写。）

**经验/规则**：
* 创建后**禁止**修改 winit 拥有的窗口位（style/exstyle/可见性）；
* 需要 shell 行为（任务栏/Alt-Tab）时，选 winit 不管理的维度：**ownership**
  （`GWLP_HWNDPARENT`）、X11 的 WM 管理属性、macOS 的进程级激活策略；
* 任何“生效一次”的后处理都要问：winit 下一次 apply 会不会覆写它？

**验证**：回退+所有权方案后 X11 E2E 全绿（拖动 1:1、单击静音、右键耳返、
`_NET_WM_STATE_SKIP_TASKBAR` 在位、快照 84×84 正常、deinit 干净）。

### 7.9 第五轮回滚：helper 子进程 → 进程内 UI 线程（用户实测驱动）

**症状**（用户实测 round-4 构建）：悬浮窗不能互动；helper 隔一段时间崩溃。
**处置**（按用户指示）：悬浮窗的解释运行**回退到初版架构**——Slint 事件循环跑在
插件进程内自建线程（用户曾自证该架构在 Windows 不产生任务栏任务且可互动），并参考
其粘贴实现落实 Windows 窗口配方（去帽/TOOLWINDOW/TOPMOST/无 hide-show/周期刷新
TOPMOST）。跨进程 IPC、管道监督、owner-window 等 round 1–4 机制全部移除，失败面收敛
为单进程双线程 + 通道；macOS 因主线程铁律保留子进程（`uiruntime` 双端复用同一运行时）。
**经验**：跨进程 UI 带来的收益（崩溃隔离）不足以抵消其失败面（管道生命周期、监督重启、
平台 flag 跨进程时序）；单进程线程模型 + `catch_unwind` 边界已足够，且与宿主线程契约
兼容（Host API 仍只出现在宿主分发线程）。
**回归验证**：Xvfb E2E 全绿——拖动 1:1（968/258）、单击静音、右键耳返、WDIS 注入、
`_NET_WM_STATE_SKIP_TASKBAR` 在位、快照 84×84、deinit 干净；actionlint 零告警；
windows-gnu / aarch64-apple-darwin 交叉 check 零告警。

## 8. 资源利用设计

| 项 | 设计 |
|---|---|
| tick 频率 | 默认 100ms（面板可调 50–1000）；`audio_state` 为原子读+try_lock，µs 级 |
| 属性写 | 30Hz 但带 ε 门控（无可见变化不写）；bool/文本随 State 即时 |
| 配置写盘 | 状态镜像 ≥1s 且内容变化；位置 ≥1s 节流 + deinit 落盘；themeList 仅变化时 |
| 管道 | stdin 独立写线程（tick 不阻塞）；事件队列上限 64 溢出丢旧 |
| 内存 | helper 隐藏即退出；插件核心无堆增长点（队列/尾日志有界） |
| 崩溃 | 退避 0.5s→15s，6 次放弃+通知；健康 60s 复位 |
| 二进制 | helper：opt-level 2 + lto thin + strip；cdylib 不 abort（§4.6） |

## 9. 验证记录（本仓库可复跑）

1. `cargo test`：18 项通过——协议 round-trip/前向兼容、会话计时四态、
   弧几何（0/¼/½/¾/满/钳制/单调）、bars 八段、**三内置主题编译+实例化+契约断言**、
   坏主题回退、主题名白名单、helper 路径解析。
2. `--compile-check`：三主题契约报告（minimal 只实现子集 → 验证“全可选”设计）。
3. Xvfb `--selftest`：三主题×三态 PNG 快照像素级目检（`dist/shots/themes-sheet.png`）；
   `window flags applied: skip-taskbar-x11` 日志确认 X11 任务栏位写入。
4. **mock-host E2E**（真实 cdylib + 真实 helper + X11）：
   init→theme ready→device_connected→会话计时→`ui:apply-theme` 热替换 ring→pill→
   快照；**xdotool 真实点击**两次 → `*** CONTROL-PLANE set_muted(true/false) ***`；
   **真实拖动** → `windowX=858/windowY=358`（与几何推算一致）→ deinit 干净退出。
5. 交叉目标：`cargo check --target x86_64-pc-windows-gnu` 与
   `--target aarch64-apple-darwin`（CI 再做真构建）。

## 10. 打包与 CI（Focus-Capture 分发模型）

* **push（main/dev）**：仅构建 + 发布 Actions artifact
  `opss.customizable-floating-window-v<ver>-development`（**名称带版本号**；
  artifact zip 根即安装包，无嵌套 zip）；development 任务**不产生任何 release 资产**；
* **手动触发（workflow_dispatch）**：与 development **平级**的任务（均 `needs: build`）：
  bump 档位（none/patch/minor/major）→ 防重复发布检查 → 打包前回写版本化
  `downloadUrl` → 版本化主包 `opss.customizable-floating-window-v<ver>.zip` →
  tag `v<ver>` + Release（资产：版本化 zip + `plugin.json`，与 zip 内 manifest 同一份）；
* 矩阵三平台（按项目决定**不含 macOS x86_64**）：windows-latest(MSVC x64) /
  ubuntu-latest(x64) / macos-latest(arm64)；步骤：actionlint → test → build →
  主题 compile-check → 归一化（unix 去 `lib` 前缀）→ package；
* **workflow 形式化检查**：CI 内运行 `actionlint`（GitHub Actions 专用静态检查器）；
  第三轮 `${{ matrix.libname##*. }}`（bash 参数展开混入 Actions 表达式）即此类
  工具可抓的典型错误，已改为显式 `libext` 矩阵字段；
* 本地开发包：`scripts/package.sh`（PROFILE=sandbox 适用于 ≤1GB 内存容器；
  release profile 的 `codegen-units=1` 在大内存机器/CI 上构建）。

## 11. 许可证：GPL-3.0-only（Slint 许可证查证结论）

Slint 采用三选一许可（README/LICENSES）：① Royalty-free（面向**闭源**桌面/移动/Web
应用，条件含 AboutSlint 署名，且**禁止分发“暴露 Slint API”的应用**、禁止单独分发
Slint）；② **GPLv3**（面向开源应用）；③ 商业许可。

本项目情形：开源、随 MicYou（GPLv3+插件例外）生态分发、且**把 .slint 编写能力暴露
给终端用户**（主题契约 = 暴露 Slint 语言/API）→ Royalty-free 路径不适用/高风险；
免费且契合的路径即 **GPLv3**。故仓库许可由 Unlicense 改为 **GPL-3.0-only**
（LICENSE 全文、manifest `license` 字段、workspace Cargo `license`），并与
MicYou 市场“GPL 兼容”准入一致。Slint © SixtyFPS GmbH，经 GPLv3 路径使用。

## 11. 源码地图

```text
protocol/src/lib.rs        Cmd/Ev + 行编解码（双端共用，防漂移）
plugin/src/abi.rs          v2 host 表镜像 + 缓冲契约包装（值拷贝/非可重入修复）
plugin/src/session.rs      连接计时（事件权威 + streaming 回退 + 宽限）
plugin/src/helper.rs       子进程生命周期（三管道线程/退避/kill 宽限）
plugin/src/lib.rs          FFI 入口 + tick 状态机 + 面板动作 + 主题目录管理
helper/src/main.rs         事件循环编排/热替换/拖动/轮询/快照/selftest
helper/src/theme.rs        运行时编译 + 契约内省绑定 + 回退
helper/src/arc.rs          弧/波形条 SVG path 生成（含单元测试）
helper/src/platform.rs     任务栏 FFI/屏幕尺寸/macOS 策略/Wayland 探测
helper/src/ipc.rs          stdout 协议出口 + panic hook
themes/*.slint             三内置主题（契约示例 + v1 移植）
panel.html                 设置面板（桥 API：get_config/set_config/trigger/locale）
tools/mock-host/           宿主 ABI 镜像 E2E 台（含 interval 泵/事件时间线）
```
