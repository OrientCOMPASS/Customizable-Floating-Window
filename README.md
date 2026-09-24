# 🪟 可自定义悬浮窗 — Customizable Floating Window for MicYou

> A customizable floating window for MicYou status monitoring and control.

用 **Slint 1.18** 运行时编译用户自定义的 `.slint` 主题文件，绘制一枚透明、无边框、
置顶、**不占任务栏**的悬浮窗：实时展示 `audio.state` 数据与**本次连接时长**，
左键单击切静音、拖动移窗、右键菜单——全部交互在 `.slint` 主题内定义。

| | |
|---|---|
| 插件 id | `opss.customizable-floating-window` |
| 运行时 | Native（cdylib，C ABI v1 / Host API v2）+ 独立 Slint helper 子进程 |
| 平台 | Windows 10+ (x86_64) · Linux X11/Wayland (x86_64) · macOS 11+ (arm64/x86_64) |
| 插件类型 | `ui`（不进 DSP 链，不碰音频数据） |
| 权限 | `audio.state` `control.observe` `control.intercept` `config.read` `config.write`（最小化） |
| 许可 | GPL-3.0-only（Slint 经 GPLv3 路径使用；Slint © SixtyFPS GmbH） |

![themes](dist/shots/themes-sheet.png)
*三个内置主题 × 三种状态（串流 / 静音 / 空闲），透明背景合成于深色桌面预览。*

## 安装

1. 下载 Release 的 `plugin.zip`（单包含三平台产物，宿主按平台自选）；
2. 解压到插件目录的子目录 `opss.customizable-floating-window/`：
   Windows `%APPDATA%\micyou\plugins\`，Linux/macOS `~/.config/micyou/plugins\`；
3. MicYou「设置 → 插件」点「刷新」，启用「可自定义悬浮窗」。

目录结构：

```text
opss.customizable-floating-window/
├── plugin.json                     # 清单
├── customizable_floating_window.{dll,so,dylib}   # 插件核心（宿主按平台补后缀）
├── panel.html                      # 设置面板（主题切换/编辑器/状态）
├── bin/
│   ├── floating-helper-windows-x86_64.exe
│   ├── floating-helper-linux-x86_64
│   └── floating-helper-macos-aarch64             # Slint UI 子进程
└── themes/
    ├── ring.slint                  # 默认：复刻 MicYou v1 音量环
    ├── pill.slint                  # 信息条：电平条+时长+按钮
    └── minimal.slint               # 最小示例（自定义主题模板）
```

## 使用

* **左键单击**悬浮窗 = 切换宿主静音（移动事件 <2 次判为点击）；
* **左键拖动** = 移动窗口（drag v2 协议，1:1 跟手；位置自动持久化，重启恢复）；
* **右键**（ring/minimal）= 切换耳返；pill 为右键上下文菜单；
* 耳返开启时 ring/minimal 外圈显示绿色细环；
* **WDIS 语音转录**：安装并启用 WhatdidIsay 插件后，悬浮窗会短暂展示识别文本
  （ring：圆下方直径宽文本框渐变消失；pill：底部下伸面板收回）；
  面板「选项」可开关与调整保持时长；
* **设置面板**（设置 → 插件 → 「悬浮窗设置」）：
  * 实时状态（串流/电平/静音/耳返/本次连接时长/设备/格式）；
  * 主题列表单选切换（**热替换**，不重启进程）、「热重载」、「恢复内置主题」、「位置复位」；
  * 在线 `.slint` 编辑器：保存即入列表；
  * 显示开关、刷新间隔（50–1000ms）。
* 直接增删 `themes/*.slint` 文件也可以：列表约 5 秒内自动刷新。

### 修改主题（推荐路线）

**优先使用 AI/LLM**：把目标 `.slint` 文件与面板内「主题契约」一起交给 AI 助手修改，
或直接用文本编辑器打开面板显示的主题目录（`themesDir`，形如
`~/.config/micyou/plugins/opss.customizable-floating-window/themes/`）中的文件；
保存后回面板点「热重载当前主题」。面板在线编辑器仅用于快速微调。
内置主题首行带 `// cfw-bundled:` 标记：插件升级时仅覆盖**带标记**的文件；
你的自定义文件（或删掉标记的副本）永不动。

主题编译失败时：悬浮窗回退到内置 `ring.slint` 保证有窗可用，面板与系统通知给出诊断。

## 自定义主题（契约摘要）

主题 = 一个导出 Window 组件的 `.slint` 文件，**所有契约成员可选**（helper 内省后只绑定存在的）：

* 输入属性：`input-level` `processed-level` `smooth-level` `level-percent` `muted`
  `streaming` `monitoring` `sample-rate` `channels` `queued-ms` `session-seconds`
  `session-text` `device-label` `device-mode` `info-text` `arc-path` `bars-path`
* 输出回调：`mute-toggle()` `monitoring-toggle()` `menu-action(string)`
  `hide-window()` `drag-start()` `drag-move(float,float)` `drag-end()`
* 窗口建议：`no-frame: true; always-on-top: true; background: transparent;`
  （“不占任务栏”由 helper 在窗口层实现，主题无需关心）

完整契约与示例见面板内「主题契约」折叠区与 [docs/TECHNICAL.md](docs/TECHNICAL.md)。

> ⚠️ Slint 1.18 的**软件渲染器不支持元素旋转**（`transform-rotation` 被静默忽略）。
> 斜线/旋转几何请用 `Path` 线段表达：静态 `commands`，或 helper 提供的
> `arc-path`（电平圆弧）/ `bars-path`（旋转波形条，~30Hz 相位）。

## 平台说明

| 平台 | 不占任务栏实现 | 备注 |
|---|---|---|
| Windows | `WS_EX_TOOLWINDOW` 扩展样式 | 同时不出现在 Alt-Tab |
| Linux X11 | `_NET_WM_STATE_SKIP_TASKBAR` + `SKIP_PAGER` | GNOME/KDE/XFCE 等 EWMH 合成器 |
| Linux Wayland | —（合成器策略，无客户端协议） | 多数 Wayland 桌面无任务栏概念；窗口拖动建议用 `WindowMoveArea` |
| macOS | `NSApplicationActivationPolicyAccessory` | 无 Dock 图标 / Cmd-Tab 条目 |

Linux 运行时依赖（桌面发行版默认均有）：`libx11-6`、`libxkbcommon0`、
`libxkbcommon-x11-0`、`libfontconfig1`。

## 构建与发布

CI（`.github/workflows/release.yml`）三平台矩阵构建（Windows MSVC / ubuntu /
macOS arm64+x86_64），产物去 `lib` 前缀归一化后与 helper 二进制、清单、面板、
主题打包为单 `plugin.zip`；push 产出 development artifact，打 tag 发 Release。

本地验证：

```bash
cargo test                                   # 单元 + 主题编译契约测试
cargo build --release
helper --compile-check themes/ring.slint     # 主题语法/契约报告
helper --theme themes/ring.slint --selftest out   # 无宿主自测（需显示服务）
tools/mock-host --plugin target/release/libcustomizable_floating_window.so \
                --dir <plugin-dir> --seconds 15    # 模拟宿主 E2E（含点击/拖拽注入）
```

实现细节、架构决策与验证记录见 [docs/TECHNICAL.md](docs/TECHNICAL.md)。

## 许可

GPL-3.0-only — 见 [LICENSE](LICENSE)。Slint GUI  toolkit 经其 GPLv3 路径使用
（Slint © SixtyFPS GmbH）；许可证选择缘由见 docs/TECHNICAL.md §11。
