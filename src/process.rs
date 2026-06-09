use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::Mode;
use crate::status;

pub struct Process {
    pub id: usize,
    pub name: String,
    pub child: Option<Box<dyn portable_pty::Child + Send>>,
    pub master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub master_writer: Option<Box<dyn Write + Send>>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub alive: Arc<AtomicBool>,
    pub status: Arc<AtomicU8>,
    pub active_ms: Arc<AtomicU64>,
    pub cycle_start: Arc<Mutex<Option<Instant>>>,
    status_socket_path: Option<String>,
    _shutdown_flag: Option<Arc<AtomicBool>>,
    _listener_thread: Option<JoinHandle<()>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(ref flag) = self._shutdown_flag {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(handle) = self._listener_thread.take() {
            let _ = handle.join();
        }
        if let Some(ref path) = self.status_socket_path {
            let _ = std::fs::remove_file(path);
        }
        drop(self.master_writer.take());
        drop(self.master.take());
        if let Some(ref mut child) = self.child {
            let _ = child.wait();
        }
    }
}

pub fn check_tty_alive(mode: &mut Mode, processes: &mut Vec<Process>) -> bool {
    if let Mode::Tty { process_id } = mode {
        let pid = *process_id;
        let alive = processes
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.alive.load(Ordering::SeqCst));
        match alive {
            Some(false) => {
                let idx = processes.iter().position(|p| p.id == pid).unwrap_or(0);
                processes.retain(|p| p.id != pid);
                let selected = if processes.is_empty() { 0 } else { idx.min(processes.len() - 1) };
                *mode = Mode::Normal { selected };
                return true;
            }
            None => {
                let selected = if processes.is_empty() { 0 } else { 0 };
                *mode = Mode::Normal { selected };
                return true;
            }
            _ => {}
        }
    }
    false
}

pub fn spawn_process(
    pty_system: &NativePtySystem,
    next_id: &mut usize,
    cmd: &str,
    args: &[&str],
    title: Option<&str>,
    rows: u16,
    cols: u16,
    status_socket: Option<&str>,
) -> std::io::Result<Process> {
    let id = *next_id;
    *next_id += 1;

    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let mut cmd_builder = CommandBuilder::new(cmd);
    for arg in args {
        cmd_builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd_builder.cwd(cwd);
    }
    let child = pair
        .slave
        .spawn_command(cmd_builder)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let parser_clone = parser.clone();
    let alive = Arc::new(AtomicBool::new(true));
    let alive_clone = alive.clone();

    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    parser_clone.lock().process(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        alive_clone.store(false, Ordering::SeqCst);
    });

    let name = if let Some(title) = title {
        format!("{} [{}]", title, id)
    } else {
        let display = if args.is_empty() {
            cmd.to_string()
        } else {
            format!("{} {}", cmd, args.join(" "))
        };
        format!("{} [{}]", display, id)
    };

    let status = Arc::new(AtomicU8::new(status::STATUS_NOT_YET));
    let active_ms = Arc::new(AtomicU64::new(0));
    let cycle_start = Arc::new(Mutex::new(None));
    let (status_socket_path, shutdown_flag, listener_thread) = if let Some(path) = status_socket {
        let (flag, handle) = status::spawn_status_listener(
            status.clone(),
            active_ms.clone(),
            cycle_start.clone(),
            path.to_string(),
        );
        (Some(path.to_string()), Some(flag), Some(handle))
    } else {
        (None, None, None)
    };

    Ok(Process {
        id,
        name,
        child: Some(child),
        master: Some(pair.master),
        master_writer: Some(writer),
        parser,
        alive,
        status,
        active_ms,
        cycle_start,
        status_socket_path,
        _shutdown_flag: shutdown_flag,
        _listener_thread: listener_thread,
    })
}

pub fn sync_statuses(processes: &[Process]) {
    for p in processes {
        if !p.alive.load(Ordering::SeqCst) && p.status.load(Ordering::SeqCst) == status::STATUS_WORKING {
            p.status.store(status::STATUS_DEAD, Ordering::SeqCst);
            p.cycle_start.lock().take();
        }
    }
}
