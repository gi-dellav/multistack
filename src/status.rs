use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify_rust::Notification;
use parking_lot::Mutex;

pub const STATUS_NOT_YET: u8 = 0;
pub const STATUS_WORKING: u8 = 1;
pub const STATUS_FINISHED: u8 = 2;
pub const STATUS_DEAD: u8 = 3;
pub const STATUS_GIT_CONFLICT: u8 = 4;
/// Status-signal protocol v1.1: zerostack is waiting on a human decision
/// (`blocked:<reason>`), and will resume the same run on `state:working`.
pub const STATUS_BLOCKED: u8 = 5;

pub fn status_prefix(status: u8) -> &'static str {
    match status {
        STATUS_NOT_YET => "[ ]",
        STATUS_WORKING => "[~]",
        STATUS_FINISHED => "[✓]",
        STATUS_DEAD => "[X]",
        STATUS_GIT_CONFLICT => "[!]",
        STATUS_BLOCKED => "[?]",
        _ => "[ ]",
    }
}

pub fn status_color(status: u8) -> ratatui::style::Color {
    use ratatui::style::Color;
    match status {
        STATUS_NOT_YET => Color::Gray,
        STATUS_WORKING => Color::Yellow,
        STATUS_FINISHED => Color::Green,
        STATUS_DEAD => Color::Red,
        STATUS_GIT_CONFLICT => Color::Magenta,
        STATUS_BLOCKED => Color::Cyan,
        _ => Color::Gray,
    }
}

