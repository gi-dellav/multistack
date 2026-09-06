<img width="3281" height="1171" alt="banner" src="https://github.com/user-attachments/assets/11f73a85-c12b-492f-8cff-1ef1369107d8" />

---

# Multistack

Run multiple [zerostack](https://gi-dellav.github.io/zerostack/) agents side by side in your terminal, allowing to spawn, watch, rename, kill all underlying processes.

Multistack is designed to be a native, lightweight and open-soruce competitor to [Conductor](https://www.conductor.build).


## What it does

- **Spawn parallel agents**: each one gets its own PTY, running `zerostack --parallel` under the hood
- **Live status tracking**: see at a glance who's working `[~]`, done `[✓]`, dead `[X]`, or waiting `[ ]`, with per-agent timers
- **Drop into any agent**: hit Enter on a process to see its full terminal output, keystrokes pass straight through
- **Status signal support**: multistack listens for `start`/`stop` signals from zerostack via Unix sockets, so timers and indicators stay accurate even across headless loop iterations

## Install

### Homebrew (recommended)

```bash
brew tap gi-dellav/tap
brew trust gi-dellav/tap   # required for Homebrew 6.0.0+
brew install multistack    # zerostack installed automatically
```

### Script

```bash
curl -fsSL https://raw.githubusercontent.com/gi-dellav/multistack/main/install.sh | bash
```

### Cargo

```bash
cargo install multistack
```

You need [zerostack](https://gi-dellav.github.io/zerostack/) on your PATH, plus a recent Rust toolchain.

## Keybindings

### Agent list (normal mode)

| Key | Action |
|---|---|
| `n` | Spawn new agent — prompts for a name, creates a git worktree |
| `N` | Spawn new agent in parallel mode — no prompt, jumps straight to TTY |
| `m` | Spawn bare agent — no worktree, no `--parallel`, jumps to TTY |
| `p` | Add project — opens directory picker |
| `l` | Remove project and all its agents |
| `h` | Open `lazygit` in the project (or agent worktree) directory |
| `s` | Open `$SHELL` in the project (or agent worktree) directory |
| `r` | Rename selected agent |
| `d` | Delete (kill) selected agent |
| `Enter` | Drop into the selected agent's TTY |
| `↑` / `↓` | Move selection |
| `PageUp` / `PageDown` | Jump to previous / next project header |
| `?` / `F1` | Scrollable help overlay (keybindings, statuses, tips) |
| `q` | Quit — shows confirmation with agent counts (`q`/`Enter` quits, `Esc`/`n` stays) |
| `Esc` | Quit (confirmation; git conflicts get a warning) |

### TTY view (agent / lazygit / shell)

| Key | Action |
|---|---|
| `Esc` | Return to agent list |
| *All other keys* | Forwarded to the underlying process |

### Prompt (new agent / rename / new project)

| Key | Action |
|---|---|
| `Enter` | Confirm |
| `Esc` | Cancel |
| `Backspace` | Delete last character |
| *Printable chars* | Append to input |

### Directory picker

| Key | Action |
|---|---|
| `Enter` | Add selected directory as a project |
| `Esc` | Cancel, return to agent list |
| `/` | Start filtering directories (type to narrow, `Esc` to clear) |
| *All other keys* | Navigate the file tree |

## Requirements

- **Linux/BSD/macOS** (uses Unix domain sockets and PTYs)
- **zerostack v1.5+** built with the `status-signals` feature (default from v1.5)
- Rust 1.85+ (2024 edition)

## License

GPL-3.0-only
