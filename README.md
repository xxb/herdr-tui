# herdr-tui (`hct`)

Lightweight multi-tab TUI client for [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core) with real-time Space & Tab sync across Herdr Desktop and Android.

[![Release](https://img.shields.io/github/v/release/xxb/herdr-tui)](https://github.com/xxb/herdr-tui/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS-lightgrey)

**English** | [简体中文](README.zh-CN.md)

```text
[ ⏏ ] [ ▾ Work ] ╱ 1:codex  2:shell  [ + ]      [ ? ]
──────────────────────────────────────────────────────
                  Terminal Viewport
──────────────────────────────────────────────────────
Codex  5h ██████▋░ 83% (3h12m) · 7d █▎░░░░░░ 16%
```

---

## Quick Start

> Requires [`herdr-core` (`hc`)](https://github.com/herdrdev/herdr-core) on `PATH` (or `~/.local/bin/herdr-core`).

### Install or Update

Supports **Linux** (`x86_64`, `aarch64` static binary) and **macOS** (Apple Silicon & Intel). Verifies `SHA256SUMS` and installs to `~/.local/bin/hct` without `sudo`. Re-run the same command anytime to update:

```sh
curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh
```

Start `hct`:

```sh
hct
```

### Uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --uninstall
```

Removes only the `hct` binary — `herdr-core` (`hc`), running sessions, and existing configs stay untouched.

- Remove offline manually: `rm -f ~/.local/bin/hct`.
- Also remove `~/.config/herdr-tui`:

  ```sh
  curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --uninstall --purge-config
  ```

<details>
<summary>More installation options (pin version, mirror, manual download, build from source)</summary>

**Pin a version or choose another install directory:**

```sh
curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh -s -- --version v0.13.0 --dir ~/.local/bin
```

**Use a release mirror or proxy:**

```sh
HCT_RELEASE_URL="https://your-mirror.example.com/herdr-tui/releases" sh -c 'curl -fsSL https://raw.githubusercontent.com/xxb/herdr-tui/main/install.sh | sh'
```

**Manual download:**

Grab the matching `.tar.gz` archive and `SHA256SUMS` from [GitHub Releases](https://github.com/xxb/herdr-tui/releases).

**Build from source:**

```sh
cargo build --release
mkdir -p ~/.local/bin && install -m 755 target/release/hct ~/.local/bin/hct
```

</details>

---

## Why `herdr-tui`?

`herdr-core` separates terminal runtime from UI so Desktop and Android apps can draw native chrome. `hct` brings that same Space/Tab workflow back to pure SSH and terminal sessions—one clean full-width viewport per tab, zero pane-splitting clutter.

---

## Highlights

- **Real-Time Client Sync** — Spaces, tabs, and unread agent state stay synced with Herdr Desktop and Android.
- **Theme-Aware Minimal Chrome** — 1-row transparent top bar (`[ ⏏ ] [ ▾ <space> ] ╱ Tabs [ + ] [ ? ]`) with accordion tab folding and a floating Space switcher popover; automatically follows your terminal's light or dark theme and subtly italicizes AI agent intermediate steps while leaving plain shells untouched.
- **Focused Agent Quota Bar** — 1-row bottom progress bar displaying remaining quota for the focused agent (Codex, Antigravity, OpenCode, Claude, Freebuff) strictly from local session logs/files with no network requests or credential access, staying transparent when idle.
- **Touch & Mobile SSH Friendly** — Drag to select and auto-copy over SSH (OSC 52), tap tabs and menus directly, swipe horizontally to switch tabs (or open the full-screen Tab Overview), and toggle dual floating draggable `+` cross pads (`hct --touch`).
- **E-Ink Mode & Built-In Reader** — High-contrast monochrome mode (`hct -e`) with optional Kaleido color accents (`--eink-color`), spinner freezing, frame pacing, edge tab buttons, and a full-screen paginated Reader (`^b v`) with Clean/Full toggle for reading long AI responses comfortably.
- **Familiar & Safe** — Compatible with classic `herdr` shortcuts and `~/.config/herdr/config.toml`, reports focused session and terminal to outer terminals (`OSC 1337`), and guards against accidental multiplexer nesting (override with `--allow-nested`).

---

## CLI Flags & Configuration

| Flag | Purpose |
|---|---|
| `-s, --session <name>` | Target `herdr-core` session (same as `HC_SESSION`) |
| `--touch` / `--no-touch` | Show or hide dual floating `+` touch pads |
| `-e, --eink` / `--no-eink` | Enable or disable monochrome E-Ink mode |
| `--eink-color` / `--no-eink-color` | Enable or disable Kaleido color E-Ink accents (implies `-e`) |
| `--eink-interval-ms <ms>` | Frame coalescing interval in E-Ink mode (default `250`, max `2000`) |
| `--allow-nested` / `--no-allow-nested` | Allow or block launching inside `hc`, `herdr`, `tmux`, `zellij`, or `screen` |
| `--version` | Print installed version |

Optional config file at `~/.config/herdr-tui/config.toml` (falls back to `~/.config/herdr/config.toml`):

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

## Shortcuts

Default prefix is `Ctrl+b` (written `^b` below). Tap `[ ? ]` in the top-right corner anytime for interactive quick actions.

### Tabs & Spaces

| Action | Prefix (`Ctrl+b`) | Direct / Touch |
|---|---|---|
| Next / Previous Tab | `^b n` / `^b p` (or `^b →` / `^b ←`) | `Alt+→` / `Alt+←`, tap tab, swipe, top-bar wheel |
| Jump to Tab 1–9 | `^b 1`..`9` | `Alt+1`..`9` |
| New / Rename / Close Tab | `^b c` / `^b ,` / `^b x` | Tap `[ + ]` for new tab |
| Next / Previous Space | `^b ↓` / `^b ↑` | `Alt+↓` / `Alt+↑` |
| Toggle Space Menu | `^b b` | Tap `[ ▾ <space> ]` |
| New / Rename Space | `^b C` (`^b s`) / `^b $` (`^b R`) | Tap `[ + New Space ]` in Space menu |
| Fuzzy Switcher | `^b w` (`^b f`) | Tap overflow badge `[ ‹N ]` / `[ N› ]` |
| Jump to Alert Agent | `^b a` | — |
| Cycle Split Pane | `^b o` | — |

### Reading, Scrolling & Display

| Action | Prefix (`Ctrl+b`) | Direct / Touch |
|---|---|---|
| Scrollback Mode | `^b [` | Mouse wheel up / down |
| Page Up / Down | `^b PgUp` / `^b PgDn` | `Shift+PgUp` (`PgUp` / `PgDn` in scrollback) |
| Built-In Reader | `^b v` | Tap `[ 📖 Reader ]` in `[ ? ]` (`c`/`f`/`Tab` Clean/Full, `r` refresh, `l` latest) |
| E-Ink / Kaleido Mode | `^b e` / `^b E` | Tap in `[ ? ]` |
| Clean Redraw | `^b r` | Tap `[ ↻ Redraw ]` in `[ ? ]` |

### Session & Help

| Action | Prefix (`Ctrl+b`) | Direct / Touch |
|---|---|---|
| Quick Actions & Help | `^b ?` | Tap `[ ? ]` |
| Detach (keep running) | `^b q` (`^b d`) | `Ctrl+\` or tap `[ ⏏ ]` |

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
