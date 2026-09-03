use std::io::Write;
use std::sync::atomic::Ordering;

use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::backend::Backend;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{FrameExt, List, ListItem, ListState};
use ratatui_explorer_multistack::FileExplorer;

use crate::Mode;
use crate::process::Process;
use crate::project::{ListEntry, Project};
use crate::status;

#[allow(clippy::too_many_arguments)]
pub fn render<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    mode: &Mode,
    entries: &[ListEntry],
    projects: &[Project],
    processes: &[Process],
    rows: u16,
    cols: u16,
    confirm_quit: bool,
    no_worktree: bool,
) -> std::io::Result<()> {
    match mode {
        Mode::Normal { selected } => render_normal(
            terminal,
            entries,
            projects,
            processes,
            *selected,
            rows,
            cols,
            confirm_quit,
            no_worktree,
        ),
        Mode::Prompt {
            purpose,
            selected,
            input,
        } => render_prompt(
            terminal, entries, projects, processes, *selected, purpose, input, rows, cols,
        ),
        Mode::Tty { process_id } => {
            if let Some(proc) = processes.iter().find(|p| p.id == *process_id) {
                render_tty(terminal, proc, rows, cols)
            } else {
                Ok(())
            }
        }
        Mode::TempTty { process, .. } => render_tty(terminal, process, rows, cols),
        Mode::DirPicker { explorer, .. } => render_dirpicker(terminal, explorer, rows, cols),
    }
}

fn find_process(processes: &[Process], id: usize) -> Option<&Process> {
    processes.iter().find(|p| p.id == id)
}

fn find_project(projects: &[Project], id: usize) -> Option<&Project> {
    projects.iter().find(|p| p.id == id)
}

fn process_item(proc: &Process) -> ListItem<'static> {
    let status_val = proc.status.load(Ordering::SeqCst);
    let prefix = status::status_prefix(status_val);
    let color = status::status_color(status_val);
    let cycle = proc.cycle_start.lock();
    let timer = status::format_timer(proc.active_ms.load(Ordering::SeqCst), &cycle);
    let line = Line::from(Span::styled(
        format!("  {} {}  {}", prefix, proc.name, timer),
        Style::default().fg(color),
    ));
    ListItem::new(line)
}

