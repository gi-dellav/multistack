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

NOTE: Multistack is currently in beta stage, and requires *zerostack v1.5*, which is also in a beta stage.

```bash
cargo install zerostack --version 1.5.0-rc2   # Run if you don't have zerostack v1.5+
cargo install multistack --version 1.0.0-rc1
```

You need [zerostack](https://gi-dellav.github.io/zerostack/) on your PATH, plus a recent Rust toolchain.

## Keybindings

| Key | Action |
|---|---|
| `n` | Spawn a new agent (prompts for a name) |
| `Enter` | Open the selected agent's TTY |
| `r` | Rename selected agent |
| `k` | Kill selected agent |
| `↑` / `↓` | Move selection |
| `Esc` | Leave TTY / dismiss prompt / quit |
| `q` | Quit |

Inside a TTY view, all keystrokes forward to the agent. Press `Esc` to go back to the list.

## Requirements

- **Linux/BSD/macOS** (uses Unix domain sockets and PTYs)
- **zerostack v1.5+** built with the `status-signals` feature (default from v1.5)
- Rust 1.85+ (2024 edition)

## License

GPL-3.0-only
