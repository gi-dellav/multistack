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
    event::{
        DisableBracketedPaste, EnableBracketedPaste, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::{Attribute, Color, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use portable_pty::NativePtySystem;
use ratatui::{Terminal, backend::CrosstermBackend};
use ratatui_explorer_multistack::FileExplorer;

use input::process_event;
use persistence::load_project_dirs;
use process::{Process, check_tty_alive, sync_statuses};
use project::{Project, build_entries};
use status::STATUS_GIT_CONFLICT;
use ui::{enter_tty_real, exit_temp_tty_real, exit_tty_real, render, wipe_real};

#[derive(Parser)]
#[command(
    name = "multistack",
    version,
    about = "Lightweight TUI for parallel AI agent management"
)]
struct Cli {
    #[arg(
        short = 'c',
        long = "continue",
        default_value_t = false,
        help = "Load the saved project list from the previous session"
    )]
    continue_session: bool,
    #[arg(
        long = "dont-save",
        default_value_t = false,
        help = "Do not load or save the project list"
    )]
    dont_save: bool,
    #[arg(
        short = 'w',
        long = "no-worktree",
        default_value_t = false,
        help = "Disable worktree integration (n spawns bare agent, N disabled)"
    )]
    no_worktree: bool,
}

pub enum PromptPurpose {
    NewProcess(usize),
    NewBareProcess(usize),
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
        process: Box<Process>,
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

