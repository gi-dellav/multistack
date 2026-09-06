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
    no_worktree: bool,
    activity_dot_enabled: bool,
    confirm_quit: bool,
    show_help: &mut bool,
    help_scroll: &mut u16,
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
            // Also resize TempTty if active (its process is stored inside Mode)
            if let Mode::TempTty { process, .. } = mode {
                if let Some(ref master) = process.master {
                    let _ = master.resize(PtySize {
                        rows: h,
                        cols: w,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                // Resize its parser and invalidate cache
                {
                    let mut parser = process.parser.lock();
                    let old_screen = parser.screen().clone();
                    let rows = if h == 0 { 24 } else { h };
                    let cols = if w == 0 { 80 } else { w };
                    let mut new_parser = vt100::Parser::new(rows, cols, old_screen.scrollback());
                    new_parser.process(&old_screen.contents_formatted());
                    *parser = new_parser;
                }
                *process.prev_screen.lock() = None;
            }
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
                no_worktree,
                activity_dot_enabled,
                confirm_quit,
                show_help,
                help_scroll,
            );
        }
        Event::Paste(text) => {
            match mode {
                Mode::Tty { process_id } => {
                    let pid = *process_id;
                    if let Some(proc) = processes.iter_mut().find(|p| p.id == pid)
                        && let Some(ref mut writer) = proc.master_writer
                    {
                        let _ = writer.write_all(text.as_bytes());
                        let _ = writer.flush();
                    }
                }
                Mode::TempTty { process, .. } => {
                    if let Some(ref mut writer) = process.master_writer {
                        let _ = writer.write_all(text.as_bytes());
                        let _ = writer.flush();
                    }
                }
                Mode::Prompt { input, .. } => {
                    // Filter out control characters that could trigger unintended actions;
                    // keep printable pasted text for prompt input.
                    let filtered: String =
                        text.chars().filter(|c| *c != '\r' && *c != '\n').collect();
                    input.push_str(&filtered);
                }
                Mode::DirPicker { explorer, .. } => {
                    if let Some(current) = explorer.search_query().cloned() {
                        let filtered: String =
                            text.chars().filter(|c| *c != '\r' && *c != '\n').collect();
                        let _ = explorer.set_search_query(Some(format!("{}{}", current, filtered)));
                    }
                }
                _ => {}
            }
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
    activity_dot_enabled: bool,
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
    let worktree_dir = match &mode {
        SpawnMode::Worktree(wt_name) => {
            let parent = Path::new(project_dir)
                .parent()
                .filter(|p| !p.as_os_str().is_empty());
            match parent {
                Some(p) => Some(format!("{}/{wt_name}", p.to_string_lossy())),
                None => Some(format!("./{wt_name}")),
            }
        }
        _ => None,
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
        worktree_dir.as_deref(),
        activity_dot_enabled,
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
    let sanitized: String = title
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    let sanitized = if sanitized.is_empty() {
        "agent".to_string()
    } else {
        sanitized.chars().take(32).collect()
    };
    // Second + sub-second granularity: two agents spawned in the same wall
    // second previously got the same branch name and collided.
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    let sub = dur.subsec_nanos();
    let pid = std::process::id();
    // Nanos + pid make same-millisecond spawns unique. A per-process counter
    // would also work but nanos+pid keeps the name stateless/predictable.
    static WT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ctr = WT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("wt-{sanitized}-{h:02}-{m:02}-{s:02}-{sub:09}-{pid}-{ctr}")
}

#[cfg(test)]
fn worktree_dir(project_dir: &str, wt_name: &str) -> String {
    // `parent()` returns Some("") for relative paths ("project" -> "") and
    // None for "/" — both previous cases produced garbage like "/wt-foo".
    let parent = Path::new(project_dir)
        .parent()
        .filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(p) => format!("{}/{wt_name}", p.to_string_lossy()),
        None => format!("./{wt_name}"),
    }
}

/// Sibling directory a `--worktree` agent is expected to use. Shared with
/// `spawn_zerostack` above so h/lazygit/shell and speck-apply target the same
/// path the agent was told to create.
///
/// Kept as a free function (wrapping the same parent/sibling logic) so it
/// stays unit testable without constructing a full `Process`.
#[allow(dead_code)]
pub(crate) fn expected_worktree_dir(project_dir: &str, wt_name: &str) -> String {
    let parent = Path::new(project_dir)
        .parent()
        .filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(p) => format!("{}/{wt_name}", p.to_string_lossy()),
        None => format!("./{wt_name}"),
    }
}

