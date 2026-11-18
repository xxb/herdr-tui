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
  - **Top Tab Bar**: Displays `[ ⏏ ] [ ▾ Space ] ╱ [ 1:tab1 ] [ 2:tab2 ] [ + ] ... [ ? ]` along the top edge, with `[ ⏏ ]` on the far left for one-tap detach (`^b q`), classic `herdr` Catppuccin high-contrast accent fill (`#89b4fa` / `#181825`) on the active tab, and zero-width color-coded Agent states (`Blocked` / `Done` / `Working`).
  - **Accordion Folding & Overflow Count Badges**: When many tabs are open, inactive tabs automatically fold into compact `[ 1 ] [ 2 ] [ 3:active ] [ 4 ]` pills so all tabs stay visible in fixed order without disorienting horizontal scrolling; on extreme tab counts, clickable `[ ‹N ]` / `[ N› ]` badges open the Quick Fuzzy Switcher directly.
  - **Secondary Space Popover (`^b b` or Tap `[ ▾ Space ]`)**: Tapping the Space button opens a floating popover menu listing all Spaces with live Agent status and a `[ + New Space ]` button—eliminating the need for a permanent Left Sidebar so the terminal viewport always enjoys 100% horizontal width.
  - **Direct Tab Switching & `[ + ]` New Tab**: Tap any Tab pill on the top bar, swipe horizontally / use horizontal mouse wheel (`ScrollLeft` / `ScrollRight`) anywhere in the TUI, or scroll the vertical mouse wheel over the top bar for linear (non-wrapping) tab navigation—stopping at Tab 1 on the left and swiping past the last tab into a full-viewport `hc`-style Space Tab Overview / `+ New Tab` page (swipe back or press `Esc` to exit); tap `[ + ]` on the top bar to immediately create a new Tab.
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
  - **Touch-Interactive Modals**: Directly tap items in the Space Popover (`^b b`) or Fuzzy Switcher (`^b w`), tap Confirm/Cancel buttons in dialogs, or tap any of the 17 quick-action buttons in the `[ ? ]` panel.
- **E-Ink Monochrome & Agent Pacing Mode (`hct -e` / `hct --eink` / `^b e` 墨水屏高对比低刷模式)**
  - **Strict 300 PPI Monochrome Chrome & Viewport Filter**: Quantizes UI chrome, tabs, floating crosses, and modals to Carta 4-bit neutral grays (`#FFFFFF`, `#EEEEEE`, `#DDDDDD`, `#777777`, `#666666`, `#333333`, `#000000`) with a 4-tier hierarchy (light gray italic `#777777` for AI thought/tool/spinner process lines, muted upright `#666666` for secondary annotations, crisp `#000000` body text, and bold headings), keeps the top bar on white paper (`#FFFFFF`) with an active-tab highlight underline to minimize block ghosting and redraws, adds monochrome non-color Agent prefixes (`!` / `*` / `✓`) and dither-free progress bars (`█` / `─` with `!` low-quota warning), strips `Modifier::DIM`, and remaps dark-mode RGB/TrueColor agent output onto crisp white paper without restarting running sessions.
  - **Spinner Freezing & Frame Pacing**: Freezes isolated Braille/quarter-circle/arc agent spinners to static `◈` while preserving multi-cell Braille charts and decorative box corners, keeps user input at 0 ms responsiveness, coalesces passive PTY bursts over `--eink-interval-ms` (default `250`, max `2000`, configurable via `ui.eink_interval_ms`), and skips redundant draws when the snapped screen is unchanged.
  - **Edge Tab Controls, Book Reader Export & Semantic Clean Refresh**: Adds left/right vertically-centered see-through edge tabs (`[ ‹ ]` / `[ › ]`) in `-e` / `--eink` for linear tab stepping (`[ ‹ ]` clamps at Tab 1 and exits `TabOverview` back to the last tab; `[ › ]` steps past the last tab into trailing `TabOverview`), keeps keyboard page scrolling (`Shift+PgUp` / `^b PgUp` / `^b PgDn` and `[ ? ]` quick actions), `^b v` / `^b B` (or `[ 📖 Book View ]`) clean `OSC 777;shelldeck;reader-open` export for ShellDeck's paginated Book Reader, `^b r` clean redraw, environment auto-detection (`HCT_EINK`, `SHELLDECK_EINK`, `LC_TERMINAL_PROFILE`, `HCT_THEME`, `TERM`), and `OSC 777;shelldeck;eink-refresh` signals on agent completion (`Working` → `Done`/`Idle`/`Blocked`) and tab/modal settlement.
  - **Optional Kaleido Color E-Ink Accent Overprint (`--eink-color` / `^b E` 彩墨套印点缀模式)**: Opt-in overprint mode ("95% 300 PPI monochrome + 5% high-contrast semantic accent") designed for Kaleido color E-Ink panels (`ui.eink_color = true`, `HCT_EINK_COLOR`, `SHELLDECK_EINK_COLOR`, `LC_TERMINAL_PROFILE=shelldeck-eink-color`). Keeps backgrounds `#FFFFFF` white and all normal/muted text strictly neutral (`R == G == B`), adding deep vermilion/forest/navy semantic inks (`#990000`, `#005522`, `#003388`) only to top tab agent state markers, git diff/patch markers & side bars, and `< 15%` low-quota warnings.
