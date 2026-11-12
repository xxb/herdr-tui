# herdr-tui (`hct`)

A fast, lightweight **multi-tab TUI client** for [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core) with **bidirectional Space & Tab synchronization** across Herdr Desktop and Android clients.

> **Why `herdr-tui`?**
> `herdr-core` (`hc`) decoupled the terminal runtime from the UI ("Core draws nothing"). While great for native desktop and mobile apps, pure SSH / terminal workflows (such as connecting from an iPad or laptop over SSH) lost the persistent tab bar, collapsible workspace sidebar, and instant tab-switching shortcuts of the classic `herdr` TUI.
>
> `herdr-tui` (`hct`) brings back the classic `herdr` TUI experience—top tab bar, collapsible left sidebar, touch/mouse tab switching, and `Ctrl+b` / `Alt+Arrow` shortcuts—directly on top of `herdr-core`, without complex pane splitting overhead.

---

## Features / 功能特性

- **Bidirectional Official Client Sync (与官方客户端双向实时同步)**
  Reads and writes `herdr-core`'s `doc("herdr-app")` layout document directly. Spaces and Tabs created, renamed, or closed in `hct` immediately appear in the official Herdr Android / Desktop client, and vice versa.
- **Top `Space / Tab1 / Tab2 / [+]` Bar + Secondary Space Popover (顶部 Space/Tab 状态栏 + 二级浮层菜单)**
  - **Top Tab Bar**: Displays `[ ⏏ ] [ ▾ Space ] ╱ [ ● 1:tab1 ] [ ○ 2:tab2 ] [ + ] ... [ ? ]` along the top edge, with `[ ⏏ ]` on the far left for one-tap detach (`^b q`).
  - **Secondary Space Popover (`^b b` or Tap `[ ▾ Space ]`)**: Tapping the Space button opens a floating popover menu listing all Spaces with live Agent status and a `[ + New Space ]` button—eliminating the need for a permanent Left Sidebar so the terminal viewport always enjoys 100% horizontal width.
  - **Direct Tab Switching & `[ + ]` New Tab**: Tap any Tab pill on the top bar to switch directly to that Tab, or tap `[ + ]` on the right of the tabs to add a new Tab.
- **Dual Floating Draggable Semi-Transparent `+` Cross Pads (`hct --touch` 开启双悬浮可拖动半透明十字按钮盘)**
  - Enabled with `hct --touch` (hidden by default when running `hct` on desktop without `--touch`).
  - **Left Modifier Cross (`Esc` / `Ctl` / `^B` / `Alt` / `Tab`)**: `15 × 7` hollow-corner `+` cross with `Esc` (top), sticky `Ctl` (left), sticky `^B` prefix (center), sticky `Alt` (right), and `Tab` (bottom).
  - **Right Directional & Paste Cross (`↑` / `←` / `⎘` / `→` / `↓`)**: `15 × 7` hollow-corner `+` cross with 4 directional keys (`↑`, `↓`, `←`, `→`) and centered Paste (`⎘`).
  - Both `+` crosses can be tapped or dragged independently anywhere over the terminal viewport, with 4 completely hollow corners so underlying terminal text remains visible and clickable.
- **Bottom 1-Row Read-Only Focused-Agent Quota Bar (底部单行聚焦 Agent 配额剩余百分比进度条)**
  - Displays **only** the currently focused Agent's quota remaining percentage (`剩余百分比`) with an 8-step Unicode fractional block progress bar (e.g. `Codex  5h ██████▋░ 83% (3h12m)  ·  7d █▎░░░░░░ 16% (5d04h)`, `Antigravity  5h ███████▋ 95% (3h51m)  ·  7d ███▌░░░░ 44% (2d17h)`, `OpenCode  今日 ██████▉░ 86% (28/200 · 142k)`), never exceeding 1 line, without repeating status/CWD/turn info that the agent TUI already shows, plus `[ ✓ Copied ]` and clickable `[ ▼ BOT (+N) ]` when viewing scrollback.
- **Touch & Mouse Friendly for iPad / Mobile SSH (触控与鼠标丝滑切换)**
  - **Touch Drag Text Selection + Auto Copy over SSH (手指拖动选字自动复制)**: Drag across terminal text with your finger (or mouse) to highlight text in real time; releasing automatically copies the selected text to the remote SSH client's system clipboard via OSC 52 (and local clipboard when available) and flashes `[✓ Copied]`.
  - **1-Row Compact Touch Top Bar**: Uses a crisp 1-row pill bar (with bottom highlight underline on the active tab) so terminal sessions immediately start on Row 1 without wasting vertical screen space.
  - **Touch-Interactive Modals**: Directly tap items in the Space Popover (`^b b`) or Fuzzy Switcher (`^b w`), tap Confirm/Cancel buttons in dialogs, or tap any of the 12 quick-action buttons in the `[ ? ]` panel.
- **Classic `herdr` Keybindings (兼容旧版 `herdr` 快捷键)**
  Compatible with `~/.config/herdr/config.toml` and `~/.config/herdr-tui/config.toml`.
- **Shared Attention (`herdr.seen`)**
  Follows the `herdr.seen=<turn>` label specification so viewing a finished agent in `hct` clears its unread `done` indicator across all connected devices.

---

## Default Shortcuts / 默认快捷键

| Action / 功能 | Prefix Shortcut (`Ctrl+b`) | Direct / Touch Shortcut |
|---|---|---|
| **Next / Previous Tab** (切换标签页) | `^b n` / `^b p` | `Alt+Right` / `Alt+Left` or Tap Tab |
| **Select Tab 1..9** (跳转标签页 1..9) | `^b 1` .. `^b 9` | `Alt+1` .. `Alt+9` |
| **New Tab in Space** (新建标签页) | `^b c` | Tap `[ + ]` on top tab bar |
| **Rename Current Tab** (重命名标签页) | `^b ,` | — |
| **Close Current Tab** (关闭当前标签页) | `^b x` | — |
| **Next / Previous Space** (切换工作区) | `^b Down` / `^b Up` | `Alt+Down` / `Alt+Up` or Tap Space |
| **New Space** (新建工作区) | `^b C` (or `^b s`) | Tap `[ + New Space ]` in Space menu |
| **Rename Current Space** (重命名工作区) | `^b $` (or `^b R`) | — |
| **Toggle Space Menu** (展开/收起工作区浮层菜单) | `^b b` | Tap `[ ▾ Space ]` on top tab bar |
| **Quick Fuzzy Switcher** (模糊搜索跳转) | `^b w` (or `^b f`) | — |
| **Jump to Alert Agent** (跳转待处理 Agent) | `^b a` | — |
| **Cycle Pane in Split Tab** (多窗格轮换) | `^b o` | — |
| **Scrollback Mode** (历史回滚查看) | `^b [` | Mouse Wheel Up / Down |
| **Help Overlay** (快捷键帮助) | `^b ?` | Tap `[ ? ]` on top tab bar |
| **Detach `hct`** (脱离并保持后台运行) | `^b q` (or `^b d`) | `Ctrl+\` or Tap `[ ⏏ ]` on top tab bar |

---

## Build & Install / 编译与安装

Requires [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core) installed on `PATH` (or `~/.local/bin/herdr-core`).

```bash
cargo build --release
install -Dm755 target/release/hct ~/.local/bin/hct
```

Run:

```bash
hct
```

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
