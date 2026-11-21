# herdr-tui (`hct`)

面向 [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core) 的轻量级多标签页终端客户端，并与 Herdr 桌面端、Android 端实时双向同步工作区（Space）与标签页（Tab）。

[![Release](https://img.shields.io/github/v/release/xxb/herdr-tui)](https://github.com/xxb/herdr-tui/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS-lightgrey)

[English](README.md) | **简体中文**

```text
[ ⏏ ] [ ▾ Work ] ╱ 1:codex  2:shell  [ + ]      [ ? ]
──────────────────────────────────────────────────────
                  终端主视口
──────────────────────────────────────────────────────
Codex  5h ██████▋░ 83% (3h12m) · 7d █▎░░░░░░ 16%
```

---

## 快速开始

> 使用前需在 `PATH`（或 `~/.local/bin/herdr-core`）中安装 [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core)。

### 安装或更新

支持 **Linux**（`x86_64`、`aarch64` 静态独立版）与 **macOS**（Apple 芯片与 Intel）。自动校验 `SHA256SUMS` 并免 `sudo` 安装到 `~/.local/bin/hct`，日后重跑同一条命令即可更新：

```sh
curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh
```

启动：

```sh
hct
```

### 卸载

```sh
curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --uninstall
```

默认仅移除 `hct` 程序本体，不会影响 `herdr-core`（`hc`）、运行中的会话或已有配置文件。

- 离线手动清理：`rm -f ~/.local/bin/hct`。
- 连带删除 `~/.config/herdr-tui` 配置目录：

  ```sh
  curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --uninstall --purge-config
  ```

<details>
<summary>更多安装方式（指定版本、更换目录、镜像代理、手动下载、源码编译）</summary>

**安装指定版本或自定义目录：**

```sh
curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --version v0.13.0 --dir ~/.local/bin
```

**通过镜像或代理下载：**

```sh
HCT_RELEASE_URL="https://your-mirror.example.com/herdr-tui/releases" sh -c 'curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh'
```

**手动下载：**

前往 [GitHub Releases](https://github.com/xxb/herdr-tui/releases) 下载对应平台的 `.tar.gz` 压缩包与 `SHA256SUMS`。

**从源码编译：**

```sh
cargo build --release
mkdir -p ~/.local/bin && install -m 755 target/release/hct ~/.local/bin/hct
```

</details>

---

## 为什么做 `herdr-tui`？

`herdr-core` 将终端运行时与界面彻底解耦，让桌面端和手机端都能绘制原生界面；而在纯 SSH 或终端环境下，我们需要一个轻巧顺手的入口。`hct` 直接接管这套工作区与标签页体验，每个标签页始终占满 100% 屏幕宽度，不做繁杂的二维分屏。

---

## 核心特性

- **多端实时同步** — 工作区、标签页及 Agent 未读完成提示，与 Herdr 桌面端和 Android 客户端双向同步。
- **随终端主题自适应的极简界面** — 顶部单行透明标签栏（`[ ⏏ ] [ ▾ <工作区名> ] ╱ 标签页 [ + ] [ ? ]`），支持多标签自动折叠与工作区浮层切换；实时跟随终端明暗主题，并在 Agent 标签页中将思考与工具步骤自动弱化为斜体，普通终端保持原样。
- **底部单行 Agent 配额条** — 仅展示当前聚焦 Agent 的剩余配额进度条（支持 Codex、Antigravity、OpenCode、Claude、Freebuff），无配额信息时保持透明。
- **为 iPad / 手机 SSH 优化的触控体验** — 拖动选字松手自动通过 OSC 52 复制到本地剪贴板，支持点击切换标签与菜单、左右滑动切换标签或进入全屏总览页，还可用 `hct --touch` 开启双悬浮可拖动 `+` 十字键盘。
- **墨水屏模式与内置阅读器** — `hct -e` 开启高对比度黑白低刷模式（可选 `--eink-color` 彩墨点缀），支持转圈动画冻结、帧合并限流、左右边缘翻页标签，以及 `^b v` 全屏书页阅读器（支持 Clean/Full 模式切换，长篇 AI 回复阅读更轻松）。
- **兼容顺手、防误嵌套** — 兼容经典 `herdr` 快捷键与 `~/.config/herdr/config.toml`，向外层终端上报当前会话状态（`OSC 1337`），并自动阻止在 `hc` / `tmux` / `zellij` / `screen` 内误嵌套启动（需要时可加 `--allow-nested`）。

---

## 常用命令行参数与配置

| 参数 | 说明 |
|---|---|
| `-s, --session <名称>` | 指定连接的 `herdr-core` 会话（等同环境变量 `HC_SESSION`） |
| `--touch` / `--no-touch` | 显示或隐藏双悬浮 `+` 触控十字键盘 |
| `-e, --eink` / `--no-eink` | 开启或关闭黑白墨水屏高对比低刷模式 |
| `--eink-color` / `--no-eink-color` | 开启或关闭 Kaleido 彩墨点缀模式（会自动开启 `-e`） |
| `--eink-interval-ms <毫秒>` | 墨水屏模式下的被动输出合并间隔（默认 `250`，上限 `2000`） |
| `--allow-nested` / `--no-allow-nested` | 允许或禁止在 `hc`、`herdr`、`tmux`、`zellij`、`screen` 内嵌套运行 |
| `--version` | 查看当前安装版本号 |

配置文件路径为 `~/.config/herdr-tui/config.toml`（若不存在则回退读取 `~/.config/herdr/config.toml`）：

```toml
[keys]
prefix = "ctrl+b"

[ui]
touch_mode = false
eink = false
eink_color = false
eink_interval_ms = 250
allow_nested = false
```

---

## 默认快捷键

默认前缀键为 `Ctrl+b`（下表简写为 `^b`），随时点击右上角 `[ ? ]` 可打开触控快捷面板。

### 标签页与工作区

| 功能 | 前缀快捷键（`Ctrl+b`） | 直达 / 触控操作 |
|---|---|---|
| 切换上 / 下一个标签页 | `^b p` / `^b n`（或 `^b ←` / `^b →`） | `Alt+←` / `Alt+→`、点击标签、水平滑动、顶栏滚轮 |
| 跳转第 1–9 个标签页 | `^b 1`..`9` | `Alt+1`..`9` |
| 新建 / 重命名 / 关闭标签页 | `^b c` / `^b ,` / `^b x` | 点击顶栏 `[ + ]` 新建 |
| 切换上 / 下一个工作区 | `^b ↑` / `^b ↓` | `Alt+↑` / `Alt+↓` |
| 展开 / 收起工作区菜单 | `^b b` | 点击 `[ ▾ <工作区名> ]` |
| 新建 / 重命名工作区 | `^b C`（`^b s`） / `^b $`（`^b R`） | 在工作区菜单中点击 `[ + New Space ]` |
| 模糊搜索跳转 | `^b w`（`^b f`） | 点击溢出角标 `[ ‹N ]` / `[ N› ]` |
| 跳转待处理 Agent | `^b a` | — |
| 轮换多窗格 | `^b o` | — |

### 翻页、阅读与显示

| 功能 | 前缀快捷键（`Ctrl+b`） | 直达 / 触控操作 |
|---|---|---|
| 进入历史回滚 | `^b [` | 鼠标滚轮上下滚动 |
| 整页上 / 下翻页 | `^b PgUp` / `^b PgDn` | `Shift+PgUp`（回滚模式中直接 `PgUp` / `PgDn`） |
| 内置书页阅读器 | `^b v` | 点 `[ ? ]` 中 `[ 📖 Reader ]`（`c`/`f`/`Tab` 切换精简/完整，`r` 刷新，`l` 回最新） |
| 墨水屏 / 彩墨模式 | `^b e` / `^b E` | 在 `[ ? ]` 面板中点击切换 |
| 手动清残影重绘 | `^b r` | 在 `[ ? ]` 面板中点击 `[ ↻ Redraw ]` |

### 会话与帮助

| 功能 | 前缀快捷键（`Ctrl+b`） | 直达 / 触控操作 |
|---|---|---|
| 帮助与快捷面板 | `^b ?` | 点击右上角 `[ ? ]` |
| 脱离并保持后台运行 | `^b q`（`^b d`） | `Ctrl+\` 或点击左上角 `[ ⏏ ]` |

---

## 开源协议

基于 [Apache License, Version 2.0](LICENSE) 开源。
