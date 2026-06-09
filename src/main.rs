use std::io::{stdout, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

struct Process {
    id: usize,
    name: String,
    child: Option<Box<dyn portable_pty::Child + Send>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    master_writer: Option<Box<dyn Write + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    alive: Arc<AtomicBool>,
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

enum Mode {
    Normal { selected: usize },
    Tty { process_id: usize },
}

fn main() -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    rt.block_on(run())
}

async fn run() -> std::io::Result<()> {
    let mut stdout = stdout();
    enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let pty_system = NativePtySystem::default();
    let mut processes: Vec<Process> = Vec::new();
    let mut next_id: usize = 1;
    let mut mode = Mode::Normal { selected: 0 };

    let (cols, rows) = terminal::size()?;
    let mut term_rows = rows;
    let mut term_cols = cols;

    let mut reader = EventStream::new();

    loop {
        check_tty_alive(&mut mode, &processes);

        render(&mut stdout, &mode, &processes, term_rows, term_cols)?;

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let should_quit = process_event(
                            &mut mode,
                            &mut processes,
                            &mut next_id,
                            &pty_system,
                            event,
                            &mut term_rows,
                            &mut term_cols,
                        )?;

                        if should_quit {
                            cleanup(&mut stdout)?;
                            return Ok(());
                        }
                    }
                    Some(Err(_)) => {}
                    None => {
                        cleanup(&mut stdout)?;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn process_event(
    mode: &mut Mode,
    processes: &mut Vec<Process>,
    next_id: &mut usize,
    pty_system: &NativePtySystem,
    event: Event,
    term_rows: &mut u16,
    term_cols: &mut u16,
) -> std::io::Result<bool> {
    match event {
        Event::Resize(w, h) => {
            *term_cols = w;
            *term_rows = h;
            for proc in processes.iter() {
                if let Some(ref master) = proc.master {
                    let _ = master.resize(PtySize {
                        rows: h,
                        cols: w,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            return process_key(mode, processes, next_id, pty_system, key, *term_rows, *term_cols);
        }
        _ => {}
    }
    Ok(false)
}

fn process_key(
    mode: &mut Mode,
    processes: &mut Vec<Process>,
    next_id: &mut usize,
    pty_system: &NativePtySystem,
    key: crossterm::event::KeyEvent,
    term_rows: u16,
    term_cols: u16,
) -> std::io::Result<bool> {
    match mode {
        Mode::Normal { selected } => match key.code {
            KeyCode::Char('n') => {
                let proc = spawn_process(pty_system, next_id, "zerostack", term_rows, term_cols)?;
                if processes.is_empty() {
                    *selected = 0;
                }
                processes.push(proc);
            }
            KeyCode::Char('k') => {
                if !processes.is_empty() && *selected < processes.len() {
                    processes.remove(*selected);
                    if *selected >= processes.len() && *selected > 0 {
                        *selected -= 1;
                    }
                }
            }
            KeyCode::Enter => {
                if !processes.is_empty() && *selected < processes.len() {
                    let pid = processes[*selected].id;
                    *mode = Mode::Tty { process_id: pid };
                }
            }
            KeyCode::Up => {
                if *selected > 0 {
                    *selected -= 1;
                }
            }
            KeyCode::Down => {
                if *selected + 1 < processes.len() {
                    *selected += 1;
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
            _ => {}
        },
        Mode::Tty { process_id } => {
            let pid = *process_id;
            match key.code {
                KeyCode::Esc => {
                    let idx = processes.iter().position(|p| p.id == pid).unwrap_or(0);
                    *mode = Mode::Normal { selected: idx };
                }
                _ => {
                    if let Some(proc) = processes.iter_mut().find(|p| p.id == pid) {
                        if let Some(ref mut writer) = proc.master_writer {
                            let bytes = key_to_bytes(&key);
                            if !bytes.is_empty() {
                                let _ = writer.write_all(&bytes);
                                let _ = writer.flush();
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

fn check_tty_alive(mode: &mut Mode, processes: &[Process]) {
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

fn spawn_process(
    pty_system: &NativePtySystem,
    next_id: &mut usize,
    cmd: &str,
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

    let cmd_builder = CommandBuilder::new(cmd);
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

    Ok(Process {
        id,
        name: format!("{} [{}]", cmd, id),
        child: Some(child),
        master: Some(pair.master),
        master_writer: Some(writer),
        parser,
        alive,
    })
}

fn render(
    stdout: &mut std::io::Stdout,
    mode: &Mode,
    processes: &[Process],
    _rows: u16,
    _cols: u16,
) -> std::io::Result<()> {
    match mode {
        Mode::Normal { selected } => render_normal(stdout, processes, *selected),
        Mode::Tty { process_id } => {
            if let Some(proc) = processes.iter().find(|p| p.id == *process_id) {
                render_tty(stdout, proc)
            } else {
                Ok(())
            }
        }
    }
}

fn render_normal(
    stdout: &mut std::io::Stdout,
    processes: &[Process],
    selected: usize,
) -> std::io::Result<()> {
    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(terminal::ClearType::All)
    )?;
    let mut w = stdout.lock();
    writeln!(w, "Multistack")?;
    writeln!(w, "==========")?;
    writeln!(w)?;

    if processes.is_empty() {
        writeln!(w, "  (no processes)")?;
    } else {
        for (i, proc) in processes.iter().enumerate() {
            let cursor_mark = if i == selected { ">" } else { " " };
            let dead = if !proc.alive.load(Ordering::SeqCst) {
                " [dead]"
            } else {
                ""
            };
            writeln!(w, "{cursor_mark} {}. {}{dead}", i + 1, proc.name)?;
        }
    }

    writeln!(w)?;
    writeln!(w, "n: new  k: kill  Enter: open TTY  q/Esc: quit")?;
    w.flush()?;
    Ok(())
}

fn render_tty(stdout: &mut std::io::Stdout, proc: &Process) -> std::io::Result<()> {
    let (contents, cursor_row, cursor_col) = {
        let parser = proc.parser.lock();
        let screen = parser.screen();
        let contents = screen.contents_formatted();
        let (row, col) = screen.cursor_position();
        (contents, row, col)
    };

    execute!(stdout, cursor::MoveTo(0, 0))?;
    let mut w = stdout.lock();
    w.write_all(&contents)?;
    execute!(w, cursor::MoveTo(cursor_col, cursor_row))?;
    w.flush()?;
    Ok(())
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    if key.modifiers.contains(KeyModifiers::ALT) {
        if let KeyCode::Char(c) = key.code {
            let mut bytes = vec![0x1b];
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
            return bytes;
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'a'..='z' => vec![c as u8 - b'a' + 1],
                    'A'..='Z' => vec![c as u8 - b'A' + 1],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    '^' => vec![0x1e],
                    '_' => vec![0x1f],
                    '?' => vec![0x7f],
                    '2' => vec![0x00],
                    '6' => vec![0x1e],
                    _ => vec![],
                }
            } else {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                encoded.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::F(n) => f_key(n),
        KeyCode::Null => vec![],
        _ => vec![],
    }
}

fn f_key(n: u8) -> Vec<u8> {
    match n {
        1 => vec![0x1b, b'O', b'P'],
        2 => vec![0x1b, b'O', b'Q'],
        3 => vec![0x1b, b'O', b'R'],
        4 => vec![0x1b, b'O', b'S'],
        5 => vec![0x1b, b'[', b'1', b'5', b'~'],
        6 => vec![0x1b, b'[', b'1', b'7', b'~'],
        7 => vec![0x1b, b'[', b'1', b'8', b'~'],
        8 => vec![0x1b, b'[', b'1', b'9', b'~'],
        9 => vec![0x1b, b'[', b'2', b'0', b'~'],
        10 => vec![0x1b, b'[', b'2', b'1', b'~'],
        11 => vec![0x1b, b'[', b'2', b'3', b'~'],
        12 => vec![0x1b, b'[', b'2', b'4', b'~'],
        13 => vec![0x1b, b'[', b'2', b'5', b'~'],
        14 => vec![0x1b, b'[', b'2', b'6', b'~'],
        15 => vec![0x1b, b'[', b'2', b'8', b'~'],
        _ => vec![],
    }
}

fn cleanup(stdout: &mut std::io::Stdout) -> std::io::Result<()> {
    execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen)?;
    disable_raw_mode()
}