fn open_help(show_help: &mut bool, help_scroll: &mut u16) {
    *show_help = true;
    *help_scroll = 0;
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
    no_worktree: bool,
    activity_dot_enabled: bool,
    confirm_quit: bool,
    show_help: &mut bool,
    help_scroll: &mut u16,
) -> std::io::Result<bool> {
    match mode {
        Mode::Normal { selected } => {
            match key.code {
                KeyCode::Char('n') => {
                    if no_worktree {
                        if let Some(project_id) = resolve_project(entries, projects, *selected) {
                            *mode = Mode::Prompt {
                                purpose: crate::PromptPurpose::NewBareProcess(project_id),
                                selected: *selected,
                                input: String::new(),
                            };
                        }
                    } else if let Some(project_id) = resolve_project(entries, projects, *selected) {
                        *mode = Mode::Prompt {
                            purpose: crate::PromptPurpose::NewProcess(project_id),
                            selected: *selected,
                            input: String::new(),
                        };
                    }
                }
                KeyCode::Char('N') => {
                    if !no_worktree
                        && let Some(project_id) = resolve_project(entries, projects, *selected)
                        && let Some(dir) = find_project_dir(projects, project_id)
                    {
                        let new_selected = *selected;
                        let len_before = processes.len();
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
                            activity_dot_enabled,
                        );
                        if processes.len() > len_before
                            && let Some(proc) = processes.last()
                        {
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
                        let header_idx = if entries.is_empty() || *selected >= entries.len() {
                            0
                        } else {
                            entries[..=*selected]
                            .iter()
                            .rposition(|e| matches!(e, ListEntry::ProjectHeader(pid) if *pid == project_id))
                            .unwrap_or(0)
                        };
                        if let Some(dir) = find_project_dir(projects, project_id) {
                            run_speck_apply_if_present(&dir);
                        }
                        // Also flush any agent worktrees under this project.
                        for p in processes.iter().filter(|p| p.project_id == project_id) {
                            run_speck_apply_if_present(&p.effective_dir());
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
                            let eff = proc.effective_dir();
                            if Path::new(&eff).is_dir() { eff } else { dir }
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
                                process.project_dir = target_dir.clone();
                                *mode = Mode::TempTty {
                                    process: Box::new(process),
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
                            let eff = proc.effective_dir();
                            if Path::new(&eff).is_dir() { eff } else { dir }
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
                                process.project_dir = target_dir.clone();
                                *mode = Mode::TempTty {
                                    process: Box::new(process),
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
                    if !entries.is_empty()
                        && *selected < entries.len()
                        && let Some(idx) = entries[..*selected]
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
                KeyCode::Char('?') | KeyCode::F(1) => {
                    open_help(show_help, help_scroll);
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
                    // `processes` order != visible list order (headers
                    // interleave in `entries`): translate the pid back to the
                    // list index so Esc lands the cursor on the right row.
                    let idx = entries
                        .iter()
                        .position(|e| matches!(e, ListEntry::Agent(id) if *id == pid))
                        .unwrap_or(0);
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
                let len_before = processes.len();
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
                                activity_dot_enabled,
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
                                activity_dot_enabled,
                            );
                        }
                    }
                    crate::PromptPurpose::NewProject => {
                        if !title.is_empty() {
                            let path = Path::new(&title);
                            if path.is_dir() {
                                let dir = path
                                    .canonicalize()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_else(|_| title.to_string());
                                // Canonicalize before comparing: the same dir
                                // typed as `~/proj` vs `/home/u/proj` or with
                                // a trailing slash must not create duplicates.
                                if projects.iter().any(|p| p.directory == dir) {
                                    let _ = notify_rust::Notification::new()
                                        .summary("Project already added")
                                        .body(&dir)
                                        .show();
                                } else {
                                    let name = Path::new(&dir)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(&dir)
                                        .to_string();
                                    projects.push(Project {
                                        id: *next_project_id,
                                        name,
                                        directory: dir,
                                    });
                                    *next_project_id += 1;
                                    if !dont_save {
                                        let _ = save_projects(projects);
                                    }
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
                        // Only enter the TTY when this spawn actually
                        // succeeded; a failed spawn (bad $SHELL, missing
                        // binary, PTY error) must return to the list instead
                        // of attaching to a stale pre-existing agent.
                        if processes.len() > len_before
                            && let Some(proc) = processes.last()
                        {
                            let pid = proc.id;
                            *mode = Mode::Tty { process_id: pid };
                        } else {
                            *mode = Mode::Normal {
                                selected: *selected,
                            };
                        }
                    }
                    _ => {
                        let new_selected = if processes.is_empty() { 0 } else { *selected };
                        *mode = Mode::Normal {
                            selected: new_selected,
                        };
                    }
                }
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::F(1) => {
                open_help(show_help, help_scroll);
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
                KeyCode::F(1) => {
                    open_help(show_help, help_scroll);
                    return Ok(false);
                }
                KeyCode::Char('?') if explorer.search_query().is_none() => {
                    open_help(show_help, help_scroll);
                    return Ok(false);
                }
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
                        let name = Path::new(&canonical)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(&canonical)
                            .to_string();
                        (name, canonical)
                    } else {
                        return Ok(false);
                    };
                    if projects.iter().any(|p| p.directory == dir) {
                        let _ = notify_rust::Notification::new()
                            .summary("Project already added")
                            .body(&dir)
                            .show();
                    } else {
                        projects.push(Project {
                            id: *next_project_id,
                            name,
                            directory: dir,
                        });
                        *next_project_id += 1;
                        if !dont_save {
                            let _ = save_projects(projects);
                        }
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
                vec![0x1b, b'[', b'1', b'3', b';', b'2', b'u']
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                vec![0x1b, b'\r']
            } else if key.modifiers.contains(KeyModifiers::CONTROL) {
                vec![0x1b, b'[', b'1', b'3', b';', b'5', b'u']
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
        assert_eq!(
            key_to_bytes(&shift(KeyCode::Enter)),
            vec![0x1b, b'[', b'1', b'3', b';', b'2', b'u']
        );
        assert_eq!(key_to_bytes(&alt(KeyCode::Enter)), vec![0x1b, b'\r']);
        assert_eq!(
            key_to_bytes(&ctrl(KeyCode::Enter)),
            vec![0x1b, b'[', b'1', b'3', b';', b'5', b'u']
        );
    }

    #[test]
    fn test_null_and_unknown() {
        assert_eq!(key_to_bytes(&kc(KeyCode::Null)), Vec::<u8>::new());
    }

    // ---- Additional TTY logic tests ----

    #[test]
    fn test_f_keys_all() {
        assert_eq!(key_to_bytes(&kc(KeyCode::F(1))), vec![0x1b, b'O', b'P']);
        assert_eq!(key_to_bytes(&kc(KeyCode::F(2))), vec![0x1b, b'O', b'Q']);
        assert_eq!(key_to_bytes(&kc(KeyCode::F(3))), vec![0x1b, b'O', b'R']);
        assert_eq!(key_to_bytes(&kc(KeyCode::F(4))), vec![0x1b, b'O', b'S']);
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(5))),
            vec![0x1b, b'[', b'1', b'5', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(6))),
            vec![0x1b, b'[', b'1', b'7', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(7))),
            vec![0x1b, b'[', b'1', b'8', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(8))),
            vec![0x1b, b'[', b'1', b'9', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(9))),
            vec![0x1b, b'[', b'2', b'0', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(10))),
            vec![0x1b, b'[', b'2', b'1', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(11))),
            vec![0x1b, b'[', b'2', b'3', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::F(12))),
            vec![0x1b, b'[', b'2', b'4', b'~']
        );
        // Unknown F key should be empty
        assert_eq!(key_to_bytes(&kc(KeyCode::F(20))), Vec::<u8>::new());
    }

    #[test]
    fn test_all_special_keys_bytes() {
        assert_eq!(key_to_bytes(&kc(KeyCode::BackTab)), vec![0x1b, b'[', b'Z']);
        assert_eq!(key_to_bytes(&kc(KeyCode::Left)), vec![0x1b, b'[', b'D']);
        assert_eq!(key_to_bytes(&kc(KeyCode::Right)), vec![0x1b, b'[', b'C']);
        assert_eq!(key_to_bytes(&kc(KeyCode::Home)), vec![0x1b, b'[', b'H']);
        assert_eq!(key_to_bytes(&kc(KeyCode::End)), vec![0x1b, b'[', b'F']);
        assert_eq!(
            key_to_bytes(&kc(KeyCode::Insert)),
            vec![0x1b, b'[', b'2', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::PageUp)),
            vec![0x1b, b'[', b'5', b'~']
        );
        assert_eq!(
            key_to_bytes(&kc(KeyCode::PageDown)),
            vec![0x1b, b'[', b'6', b'~']
        );
    }

    #[test]
    fn test_key_to_bytes_ctrl_shift_handling() {
        // Ctrl+Shift+C should still be Ctrl+C (3)
        let mut key = KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        // crossterm reports Char as lowercase even with shift? but we handle both
        assert_eq!(key_to_bytes(&key), vec![3]);
        key = KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(key_to_bytes(&key), vec![3]);
        // Ctrl+2 -> NUL
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('2'))), vec![0x00]);
        // Ctrl+'?' -> DEL
        assert_eq!(key_to_bytes(&ctrl(KeyCode::Char('?'))), vec![0x7f]);
    }

    #[test]
    fn test_key_to_bytes_alt_combination() {
        // Alt+Enter already tested, but Alt+Char
        assert_eq!(key_to_bytes(&alt(KeyCode::Char('a'))), vec![0x1b, b'a']);
        // Alt with non-char should fall through to match (e.g., Alt+Enter)
        // Non-char Alt handling is only for Char, other keys go to match arm
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(key_to_bytes(&alt_up), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn test_worktree_helpers() {
        let n1 = worktree_name("hello world");
        assert!(n1.starts_with("wt-hello_world-"), "{n1}");
        // No spaces/slashes/backslashes survive sanitization.
        assert!(!n1.chars().any(|c| c == ' ' || c == '/' || c == '\\'));
        assert!(worktree_name("a/b\\c").contains("a_b_c"));
        // Same title twice must not collide (ms + pid suffix).
        assert_ne!(worktree_name("dup"), worktree_name("dup"));
        assert_eq!(
            worktree_dir("/home/user/project", "wt-foo"),
            "/home/user/wt-foo"
        );
        // Relative path has no parent -> sibling in cwd.
        assert_eq!(worktree_dir("project", "wt-foo"), "./wt-foo");
        // "/" has no parent -> fallback to "."
        assert_eq!(worktree_dir("/", "wt-foo"), "./wt-foo");
        // Normal case
        assert_eq!(worktree_dir("/tmp/myproj", "wt-test"), "/tmp/wt-test");
    }

    // Helper for capture writer
    struct CaptureWriter {
        buf: std::sync::Arc<parking_lot::Mutex<Vec<u8>>>,
    }
    impl std::io::Write for CaptureWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_capture_process(
        id: usize,
        buf: std::sync::Arc<parking_lot::Mutex<Vec<u8>>>,
    ) -> Process {
        Process {
            id,
            project_id: 1,
            project_dir: "/tmp".into(),
            worktree_dir: None,
            name: format!("test{id}"),
            child: None,
            master: None,
            master_writer: Some(Box::new(CaptureWriter { buf })),
            parser: std::sync::Arc::new(parking_lot::Mutex::new(vt100::Parser::new(24, 80, 0))),
            alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            status: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            active_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cycle_start: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            has_unread: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            status_socket_path: None,
            shutdown_flag: None,
            listener_thread: None,
            kill_on_drop: false,
            name_shared: None,
            prev_screen: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            exit_code: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            exit_signal: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            log_buffer: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn test_paste_tty_writes_to_writer() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proc = make_capture_process(42, buf.clone());
        let mut processes = vec![proc];
        let mut mode = crate::Mode::Tty { process_id: 42 };
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 100usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Paste("hello world".into());
        let res = process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert!(!res);
        assert_eq!(*buf.lock(), b"hello world");
    }

    #[test]
    fn test_paste_temptty_writes() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proc = make_capture_process(0, buf.clone());
        let mut mode = crate::Mode::TempTty {
            process: Box::new(proc),
            previous_selected: 0,
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 100usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Paste("pasted\ntext".into());
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        if let crate::Mode::TempTty { process, .. } = mode {
            // Check via the process inside mode
            // The buffer should contain the pasted text verbatim for TempTty
            assert_eq!(*buf.lock(), b"pasted\ntext");
            // Also ensure process still has writer
            assert!(process.master_writer.is_some());
        } else {
            panic!("mode should still be TempTty");
        }
    }

    #[test]
    fn test_paste_prompt_filters_and_appends() {
        let mut mode = crate::Mode::Prompt {
            purpose: crate::PromptPurpose::NewProject,
            selected: 0,
            input: "hello".into(),
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        // Paste with newlines should be filtered
        let event = Event::Paste(" world\r\nwith\nnewlines".into());
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        if let crate::Mode::Prompt { input, .. } = mode {
            assert_eq!(input, "hello worldwithnewlines");
        } else {
            panic!("expected Prompt");
        }
    }

    #[test]
    fn test_paste_normal_ignored() {
        let mut mode = crate::Mode::Normal { selected: 0 };
        let mut processes = vec![];
        let mut projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = crate::project::build_entries(&projects, &processes);
        let event = Event::Paste("should be ignored".into());
        let res = process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert!(!res);
        assert!(matches!(mode, crate::Mode::Normal { .. }));
    }

    #[test]
    fn test_paste_dirpicker_search_append() {
        let theme = Theme::default().add_default_title();
        let mut explorer = FileExplorerBuilder::build_with_theme(theme).unwrap();
        let _ = explorer.set_only_dirs(true);
        // Activate search with "foo"
        let _ = explorer.set_search_query(Some("foo".into()));
        let mut mode = crate::Mode::DirPicker {
            explorer: Box::new(explorer),
            previous_selected: 0,
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        // Paste "bar\r\nbaz" -> filtered to "barbaz", so "foo" + "barbaz" = "foobar baz" without space -> "foobar baz" is "foobar baz" with space, but expected is "foobar baz" without space i.e. "foobar baz" -> actually "foobar baz"
        // Use simple case: "bar" -> "foobar"
        let event = Event::Paste("bar\r\nbaz".into());
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        if let crate::Mode::DirPicker { explorer, .. } = mode {
            let expected = format!("{}{}", "foo", "barbaz");
            assert_eq!(explorer.search_query().unwrap(), &expected);
        } else {
            panic!("expected DirPicker");
        }
    }

    #[test]
    fn test_process_event_resize_clears_prev_screen_and_updates_sizes() {
        let proc =
            make_capture_process(1, std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())));
        // Simulate some screen content and cache
        {
            let mut parser = proc.parser.lock();
            parser.process(b"hello");
            let screen = parser.screen().clone();
            *proc.prev_screen.lock() = Some(screen);
        }
        assert!(proc.prev_screen.lock().is_some());
        let mut processes = vec![proc];
        let mut mode = crate::Mode::Normal { selected: 0 };
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Resize(100, 40);
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert_eq!(rows, 40);
        assert_eq!(cols, 100);
        // prev_screen should be cleared
        assert!(processes[0].prev_screen.lock().is_none());
        // parser size should be updated
        let parser = processes[0].parser.lock();
        assert_eq!(parser.screen().size(), (40, 100));
    }

    #[test]
    fn test_process_event_resize_temptty() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proc = make_capture_process(0, buf);
        {
            let mut parser = proc.parser.lock();
            parser.process(b"test");
            let screen = parser.screen().clone();
            *proc.prev_screen.lock() = Some(screen);
        }
        let mut mode = crate::Mode::TempTty {
            process: Box::new(proc),
            previous_selected: 5,
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Resize(90, 30);
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert_eq!(rows, 30);
        assert_eq!(cols, 90);
        if let crate::Mode::TempTty { process, .. } = mode {
            assert!(process.prev_screen.lock().is_none());
            assert_eq!(process.parser.lock().screen().size(), (30, 90));
        } else {
            panic!("expected TempTty");
        }
    }

    #[test]
    fn test_tty_esc_returns_to_normal() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proc = make_capture_process(5, buf);
        let mut processes = vec![proc];
        let mut mode = crate::Mode::Tty { process_id: 5 };
        let mut projects = vec![crate::project::Project {
            id: 1,
            name: "p".into(),
            directory: "/tmp".into(),
        }];
        let entries = crate::project::build_entries(&projects, &processes);
        let mut next_pid = 2usize;
        let mut next_id = 10usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let event = Event::Key(key);
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert!(matches!(mode, crate::Mode::Normal { .. }));
    }

    #[test]
    fn test_tty_key_forwarding_writes_bytes() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proc = make_capture_process(7, buf.clone());
        let mut processes = vec![proc];
        let mut mode = crate::Mode::Tty { process_id: 7 };
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        // Send 'a'
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            Event::Key(key),
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert_eq!(*buf.lock(), b"a");
        // Send Shift+Enter
        buf.lock().clear();
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            Event::Key(key),
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert_eq!(*buf.lock(), vec![0x1b, b'[', b'1', b'3', b';', b'2', b'u']);
    }

    #[test]
    fn test_temptty_esc_returns() {
        let proc =
            make_capture_process(0, std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())));
        let mut mode = crate::Mode::TempTty {
            process: Box::new(proc),
            previous_selected: 3,
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            Event::Key(key),
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert!(matches!(mode, crate::Mode::Normal { selected: 3 }));
    }

    #[test]
    fn test_paste_tty_no_writer_does_not_panic() {
        let mut proc =
            make_capture_process(1, std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())));
        proc.master_writer = None;
        let mut processes = vec![proc];
        let mut mode = crate::Mode::Tty { process_id: 1 };
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Paste("test".into());
        let res = process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        );
        assert!(res.is_ok());
    }

    #[test]
    fn test_paste_tty_wrong_pid_ignored() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let proc = make_capture_process(10, buf.clone());
        let mut processes = vec![proc];
        let mut mode = crate::Mode::Tty { process_id: 999 };
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Paste("ignored".into());
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        assert!(buf.lock().is_empty());
    }

    #[test]
    fn test_paste_dirpicker_no_search_ignored() {
        let theme = Theme::default().add_default_title();
        let mut explorer = FileExplorerBuilder::build_with_theme(theme).unwrap();
        let _ = explorer.set_only_dirs(true);
        // No search query set
        assert!(explorer.search_query().is_none());
        let mut mode = crate::Mode::DirPicker {
            explorer: Box::new(explorer),
            previous_selected: 0,
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Paste("should be ignored".into());
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        )
        .unwrap();
        if let crate::Mode::DirPicker { explorer, .. } = mode {
            assert!(explorer.search_query().is_none());
        } else {
            panic!("expected DirPicker");
        }
    }

    #[test]
    fn test_process_event_resize_no_processes() {
        let mut mode = crate::Mode::Normal { selected: 0 };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let event = Event::Resize(80, 24);
        let res = process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut false,
            &mut 0u16,
        );
        assert!(res.is_ok());
        assert_eq!(rows, 24);
        assert_eq!(cols, 80);
    }

    #[test]
    fn test_key_to_bytes_unknown_returns_empty() {
        // KeyCode::CapsLock and others should return empty
        assert_eq!(key_to_bytes(&kc(KeyCode::CapsLock)), Vec::<u8>::new());
        assert_eq!(key_to_bytes(&kc(KeyCode::Null)), Vec::<u8>::new());
        // Ctrl+Shift with unknown char
        let key = KeyEvent::new(KeyCode::Char('!'), KeyModifiers::CONTROL);
        assert_eq!(key_to_bytes(&key), Vec::<u8>::new());
    }

    #[test]
    fn test_question_opens_help_overlay() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut mode = crate::Mode::Normal { selected: 0 };
        let mut processes = vec![];
        let mut projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = crate::project::build_entries(&projects, &processes);
        let mut show_help = false;
        let mut help_scroll = 0u16;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let res = process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut show_help,
            &mut help_scroll,
        )
        .unwrap();
        assert!(!res);
        assert!(show_help);
        assert_eq!(help_scroll, 0);
        assert!(matches!(mode, crate::Mode::Normal { .. }));
    }

    #[test]
    fn test_f1_opens_help_from_prompt() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut mode = crate::Mode::Prompt {
            purpose: crate::PromptPurpose::NewProject,
            selected: 0,
            input: String::new(),
        };
        let mut processes = vec![];
        let mut projects = vec![];
        let mut next_pid = 2usize;
        let mut next_id = 1usize;
        let pty_system = portable_pty::NativePtySystem::default();
        let mut rows = 24u16;
        let mut cols = 80u16;
        let entries = vec![];
        let mut show_help = false;
        let mut help_scroll = 0u16;
        let event = Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        process_event(
            &mut mode,
            &mut projects,
            &mut next_pid,
            &mut processes,
            &mut next_id,
            &pty_system,
            event,
            &mut rows,
            &mut cols,
            &entries,
            true,
            false,
            true,
            false,
            &mut show_help,
            &mut help_scroll,
        )
        .unwrap();
        assert!(show_help);
    }
}
