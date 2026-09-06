#[cfg(feature = "attach")]
mod attach;
#[cfg(feature = "attach")]
mod attach_proto;
mod input;
mod persistence;
mod process;
mod project;
mod status;
mod ui;

use std::io::stdout;
use std::path::Path;
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
use process::{Process, check_tty_alive, mark_tty_seen, sync_statuses};
use project::{Project, build_entries};
#[cfg(feature = "attach")]
use ui::force_full_repaint;
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
    #[arg(
        short = 'D',
        long = "no-activity-dot",
        default_value_t = false,
        help = "Disable the unread-activity dot shown when an agent finishes"
    )]
    no_activity_dot: bool,
    /// Attach to another running multistack instance. With no value, attaches
    /// to the oldest running instance; with a PID (`--attach 1234`), attaches
    /// to that instance. Ctrl+\ detaches. Compile-time feature `attach`
    /// (enabled by default); without it this flag does not exist.
    #[cfg(feature = "attach")]
    #[arg(
        long = "attach",
        num_args = 0..=1,
        default_missing_value = "",
        value_name = "PID",
        help = "Attach to another running multistack instance (default: oldest)"
    )]
    attach: Option<String>,
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
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

#[cfg(feature = "attach")]
async fn run(cli: Cli) -> std::io::Result<()> {
    // Client mode: mirror another instance, never bind our own socket.
    if let Some(target) = cli.attach.as_deref() {
        return attach::run_attach(target).await;
    }
    run_server(cli).await
}

#[cfg(not(feature = "attach"))]
async fn run(cli: Cli) -> std::io::Result<()> {
    run_server(cli).await
}

