use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use portable_pty::{NativePtySystem, PtySize};
use ratatui_explorer_multistack::{FileExplorerBuilder, Theme};

use crate::Mode;
use crate::persistence::save_projects;
use crate::process::{
    Process, resize_parsers, run_speck_apply_if_present, spawn_process, spawn_pty,
};
use crate::project::{ListEntry, Project, is_agent, resolve_project};

#[allow(clippy::too_many_arguments)]
pub fn process_event(
    mode: &mut Mode,
    projects: &mut Vec<Project>,
    next_project_id: &mut usize,
    processes: &mut Vec<Process>,
    next_id: &mut usize,
    pty_system: &NativePtySystem,
    event: Event,
    term_rows: &mut u16,
    term_cols: &mut u16,
    entries: &[ListEntry],
    dont_save: bool,
    confirm_quit: bool,
) -> std::io::Result<bool> {
    match event {
        Event::Resize(w, h) => {
            *term_cols = w;
            *term_rows = h;
            for proc in processes.iter_mut() {
                if let Some(ref master) = proc.master {
                    let _ = master.resize(PtySize {
                        rows: h,
                        cols: w,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            resize_parsers(processes, h, w);
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            return process_key(
                mode,
                projects,
                next_project_id,
                processes,
                next_id,
                pty_system,
                key,
                *term_rows,
                *term_cols,
                entries,
                dont_save,
                confirm_quit,
            );
        }
        _ => {}
    }
    Ok(false)
}

fn find_project_dir(projects: &[Project], project_id: usize) -> Option<String> {
    projects
        .iter()
        .find(|p| p.id == project_id)
        .map(|p| p.directory.clone())
}

enum SpawnMode {
    Worktree(String),
    Parallel,
    Bare,
}

#[allow(clippy::too_many_arguments)]
fn spawn_zerostack(
    pty_system: &NativePtySystem,
    next_id: &mut usize,
    processes: &mut Vec<Process>,
    project_id: usize,
    project_dir: &str,
    title: Option<&str>,
    mode: SpawnMode,
    term_rows: u16,
    term_cols: u16,
    selected: &mut usize,
) {
    let id = *next_id;
    let mut rand_bytes = [0u8; 4];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut rand_bytes));
    let rand_suffix = format!("{:08x}", u32::from_le_bytes(rand_bytes));
    let socket_path = format!("/tmp/multistack-{}-{}.sock", id, rand_suffix);
    let args: Vec<&str> = match &mode {
        SpawnMode::Worktree(wt) => vec![
            "--worktree",
            wt.as_str(),
            "--wt-auto-merge",
            "--status-socket",
            &socket_path,
        ],
        SpawnMode::Parallel => vec![
            "--parallel",
            "--wt-auto-merge",
            "--status-socket",
            &socket_path,
        ],
        SpawnMode::Bare => vec!["--status-socket", &socket_path],
    };
    match spawn_process(
        pty_system,
        next_id,
        project_id,
        "zerostack",
        &args,
        title,
        term_rows,
        term_cols,
        Some(&socket_path),
        project_dir,
    ) {
        Ok(proc) => {
            if processes.is_empty() {
                *selected = 0;
            }
            processes.push(proc);
        }
        Err(e) => {
            let _ = notify_rust::Notification::new()
                .summary("Failed to spawn agent")
                .body(&format!("Could not launch zerostack: {e}"))
                .show();
        }
    }
}

fn worktree_name(title: &str) -> String {
    let sanitized = title.trim().replace([' ', '/', '\\'], "_");
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("wt-{}-{:02}-{:02}-{:02}", sanitized, h, m, s)
}

fn worktree_dir(project_dir: &str, wt_name: &str) -> String {
    let parent = Path::new(project_dir)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    format!("{}/{}", parent, wt_name)
}

#[allow(clippy::too_many_arguments)]
fn process_key(
    mode: &mut Mode,
    projects: &mut Vec<Project>,
    next_project_id: &mut usize,
    processes: &mut Vec<Process>,
    next_id: &mut usize,
    pty_system: &NativePtySystem,
    key: crossterm::event::KeyEvent,
    term_rows: u16,
    term_cols: u16,
    entries: &[ListEntry],
    dont_save: bool,
    confirm_quit: bool,
) -> std::io::Result<bool> {
    match mode {
        Mode::Normal { selected } => {
            match key.code {
                KeyCode::Char('n') => {
                    if let Some(project_id) = resolve_project(entries, projects, *selected) {
                        *mode = Mode::Prompt {
                            purpose: crate::PromptPurpose::NewProcess(project_id),
                            selected: *selected,
                            input: String::new(),
                        };
                    }
                }
                KeyCode::Char('N') => {
                    if let Some(project_id) = resolve_project(entries, projects, *selected)
                        && let Some(dir) = find_project_dir(projects, project_id)
                    {
                        let new_selected = *selected;
                        spawn_zerostack(
                            pty_system,
                            next_id,
                            processes,
                            project_id,
                            &dir,
                            None,
                            SpawnMode::Parallel,
                            term_rows,
                            term_cols,
                            selected,
                        );
                        if let Some(proc) = processes.last() {
                            let pid = proc.id;
                            *selected = new_selected;
                            *mode = Mode::Tty { process_id: pid };
                        }
                    }
                }
                KeyCode::Char('m') => {
                    if let Some(project_id) = resolve_project(entries, projects, *selected) {
                        *mode = Mode::Prompt {
                            purpose: crate::PromptPurpose::NewBareProcess(project_id),
                            selected: *selected,
                            input: String::new(),
                        };
                    }
                }
                KeyCode::Char('p') => {
                    let theme = Theme::default().add_default_title();
                    match FileExplorerBuilder::build_with_theme(theme) {
                        Ok(mut explorer) => {
                            let _ = explorer.set_only_dirs(true);
                            *mode = Mode::DirPicker {
                                explorer: Box::new(explorer),
                                previous_selected: *selected,
                            };
                        }
                        Err(e) => {
                            let _ = notify_rust::Notification::new()
                                .summary("Failed to open file explorer")
                                .body(&format!("{e}"))
                                .show();
                        }
                    }
                }
                KeyCode::Char('l') => {
                    if let Some(project_id) = resolve_project(entries, projects, *selected) {
                        let header_idx = entries[..=*selected]
                        .iter()
                        .rposition(|e| matches!(e, ListEntry::ProjectHeader(pid) if *pid == project_id))
                        .unwrap_or(0);
                        if let Some(dir) = find_project_dir(projects, project_id) {
                            run_speck_apply_if_present(&dir);
                        }
                        processes.retain(|p| p.project_id != project_id);
                        projects.retain(|p| p.id != project_id);
                        *selected = header_idx.saturating_sub(1);
                        if !dont_save {
                            let _ = save_projects(projects);
                        }
                    }
                }
                KeyCode::Char('h') => {
                    if let Some(project_id) = resolve_project(entries, projects, *selected)
                        && let Some(dir) = find_project_dir(projects, project_id)
                    {
                        let target_dir = if is_agent(entries, *selected)
                            && let ListEntry::Agent(proc_id) = entries[*selected]
                            && let Some(proc) = processes.iter().find(|p| p.id == proc_id)
                        {
                            let wt_dir = worktree_dir(&dir, &proc.name);
                            if Path::new(&wt_dir).is_dir() {
                                wt_dir
                            } else {
                                dir
                            }
                        } else {
                            dir
                        };
                        match spawn_pty(
                            pty_system,
                            "lazygit",
                            &[],
                            None,
                            term_rows,
                            term_cols,
                            &target_dir,
                        ) {
                            Ok(mut process) => {
                                process.kill_on_drop = true;
                                *mode = Mode::TempTty {
                                    process,
                                    previous_selected: *selected,
                                };
                            }
                            Err(e) => {
                                let _ = notify_rust::Notification::new()
                                    .summary("Failed to launch lazygit")
                                    .body(&format!("{e}"))
                                    .show();
                            }
                        }
                    }
                }
                KeyCode::Char('s') => {
                    if let Some(project_id) = resolve_project(entries, projects, *selected)
                        && let Some(dir) = find_project_dir(projects, project_id)
                    {
                        let target_dir = if is_agent(entries, *selected)
                            && let ListEntry::Agent(proc_id) = entries[*selected]
                            && let Some(proc) = processes.iter().find(|p| p.id == proc_id)
                        {
                            let wt_dir = worktree_dir(&dir, &proc.name);
                            if Path::new(&wt_dir).is_dir() {
                                wt_dir
                            } else {
                                dir
                            }
                        } else {
                            dir
                        };
                        let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
                        match spawn_pty(
                            pty_system,
                            &shell,
                            &[],
                            None,
                            term_rows,
                            term_cols,
                            &target_dir,
                        ) {
                            Ok(mut process) => {
                                process.kill_on_drop = true;
                                *mode = Mode::TempTty {
                                    process,
                                    previous_selected: *selected,
                                };
                            }
                            Err(e) => {
                                let _ = notify_rust::Notification::new()
                                    .summary("Failed to launch shell")
                                    .body(&format!("{e}"))
                                    .show();
                            }
                        }
                    }
                }
                KeyCode::Char('r') => {
                    if is_agent(entries, *selected)
                        && let ListEntry::Agent(proc_id) = entries[*selected]
                        && let Some(proc) = processes.iter().find(|p| p.id == proc_id)
                    {
                        let default = proc.name.clone();
                        *mode = Mode::Prompt {
                            purpose: crate::PromptPurpose::Rename(proc_id),
                            selected: *selected,
                            input: default,
                        };
                    }
                }
                KeyCode::Char('d') => {
                    if is_agent(entries, *selected)
                        && let ListEntry::Agent(proc_id) = entries[*selected]
                    {
                        if let Some(dir) = processes
                            .iter()
                            .find(|p| p.id == proc_id)
                            .map(|p| p.project_dir.clone())
                        {
                            run_speck_apply_if_present(&dir);
                        }
                        processes.retain(|p| p.id != proc_id);
                        if *selected > 0 {
                            *selected -= 1;
                        }
                    }
                }
                KeyCode::Enter => {
                    if is_agent(entries, *selected)
                        && let ListEntry::Agent(proc_id) = entries[*selected]
                    {
                        *mode = Mode::Tty {
                            process_id: proc_id,
                        };
                    }
                }
                KeyCode::Up => {
                    if *selected > 0 {
                        *selected -= 1;
                    }
                }
                KeyCode::Down => {
                    if *selected + 1 < entries.len() {
                        *selected += 1;
                    }
                }
                KeyCode::PageUp => {
                    if let Some(idx) = entries[..*selected]
                        .iter()
                        .rposition(|e| matches!(e, ListEntry::ProjectHeader(_)))
                    {
                        *selected = idx;
                    }
                }
                KeyCode::PageDown => {
                    let start = (*selected + 1).min(entries.len());
                    if let Some(idx) = entries[start..]
                        .iter()
                        .position(|e| matches!(e, ListEntry::ProjectHeader(_)))
                    {
                        *selected = start + idx;
                    }
                }
                KeyCode::Esc if confirm_quit => return Ok(false),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(true),
                _ => {}
            }
        }
        Mode::Tty { process_id } => {
            let pid = *process_id;
            match key.code {
                KeyCode::Esc => {
                    let idx = processes.iter().position(|p| p.id == pid).unwrap_or(0);
                    *mode = Mode::Normal { selected: idx };
                }
                _ => {
                    if let Some(proc) = processes.iter_mut().find(|p| p.id == pid)
                        && let Some(ref mut writer) = proc.master_writer
                    {
                        let bytes = key_to_bytes(&key);
                        if !bytes.is_empty() {
                            let _ = writer.write_all(&bytes);
                            let _ = writer.flush();
                        }
                    }
                }
            }
        }
        Mode::TempTty {
            process,
            previous_selected,
        } => match key.code {
            KeyCode::Esc => {
                *mode = Mode::Normal {
                    selected: *previous_selected,
                };
            }
            _ => {
                if let Some(ref mut writer) = process.master_writer {
                    let bytes = key_to_bytes(&key);
                    if !bytes.is_empty() {
                        let _ = writer.write_all(&bytes);
                        let _ = writer.flush();
                    }
                }
            }
        },
        Mode::Prompt {
            purpose,
            selected,
            input,
        } => match key.code {
            KeyCode::Esc => {
                *mode = Mode::Normal {
                    selected: *selected,
                };
            }
            KeyCode::Enter => {
                let title = std::mem::take(input);
                let title = title.trim().to_string();
                match purpose {
                    crate::PromptPurpose::NewProcess(project_id) => {
                        let pid = *project_id;
                        if let Some(dir) = find_project_dir(projects, pid) {
                            let display = if title.is_empty() {
                                "agent".to_string()
                            } else {
                                title.clone()
                            };
                            let wt_name = if title.is_empty() {
                                worktree_name("agent")
                            } else {
                                worktree_name(&title)
                            };
                            spawn_zerostack(
                                pty_system,
                                next_id,
                                processes,
                                pid,
                                &dir,
                                Some(&display),
                                SpawnMode::Worktree(wt_name),
                                term_rows,
                                term_cols,
                                selected,
                            );
                        }
                    }
                    crate::PromptPurpose::NewBareProcess(project_id) => {
                        let pid = *project_id;
                        if let Some(dir) = find_project_dir(projects, pid) {
                            let display = if title.is_empty() {
                                "agent".to_string()
                            } else {
                                title.clone()
                            };
                            spawn_zerostack(
                                pty_system,
                                next_id,
                                processes,
                                pid,
                                &dir,
                                Some(&display),
                                SpawnMode::Bare,
                                term_rows,
                                term_cols,
                                selected,
                            );
                        }
                    }
                    crate::PromptPurpose::NewProject => {
                        if !title.is_empty() {
                            let path = Path::new(&title);
                            if path.is_dir() {
                                let name = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(&title)
                                    .to_string();
                                let dir = path
                                    .canonicalize()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_else(|_| title.to_string());
                                projects.push(Project {
                                    id: *next_project_id,
                                    name,
                                    directory: dir,
                                });
                                *next_project_id += 1;
                                if !dont_save {
                                    let _ = save_projects(projects);
                                }
                            } else {
                                let _ = notify_rust::Notification::new()
                                    .summary("Invalid directory")
                                    .body(&format!("Directory does not exist: {}", title))
                                    .show();
                            }
                        }
                    }
                    crate::PromptPurpose::Rename(pid) => {
                        if !title.is_empty()
                            && let Some(proc) = processes.iter_mut().find(|p| p.id == *pid)
                        {
                            proc.name = title.clone();
                            if let Some(ref name_shared) = proc.name_shared {
                                *name_shared.lock() = title;
                            }
                        }
                    }
                }
                match purpose {
                    crate::PromptPurpose::NewBareProcess(_) => {
                        if let Some(proc) = processes.last() {
                            let pid = proc.id;
                            *mode = Mode::Tty { process_id: pid };
                        }
                    }
                    _ => {
                        let new_selected =
                            if processes.is_empty() { 0 } else { *selected };
                        *mode = Mode::Normal {
                            selected: new_selected,
                        };
                    }
                }
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => {
                input.push(c);
            }
            _ => {}
        },
        Mode::DirPicker {
            explorer,
            previous_selected,
        } => {
            // Handle search typing if search is active
            if explorer.search_query().is_some() {
                match key.code {
                    KeyCode::Esc => {
                        let _ = explorer.set_search_query(None);
                        return Ok(false);
                    }
                    KeyCode::Backspace => {
                        let current = explorer.search_query().unwrap().clone();
                        let mut chars: Vec<char> = current.chars().collect();
                        chars.pop();
                        if chars.is_empty() {
                            let _ = explorer.set_search_query(None);
                        } else {
                            let _ = explorer.set_search_query(Some(chars.into_iter().collect()));
                        }
                        return Ok(false);
                    }
                    KeyCode::Char(c) if c != '/' => {
                        let current = explorer.search_query().unwrap().clone();
                        let _ = explorer.set_search_query(Some(format!("{}{}", current, c)));
                        return Ok(false);
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Esc => {
                    *mode = Mode::Normal {
                        selected: *previous_selected,
                    };
                }
                KeyCode::Enter => {
                    let current = explorer.current();
                    let (name, dir) = if current.is_dir {
                        let path = &current.path;
                        let canonical = path
                            .canonicalize()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| path.to_string_lossy().to_string());
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&canonical)
                            .to_string();
                        (name, canonical)
                    } else {
                        return Ok(false);
                    };
                    projects.push(Project {
                        id: *next_project_id,
                        name,
                        directory: dir,
                    });
                    *next_project_id += 1;
                    if !dont_save {
                        let _ = save_projects(projects);
                    }
                    *mode = Mode::Normal {
                        selected: *previous_selected,
                    };
                }
                _ => {
                    let event = Event::Key(key);
                    let _ = explorer.handle(&event);
                }
            }
        }
    }
    Ok(false)
}

fn key_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    if key.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = key.code
    {
        let mut bytes = vec![0x1b];
        let mut buf = [0u8; 4];
        let encoded = c.encode_utf8(&mut buf);
        bytes.extend_from_slice(encoded.as_bytes());
        return bytes;
    }

    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'a'..='z' => vec![c as u8 - b'a' + 1],
                    'A'..='Z' => vec![c as u8 - b'A' + 1],
                    '@' => vec![0x00],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    '^' => vec![0x1e],
                    '_' => vec![0x1f],
                    '?' => vec![0x7f],
                    '2' => vec![0x00],
                    _ => vec![],
                }
            } else {
                let mut buf = [0u8; 4];
                let encoded = c.encode_utf8(&mut buf);
                encoded.as_bytes().to_vec()
            }
        }
        KeyCode::Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                vec![b'\n']
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                vec![0x1b, b'\r']
            } else {
                vec![b'\r']
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn kc(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn test_plain_chars() {
        assert_eq!(key_to_bytes(&kc(KeyCode::Char('a'))), b"a");
        assert_eq!(key_to_bytes(&kc(KeyCode::Char('Z'))), b"Z");
        assert_eq!(key_to_bytes(&kc(KeyCode::Char('1'))), b"1");
    }

    #[test]
    fn test_ctrl_chars() {
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('c'))), vec![3]);
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('z'))), vec![26]);
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('['))), vec![0x1b]);
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('@'))), vec![0x00]);
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('^'))), vec![0x1e]);
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('_'))), vec![0x1f]);
    }

    #[test]
    fn test_alt_chars() {
        assert_eq!(key_to_bytes(&alt(KeyCode::Char('x'))), vec![0x1b, b'x']);
    }

    #[test]
    fn test_special_keys() {
        assert_eq!(key_to_bytes(&kc(KeyCode::Enter)), b"\r");
        assert_eq!(key_to_bytes(&kc(KeyCode::Backspace)), vec![0x7f]);
        assert_eq!(key_to_bytes(&kc(KeyCode::Tab)), b"\t");
        assert_eq!(key_to_bytes(&kc(KeyCode::Esc)), vec![0x1b]);
        assert_eq!(key_to_bytes(&kc(KeyCode::Up)), vec![0x1b, b'[', b'A']);
        assert_eq!(key_to_bytes(&kc(KeyCode::Down)), vec![0x1b, b'[', b'B']);
        assert_eq!(
            key_to_bytes(&kc(KeyCode::Delete)),
            vec![0x1b, b'[', b'3', b'~']
        );
        assert_eq!(key_to_bytes(&kc(KeyCode::F(1))), vec![0x1b, b'O', b'P']);
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(12))),
            vec![0x1b, b'[', b'2', b'4', b'~']
        );
    }

    #[test]
    fn test_enter_modifiers() {
        assert_eq!(key_to_bytes(&kc(KeyCode::Enter)), b"\r");
        assert_eq!(key_to_bytes(&shift(KeyCode::Enter)), b"\n");
        assert_eq!(key_to_bytes(&alt(KeyCode::Enter)), vec![0x1b, b'\r']);
    }

    #[test]
    fn test_null_and_unknown() {
        assert_eq!(key_to_bytes(&kc(KeyCode::Null)), Vec::<u8>::new());
    }
}
