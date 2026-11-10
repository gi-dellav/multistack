<img width="3281" height="1171" alt="banner" src="https://github.com/user-attachments/assets/11f73a85-c12b-492f-8cff-1ef1369107d8" />

---

# Multistack

Run multiple [zerostack](https://gi-dellav.github.io/zerostack/) agents side by side in your terminal, allowing to spawn, watch, rename, kill all underlying processes.

Multistack is designed to be a native, lightweight and open-soruce competitor to [Conductor](https://www.conductor.build).


## What it does

- **Spawn parallel agents**: each one gets its own PTY, running `zerostack --parallel` under the hood
- **Local and remote projects**: run agents in a local directory or on an SSH server, with the same keybindings and status tracking
- **Live status tracking**: see at a glance who's working `[~]`, done `[✓]`, dead `[X]`, waiting `[ ]`, or waiting on you `[?]`, with per-agent timers
- **Drop into any agent**: hit Enter on a process to see its full terminal output, keystrokes pass straight through
- **Status signal support**: multistack listens for zerostack's `start` and `stop` signals over Unix sockets, so timers and indicators stay accurate even across headless loop iterations, and for the protocol v1.1 `blocked:<reason>` and `state:working` signals, so an agent parked on its permission prompt shows as `[?]`, raises a desktop notification, and stops its timer until you answer

## Agent status

| Glyph | Colour  | Meaning |
|-------|---------|---------|
| `[ ]` | gray    | waiting, the agent hasn't started yet |
| `[~]` | yellow  | working, the timer is running |
| `[?]` | cyan    | waiting on you, answer the prompt inside the agent |
| `[✓]` | green   | finished, `stop` signal received |
| `[X]` | red     | dead, the process exited |
| `[!]` | magenta | git conflict, resolve it before quitting |

See [STATUS_SIGNAL.md](STATUS_SIGNAL.md) for the wire protocol behind these.

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

## Remote projects

A project can live on an SSH server. Multistack keeps owning the UI and local
PTYs; each remote agent is a local `ssh` process running `zerostack` in the
remote directory. Worktrees, lazygit, `$SHELL`, setup, status signals and
timers all work the same way as for local projects.

Add one from the TUI with `P`, or at startup:

```bash
multistack --remote=deploy@buildbox:/srv/app
multistack --remote=buildbox:2222/srv/app --remote=user@other:/srv/other
```

The accepted form is `[user@]host[:port]/path` (or the equivalent
`ssh://[user@]host[:port]/path`). Paths must be absolute on the remote host.
Host aliases and options from `~/.ssh/config` are honored because multistack
shells out to the system OpenSSH client.

Remote status signals use OpenSSH Unix-socket forwarding
(`ssh -R remote_sock:local_sock`), so the server needs Unix-socket forwarding
enabled (`AllowStreamLocalForwarding yes`, the default). If the server disables
it, remote agents still run but their status stays `[ ]`.

Requirements on the remote host: zerostack (v1.5+ for status signals),
`lazygit`/`speck` if you use those actions, and a POSIX shell. Authentication
and host-key prompts appear inside the agent's TTY; press Enter on the agent to
answer them. To avoid repeated auth prompts across agents and tools, enable
`ControlMaster` in `~/.ssh/config`.

Use `--ssh-suffix` to pass extra flags to every ssh invocation:

```bash
multistack --ssh-suffix="-o ConnectTimeout=5" --ssh-suffix="-i /home/me/.ssh/build_key"
```

## Keybindings

### Agent list (normal mode)

| Key | Action |
|---|---|
| `n` | Spawn new agent — prompts for a name, creates a git worktree |
| `N` | Spawn new agent in parallel mode — no prompt, jumps straight to TTY |
| `m` | Spawn bare agent — no worktree, no `--parallel`, jumps to TTY |
| `p` | Add local project — opens directory picker |
| `P` | Add remote project — prompts for `[user@]host[:port]/path` |
| `l` | Remove project and all its agents |
| `h` | Open `lazygit` in the project (or agent worktree) directory |
| `s` | Open `$SHELL` in the project (or agent worktree) directory |
| `r` | Rename selected agent |
| `d` | Delete (kill) selected agent |
| `Enter` | Drop into the selected agent's TTY |
| `↑` / `↓` | Move selection |
| `PageUp` / `PageDown` | Jump to previous / next project header |
| `?` / `F1` | Scrollable help overlay (keybindings, statuses, tips) |
| `q` | Quit — shows confirmation with agent counts (`Enter` quits, `Esc`/`n` stays) |
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
- **zerostack v1.5+** built with the `status-signals` feature (default from v1.5). The `[?]` state needs a zerostack carrying status-signal protocol v1.1; older builds simply never send those lines and behave exactly as before. Minimum version: TBD, to be filled in at zerostack's next release
- Rust 1.85+ (2024 edition)

## License

GPL-3.0-only
