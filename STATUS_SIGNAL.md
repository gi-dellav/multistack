# Status Signals: zerostack Unix Socket Protocol v1.1

## Overview

zerostack exposes agent lifecycle signals over a Unix domain socket, allowing external processes (status bars, daemons, UI wrappers) to track whether the agent is actively processing, idle, or waiting on a human.

zerostack's own `docs/STATUS_SIGNALS.md` is the canonical protocol reference. This file documents the protocol as multistack consumes it; if the two ever disagree, zerostack's document wins.

The protocol is **one-directional**: zerostack connects to a pre-existing Unix socket as a client and writes plain-text messages. The external process acts as the server — it creates, binds, and listens on the socket.

```
┌──────────────┐     connects     ┌──────────────────┐
│  zerostack   │ ───────────────> │  your listener   │
│  (client)    │   writes msgs    │  (server)        │
└──────────────┘                  └──────────────────┘
```

## Enabling

### Build

```bash
cargo install --path .
```

The `status-signals` feature has been part of zerostack's **default** feature set since zerostack v1.5, so no extra build flags are needed. Older builds, and builds made with `--no-default-features`, need it enabled explicitly with `--features status-signals`.

### Runtime

```bash
zerostack --status-socket /tmp/zerostack.sock
```

The path must point to an **already-existing** Unix domain socket. zerostack will **never** create the socket — it only connects as a client.

## Protocol

### Message Format

Each message is a single ASCII line terminated by `\n`:

| Message                | Since | Meaning                                   |
|------------------------|-------|-------------------------------------------|
| `start\n`              | v1.0  | Agent run has begun (streaming or single) |
| `stop\n`               | v1.0  | Agent run has completed or was cancelled  |
| `git-conflict\n`       | v1.0  | Agent is blocked, the user must resolve a Git conflict |
| `blocked:permission\n` | v1.1  | The interactive permission prompt is on screen and zerostack is waiting for a human decision |
| `state:working\n`      | v1.1  | A wait reported by `blocked:<reason>` has ended and zerostack is working again |

`blocked:<reason>` and `state:<state>` are lowercase ASCII tokens with no whitespace. Protocol v1.1 defines exactly one reason, `permission`, and exactly one state, `working`; further reasons and states are reserved for later protocol versions. Match on the `blocked:` prefix rather than the full literal, so a reason added later still reads as "waiting on the user".

