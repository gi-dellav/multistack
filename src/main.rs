mod input;
mod process;
mod project;
mod status;
mod ui;

use std::io::stdout;
use std::path::Path;
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
use ratatui_explorer::FileExplorer;

use input::process_event;
use process::{Process, check_tty_alive, sync_statuses};
use project::{Project, build_entries};
use ui::render;

pub enum PromptPurpose {
    NewProcess(usize),
    NewProject,
    Rename(usize),
}

enum Mode {
    Normal {
        selected: usize,
    },
    Tty {
        process_id: usize,
    },
    TempTty {
        process: Process,
        previous_selected: usize,
    },
    Prompt {
        purpose: PromptPurpose,
        selected: usize,
        input: String,
    },
    DirPicker {
        explorer: Box<FileExplorer>,
        previous_selected: usize,
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

    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let main_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("main")
        .to_string();
    let mut projects = vec![Project {
        id: 1,
        name: main_name,
        directory: cwd.to_string_lossy().to_string(),
    }];
    let mut next_project_id: usize = 2;

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
        let was_tty = matches!(mode, Mode::Tty { .. } | Mode::TempTty { .. });

        if let Some(restore_selected) = check_tty_alive(&mode, &mut processes) {
            mode = Mode::Normal {
                selected: restore_selected,
            };
            if was_tty {
                suppress_quit = true;
            }
            let size = terminal.size()?;
            terminal.resize(size.into())?;
        }

        let entries = build_entries(&projects, &processes);

        if let Mode::Normal { ref mut selected } = mode {
            if entries.is_empty() {
                *selected = 0;
            } else if *selected >= entries.len() {
                *selected = entries.len() - 1;
            }
        }

        sync_statuses(&processes);

        tokio::select! {
                _ = render_interval.tick() => {
                render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit)?;
            }
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let was_tty_before_event = matches!(mode, Mode::Tty { .. } | Mode::TempTty { .. });

                        let should_quit = process_event(
                            &mut mode,
                            &mut projects,
                            &mut next_project_id,
                            &mut processes,
                            &mut next_id,
                            &pty_system,
                            event,
                            &mut term_rows,
                            &mut term_cols,
                            &entries,
                        )?;

                        if was_tty_before_event && matches!(mode, Mode::Normal { .. }) {
                            suppress_quit = true;
                            confirm_quit = false;
                            let size = terminal.size()?;
                            terminal.resize(size.into())?;
                            let entries = build_entries(&projects, &processes);
                            sync_statuses(&processes);
                            render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit)?;
                        } else if should_quit {
                            if suppress_quit {
                                suppress_quit = false;
                                confirm_quit = true;
                                let entries = build_entries(&projects, &processes);
                                render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit)?;
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
                            let entries = build_entries(&projects, &processes);
                            if let Mode::Normal { ref mut selected } = mode {
                                if entries.is_empty() {
                                    *selected = 0;
                                } else if *selected >= entries.len() {
                                    *selected = entries.len() - 1;
                                }
                            }
                            render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit)?;
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
