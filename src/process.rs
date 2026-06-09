use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::Mode;

pub struct Process {
    pub id: usize,
    pub name: String,
    pub child: Option<Box<dyn portable_pty::Child + Send>>,
    pub master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub master_writer: Option<Box<dyn Write + Send>>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub alive: Arc<AtomicBool>,
}

impl Drop for Process {
    fn drop(&mut self) {
        drop(self.master_writer.take());
        drop(self.master.take());
        if let Some(ref mut child) = self.child {
            let _ = child.wait();
        }
    }
}

pub fn check_tty_alive(mode: &mut Mode, processes: &[Process]) {
    if let Mode::Tty { process_id } = mode {
        let pid = *process_id;
        let alive = processes
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.alive.load(Ordering::SeqCst));
        match alive {
            Some(false) | None => {
                let idx = processes.iter().position(|p| p.id == pid).unwrap_or(0);
                *mode = Mode::Normal { selected: idx };
            }
            _ => {}
        }
    }
}

pub fn spawn_process(
    pty_system: &NativePtySystem,
    next_id: &mut usize,
    cmd: &str,
    args: &[&str],
    rows: u16,
    cols: u16,
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

    let display = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, args.join(" "))
    };

    Ok(Process {
        id,
        name: format!("{} [{}]", display, id),
        child: Some(child),
        master: Some(pair.master),
        master_writer: Some(writer),
        parser,
        alive,
    })
}
