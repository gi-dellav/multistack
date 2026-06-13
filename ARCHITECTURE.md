# Architecture

This file documents the high-level architecture of **multistack**, an open-source lightweight TUI for parallel AI agent management.

## Directory Layout

```
multistack/
├── src/
│   ├── main.rs       # Entry point, tokio runtime, event loop, Mode enum
│   ├── input.rs      # Keyboard/event dispatch, modal keybindings, key→bytes encoding
│   ├── process.rs    # Process struct, PTY spawn/teardown, vt100 parser lifecycle
│   ├── project.rs    # Project struct, ListEntry enum, view-model construction
│   ├── status.rs     # Status constants, formatting, Unix-socket status listener thread
│   └── ui.rs         # Ratatui rendering for all modes, layout, styling
├── reference_examples/  # Standalone examples (not compiled) — PTY, crossterm, vt100, ratatui
├── .github/workflows/   # CI (ci.yml) and release (release.yml)
├── Cargo.toml           # Single binary crate, no workspace
└── STATUS_SIGNAL.md     # Protocol spec for agent↔UI status communication
```

## Key Types and Traits

**No user-defined traits exist.** The system uses concrete structs and enums with plain functions.

### Core Structs

- **`Process`** (`process.rs:14-30`) — Central abstraction. Wraps a PTY child process with its master/writer handles, a shared `vt100::Parser` (screen buffer), and atomic status/timing/alive flags shared across threads. Implements `Drop` for clean shutdown.

- **`Project`** (`project.rs:3-7`) — Groups agents by working directory: `{ id, name, directory }`.

- **`ListEntry`** (`project.rs:9-12`) — Flattened enum: `ProjectHeader(usize)` or `Agent(usize)`. Built from projects+processes for the TUI navigable list.

### State Machine

- **`Mode`** (`main.rs:23-42`) — The application state machine with five variants:
  - `Normal { selected }` — browse agent list
  - `Tty { process_id }` — view/interact with a managed agent's terminal
  - `TempTty { process, previous_selected }` — view a transient shell (not a managed agent)
  - `Prompt { purpose, selected, input }` — text input for naming/renaming/new project
  - `DirPicker { explorer, previous_selected }` — directory selection via ratatui-explorer

- **`PromptPurpose`** (`main.rs:17-21`) — Why a prompt is open: `NewProcess(project_id)`, `NewProject`, `Rename(process_id)`.

### Status System

Five `u8` constants in `status.rs:12-16`: `STATUS_NOT_YET(0)`, `STATUS_WORKING(1)`, `STATUS_FINISHED(2)`, `STATUS_DEAD(3)`, `STATUS_GIT_CONFLICT(4)`. Helper functions map status to display prefix (`[ ]`, `[~]`, `[✓]`, `[X]`, `[!]`), ratatui colors, and formatted timers.

## Control Flow

### Event Loop (`main.rs:85-195`)

A single-threaded tokio `current_thread` runtime runs a `select!` over two futures:

1. **Render tick** — `tokio::time::interval(50ms)` triggers `ui::render()` and `process::sync_statuses()`.
2. **Input stream** — `crossterm::EventStream` produces keyboard/resize events dispatched to `input::process_event()`.

`process_event()` returns `Ok(true)` to signal the loop should exit (quit requested).

### Modal Input Dispatch (`input.rs`)

`process_event()` pattern-matches `Event::Resize` (resize all PTY masters + vt100 parsers) or `Event::Key`. Key events delegate to `process_key()`, which `match`es on `Mode` — each mode handles the same keypress differently. Key-to-byte translation (`key_to_bytes()`) encodes keystrokes as raw escape/control sequences for forwarding to PTY masters in Tty/TempTty modes.

### Process Lifecycle (`process.rs`)

- **`spawn_pty()`** (line 79) — Low-level factory: opens a PTY pair, spawns a child via `portable_pty`, starts a reader thread that feeds raw bytes into `vt100::Parser`.
- **`spawn_process()`** (line 141) — Higher-level wrapper: calls `spawn_pty()`, assigns an ID, optionally attaches a status-socket listener.
- **`check_tty_alive()`** (line 49) — Polls child process liveness; if dead, cleans up and returns the previous selected index for UI restoration.

## Data Flow

### Terminal Output Pipeline

```
PTY child process → PTY master reader thread → vt100::Parser.process(bytes) → screen grid
                                                                                    ↓
                                                              ui::render_tty() reads screen().contents_formatted()
                                                              and writes directly to stdout (bypassing ratatui buffer)
```

### Status Communication (Actor Pattern)

```
zerostack agent → writes "start"/"stop"/"git-conflict" to Unix socket
                                                                    ↓
status.rs spawn_status_listener() background thread reads socket
                                                                    ↓
          Updates shared Arc<AtomicU8> status, Arc<AtomicU64> active_ms,
          Arc<Mutex<Option<Instant>>> cycle_start
                                                                    ↓
          main loop reads atomics each frame → ui renders status icon + timer
```

No Rust channels are used. Inter-thread communication is exclusively through shared atomic state (`Arc<AtomicBool/U8/U64>`) and `parking_lot::Mutex` (for the cycle timer).

### View-Model Assembly

`project::build_entries()` flattens `Vec<Project>` + `Vec<Process>` into `Vec<ListEntry>`. `ProjectHeader` entries appear only when multiple projects exist. The UI list widget renders each entry with `process_item()` (status prefix, color, timer, truncated name).

## Design Decisions

- **Single-threaded async** — Tokio `current_thread` rather than multi-threaded. All UI and state is single-owner; atomics serve background threads (PTY reader, status listener). Avoids synchronization complexity.
- **No configuration** — Zero CLI flags, zero config files. Initial context from `std::env::current_dir()` and `$SHELL`. All state is runtime-constructed through the TUI. Persistence is explicitly out of scope.
- **Modal UI** — Vim-inspired modes (Normal, Insert/Tty, Prompt) mapped via the `Mode` enum. Each mode has its own keybinding table in `input.rs`.
- **Direct terminal rendering in TTY mode** — `render_tty()` writes vt100-parsed screen content directly to stdout, bypassing ratatui's framebuffer. This gives pixel-perfect terminal emulation for the active agent.
- **Shared-memory actor pattern** — Status listener thread and PTY reader threads communicate with the main loop via `Arc<Atomic*>` rather than channels. Simple, lock-free for reads.
- **No user-defined traits** — The system is small enough (~60KB of source) that concrete types with plain functions suffice.
- **Hardcoded agent** — Spawns `zerostack` with specific flags. Not a generic process manager; tightly coupled to the zerostack agent workflow.

## External Dependencies

| Crate | Purpose |
|-------|---------|
| `ratatui` 0.30 | TUI framework (layout, widgets, styling) |
| `crossterm` 0.29 | Terminal backend (raw mode, events, cursor control) |
| `tokio` 1 (full) | Async runtime powering the event loop |
| `portable-pty` 0.9 | Cross-platform PTY pair creation and child process spawning |
| `vt100` 0.16 | ANSI terminal parser — transforms raw bytes into screen grid |
| `ratatui-explorer` 0.3 | File/directory explorer widget (git dependency) |
| `notify-rust` 4.17 | Desktop notifications on agent state changes |
| `parking_lot` 0.12 | Faster `Mutex` for `vt100::Parser` and cycle timer |
| `futures` 0.3 | `StreamExt` trait for async event stream consumption |

## Entry Points

- **`src/main.rs:fn main()`** — The sole entry point. Creates a `current_thread` tokio runtime and calls `run()`, which initializes the terminal, spawns the initial project/process, and enters the event loop. No subcommands, no arguments.
