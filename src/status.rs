use std::io::Read;
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub const STATUS_NOT_YET: u8 = 0;
pub const STATUS_WORKING: u8 = 1;
pub const STATUS_FINISHED: u8 = 2;
pub const STATUS_DEAD: u8 = 3;

pub fn status_prefix(status: u8) -> &'static str {
    match status {
        STATUS_NOT_YET => "[ ]",
        STATUS_WORKING => "[~]",
        STATUS_FINISHED => "[✓]",
        STATUS_DEAD => "[X]",
        _ => "[ ]",
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

        let mut buf = vec![0u8; 1024];

        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let n = match stream.read(&mut buf) {
                        Ok(n) if n > 0 => n,
                        _ => continue,
                    };

                    let text = String::from_utf8_lossy(&buf[..n]);
                    for line in text.lines() {
                        match line {
                            "start" => {
                                status.store(STATUS_WORKING, Ordering::SeqCst);
                                *cycle_start.lock() = Some(Instant::now());
                            }
                            "stop" => {
                                if let Some(start) = cycle_start.lock().take() {
                                    let elapsed = start.elapsed();
                                    active_ms.fetch_add(
                                        elapsed.as_millis() as u64,
                                        Ordering::SeqCst,
                                    );
                                }
                                if status.load(Ordering::SeqCst) == STATUS_WORKING {
                                    status.store(STATUS_FINISHED, Ordering::SeqCst);
                                }
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
                Err(_) => return,
            }
        }
    });

    (shutdown, handle)
}