async fn run_server(cli: Cli) -> std::io::Result<()> {
    // Attach listener (server side). Bound before entering the alternate
    // screen so a bind failure is visible on stderr. A bind failure is
    // non-fatal: the session runs without attach.
    #[cfg(feature = "attach")]
    let mut attach_guard = match attach::AttachListener::bind() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("multistack: attach disabled (cannot bind socket): {e}");
            None
        }
    };
    #[cfg(feature = "attach")]
    let mut attach_tokio: Option<tokio::net::UnixListener> =
        attach_guard.as_mut().and_then(|g| g.take_listener());
    #[cfg(not(feature = "attach"))]
    let mut attach_tokio: Option<()> = None;

    // Remote input from an attached client. Its sender is cloned into each
    // accepted pump; `recv()` yields the forwarded events. Always created
    // (even without the feature) so the `select!` below is identical — in
    // non-attach builds the sender is simply never used.
    #[allow(unused_mut)]
    let (remote_tx, mut remote_rx) = tokio::sync::mpsc::channel::<crossterm::event::Event>(128);
    #[cfg(not(feature = "attach"))]
    let _ = remote_tx.clone();

    let mut stdout = stdout();
    enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    #[cfg(feature = "attach")]
    let tee = attach::TeeWriter::new(stdout);
    #[cfg(feature = "attach")]
    let mut terminal = Terminal::new(CrosstermBackend::new(tee.clone()))?;
    #[cfg(not(feature = "attach"))]
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
    let mut confirm_quit = false;
    let mut show_help = false;
    let mut help_scroll: u16 = 0;

    let mut render_interval = tokio::time::interval(Duration::from_millis(50));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
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

        sync_statuses(&mut processes);
        let activity_dot_enabled = !cli.no_activity_dot;
        // A `stop` that lands while its TTY is open was already seen: clear
        // it every frame so no stale dot appears when returning to the list.
        mark_tty_seen(&mode, &processes);

        tokio::select! {
                _ = render_interval.tick() => {
                render(&mut terminal, &mode, &entries, &projects, &processes, term_rows, term_cols, confirm_quit, cli.no_worktree, activity_dot_enabled, show_help, help_scroll)?;
            }
            accepted = accept_attach_conn(&mut attach_tokio) => {
                #[cfg(feature = "attach")]
                {
                    if let Some(stream) = accepted {
                        let conn = attach::AttachConn { stream };
                        let tee2 = tee.clone();
                        let tx2 = remote_tx.clone();
                        // `current_thread` runtime: `spawn` runs the pump
                        // cooperatively on this same thread. `pump_server_conn`
                        // is fully async (tokio UnixStream), so it never blocks
                        // the event loop.
                        tokio::spawn(async move {
                            attach::pump_server_conn(conn, tee2, tx2).await;
                        });
                    }
                }
                #[cfg(not(feature = "attach"))]
                {
                    drop(accepted);
                }
            }
            // Forwarded input from the attached client: inject as if local.
            // Falls through to the same dispatch as `EventStream` events.
            Some(remote_event) = remote_rx.recv() => {
                #[cfg(feature = "attach")]
                if matches!(remote_event, crossterm::event::Event::Resize(_, _)) {
                    // Remote Hello (and real client resizes) must produce
                    // an immediate full frame: the client starts from a
                    // blank screen, but a plain render would diff against
                    // the server's current screen, emit nothing (no state
                    // changed), and leave the client blank until the next
                    // keystroke.
                    force_full_repaint(&mut terminal, &mode, &processes);
                }
                let quit = dispatch_event(
                    remote_event,
                    &mut mode,
                    &mut projects,
                    &mut next_project_id,
                    &mut processes,
                    &mut next_id,
                    &pty_system,
                    &mut terminal,
                    &mut term_rows,
                    &mut term_cols,
                    cli.dont_save,
                    cli.no_worktree,
                    activity_dot_enabled,
                    &mut confirm_quit,
                    &mut show_help,
                    &mut help_scroll,
                )?;
                if quit {
                    restore_terminal(&mut terminal, supports_keyboard_enhancement);
                    #[cfg(feature = "attach")]
                    shutdown_attach(&tee, &mut attach_guard);
                    return Ok(());
                }
            }
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let quit = dispatch_event(
                            event,
                            &mut mode,
                            &mut projects,
                            &mut next_project_id,
                            &mut processes,
                            &mut next_id,
                            &pty_system,
                            &mut terminal,
                            &mut term_rows,
                            &mut term_cols,
                            cli.dont_save,
                            cli.no_worktree,
                            activity_dot_enabled,
                            &mut confirm_quit,
                            &mut show_help,
                            &mut help_scroll,
                        )?;
                        if quit {
                            restore_terminal(&mut terminal, supports_keyboard_enhancement);
                            #[cfg(feature = "attach")]
                            shutdown_attach(&tee, &mut attach_guard);
                            return Ok(());
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("Event stream error: {e}. Shutting down.");
                        restore_terminal(&mut terminal, supports_keyboard_enhancement);
                        #[cfg(feature = "attach")]
                        shutdown_attach(&tee, &mut attach_guard);
                        return Err(e);
                    }
                    None => {
                        restore_terminal(&mut terminal, supports_keyboard_enhancement);
                        #[cfg(feature = "attach")]
                        shutdown_attach(&tee, &mut attach_guard);
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Wait for the next attach connection. Returns `None` when there is no
/// listener (attach disabled at runtime, or compiled without the feature —
/// pending forever so the `select!` branch stays dormant).
#[cfg(feature = "attach")]
async fn accept_attach_conn(
    listener: &mut Option<tokio::net::UnixListener>,
) -> Option<tokio::net::UnixStream> {
    match listener.as_mut() {
        None => std::future::pending().await,
        Some(l) => l.accept().await.map(|(s, _)| Some(s)).unwrap_or(None),
    }
}

#[cfg(not(feature = "attach"))]
async fn accept_attach_conn(_listener: &mut Option<()>) -> Option<tokio::net::UnixStream> {
    std::future::pending().await
}

/// Tell the attached client to go away and unlink our socket. Runs on every
/// server exit path (also via `AttachListener::drop` as a backstop).
#[cfg(feature = "attach")]
fn shutdown_attach(tee: &attach::TeeWriter, guard: &mut Option<attach::AttachListener>) {
    tee.send_bye();
    if let Some(g) = guard.as_mut() {
        g.shutdown();
    }
}

/// Shared event dispatch for local (`EventStream`) and remote (attach
/// client) events: TTY enter/exit framing, quit confirmation, and render.
/// Returns `true` when the server should exit.
#[allow(clippy::too_many_arguments)]
fn dispatch_event<W: std::io::Write>(
    event: crossterm::event::Event,
    mode: &mut Mode,
    projects: &mut Vec<Project>,
    next_project_id: &mut usize,
    processes: &mut Vec<Process>,
    next_id: &mut usize,
    pty_system: &NativePtySystem,
    terminal: &mut Terminal<CrosstermBackend<W>>,
    term_rows: &mut u16,
    term_cols: &mut u16,
    dont_save: bool,
    no_worktree: bool,
    activity_dot_enabled: bool,
    confirm_quit: &mut bool,
    show_help: &mut bool,
    help_scroll: &mut u16,
) -> std::io::Result<bool> {
    // Help overlay is modal: scroll / close here, never reaching the
    // underlying mode. Only Esc closes it, so `?`/`q`/typing while open
    // never quits or mutates state.
    if *show_help {
        use crossterm::event::{Event as CEvent, KeyCode};
        if let CEvent::Key(key) = &event {
            match key.code {
                KeyCode::Esc => {
                    *show_help = false;
                    *help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *help_scroll = help_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => {
                    *help_scroll = help_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    *help_scroll = help_scroll.saturating_sub(10);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    *help_scroll = help_scroll.saturating_add(10);
                }
                KeyCode::Home => {
                    *help_scroll = 0;
                }
                KeyCode::End => {
                    *help_scroll = u16::MAX;
                }
                _ => {}
            }
        }
        let entries = build_entries(projects, processes);
        sync_statuses(processes);
        render(
            terminal,
            mode,
            &entries,
            projects,
            processes,
            *term_rows,
            *term_cols,
            *confirm_quit,
            no_worktree,
            activity_dot_enabled,
            *show_help,
            *help_scroll,
        )?;
        return Ok(false);
    }

    // Quit confirmation is modal too: only Enter quits, Esc/n stay.
    // Every other key (including `q`) is swallowed and keeps the popup
    // on screen.
    if *confirm_quit {
        use crossterm::event::{Event as CEvent, KeyCode};
        if let CEvent::Key(key) = &event {
            match key.code {
                KeyCode::Enter => {
                    return Ok(true);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    *confirm_quit = false;
                }
                _ => {
                    // Swallow: keep the popup visible instead of dismissing
                    // it on unrelated keys (arrows, etc.).
                }
            }
        }
        let entries = build_entries(projects, processes);
        sync_statuses(processes);
        render(
            terminal,
            mode,
            &entries,
            projects,
            processes,
            *term_rows,
            *term_cols,
            *confirm_quit,
            no_worktree,
            activity_dot_enabled,
            *show_help,
            *help_scroll,
        )?;
        return Ok(false);
    }

    let was_tty_before_event = matches!(mode, Mode::Tty { .. } | Mode::TempTty { .. });
    // Snapshot the TTY identity *before* dispatch: after `process_event`
    // runs, `mode` may already be the new mode and we can no longer tell
    // which process's cache to invalidate.
    let exited_tty_pid = match &mode {
        Mode::Tty { process_id } => Some(*process_id),
        _ => None,
    };
    let was_temp_tty = matches!(mode, Mode::TempTty { .. });

    let entries = build_entries(projects, processes);
    let should_quit = process_event(
        mode,
        projects,
        next_project_id,
        processes,
        next_id,
        pty_system,
        event,
        term_rows,
        term_cols,
        &entries,
        dont_save,
        no_worktree,
        activity_dot_enabled,
        *confirm_quit,
        show_help,
        help_scroll,
    )?;
    // Entering (or sitting in) a TTY means its finish was seen.
    mark_tty_seen(mode, processes);
    // Opening the help overlay renders immediately below.
    if *show_help {
        let entries = build_entries(projects, processes);
        sync_statuses(processes);
        render(
            terminal,
            mode,
            &entries,
            projects,
            processes,
            *term_rows,
            *term_cols,
            *confirm_quit,
            no_worktree,
            activity_dot_enabled,
            *show_help,
            *help_scroll,
        )?;
        return Ok(false);
    }

    // Any transition *into* a TTY mode must start from a blank physical
    // screen with a dropped diff cache, or the first frame diffs TTY content
    // against the stale TUI screen (= mixed TUI/TTY rendering). TTY(A) ->
    // TTY(B) (e.g. re-entering after spawn) needs the same fresh full
    // repaint for the new child.
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
            let _ = enter_tty_real(terminal, proc);
        }
    } else if matches!(mode, Mode::TempTty { .. })
        && !was_tty_before_event
        && let Mode::TempTty { process, .. } = &mode
    {
        let _ = enter_tty_real(terminal, process);
    }

    if was_tty_before_event && matches!(mode, Mode::Normal { .. }) {
        *confirm_quit = false;
        // Leaving raw mode: the physical screen holds TTY content and the
        // vt100 cache describes it. Either must be discarded before the TUI
        // redraws. (`wipe_real`, not `terminal.clear()` — the latter blocks
        // ~500ms on a cursor query.)
        if let Some(pid) = exited_tty_pid {
            if let Some(proc) = processes.iter().find(|p| p.id == pid) {
                let _ = exit_tty_real(terminal, proc);
            } else {
                let _ = wipe_real(terminal);
            }
        } else if was_temp_tty {
            // TempTty's Process was moved out of `mode` already; fall back to
            // a bare wipe — its cache dies with the value.
            let _ = wipe_real(terminal);
        }
        execute!(
            terminal.backend_mut(),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(Color::Reset),
            SetBackgroundColor(Color::Reset)
        )?;
        let entries = build_entries(projects, processes);
        sync_statuses(processes);
        render(
            terminal,
            mode,
            &entries,
            projects,
            processes,
            *term_rows,
            *term_cols,
            *confirm_quit,
            no_worktree,
            activity_dot_enabled,
            *show_help,
            *help_scroll,
        )?;
        return Ok(false);
    }

    if should_quit {
        // Quitting is always two-step: the first `q`/`Esc` raises the
        // confirmation popup (with live agent counts) and the modal branch
        // at the top of this function resolves the follow-up keypress
        // (only `Enter` quits, only `Esc`/`n` stay; `q` does nothing).
        // Previously only a
        // post-TTY quit or a git-conflict quit was confirmed, so a plain
        // `q` returned `Ok(true)` here immediately and the popup never
        // appeared.
        if !*confirm_quit {
            *confirm_quit = true;
            let entries = build_entries(projects, processes);
            render(
                terminal,
                mode,
                &entries,
                projects,
                processes,
                *term_rows,
                *term_cols,
                *confirm_quit,
                no_worktree,
                activity_dot_enabled,
                *show_help,
                *help_scroll,
            )?;
            return Ok(false);
        }
        return Ok(true);
    }

    let entries = build_entries(projects, processes);
    if let Mode::Normal { selected } = mode {
        if entries.is_empty() {
            *selected = 0;
        } else if *selected >= entries.len() {
            *selected = entries.len() - 1;
        }
    }
    render(
        terminal,
        mode,
        &entries,
        projects,
        processes,
        *term_rows,
        *term_cols,
        *confirm_quit,
        no_worktree,
        activity_dot_enabled,
        *show_help,
        *help_scroll,
    )?;
    Ok(false)
}

/// Leave the alternate screen and raw mode. Idempotent best-effort: every
/// exit path (quit, event error, stream end, client-detach race) funnels here
/// so the physical terminal is never left hidden/raw.
fn restore_terminal<W: std::io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    supports_keyboard_enhancement: bool,
) {
    let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
    if supports_keyboard_enhancement {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        terminal.backend_mut(),
        cursor::Show,
        terminal::LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[allow(clippy::type_complexity)]
    fn harness() -> (
        Mode,
        Vec<Project>,
        usize,
        Vec<Process>,
        usize,
        NativePtySystem,
        Terminal<CrosstermBackend<Vec<u8>>>,
        u16,
        u16,
        bool,
        bool,
        u16,
    ) {
        // Fixed viewport: `Terminal::new` queries the *real* terminal size
        // via ioctl, which fails headless (WouldBlock). A fixed area never
        // touches the OS terminal.
        let backend = CrosstermBackend::new(Vec::new());
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(area),
        };
        let terminal = Terminal::with_options(backend, options).unwrap();
        let projects = vec![Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        (
            Mode::Normal { selected: 0 },
            projects,
            2,
            Vec::new(),
            1,
            NativePtySystem::default(),
            terminal,
            24,
            80,
            false,
            false,
            0,
        )
    }

    #[test]
    fn test_quit_is_two_step_with_popup() {
        let (
            mut mode,
            mut projects,
            mut next_project_id,
            mut processes,
            mut next_id,
            pty_system,
            mut terminal,
            mut rows,
            mut cols,
            mut confirm_quit,
            mut show_help,
            mut help_scroll,
        ) = harness();
        // First `q`: no quit, confirmation popup raised.
        let quit = dispatch_event(
            key(KeyCode::Char('q')),
            &mut mode,
            &mut projects,
            &mut next_project_id,
            &mut processes,
            &mut next_id,
            &pty_system,
            &mut terminal,
            &mut rows,
            &mut cols,
            true,
            false,
            true,
            &mut confirm_quit,
            &mut show_help,
            &mut help_scroll,
        )
        .unwrap();
        assert!(!quit);
        assert!(confirm_quit);
        // `q` is inert while confirming: no quit, popup stays up.
        let quit = dispatch_event(
            key(KeyCode::Char('q')),
            &mut mode,
            &mut projects,
            &mut next_project_id,
            &mut processes,
            &mut next_id,
            &pty_system,
            &mut terminal,
            &mut rows,
            &mut cols,
            true,
            false,
            true,
            &mut confirm_quit,
            &mut show_help,
            &mut help_scroll,
        )
        .unwrap();
        assert!(!quit);
        assert!(confirm_quit);
        let quit = dispatch_event(
            key(KeyCode::Enter),
            &mut mode,
            &mut projects,
            &mut next_project_id,
            &mut processes,
            &mut next_id,
            &pty_system,
            &mut terminal,
            &mut rows,
            &mut cols,
            true,
            false,
            true,
            &mut confirm_quit,
            &mut show_help,
            &mut help_scroll,
        )
        .unwrap();
        assert!(quit);
    }

    #[test]
    fn test_quit_confirm_then_cancel_and_enter() {
        let (
            mut mode,
            mut projects,
            mut next_project_id,
            mut processes,
            mut next_id,
            pty_system,
            mut terminal,
            mut rows,
            mut cols,
            mut confirm_quit,
            mut show_help,
            mut help_scroll,
        ) = harness();
        macro_rules! dispatch {
            ($event:expr) => {
                dispatch_event(
                    $event,
                    &mut mode,
                    &mut projects,
                    &mut next_project_id,
                    &mut processes,
                    &mut next_id,
                    &pty_system,
                    &mut terminal,
                    &mut rows,
                    &mut cols,
                    true,
                    false,
                    true,
                    &mut confirm_quit,
                    &mut show_help,
                    &mut help_scroll,
                )
                .unwrap()
            };
        }
        assert!(!dispatch!(key(KeyCode::Char('q'))));
        assert!(confirm_quit);
        // Unrelated keys keep the popup instead of dismissing it.
        assert!(!dispatch!(key(KeyCode::Up)));
        assert!(confirm_quit);
        // Esc cancels.
        assert!(!dispatch!(key(KeyCode::Esc)));
        assert!(!confirm_quit);
        // `q` then `Enter` quits.
        assert!(!dispatch!(key(KeyCode::Char('q'))));
        assert!(dispatch!(key(KeyCode::Enter)));
    }
}
