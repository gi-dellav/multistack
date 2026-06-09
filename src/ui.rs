use std::io::Write;
use std::sync::atomic::Ordering;

use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListState};
use ratatui::Terminal;

use crate::Mode;
use crate::PromptPurpose;
use crate::process::Process;
use crate::status;

pub fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mode: &Mode,
    processes: &[Process],
    _rows: u16,
    _cols: u16,
) -> std::io::Result<()> {
    match mode {
        Mode::Normal { selected } => render_normal(terminal, processes, *selected),
        Mode::Prompt { purpose, selected, input } => render_prompt(terminal, processes, *selected, purpose, input),
        Mode::Tty { process_id } => {
            if let Some(proc) = processes.iter().find(|p| p.id == *process_id) {
                render_tty(terminal, proc)
            } else {
                Ok(())
            }
        }
    }
}

fn render_normal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    processes: &[Process],
    selected: usize,
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

        let title = Line::from(vec![
            Span::styled("Multistack", Style::default().add_modifier(Modifier::BOLD)),
        ]);
        frame.render_widget(title.centered(), title_area);

        let sep = Line::from("════════════════════════════════");
        frame.render_widget(sep.centered(), sep_area);

        if processes.is_empty() {
            let empty = Line::from("  (no processes)");
            frame.render_widget(empty, list_area);
        } else {
            let items: Vec<String> = processes
                .iter()
                .map(|p| {
                    let prefix = status::status_prefix(p.status.load(Ordering::SeqCst));
                    let cycle = p.cycle_start.lock();
                    let timer = status::format_timer(p.active_ms.load(Ordering::SeqCst), &cycle);
                    format!("{} {}  {}", prefix, p.name, timer)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Modifier::REVERSED)
                .highlight_symbol("> ");
            let mut list_state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(list, list_area, &mut list_state);
        }

        let help = Line::from("n: new  r: rename  k: kill  Enter: TTY  q/Esc: quit");
        frame.render_widget(help, help_area);
    })?;
    Ok(())
}

fn render_prompt(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    processes: &[Process],
    selected: usize,
    purpose: &PromptPurpose,
    input: &str,
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

        let title = Line::from(vec![
            Span::styled("Multistack", Style::default().add_modifier(Modifier::BOLD)),
        ]);
        frame.render_widget(title.centered(), title_area);

        let sep = Line::from("════════════════════════════════");
        frame.render_widget(sep.centered(), sep_area);

        if processes.is_empty() {
            let empty = Line::from("  (no processes)");
            frame.render_widget(empty, list_area);
        } else {
            let items: Vec<String> = processes
                .iter()
                .map(|p| {
                    let prefix = status::status_prefix(p.status.load(Ordering::SeqCst));
                    let cycle = p.cycle_start.lock();
                    let timer = status::format_timer(p.active_ms.load(Ordering::SeqCst), &cycle);
                    format!("{} {}  {}", prefix, p.name, timer)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Modifier::REVERSED)
                .highlight_symbol("> ");
            let mut list_state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(list, list_area, &mut list_state);
        }

        let label = match purpose {
            PromptPurpose::NewProcess => "new name: ",
            PromptPurpose::Rename(_) => "rename: ",
        };
        let help = Line::from(format!("{}{}_", label, input));
        frame.render_widget(help, help_area);
    })?;
    Ok(())
}

fn render_tty(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    proc: &Process,
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