pub fn format_timer(active_ms: u64, cycle_start: &Option<Instant>) -> String {
    let total_ms = active_ms
        + cycle_start
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);
    let total_secs = total_ms / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, mins, secs)
    } else {
        format!("{}:{:02}", mins, secs)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_status_listener(
    status: Arc<AtomicU8>,
    active_ms: Arc<AtomicU64>,
    cycle_start: Arc<Mutex<Option<Instant>>>,
    has_unread: Arc<AtomicBool>,
    activity_dot_enabled: bool,
    socket_path: String,
    process_name: Arc<Mutex<String>>,
    project_dir: String,
) -> (Arc<AtomicBool>, JoinHandle<()>) {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    let handle = std::thread::spawn(move || {
        let _ = std::fs::remove_file(&socket_path);
        let listener = match UnixListener::bind(&socket_path) {
            Ok(l) => l,
            Err(_) => return,
        };
        listener.set_nonblocking(true).ok();

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Accepted sockets inherit the listener's non-blocking
                    // mode: a client connects first and writes a moment
                    // later, so a single non-blocking `read()` would usually
                    // hit `WouldBlock` and drop the signal. Force blocking
                    // with a short timeout and drain to EOF instead.
                    let mut stream: UnixStream = stream;
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    let mut data = Vec::new();
                    match stream.read_to_end(&mut data) {
                        Ok(0) => continue, // spurious connect, no payload
                        Ok(_) => {}
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            continue;
                        }
                        Err(_) => continue,
                    }
                    if data.is_empty() {
                        continue;
                    }

                    let text = String::from_utf8_lossy(&data);
                    for raw in text.lines() {
                        // Trim `\r` too: a client writing `start\r\n`
                        // would otherwise never match.
                        let line = raw.trim();
                        match line {
                            "start" => {
                                // Hold the timer lock across both writes. A
                                // `sync_statuses` that slipped between them
                                // would credit the still-empty cycle, store
                                // DEAD, and then be handed a running timer
                                // that every later sync skips, leaving the
                                // panel counting up for a dead agent.
                                let mut cycle = cycle_start.lock();
                                status.store(STATUS_WORKING, Ordering::SeqCst);
                                *cycle = Some(Instant::now());
                            }
                            "stop" => {
                                // Same lock scope as `start` / `state:working`:
                                // crediting the cycle and scanning the status
                                // CAS have to reach `sync_statuses` as one
                                // step. A `sync_statuses` that slipped between
                                // the credit and the scan would see a dead
                                // process with no open cycle, store DEAD, and
                                // fail every compare_exchange below, stranding
                                // the panel at [X] with no "Agent finished"
                                // notification even though `stop` did arrive.
                                let transitioned = {
                                    let mut cycle = cycle_start.lock();
                                    if let Some(start) = cycle.take() {
                                        let elapsed = start.elapsed();
                                        active_ms.fetch_add(
                                            elapsed.as_millis() as u64,
                                            Ordering::SeqCst,
                                        );
                                    }
                                    // A fast run may deliver `stop` while we
                                    // are still NOT_YET (no `start` observed
                                    // yet), and a run that ends on its
                                    // permission prompt delivers it while we
                                    // are still BLOCKED.
                                    [STATUS_WORKING, STATUS_NOT_YET, STATUS_BLOCKED]
                                        .into_iter()
                                        .any(|from| {
                                            status
                                                .compare_exchange(
                                                    from,
                                                    STATUS_FINISHED,
                                                    Ordering::SeqCst,
                                                    Ordering::SeqCst,
                                                )
                                                .is_ok()
                                        })
                                };
                                if transitioned {
                                    if activity_dot_enabled {
                                        has_unread.store(true, Ordering::SeqCst);
                                    }
                                    crate::process::run_speck_apply_if_present(&project_dir);
                                    let name = process_name.lock().clone();
                                    let _ = Notification::new()
                                        .summary("Agent finished")
                                        .body(&format!("{} has completed", name))
                                        .show();
                                }
                            }
                            "git-conflict" => {
                                // Same lock scope as `start` / `state:working`
                                // / `stop`: freeze the timer at the conflict
                                // moment and publish the status as one step,
                                // so `sync_statuses` can't land between the
                                // credit and the store.
                                {
                                    let mut cycle = cycle_start.lock();
                                    if let Some(start) = cycle.take() {
                                        let elapsed = start.elapsed();
                                        active_ms.fetch_add(
                                            elapsed.as_millis() as u64,
                                            Ordering::SeqCst,
                                        );
                                    }
                                    status.store(STATUS_GIT_CONFLICT, Ordering::SeqCst);
                                }
                                let name = process_name.lock().clone();
                                let _ = Notification::new()
                                    .summary("Git conflict")
                                    .body(&format!(
                                        "{} needs your attention — resolve the Git conflict",
                                        name
                                    ))
                                    .show();
                            }
                            // Protocol v1.1: the wait reported by an earlier
                            // `blocked:<reason>` is over and the same run
                            // continues, so there is no new `start` to
                            // observe. Only BLOCKED may transition here: a
                            // stray `state:working` must not resurrect a run
                            // that already stopped or died.
                            "state:working" => {
                                // Same lock scope as `start`: the transition
                                // and the timer install have to reach
                                // `sync_statuses` as one step.
                                let mut cycle = cycle_start.lock();
                                if status
                                    .compare_exchange(
                                        STATUS_BLOCKED,
                                        STATUS_WORKING,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    *cycle = Some(Instant::now());
                                }
                            }
                            // Protocol v1.1: `blocked:<reason>`. Match on the
                            // prefix so reasons added by later protocol
                            // versions land here too; the reason itself only
                            // feeds the notification body.
                            line if line.starts_with("blocked:") => {
                                // Freeze the timer for the whole wait (the
                                // agent is idle until `state:working`) and
                                // publish the status under the same lock
                                // scope as the other arms, so `sync_statuses`
                                // can't land between the credit and the
                                // store.
                                {
                                    let mut cycle = cycle_start.lock();
                                    if let Some(start) = cycle.take() {
                                        let elapsed = start.elapsed();
                                        active_ms.fetch_add(
                                            elapsed.as_millis() as u64,
                                            Ordering::SeqCst,
                                        );
                                    }
                                    status.store(STATUS_BLOCKED, Ordering::SeqCst);
                                }
                                let name = process_name.lock().clone();
                                let reason = line["blocked:".len()..].trim();
                                let body = if reason.is_empty() {
                                    format!("{} is waiting for your decision", name)
                                } else {
                                    format!("{} is waiting for a {} decision", name, reason)
                                };
                                let _ = Notification::new()
                                    .summary("Agent needs you")
                                    .body(&body)
                                    .show();
                            }
                            _ => {}
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if shutdown_clone.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => {
                    if shutdown_clone.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });

    (shutdown, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_status_prefix_all_variants() {
        assert_eq!(status_prefix(STATUS_NOT_YET), "[ ]");
        assert_eq!(status_prefix(STATUS_WORKING), "[~]");
        assert_eq!(status_prefix(STATUS_FINISHED), "[✓]");
        assert_eq!(status_prefix(STATUS_DEAD), "[X]");
        assert_eq!(status_prefix(STATUS_GIT_CONFLICT), "[!]");
        assert_eq!(status_prefix(99), "[ ]");
    }

    #[test]
    fn test_format_timer_zero() {
        assert_eq!(format_timer(0, &None), "0:00");
    }

    #[test]
    fn test_format_timer_seconds_only() {
        assert_eq!(format_timer(45_000, &None), "0:45");
    }

    #[test]
    fn test_format_timer_minutes_and_seconds() {
        assert_eq!(format_timer(125_000, &None), "2:05");
    }

    #[test]
    fn test_format_timer_hours() {
        assert_eq!(format_timer(3_660_000, &None), "1:01:00");
    }

    #[test]
    fn test_format_timer_with_cycle_start() {
        let now = Instant::now();
        let start = now.checked_sub(Duration::from_millis(1500)).unwrap();
        let total = format_timer(30_000, &Some(start));
        assert!(total.starts_with("0:31"));
    }

    #[test]
    fn test_format_timer_large_hours() {
        assert_eq!(format_timer(36_000_000, &None), "10:00:00");
    }

    #[test]
    fn test_format_timer_padding() {
        assert_eq!(format_timer(5_000, &None), "0:05");
        assert_eq!(format_timer(65_000, &None), "1:05");
        assert_eq!(format_timer(3_605_000, &None), "1:00:05");
    }

    /// A listener bound to a private temp-dir socket, plus the state it
    /// mutates. Multistack is the server here: zerostack connects per
    /// message, so the tests play the client.
    struct Harness {
        dir: std::path::PathBuf,
        socket_path: String,
        status: Arc<AtomicU8>,
        active_ms: Arc<AtomicU64>,
        cycle_start: Arc<Mutex<Option<Instant>>>,
        has_unread: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Harness {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "multistack-status-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let socket_path = dir.join("status.sock").to_string_lossy().into_owned();

            let status = Arc::new(AtomicU8::new(STATUS_NOT_YET));
            let active_ms = Arc::new(AtomicU64::new(0));
            let cycle_start = Arc::new(Mutex::new(None));
            let has_unread = Arc::new(AtomicBool::new(false));
            let (shutdown, handle) = spawn_status_listener(
                status.clone(),
                active_ms.clone(),
                cycle_start.clone(),
                has_unread.clone(),
                true,
                socket_path.clone(),
                Arc::new(Mutex::new("agent-1".to_string())),
                dir.to_string_lossy().into_owned(),
            );

            // The listener binds on its own thread; wait for the socket.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !std::path::Path::new(&socket_path).exists() {
                assert!(Instant::now() < deadline, "listener never bound the socket");
                std::thread::sleep(Duration::from_millis(10));
            }

            Self {
                dir,
                socket_path,
                status,
                active_ms,
                cycle_start,
                has_unread,
                shutdown,
                handle: Some(handle),
            }
        }

        /// One message, one connect-write-close, exactly as zerostack sends.
        fn send(&self, msg: &str) {
            use std::io::Write;
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match UnixStream::connect(&self.socket_path) {
                    Ok(mut stream) => {
                        stream.write_all(format!("{msg}\n").as_bytes()).unwrap();
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        return;
                    }
                    Err(e) => {
                        assert!(Instant::now() < deadline, "connect failed: {e}");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }

        fn wait_for_status(&self, want: u8) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let got = self.status.load(Ordering::SeqCst);
                if got == want {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "expected status {want}, still {got}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Poll until a condition holds. `wait_for_status` returns the
        /// instant the status atomic changes, but the listener writes the
        /// unread flag (and anything else a message implies) after it, so
        /// every assertion on a post-status write has to poll for the write
        /// itself instead of riding on the status change.
        fn wait_until(&self, what: &str, cond: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if cond() {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Give the listener time to process a line we expect to be a no-op.
        /// Its accept loop backs off 100ms between polls, so wait longer.
        fn settle(&self) {
            std::thread::sleep(Duration::from_millis(500));
        }

        fn status(&self) -> u8 {
            self.status.load(Ordering::SeqCst)
        }

        /// Whether a cycle is currently being timed. Taking the lock is
        /// what makes this safe to call straight after `wait_for_status`:
        /// the listener installs a timer while holding this same lock over
        /// the status change, so a reader that gets the lock is looking at
        /// state the listener has finished writing.
        fn cycle_running(&self) -> bool {
            self.cycle_start.lock().is_some()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn test_listener_permission_wait_bracket() {
        let h = Harness::new("bracket");

        h.send("start");
        h.wait_for_status(STATUS_WORKING);
        assert!(h.cycle_running(), "start must run the timer");

        std::thread::sleep(Duration::from_millis(20));
        h.send("blocked:permission");
        h.wait_for_status(STATUS_BLOCKED);
        assert!(!h.cycle_running(), "blocked must freeze the timer");
        let banked = h.active_ms.load(Ordering::SeqCst);
        assert!(banked > 0, "blocked must bank the elapsed cycle");
        assert!(
            !h.has_unread.load(Ordering::SeqCst),
            "blocked is not an unread result"
        );

        h.send("state:working");
        h.wait_for_status(STATUS_WORKING);
        assert!(h.cycle_running(), "state:working must restart the timer");
        assert!(
            !h.has_unread.load(Ordering::SeqCst),
            "state:working is not a run boundary and raises no dot"
        );

        h.send("stop");
        h.wait_for_status(STATUS_FINISHED);
        assert!(!h.cycle_running(), "stop must freeze the timer");
        assert!(h.active_ms.load(Ordering::SeqCst) >= banked);
        h.wait_until("stop to raise the unread dot", || {
            h.has_unread.load(Ordering::SeqCst)
        });
    }

    /// The bug this pins: `start` used to store WORKING and only then
    /// install the timer, so a `sync_statuses` landing between the two
    /// credited an empty cycle, stored DEAD, and left a running timer that
    /// no later sync ever freezes. Holding the timer lock from the test
    /// thread stands in for that window: while it is held the listener must
    /// not have published the status either.
    #[test]
    fn test_listener_start_installs_status_and_timer_together() {
        let h = Harness::new("start-atomic");

        let guard = h.cycle_start.lock();
        h.send("start");
        h.settle();
        assert_eq!(
            h.status(),
            STATUS_NOT_YET,
            "start must not publish WORKING before it can install the timer"
        );
        drop(guard);

        h.wait_for_status(STATUS_WORKING);
        assert!(h.cycle_running(), "start must run the timer");
    }

    /// Same serialisation for the v1.1 resume: `state:working` must not
    /// leave WORKING visible without the timer that belongs to it.
    #[test]
    fn test_listener_state_working_installs_status_and_timer_together() {
        let h = Harness::new("resume-atomic");

        h.send("start");
        h.wait_for_status(STATUS_WORKING);
        h.send("blocked:permission");
        h.wait_for_status(STATUS_BLOCKED);

        let guard = h.cycle_start.lock();
        h.send("state:working");
        h.settle();
        assert_eq!(
            h.status(),
            STATUS_BLOCKED,
            "state:working must not publish WORKING before it can install the timer"
        );
        drop(guard);

        h.wait_for_status(STATUS_WORKING);
        assert!(h.cycle_running(), "state:working must restart the timer");
    }

    /// Same serialisation for `stop`: the bug this pins is `sync_statuses`
    /// landing between the credit and the compare_exchange scan (the lock
    /// used to be released in between), seeing a dead process with no open
    /// cycle, storing DEAD, and failing every compare_exchange, which
    /// stranded a run that did report `stop` at [X] with no "Agent
    /// finished" notification. Holding the timer lock from the test thread
    /// stands in for that window: while it is held the listener must not
    /// have published FINISHED either.
    #[test]
    fn test_listener_stop_credits_and_finishes_together() {
        let h = Harness::new("stop-atomic");

        h.send("start");
        h.wait_for_status(STATUS_WORKING);

        let guard = h.cycle_start.lock();
        h.send("stop");
        h.settle();
        assert_eq!(
            h.status(),
            STATUS_WORKING,
            "stop must not publish FINISHED before it can credit and scan the timer"
        );
        drop(guard);

        h.wait_for_status(STATUS_FINISHED);
        assert!(!h.cycle_running(), "stop must clear the timer");
        h.wait_until("stop to raise the unread dot", || {
            h.has_unread.load(Ordering::SeqCst)
        });
    }

    #[test]
    fn test_listener_stop_while_blocked_finishes() {
        let h = Harness::new("stop-blocked");

        h.send("start");
        h.wait_for_status(STATUS_WORKING);
        h.send("blocked:permission");
        h.wait_for_status(STATUS_BLOCKED);

        // A run that dies or finishes on its prompt must not strand the
        // panel at [?].
        h.send("stop");
        h.wait_for_status(STATUS_FINISHED);
        assert!(!h.cycle_running());
        h.wait_until("stop from blocked to keep the finished behaviour", || {
            h.has_unread.load(Ordering::SeqCst)
        });
    }

    #[test]
    fn test_listener_state_working_without_blocked_is_ignored() {
        let h = Harness::new("state-only");

        h.send("state:working");
        h.settle();
        assert_eq!(h.status(), STATUS_NOT_YET, "state:working is not a start");
        assert!(!h.cycle_running(), "state:working must not start a timer");

        // The listener is still alive and still handling known lines.
        h.send("start");
        h.wait_for_status(STATUS_WORKING);
    }

    #[test]
    fn test_listener_unknown_line_is_ignored() {
        let h = Harness::new("unknown");

        h.send("start");
        h.wait_for_status(STATUS_WORKING);

        h.send("something-else:v2");
        h.settle();
        assert_eq!(h.status(), STATUS_WORKING, "unknown lines change nothing");
        assert!(h.cycle_running(), "unknown lines leave the timer alone");

        h.send("blocked:permission");
        h.wait_for_status(STATUS_BLOCKED);
    }

    #[test]
    fn test_listener_blocked_matches_any_reason() {
        let h = Harness::new("reason");

        h.send("start");
        h.wait_for_status(STATUS_WORKING);

        // Reasons reserved for later protocol versions still block.
        h.send("blocked:somethingelse");
        h.wait_for_status(STATUS_BLOCKED);
        assert!(!h.cycle_running());

        h.send("state:working");
        h.wait_for_status(STATUS_WORKING);
        assert!(h.cycle_running());
    }
}
