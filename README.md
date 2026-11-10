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
- **Collapsible 2-Row Left Sidebar + Bottom Touchbar (加高左侧边栏 + 底部 Touchbar)**
  - **Left Sidebar (`^b b`)**: Displays the full tree of Spaces, Tabs, and live Agent status indicators (`◉` blocked, `●` working, `✓` done, `○` idle). Each Space and Tab entry is rendered as a 2-row-tall touch card with path/CWD subtitles, collapsible Space child tabs (tap `▾` / `▸` on any Space to fold/unfold its tabs), and spaced-out 2-row `[+ New Tab]` and `[+ New Space]` buttons at the bottom.
  - **Bottom Touchbar**: Inspired by the Herdr mobile app and legacy Herdr status dots, provides a compact `◄≡` / `≡►` sidebar toggle, left-side clickable Tab status dots (`● ○ ✓`), constant-width `[^B]` sticky prefix key and terminal shortcut keys (`[Esc]`, `[Tab]`, `[ ^C ]`, `[←]`, `[↓]`, `[↑]`, `[→]`) that never jitter, `[▼ BOT]` jump-to-bottom badge when scrolled, and `[ ? ]` Quick Actions button.
- **Touch & Mouse Friendly for iPad / Mobile SSH (触控与鼠标丝滑切换)**
  - **Tap Bottom Touchbar & Left Sidebar**: Tap any 2-row Space/Tab card in the left sidebar, tap the left-side `● ○ ✓` status dots on the touchbar, or tap terminal keys (`Esc`, `Tab`, `^C`, arrows) and `[^B]` directly on the bottom touchbar.
  - **No Terminal Selection Conflict**: Dragging a finger inside the terminal viewport never triggers tab switching, preserving native finger text selection on SSH clients.
  - **2-Row Large Touch Mode (`hct --touch`)**: Doubles the height of the bottom touchbar (2 rows) for even larger finger hit targets on tablets.
  - **Touch-Interactive Modals**: Directly tap items in the Fuzzy Switcher (`^b w`), tap Confirm/Cancel buttons in dialogs, or tap any of the 12 quick-action buttons in the `[ ? ]` panel.
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
| **New Tab in Space** (新建标签页) | `^b c` | Tap `[+ New Tab]` in sidebar |
| **Rename Current Tab** (重命名标签页) | `^b ,` | — |
| **Close Current Tab** (关闭当前标签页) | `^b x` | — |
| **Next / Previous Space** (切换工作区) | `^b Down` / `^b Up` | `Alt+Down` / `Alt+Up` or Tap Space |
| **New Space** (新建工作区) | `^b C` (or `^b s`) | Tap `[+ New Space]` in sidebar |
| **Rename Current Space** (重命名工作区) | `^b $` (or `^b R`) | — |
| **Toggle Left Sidebar** (展开/收起侧边栏) | `^b b` | Tap `◄≡` / `≡►` on bottom touchbar |
| **Quick Fuzzy Switcher** (模糊搜索跳转) | `^b w` (or `^b f`) | — |
| **Jump to Alert Agent** (跳转待处理 Agent) | `^b a` | — |
| **Cycle Pane in Split Tab** (多窗格轮换) | `^b o` | — |
| **Scrollback Mode** (历史回滚查看) | `^b [` | Mouse Wheel Up / Down |
| **Help Overlay** (快捷键帮助) | `^b ?` | Tap `[ ? ]` on bottom touchbar |
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
