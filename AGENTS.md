# AGENTS.md — herdr-tui (`hct`) Contributor & Agent Guidelines

## 1. Project Overview & Mission

`herdr-tui` (`hct`) is an open-source, lightweight multi-tab Terminal User Interface (TUI) client for [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core), licensed under Apache-2.0.

### Architectural Invariants
1. **No Pane Splitting Complexity**: `hct` intentionally omits pane splitting and complex 2D tiling. Every tab displays a single full-size terminal viewport (with `^b o` available to cycle panes if a tab created by the desktop app contains multiple panes).
2. **Top `Space / Tab1 / Tab2 / [+]` Bar + Dual Floating `+` Cross Pads + Read-Only Bottom Agent Status Bar**:
   - **Top Tab Bar (`[ ▾ Space ] ╱ [ Tab1 ] [ Tab2 ] [ + ] ... [ ? ]`)**: Occupies 3 rows in touch mode (or 1 row with `--no-touch`). Tapping `[ ▾ Space ]` (or pressing `^b b`) opens a secondary floating Space Switcher popover (`Modal::SpaceMenu`) with 2-row Space cards and a `[ + New Space ]` button. Tapping any Tab pill switches directly to that Tab; tapping `[ + ]` immediately after the rightmost Tab creates a new Tab in the active Space; tapping `[ ? ]` opens the Quick Actions & Help panel. The Left Sidebar is removed so the terminal viewport always spans 100% of the screen width.
   - **Dual Floating Draggable Semi-Transparent `+` Cross Pads**: Renders two `15 × 7` hollow-corner `+` cross overlays over the terminal viewport (scaling down on constrained viewports):
     - **Left Modifier Cross (`Esc` / `Ctl` / `^B` / `Alt` / `Tab`)**: Top = `Esc`, Left = sticky `Ctl`, Center = sticky `^B` prefix, Right = sticky `Alt`, Bottom = `Tab`.
     - **Right Directional & Paste Cross (`↑` / `←` / `⎘` / `→` / `↓`)**: Top = `↑`, Left = `←`, Center = `⎘` Paste, Right = `→`, Bottom = `↓`.
     - Both `+` crosses have 4 completely hollow corners (100% transparent to underlying terminal text and mouse events) and can be independently dragged anywhere within the terminal viewport.
   - **Bottom 1-Row Read-Only Agent Status Bar**: Displays active agent state badge (`◉ BLOCKED`, `● WORKING`, `✓ DONE`, `○ IDLE`, `○ SHELL`), active terminal title/CWD/turn count, workspace agent summary counts (with multi-agent token/quota usage tracked in Issue #10), plus `[ ✓ Copied ]` and clickable `[ ▼ BOT (+N) ]` when scrolled up.
   - **Touch Text Selection & OSC 52 Clipboard**: Dragging inside the terminal viewport highlights text and automatically copies the selection to the client clipboard on release via OSC 52.
3. **Bidirectional `doc("herdr-app")` Sync**:
   - Reads and writes `herdr-core`'s `doc("herdr-app")` layout document with CAS version checks (`doc.get` / `doc.set`).
   - Preserves unknown JSON fields (`#[serde(flatten)]`) so official Herdr Desktop and Android clients never lose metadata when `hct` saves changes.
   - Shares attention state via `herdr.seen=<turn>` terminal labels.
4. **Privacy & No Personal Data**:
   - Never commit machine hostnames, usernames, home paths, IP addresses, or personal emails in code, tests, comments, or documentation.

## 2. Build & Test Commands

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## 3. Issue, Branch & PR Workflow

- Every feature or bugfix starts from a tracked GitHub Issue (`issue-<N>`).
- Develop on branch `feat/issue-<N>` (or `fix/issue-<N>`).
- Run `cargo test` and `cargo clippy` before pushing.
- Open a Pull Request referencing `Fixes #<N>` and request `@codex review`.
- Do not push implementation code directly to `main` or merge without review approval.
