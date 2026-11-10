# AGENTS.md — herdr-tui (`hct`) Contributor & Agent Guidelines

## 1. Project Overview & Mission

`herdr-tui` (`hct`) is an open-source, lightweight multi-tab Terminal User Interface (TUI) client for [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core), licensed under Apache-2.0.

### Architectural Invariants
1. **No Pane Splitting Complexity**: `hct` intentionally omits pane splitting and complex 2D tiling. Every tab displays a single full-size terminal viewport (with `^b o` available to cycle panes if a tab created by the desktop app contains multiple panes).
2. **Collapsible 2-Row Left Sidebar + Herdr-Style Bottom Touchbar + Floating `+` D-Pad**:
   - **Left Sidebar (`^b b`)**: Displays Spaces, Tabs, and live Agent status (`◉` blocked, `●` working, `✓` done, `○` idle) as 2-row touch cards, collapsible Space child tabs (`▾` / `▸`), and spaced-out `[+ New Tab]` and `[+ New Space]` buttons. Auto-collapses when terminal width `< 80` cols and can be toggled open at any width.
   - **Bottom Touchbar (3 rows by default, or 1 row with `--no-touch`)**: Displays compact `◄≡`/`≡►` sidebar toggle, left-side current Space name with solid `●` / hollow `○` Tab indicator circles underneath, constant-width `[^B]`, `[Esc]`, `[Ctrl]`, `[Alt]`, `[Tab]` sticky modifier/terminal buttons (without `[^C]`), `[✓ Copied]` / `[▼ BOT]` badges, and `[ ? ]` Quick Actions panel toggle.
   - **Floating Draggable Semi-Transparent `+` Cross D-Pad**: Renders a `15 × 7` hollow-corner `+` cross overlay over the terminal viewport with 4 directional arms (`↑`, `↓`, `←`, `→`) and centered Paste (`⎘`). Supports tapping any of the 5 sectors or dragging anywhere on the `+` cross to reposition it within the terminal viewport.
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