No other messages are defined. The protocol is intentionally minimal: richer state is available via the [ACP server](#advanced-acp-server) for full session introspection.

### Ordering Guarantees

A permission wait is bracketed in exactly this order:

```
start -> blocked:permission -> state:working -> ... -> stop
```

`blocked:permission` is sent only once the prompt is visible, and `state:working` only once the decision has been taken, for every outcome the prompt accepts (allow once, allow always, deny, Esc), including the error paths. The pair is always balanced.

### Run Boundary Invariant

A `blocked:<reason>` or `state:<state>` message never adds or removes a `start` or a `stop`. Filter every line that is not `start`, `stop`, or `git-conflict` out of a v1.1 stream and what remains is byte for byte the v1.0 stream for that same turn. A v1.0 listener that ignores unknown lines therefore behaves identically against a v1.1 sender, and `state:working` must never be treated as the start of a new run.

Only the interactive TUI emits the v1.1 messages. Headless modes (`-p` and `--loop`) pass no ask channel to the permission checker, so no prompt is ever drawn and no permission wait exists to report.

### Socket Lifecycle

1. **Before launch**: Your process creates a Unix socket at your chosen path, binds it, and calls `listen()`.
2. **During use**: zerostack connects on each `send_start()` / `send_stop()` call, writes the message, and disconnects. The socket file persists.
3. **After shutdown**: Your process is responsible for closing the listener and `unlink()`ing the socket file.

### Error Handling

zerostack silently ignores all errors from the Unix socket (connection refused, broken pipe, path not found). If your listener isn't running, zerostack functions normally — the signals are best-effort.

## When Signals Fire

### All Modes

| Trigger                         | Signals             |
|---------------------------------|---------------------|
| Agent run begins (any mode)     | `start`             |
| Agent run completes (any mode)  | `stop`              |
| Agent run errors out            | `stop`              |
| Agent hits a Git merge conflict | `git-conflict`      |

### TUI Mode (additional triggers)

| Trigger                            | Signals                                |
|------------------------------------|----------------------------------------|
| Agent spawned for a new prompt     | `start`                                |
| Agent cancelled (user hits Esc)    | `stop`                                 |
| User invokes `/btw` command        | `stop` then `start`                    |
| Git worktree branch switch         | `stop` then `start`                    |
| Headless loop re-launch            | `stop` then `start`                    |
| Permission prompt drawn (v1.1)     | `blocked:permission`                   |
| Permission decision taken (v1.1)   | `state:working`, no new `start`        |

### Headless Loop (`--loop`)

Each iteration is wrapped in its own `start`/`stop` pair. Between iterations zerostack is idle.

### Single Prompt (`--print` / `-p`)

One `start`/`stop` pair around the single agent call.

## How Multistack Reacts

| Message              | Glyph | Timer                                   | Notification |
|----------------------|-------|-----------------------------------------|--------------|
| `start`              | `[~]` | starts a cycle                          | none |
| `stop`               | `[✓]` | banks the cycle                         | "Agent finished" |
| `git-conflict`       | `[!]` | banks the cycle, stays frozen           | "Git conflict" |
| `blocked:<reason>`   | `[?]` | banks the cycle, frozen for the wait    | "Agent needs you" |
| `state:working`      | `[~]` | starts a new cycle, same run            | none |

`state:working` only moves an agent out of `[?]`; it never revives one that already stopped or died, and it raises no unread dot because it is not a run boundary. `stop` is accepted from `[?]` as well, so an agent that ends on its prompt still lands on `[✓]`. If the process exits while blocked, multistack marks it `[X]`: the prompt died with it.

## Building a Listener

### Minimal Python Example

```python
import os
import socket
import sys

SOCKET_PATH = "/tmp/zerostack.sock"

# Clean up stale socket
try:
    os.unlink(SOCKET_PATH)
except OSError:
    pass

# Create server
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(SOCKET_PATH)
server.listen(1)

print(f"Listening on {SOCKET_PATH}")
is_running = False

while True:
    conn, _ = server.accept()
    data = conn.recv(1024).decode().strip()
    conn.close()

    for msg in data.split("\n"):
        msg = msg.strip()
        if msg == "start":
            is_running = True
            print("zerostack: started")
        elif msg == "stop":
            is_running = False
            print("zerostack: stopped")
        elif msg == "git-conflict":
            print("zerostack: git conflict — needs user attention")
        elif msg.startswith("blocked:"):
            reason = msg.split(":", 1)[1]
            print(f"zerostack: waiting on you ({reason})")
        elif msg == "state:working":
            # Same run, not a new one: do not reset your run counters here.
            print("zerostack: back to work")
```

### Minimal Rust Example (tokio)

```rust
use tokio::net::UnixListener;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let path = "/tmp/zerostack.sock";
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    println!("Listening on {path}");

    loop {
        let (mut stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            let n = match stream.try_read(&mut buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            let msg = String::from_utf8_lossy(&buf[..n]);
            for line in msg.lines() {
                match line {
                    "start"         => println!("zerostack: started"),
                    "stop"          => println!("zerostack: stopped"),
                    "git-conflict"  => println!("zerostack: git conflict — needs user attention"),
                    // Same run continues, so no new `start` follows.
                    "state:working" => println!("zerostack: back to work"),
                    // Prefix match: later versions add more reasons.
                    l if l.starts_with("blocked:") => {
                        println!("zerostack: waiting on you ({})", &l["blocked:".len()..])
                    }
                    _               => eprintln!("unknown: {line}"),
                }
            }
        });
    }
}
```

### Edge Cases to Handle

- **Multiple messages in one read**: zerostack may fire `stop` and `start` in quick succession (e.g., `/btw`). Both may arrive in a single `recv()` call. Always split on `\n`.
- **Spurious connects with no data**: zerostack may connect and immediately disconnect. Treat this as a no-op.
- **Socket not cleaned up on crash**: If zerostack is killed, the socket file from a previous listener may persist. Call `unlink()` before binding.
- **zerostack sends start but never stop**: If zerostack crashes mid-run, no `stop` message will be sent. Use a watchdog timer: if `start` was received with no `stop` within N seconds, consider the agent lost.
- **Unknown lines**: Ignore anything you do not recognise. This is what keeps a listener working across protocol versions.
- **`state:working` without a preceding `blocked:`**: Possible on error paths, and after a `stop` on some of them. The protocol itself only asks that the last recognised `state:<state>` win. Multistack applies a stricter policy of its own, treating `state:working` as a no-op unless the agent is actually blocked, so that a stray `state:working` after `stop` cannot resurrect a finished agent. Either way, `stop` stays the run boundary.
- **Blocked agent dies**: A permission prompt dies with the process it belongs to, so drop the blocked display when the agent exits. Multistack marks such an agent dead rather than leaving it at `[?]`.

## Advanced: ACP Server

For richer introspection beyond `start`/`stop`, zerostack offers the **Agent Communication Protocol (ACP)** server (feature `acp`):

```bash
zerostack --acp --acp-host 127.0.0.1 --acp-port 7243
```

ACP provides structured bidirectional communication over TCP or stdio, including:
- Full session creation and management
- Per-token streaming of agent output
- Tool call and tool result notifications
- Reasoning block visibility

The ACP protocol uses the `agent-client-protocol` crate's schema. See `src/extras/acp/mod.rs` for the implementation.

## Reference

| Aspect            | Detail                                        |
|-------------------|-----------------------------------------------|
| Transport         | Unix domain socket, `SOCK_STREAM`             |
| Direction         | zerostack connects to listener (client role)  |
| Encoding          | ASCII lines, `\n` delimited                   |
| Messages (v1.0)   | `start`, `stop`, `git-conflict`               |
| Messages (v1.1)   | `blocked:permission`, `state:working`         |
| Feature flag      | `status-signals` (default since zerostack v1.5) |
| CLI flag          | `--status-socket <PATH>`                      |
| Creation          | Listener must exist before zerostack runs     |
| Error behaviour   | Silent ignore (best-effort)                   |
| Platform          | Unix only (uses `std::os::unix::net`)         |