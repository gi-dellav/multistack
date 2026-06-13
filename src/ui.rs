use std::io::Write;
use std::sync::atomic::Ordering;

use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{FrameExt, List, ListItem, ListState};
use ratatui_explorer::FileExplorer;

use crate::Mode;
use crate::process::Process;
use crate::project::{ListEntry, Project};
use crate::status;

#[allow(clippy::too_many_arguments)]
pub fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mode: &Mode,
    entries: &[ListEntry],
    projects: &[Project],
    processes: &[Process],
    rows: u16,
    cols: u16,
    confirm_quit: bool,
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
fn render_normal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    entries: &[ListEntry],
    projects: &[Project],
    processes: &[Process],
    selected: usize,
    _rows: u16,
    cols: u16,
    confirm_quit: bool,
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
            Line::from("Press q again to quit")
        } else if cols < 40 {
            Line::from("n:new N:go m:bare r:ren d:kill h:lg s:sh p/l:new/rmprj Enter:TTY q:quit")
        } else {
            Line::from("n: new  N: spawn & enter  m: spawn bare  r: rename  d: kill  h: lazygit  s: shell  p/l: new/rm project  Enter: TTY  q/Esc: quit")
        };
        frame.render_widget(help, help_area);
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_prompt(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
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
            crate::PromptPurpose::NewProcess(_) => "new name: ",
            crate::PromptPurpose::NewProject => "project dir: ",
            crate::PromptPurpose::Rename(_) => "rename: ",
        };
        let help = Line::from(format!("{}{}_", label, input));
        frame.render_widget(help, help_area);
    })?;
    Ok(())
}

fn render_tty(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    proc: &Process,
    _rows: u16,
    _cols: u16,
) -> std::io::Result<()> {
    let (contents, cursor_row, cursor_col) = {
        let parser = proc.parser.lock();
        let screen = parser.screen();
        let contents = screen.contents_formatted();
        let (row, col) = screen.cursor_position();
        (contents, row, col)
    };

    let stdout = terminal.backend_mut();
    execute!(stdout, cursor::MoveTo(0, 0))?;
    stdout.write_all(&contents)?;
    execute!(stdout, cursor::MoveTo(cursor_col, cursor_row))?;
    stdout.flush()?;
    Ok(())
}

fn render_dirpicker(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
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
    })?;
    Ok(())
}
