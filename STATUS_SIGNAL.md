# Status Signals — zerostack Unix Socket Protocol v1.0

## Overview

zerostack exposes agent lifecycle signals over a Unix domain socket, allowing external processes (status bars, daemons, UI wrappers) to track whether the agent is actively processing or idle.

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
cargo install --path . --debug --features status-signals
```

The `status-signals` feature is **not** in the default feature set and must be explicitly enabled.

### Runtime

```bash
zerostack --status-socket /tmp/zerostack.sock
```

The path must point to an **already-existing** Unix domain socket. zerostack will **never** create the socket — it only connects as a client.

## Protocol

### Message Format

Each message is a single ASCII line terminated by `\n`:

| Message   | Meaning                                   |
|-----------|-------------------------------------------|
| `start\n` | Agent run has begun (streaming or single) |
| `stop\n`  | Agent run has completed or was cancelled  |

No other messages are defined. The protocol is intentionally minimal — richer state is available via the [ACP server](#advanced-acp-server) for full session introspection.

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

### TUI Mode (additional triggers)

| Trigger                         | Signals             |
|---------------------------------|---------------------|
| Agent spawned for a new prompt  | `start`             |
| Agent cancelled (user hits Esc) | `stop`              |
| User invokes `/btw` command     | `stop` then `start` |
| Git worktree branch switch      | `stop` then `start` |
| Headless loop re-launch         | `stop` then `start` |

### Headless Loop (`--loop`)

Each iteration is wrapped in its own `start`/`stop` pair. Between iterations zerostack is idle.

### Single Prompt (`--print` / `-p`)

One `start`/`stop` pair around the single agent call.

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
                    "start" => println!("zerostack: started"),
                    "stop"  => println!("zerostack: stopped"),
                    _       => eprintln!("unknown: {line}"),
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
| Messages          | `start`, `stop`                               |
| Feature flag      | `status-signals`                              |
| CLI flag          | `--status-socket <PATH>`                      |
| Creation          | Listener must exist before zerostack runs     |
| Error behaviour   | Silent ignore (best-effort)                   |
| Platform          | Unix only (uses `std::os::unix::net`)         |