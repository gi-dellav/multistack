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
use crate::process::Process;

pub fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mode: &Mode,
    processes: &[Process],
    _rows: u16,
    _cols: u16,
) -> std::io::Result<()> {
    match mode {
        Mode::Normal { selected } => render_normal(terminal, processes, *selected),
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
                    let dead = if !p.alive.load(Ordering::SeqCst) {
                        " [dead]"
                    } else {
                        ""
                    };
                    format!("{}{}", p.name, dead)
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Modifier::REVERSED)
                .highlight_symbol("> ");
            let mut list_state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(list, list_area, &mut list_state);
        }

        let help = Line::from("n: new  k: kill  Enter: open TTY  q/Esc: quit");
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