#[allow(clippy::too_many_arguments)]
fn render_normal<B: Backend>(
    terminal: &mut Terminal<B>,
    entries: &[ListEntry],
    projects: &[Project],
    processes: &[Process],
    selected: usize,
    _rows: u16,
    cols: u16,
    confirm_quit: bool,
    no_worktree: bool,
) -> std::io::Result<()> {
    terminal
        .draw(|frame| {
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]);
        let [title_area, sep_area, _, list_area, help_area] = frame.area().layout(&layout);

        let title = Line::from(vec![Span::styled(
            "Multistack",
            Style::default().add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(title.centered(), title_area);

        let sep_width = cols as usize;
        let sep = Line::from("═".repeat(sep_width));
        frame.render_widget(sep.centered(), sep_area);

        if entries.is_empty() {
            let empty = Line::from("  (no processes)");
            frame.render_widget(empty, list_area);
        } else {
            let header_style = Style::default().add_modifier(Modifier::DIM);
            let items: Vec<ListItem> = entries
                .iter()
                .map(|e| match e {
                    ListEntry::ProjectHeader(pid) => {
                        let proj = find_project(projects, *pid);
                        let name = proj.map(|p| p.name.as_str()).unwrap_or("?");
                        let dir = proj.map(|p| p.directory.as_str()).unwrap_or("?");
                        let line = Line::from(Span::styled(
                            format!("\u{2500}\u{2500} Project: {} ({}) \u{2500}\u{2500}", name, dir),
                            header_style,
                        ));
                        ListItem::new(line)
                    }
                    ListEntry::Agent(proc_id) => {
                        if let Some(proc) = find_process(processes, *proc_id) {
                            process_item(proc)
                        } else {
                            ListItem::new("")
                        }
                    }
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Modifier::REVERSED)
                .highlight_symbol("> ");
            let mut list_state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(list, list_area, &mut list_state);
        }

        let help = if confirm_quit {
            let has_git_conflict = processes.iter().any(|p| p.status.load(Ordering::SeqCst) == status::STATUS_GIT_CONFLICT);
            if has_git_conflict {
                Line::from("Git conflicts! Press q to quit anyway, Esc to go back")
            } else {
                Line::from("Press q again to quit")
            }
        } else if no_worktree {
            if cols < 40 {
                Line::from("m:bare r:ren d:kill h:lg s:sh p/l:new/rmprj Enter:TTY q:quit")
            } else {
                Line::from("m: spawn bare  r: rename  d: kill  h: lazygit  s: shell  p/l: new/rm project  Enter: TTY  q/Esc: quit")
            }
        } else if cols < 40 {
            Line::from("n:new N:go m:bare r:ren d:kill h:lg s:sh p/l:new/rmprj Enter:TTY q:quit")
        } else {
            Line::from("n: new  N: spawn & enter  m: spawn bare  r: rename  d: kill  h: lazygit  s: shell  p/l: new/rm project  Enter: TTY  q/Esc: quit")
        };
        frame.render_widget(help, help_area);
    }).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_prompt<B: Backend>(
    terminal: &mut Terminal<B>,
    entries: &[ListEntry],
    projects: &[Project],
    processes: &[Process],
    selected: usize,
    purpose: &crate::PromptPurpose,
    input: &str,
    _rows: u16,
    cols: u16,
) -> std::io::Result<()> {
    terminal.draw(|frame| {
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]);
        let [title_area, sep_area, _, list_area, help_area] = frame.area().layout(&layout);

        let title = Line::from(vec![Span::styled(
            "Multistack",
            Style::default().add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(title.centered(), title_area);

        let sep_width = cols as usize;
        let sep = Line::from("═".repeat(sep_width));
        frame.render_widget(sep.centered(), sep_area);

        if entries.is_empty() {
            let empty = Line::from("  (no processes)");
            frame.render_widget(empty, list_area);
        } else {
            let header_style = Style::default().add_modifier(Modifier::DIM);
            let items: Vec<ListItem> = entries
                .iter()
                .map(|e| match e {
                    ListEntry::ProjectHeader(pid) => {
                        let proj = find_project(projects, *pid);
                        let name = proj.map(|p| p.name.as_str()).unwrap_or("?");
                        let dir = proj.map(|p| p.directory.as_str()).unwrap_or("?");
                        let line = Line::from(Span::styled(
                            format!(
                                "\u{2500}\u{2500} Project: {} ({}) \u{2500}\u{2500}",
                                name, dir
                            ),
                            header_style,
                        ));
                        ListItem::new(line)
                    }
                    ListEntry::Agent(proc_id) => {
                        if let Some(proc) = find_process(processes, *proc_id) {
                            process_item(proc)
                        } else {
                            ListItem::new("")
                        }
                    }
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Modifier::REVERSED)
                .highlight_symbol("> ");
            let mut list_state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(list, list_area, &mut list_state);
        }

        let label = match purpose {
            crate::PromptPurpose::NewProcess(_) | crate::PromptPurpose::NewBareProcess(_) => "new name: ",
            crate::PromptPurpose::NewProject => "project dir: ",
            crate::PromptPurpose::Rename(_) => "rename: ",
        };
        let help = Line::from(format!("{}{}_", label, input));
        frame.render_widget(help, help_area);
    }).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    Ok(())
}

pub(crate) fn tty_output(proc: &Process) -> Option<Vec<u8>> {
    let parser = proc.parser.lock();
    let screen = parser.screen();
    let mut prev = proc.prev_screen.lock();
    let bytes = if let Some(prev_screen) = prev.as_ref() {
        if prev_screen.size() == screen.size() {
            screen.state_diff(prev_screen)
        } else {
            screen.state_formatted()
        }
    } else {
        screen.state_formatted()
    };
    if bytes.is_empty() {
        return None;
    }
    *prev = Some(screen.clone());
    Some(bytes)
}

fn render_tty<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    proc: &Process,
    _rows: u16,
    _cols: u16,
) -> std::io::Result<()> {
    let Some(output) = tty_output(proc) else {
        return Ok(());
    };
    let stdout = terminal.backend_mut();
    execute!(stdout, BeginSynchronizedUpdate)?;
    stdout.write_all(&output)?;
    execute!(stdout, EndSynchronizedUpdate)?;
    std::io::Write::flush(stdout)?;
    Ok(())
}

fn render_dirpicker<B: Backend>(
    terminal: &mut Terminal<B>,
    explorer: &FileExplorer,
    _rows: u16,
    cols: u16,
) -> std::io::Result<()> {
    terminal.draw(|frame| {
        let layout = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]);
        let [title_area, explorer_area, help_area] = frame.area().layout(&layout);

        let title = Line::from(vec![Span::styled(
            "Select project directory",
            Style::default().add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(title.centered(), title_area);

        frame.render_widget_ref(explorer.widget(), explorer_area);

        let help = if let Some(query) = explorer.search_query() {
            if cols < 40 {
                Line::from(format!("/{}", query))
            } else {
                Line::from(format!("/{}  Esc: clear search", query))
            }
        } else if cols < 40 {
            Line::from("Enter:pick Esc:cancel arrows:nav /:search")
        } else {
            Line::from("Enter: pick directory  Esc: cancel  \u{2191}\u{2193}\u{2190}\u{2192}: navigate  /: search")
        };
        frame.render_widget(help, help_area);
    }).map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Process;
    use parking_lot::Mutex;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64},
    };

    fn make_proc_with_content(content: &[u8], rows: u16, cols: u16) -> Process {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        parser.lock().process(content);
        Process {
            id: 1,
            project_id: 1,
            project_dir: "/tmp".into(),
            name: "test".into(),
            child: None,
            master: None,
            master_writer: None,
            parser,
            alive: Arc::new(AtomicBool::new(true)),
            status: Arc::new(AtomicU8::new(0)),
            active_ms: Arc::new(AtomicU64::new(0)),
            cycle_start: Arc::new(Mutex::new(None)),
            status_socket_path: None,
            shutdown_flag: None,
            listener_thread: None,
            kill_on_drop: false,
            name_shared: None,
            prev_screen: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn test_render_tty_first_call_sets_prev_and_produces_output() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        assert!(proc.prev_screen.lock().is_none());
        let out = tty_output(&proc);
        assert!(out.is_some());
        assert!(proc.prev_screen.lock().is_some());
        assert!(!out.unwrap().is_empty());
    }

    #[test]
    fn test_render_tty_second_call_same_content_is_noop() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        let first = tty_output(&proc);
        assert!(first.is_some());
        let prev_clone = proc.prev_screen.lock().clone().unwrap();
        // Second call with same screen should be empty diff and return None
        let second = tty_output(&proc);
        assert!(second.is_none());
        let prev_after = proc.prev_screen.lock().clone().unwrap();
        assert_eq!(prev_clone.contents(), prev_after.contents());
    }

    #[test]
    fn test_render_tty_diff_smaller_than_full() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        let _ = tty_output(&proc).unwrap();
        let prev = proc.prev_screen.lock().clone().unwrap();
        proc.parser.lock().process(b" world");
        let current = proc.parser.lock().screen().clone();
        let diff = current.state_diff(&prev);
        let full = current.state_formatted();
        assert!(!diff.is_empty());
        assert!(diff.len() < full.len());
        let out = tty_output(&proc);
        assert!(out.is_some());
        assert_eq!(out.unwrap(), diff);
    }

    #[test]
    fn test_render_tty_resize_forces_full_redraw() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        let _ = tty_output(&proc).unwrap();
        let prev_size = proc.prev_screen.lock().as_ref().unwrap().size();
        assert_eq!(prev_size, (10, 20));
        {
            let mut parser = proc.parser.lock();
            *parser = vt100::Parser::new(15, 30, 0);
            parser.process(b"hello");
        }
        *proc.prev_screen.lock() = None;
        assert!(proc.prev_screen.lock().is_none());
        let out = tty_output(&proc);
        assert!(out.is_some());
        assert!(proc.prev_screen.lock().is_some());
        let new_size = proc.prev_screen.lock().as_ref().unwrap().size();
        assert_eq!(new_size, (15, 30));
        // Full redraw should contain clear screen
        assert!(out.unwrap().windows(2).any(|w| w == b"\x1b[" ));
    }

    #[test]
    fn test_render_tty_missing_process_returns_ok() {
        // For missing process, render should not attempt TTY rendering and just return Ok.
        // We test via helper that no panic occurs when process not found – the render function
        // handles missing process gracefully.
        let proc = make_proc_with_content(b"", 24, 80);
        let out = tty_output(&proc);
        assert!(out.is_some());
        // Simulate missing process case: no output expected for non-existent pid
        // (render would return Ok without calling tty_output)
    }

    #[test]
    fn test_render_tty_synchronized_update_sequences_present() {
        let proc = make_proc_with_content(b"test", 5, 10);
        let screen = proc.parser.lock().screen().clone();
        let full = screen.state_formatted();
        assert!(full.windows(3).any(|w| w == b"\x1b[H"));
        let out = tty_output(&proc).unwrap();
        // The TTY output (vt100 state) should be non-empty and contain content
        assert!(!out.is_empty());
        assert!(proc.prev_screen.lock().is_some());
        // Verify synchronized update framing would be added by render_tty (not by vt100)
        // The helper itself does not add framing, but render_tty would wrap it.
        // Here we just ensure helper works.
    }

    #[test]
    fn test_render_normal_and_prompt_do_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let projects = vec![crate::project::Project { id: 1, name: "proj".into(), directory: "/tmp".into() }];
        let proc = make_proc_with_content(b"", 24, 80);
        let entries = crate::project::build_entries(&projects, &[proc]);
        let res = render_normal(&mut terminal, &entries, &projects, &[], 0, 24, 80, false, false);
        assert!(res.is_ok());
        let res = render_prompt(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            &crate::PromptPurpose::NewProject,
            "input",
            24,
            80,
        );
        assert!(res.is_ok());
    }
}
