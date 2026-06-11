mod input;
mod process;
mod status;
mod ui;

use std::io::stdout;
use std::time::Duration;

use crossterm::{
    cursor,
    event::EventStream,
    execute,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use portable_pty::NativePtySystem;
use ratatui::{Terminal, backend::CrosstermBackend};

use input::process_event;
use process::{Process, check_tty_alive, sync_statuses};
use ui::render;

enum PromptPurpose {
    NewProcess,
    Rename(usize),
}

enum Mode {
    Normal {
        selected: usize,
    },
    Tty {
        process_id: usize,
    },
    Prompt {
        purpose: PromptPurpose,
        selected: usize,
        input: String,
    },
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

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let pty_system = NativePtySystem::default();
    let mut processes: Vec<Process> = Vec::new();
    let mut next_id: usize = 1;
    let mut mode = Mode::Normal { selected: 0 };

    let (cols, rows) = terminal::size()?;
    let mut term_rows = rows;
    let mut term_cols = cols;

    let mut reader = EventStream::new();
    let mut suppress_quit = false;
    let mut confirm_quit = false;

    let mut render_interval = tokio::time::interval(Duration::from_millis(50));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let was_tty = matches!(mode, Mode::Tty { .. });

        if check_tty_alive(&mut mode, &mut processes) {
            let is_normal = matches!(mode, Mode::Normal { .. });
            if was_tty && is_normal {
                suppress_quit = true;
            }
            let size = terminal.size()?;
            terminal.resize(size.into())?;
        }

        sync_statuses(&processes);

        tokio::select! {
                _ = render_interval.tick() => {
                render(&mut terminal, &mode, &processes, term_rows, term_cols, confirm_quit)?;
            }
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let was_tty_before_event = matches!(mode, Mode::Tty { .. });

                        let should_quit = process_event(
                            &mut mode,
                            &mut processes,
                            &mut next_id,
                            &pty_system,
                            event,
                            &mut term_rows,
                            &mut term_cols,
                        )?;

                        if was_tty_before_event && matches!(mode, Mode::Normal { .. }) {
                            suppress_quit = true;
                            confirm_quit = false;
                            let size = terminal.size()?;
                            terminal.resize(size.into())?;
                            sync_statuses(&processes);
                            render(&mut terminal, &mode, &processes, term_rows, term_cols, confirm_quit)?;
                        } else if should_quit {
                            if suppress_quit {
                                suppress_quit = false;
                                confirm_quit = true;
                                render(&mut terminal, &mode, &processes, term_rows, term_cols, confirm_quit)?;
                                continue;
                            }
                            execute!(terminal.backend_mut(), cursor::Show, terminal::LeaveAlternateScreen)?;
                            disable_raw_mode()?;
                            return Ok(());
                        } else {
                            if matches!(mode, Mode::Normal { .. }) {
                                suppress_quit = false;
                                confirm_quit = false;
                            }
                            render(&mut terminal, &mode, &processes, term_rows, term_cols, confirm_quit)?;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("Event stream error: {e}. Shutting down.");
                        execute!(terminal.backend_mut(), cursor::Show, terminal::LeaveAlternateScreen)?;
                        disable_raw_mode()?;
                        return Err(e);
                    }
                    None => {
                        execute!(terminal.backend_mut(), cursor::Show, terminal::LeaveAlternateScreen)?;
                        disable_raw_mode()?;
                        return Ok(());
                    }
                }
            }
        }
    }
}
