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
- **Top Tab Bar + Collapsible Left Sidebar (顶栏多标签 + 可折叠左侧边栏)**
  - **Top Bar (1 row)**: Shows sidebar toggle `[≡]`, active `[Space]`, numbered tabs with live Agent status indicators (`◉` blocked, `●` working, `✓` done, `○` idle), and a `[+]` new-tab button.
  - **Left Sidebar (`^b b`)**: Displays the full tree of Spaces, Tabs, and Agent statuses. Automatically collapses on narrow screens (`< 80` cols) and can be toggled anytime via `Ctrl+b b` or tapping `[≡]`.
- **Touch & Mouse Friendly for iPad / Mobile SSH (触控与鼠标丝滑切换)**
  Tap any tab on the top bar, tap `[+]` to open a new tab, tap `[≡]` to toggle the sidebar, or tap any Space/Tab in the sidebar.
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
| **New Tab in Space** (新建标签页) | `^b c` | Tap `[+]` on top bar |
| **Rename Current Tab** (重命名标签页) | `^b ,` | — |
| **Close Current Tab** (关闭当前标签页) | `^b x` | — |
| **Next / Previous Space** (切换工作区) | `^b Down` / `^b Up` | `Alt+Down` / `Alt+Up` or Tap Space |
| **New Space** (新建工作区) | `^b C` (or `^b s`) | Tap `[+ New Space]` in sidebar |
| **Rename Current Space** (重命名工作区) | `^b $` (or `^b R`) | — |
| **Toggle Left Sidebar** (展开/收起侧边栏) | `^b b` | Tap `[≡]` on top bar |
| **Quick Fuzzy Switcher** (模糊搜索跳转) | `^b w` (or `^b f`) | — |
| **Jump to Alert Agent** (跳转待处理 Agent) | `^b a` | — |
| **Cycle Pane in Split Tab** (多窗格轮换) | `^b o` | — |
| **Scrollback Mode** (历史回滚查看) | `^b [` | Mouse Wheel Up / Down |
| **Help Overlay** (快捷键帮助) | `^b ?` | Tap `^b ?:Help` on top bar |
| **Detach `hct`** (脱离并保持后台运行) | `^b q` (or `^b d`) | `Ctrl+\` |

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