- **Classic `herdr` Keybindings (兼容旧版 `herdr` 快捷键)**
  Compatible with `~/.config/herdr/config.toml` and `~/.config/herdr-tui/config.toml`.
- **Shared Attention (`herdr.seen`)**
  Follows the `herdr.seen=<turn>` label specification so viewing a finished agent in `hct` clears its unread `done` indicator across all connected devices.
- **Anti-Nesting Guard (防嵌套启动保护)**
  Automatically refuses to launch inside an already active `hc` (`herdr-core` / `hct`), `herdr`, `tmux`, `zellij`, or GNU `screen` session to prevent prefix-key collisions, mouse/OSC 52 clipboard breakage, and terminal feedback loops. Pass `hct --allow-nested` (or set `ui.allow_nested = true` in `config.toml`) to override when needed.

---

## Default Shortcuts / 默认快捷键

| Action / 功能 | Prefix Shortcut (`Ctrl+b`) | Direct / Touch Shortcut |
|---|---|---|
| **Next / Previous Tab** (切换标签页) | `^b n` / `^b p` | `Alt+Right` / `Alt+Left` (wrap), Tap Left `[ ‹ ]` / Right `[ › ]` in `-e` / `--eink`, Horiz Swipe / Top-Bar Wheel (linear + trailing overview page), Tap Tab |
| **Select Tab 1..9** (跳转标签页 1..9) | `^b 1` .. `^b 9` | `Alt+1` .. `Alt+9` |
| **New Tab in Space** (新建标签页) | `^b c` | Tap `[ + ]` on top tab bar |
| **Rename Current Tab** (重命名标签页) | `^b ,` | — |
| **Close Current Tab** (关闭当前标签页) | `^b x` | — |
| **Next / Previous Space** (切换工作区) | `^b Down` / `^b Up` | `Alt+Down` / `Alt+Up`, Tap Space, or Wheel / Horiz Swipe in Space menu |
| **New Space** (新建工作区) | `^b C` (or `^b s`) | Tap `[ + New Space ]` in Space menu |
| **Rename Current Space** (重命名工作区) | `^b $` (or `^b R`) | — |
| **Toggle Space Menu** (展开/收起工作区浮层菜单) | `^b b` | Tap `[ ▾ Space ]` on top tab bar |
| **Quick Fuzzy Switcher** (模糊搜索跳转) | `^b w` (or `^b f`) | Tap `[ ‹N ]` / `[ N› ]` overflow badge |
| **Jump to Alert Agent** (跳转待处理 Agent) | `^b a` | — |
| **Cycle Pane in Split Tab** (多窗格轮换) | `^b o` | — |
| **Scrollback Mode** (历史回滚查看) | `^b [` | Mouse Wheel Up / Down |
| **Page Up / Page Down** (整页翻页) | `^b PgUp` / `^b PgDn` | `Shift+PgUp` (or `PgUp` / `PgDn` in scrollback), Tap `[▲PgUp]` / `[▼PgDn]` in `[ ? ]` |
| **E-Ink Mode / Kaleido Accent / Book Reader** (墨水屏 / 彩墨点缀 / 书页视图) | `^b e` / `^b E` / `^b v` | `hct -e` / `--eink-color` / `--no-eink-color`, or Tap `[ ◐ E-Ink ]` / `[ ◈ Kaleido ]` / `[ 📖 Book View ]` in `[ ? ]` |
| **Clean Redraw** (手动清残影重绘) | `^b r` | Tap `[ ↻ Redraw ]` in `[ ? ]` |
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
hct --version
```

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
