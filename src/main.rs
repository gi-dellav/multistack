mod input;
mod process;
mod ui;

use std::io::stdout;
use std::time::Duration;

use crossterm::{
    cursor, execute,
    event::EventStream,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use portable_pty::NativePtySystem;
use ratatui::{Terminal, backend::CrosstermBackend};

use input::process_event;
use process::{Process, check_tty_alive};
use ui::render;

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

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let pty_system = NativePtySystem::default();
    let mut processes: Vec<Process> = Vec::new();
    let mut next_id: usize = 1;
    let mut mode = Mode::Normal { selected: 0 };

    let (cols, rows) = terminal::size()?;
    let mut term_rows = rows;
    let mut term_cols = cols;

    let mut reader = EventStream::new();

    loop {
        if check_tty_alive(&mut mode, &processes) {
            let size = terminal.size()?;
            terminal.resize(size.into())?;
        }

        render(&mut terminal, &mode, &processes, term_rows, term_cols)?;

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
                            execute!(terminal.backend_mut(), cursor::Show, terminal::LeaveAlternateScreen)?;
                            disable_raw_mode()?;
                            return Ok(());
                        }
                    }
                    Some(Err(_)) => {}
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
