use std::io::Write;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use portable_pty::{NativePtySystem, PtySize};

use crate::Mode;
use crate::PromptPurpose;
use crate::process::{Process, spawn_process};

pub fn process_event(
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
                *mode = Mode::Prompt {
                    purpose: PromptPurpose::NewProcess,
                    selected: *selected,
                    input: String::new(),
                };
            }
            KeyCode::Char('r') => {
                if !processes.is_empty() && *selected < processes.len() {
                    let pid = processes[*selected].id;
                    let current = processes[*selected].name.clone();
                    let suffix = format!(" [{}]", pid);
                    let default = current.strip_suffix(&suffix).unwrap_or(&current).to_string();
                    *mode = Mode::Prompt {
                        purpose: PromptPurpose::Rename(pid),
                        selected: *selected,
                        input: default,
                    };
                }
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
        Mode::Prompt { purpose, selected, input } => match key.code {
            KeyCode::Esc => {
                *mode = Mode::Normal { selected: *selected };
            }
            KeyCode::Enter => {
                let title = std::mem::take(input);
                let title = title.trim().to_string();
                match purpose {
                    PromptPurpose::NewProcess => {
                        let title_opt = if title.is_empty() { None } else { Some(title.as_str()) };
                        let proc = spawn_process(pty_system, next_id, "zerostack", &["--parallel"], title_opt, term_rows, term_cols)?;
                        if processes.is_empty() {
                            *selected = 0;
                        }
                        processes.push(proc);
                    }
                    PromptPurpose::Rename(pid) => {
                        if !title.is_empty() {
                            if let Some(proc) = processes.iter_mut().find(|p| p.id == *pid) {
                                proc.name = format!("{} [{}]", title, pid);
                            }
                        }
                    }
                }
                let new_selected = if processes.is_empty() { 0 } else { *selected };
                *mode = Mode::Normal { selected: new_selected };
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
    }
    Ok(false)
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
