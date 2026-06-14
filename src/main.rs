mod input;
mod persistence;
mod process;
mod project;
mod status;
mod ui;

use std::io::stdout;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::Parser;
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
use persistence::load_project_dirs;
use process::{Process, check_tty_alive, sync_statuses};
use project::{Project, build_entries};
use status::STATUS_GIT_CONFLICT;
use ui::render;

#[derive(Parser)]
#[command(name = "multistack", version, about = "Lightweight TUI for parallel AI agent management")]
struct Cli {
    #[arg(short = 'c', long = "continue", default_value_t = false, help = "Load the saved project list from the previous session")]
    continue_session: bool,
    #[arg(long = "dont-save", default_value_t = false, help = "Do not load or save the project list")]
    dont_save: bool,
}

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
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> std::io::Result<()> {
    let mut stdout = stdout();
    enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let pty_system = NativePtySystem::default();
    let mut processes: Vec<Process> = Vec::new();
    let mut next_id: usize = 1;

    let mut projects: Vec<Project> = Vec::new();
    let mut next_project_id: usize = 2;

    let load_from_file = cli.continue_session && !cli.dont_save;
    if load_from_file {
        let dirs = load_project_dirs().unwrap_or_default();
        if !dirs.is_empty() {
            for (i, dir) in dirs.into_iter().enumerate() {
                let id = i + 1;
                let name = Path::new(&dir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&dir)
                    .to_string();
                projects.push(Project {
                    id,
                    name,
                    directory: dir,
                });
            }
            next_project_id = projects.len() + 1;
        }
    }

    if projects.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let main_name = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("main")
            .to_string();
        projects.push(Project {
            id: 1,
            name: main_name,
            directory: cwd.to_string_lossy().to_string(),
        });
        next_project_id = 2;
    }

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
                            cli.dont_save,
                            confirm_quit,
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
                            let has_git_conflict = processes.iter().any(|p| p.status.load(Ordering::SeqCst) == STATUS_GIT_CONFLICT);
                            if has_git_conflict && !confirm_quit {
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