    let supports_keyboard_enhancement = matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    if supports_keyboard_enhancement {
        let _ = execute!(
            terminal.backend_mut(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        );
    }
    let _ = execute!(terminal.backend_mut(), EnableBracketedPaste);

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
            // The child died while we were showing its raw output: the
            // physical screen still holds TTY content. Restore TUI ownership
            // before the next draw, or TTY rows leak into the list view.
            match &mode {
                Mode::Tty { process_id } => {
                    if let Some(proc) = processes.iter().find(|p| p.id == *process_id) {
                        let _ = exit_tty_real(&mut terminal, proc);
                    } else {
                        // Process already reaped: still need a bare cleanup
                        // (no prev_screen to invalidate, just screen state).
                        // `wipe_real`, not `terminal.clear()` — the latter
                        // blocks on a cursor-position query (~500ms).
                        let _ = wipe_real(&mut terminal);
                    }
                }
                Mode::TempTty { process, .. } => {
                    let _ = exit_temp_tty_real(&mut terminal, &process.prev_screen);
                }
                _ => {}
            }
            mode = Mode::Normal {
                selected: restore_selected,
            };
            if was_tty {
                suppress_quit = true;
            }
            execute!(
                terminal.backend_mut(),
                SetAttribute(Attribute::Reset),
                SetForegroundColor(Color::Reset),
                SetBackgroundColor(Color::Reset)
            )?;
            // NOTE: no `terminal.resize()` here — `exit_tty_real` /
            // `exit_temp_tty_real` / `wipe_real` already reset both buffers,
            // and `resize()` only re-syncs bookkeeping without repainting.
            // (`resize()` itself is cheap, but the old `terminal.clear()` in
            // this path was the blocking call; keep this path query-free.)
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
                render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit, cli.no_worktree)?;
            }
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let was_tty_before_event = matches!(mode, Mode::Tty { .. } | Mode::TempTty { .. });
                        // Snapshot the TTY identity *before* dispatch: after
                        // `process_event` runs, `mode` may already be the new
                        // mode and we can no longer tell which process's cache
                        // to invalidate.
                        let exited_tty_pid = match &mode {
                            Mode::Tty { process_id } => Some(*process_id),
                            _ => None,
                        };
                        let was_temp_tty = matches!(mode, Mode::TempTty { .. });

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
                            cli.no_worktree,
                            confirm_quit,
                        )?;

                        // Any transition *into* a TTY mode must start from a
                        // blank physical screen with a dropped diff cache, or
                        // the first frame diffs TTY content against the stale
                        // TUI screen (= mixed TUI/TTY rendering). TTY(A) ->
                        // TTY(B) (e.g. re-entering after spawn) needs the same
                        // fresh full repaint for the new child.
                        let entered_tty: Option<usize> = match &mode {
                            Mode::Tty { process_id }
                                if !was_tty_before_event || Some(*process_id) != exited_tty_pid =>
                            {
                                Some(*process_id)
                            }
                            _ => None,
                        };
                        if let Some(pid) = entered_tty {
                            if let Some(proc) = processes.iter().find(|p| p.id == pid) {
                                let _ = enter_tty_real(&mut terminal, proc);
                            }
                        } else if matches!(mode, Mode::TempTty { .. }) && !was_tty_before_event
                            && let Mode::TempTty { process, .. } = &mode
                        {
                            let _ = enter_tty_real(&mut terminal, process);
                        }

                        if was_tty_before_event && matches!(mode, Mode::Normal { .. }) {
                            suppress_quit = true;
                            confirm_quit = false;
                            // Leaving raw mode: the physical screen holds TTY
                            // content and the vt100 cache describes it. Either
                            // must be discarded before the TUI redraws.
                            // (`wipe_real`, not `terminal.clear()` — the
                            // latter blocks ~500ms on a cursor query.)
                            if let Some(pid) = exited_tty_pid {
                                if let Some(proc) = processes.iter().find(|p| p.id == pid) {
                                    let _ = exit_tty_real(&mut terminal, proc);
                                } else {
                                    let _ = wipe_real(&mut terminal);
                                }
                            } else if was_temp_tty {
                                // TempTty's Process was moved out of `mode`
                                // already; fall back to a bare wipe — its
                                // cache dies with the value.
                                let _ = wipe_real(&mut terminal);
                            }
                            execute!(
                                terminal.backend_mut(),
                                SetAttribute(Attribute::Reset),
                                SetForegroundColor(Color::Reset),
                                SetBackgroundColor(Color::Reset)
                            )?;
                            let entries = build_entries(&projects, &processes);
                            sync_statuses(&processes);
                            render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit, cli.no_worktree)?;
                        } else if should_quit {
                            if suppress_quit {
                                suppress_quit = false;
                                confirm_quit = true;
                                let entries = build_entries(&projects, &processes);
                                render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit, cli.no_worktree)?;
                                continue;
                            }
                            let has_git_conflict = processes.iter().any(|p| p.status.load(Ordering::SeqCst) == STATUS_GIT_CONFLICT);
                            if has_git_conflict && !confirm_quit {
                                confirm_quit = true;
                                let entries = build_entries(&projects, &processes);
                                render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit, cli.no_worktree)?;
                                continue;
                            }
                            let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
                            if supports_keyboard_enhancement {
                                let _ = execute!(
                                    terminal.backend_mut(),
                                    PopKeyboardEnhancementFlags
                                );
                            }
                            let _ = execute!(
                                terminal.backend_mut(),
                                cursor::Show,
                                terminal::LeaveAlternateScreen
                            );
                            let _ = disable_raw_mode();
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
                            render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit, cli.no_worktree)?;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("Event stream error: {e}. Shutting down.");
                        let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
                        if supports_keyboard_enhancement {
                            let _ = execute!(
                                terminal.backend_mut(),
                                PopKeyboardEnhancementFlags
                            );
                        }
                        let _ = execute!(
                            terminal.backend_mut(),
                            cursor::Show,
                            terminal::LeaveAlternateScreen
                        );
                        let _ = disable_raw_mode();
                        return Err(e);
                    }
                    None => {
                        let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
                        if supports_keyboard_enhancement {
                            let _ = execute!(
                                terminal.backend_mut(),
                                PopKeyboardEnhancementFlags
                            );
                        }
                        let _ = execute!(
                            terminal.backend_mut(),
                            cursor::Show,
                            terminal::LeaveAlternateScreen
                        );
                        let _ = disable_raw_mode();
                        return Ok(());
                    }
                }
            }
        }
    }
}
