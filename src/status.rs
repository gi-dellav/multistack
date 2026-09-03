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

pub fn status_prefix(status: u8) -> &'static str {
    match status {
        STATUS_NOT_YET => "[ ]",
        STATUS_WORKING => "[~]",
        STATUS_FINISHED => "[✓]",
        STATUS_DEAD => "[X]",
        STATUS_GIT_CONFLICT => "[!]",
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

pub fn spawn_status_listener(
    status: Arc<AtomicU8>,
    active_ms: Arc<AtomicU64>,
    cycle_start: Arc<Mutex<Option<Instant>>>,
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
                                status.store(STATUS_WORKING, Ordering::SeqCst);
                                *cycle_start.lock() = Some(Instant::now());
                            }
                            "stop" => {
                                if let Some(start) = cycle_start.lock().take() {
                                    let elapsed = start.elapsed();
                                    active_ms
                                        .fetch_add(elapsed.as_millis() as u64, Ordering::SeqCst);
                                }
                                let prev = status.compare_exchange(
                                    STATUS_WORKING,
                                    STATUS_FINISHED,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                );
                                // A fast run may deliver `stop` while we are
                                // still NOT_YET (no `start` observed yet).
                                let transitioned = if prev.is_ok() {
                                    true
                                } else {
                                    status
                                        .compare_exchange(
                                            STATUS_NOT_YET,
                                            STATUS_FINISHED,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok()
                                };
                                if transitioned {
                                    crate::process::run_speck_apply_if_present(&project_dir);
                                    let name = process_name.lock().clone();
                                    let _ = Notification::new()
                                        .summary("Agent finished")
                                        .body(&format!("{} has completed", name))
                                        .show();
                                }
                            }
                            "git-conflict" => {
                                // Freeze the timer at the conflict moment so
                                // the display doesn't keep accruing time.
                                if let Some(start) = cycle_start.lock().take() {
                                    let elapsed = start.elapsed();
                                    active_ms
                                        .fetch_add(elapsed.as_millis() as u64, Ordering::SeqCst);
                                }
                                status.store(STATUS_GIT_CONFLICT, Ordering::SeqCst);
                                let name = process_name.lock().clone();
                                let _ = Notification::new()
                                    .summary("Git conflict")
                                    .body(&format!(
                                        "{} needs your attention — resolve the Git conflict",
                                        name
                                    ))
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
}
