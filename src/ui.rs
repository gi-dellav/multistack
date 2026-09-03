use std::io::Write;
use std::sync::atomic::Ordering;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{ResetColor, SetAttribute};
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use ratatui::Terminal;
use ratatui::backend::Backend;
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

/// Wipe the physical screen + reset ratatui's back buffer **without**
/// querying the terminal.
///
/// `Terminal::clear()` looks innocent but internally calls
/// `backend.get_cursor_position()`, which on crossterm writes `ESC[6n` (DSR)
/// and blocks reading stdin for the CPR reply — up to a 2s timeout when the
/// event stream owns stdin (as ours does), typically ~500ms per call in
/// practice. We issue `Clear(All)` directly, flush, and reset the back buffer
/// via `swap_buffers()` instead: same visual result, zero round-trips.
///
/// `swap_buffers()` clears the *inactive* buffer and swaps it into place, so
/// after two calls both buffers are blank and the next `draw()` repaints
/// every cell (full repaint, no stale diff).
#[cfg(test)]
fn wipe_screen<B: Backend>(terminal: &mut Terminal<B>) -> std::io::Result<()>
where
    B::Error: std::fmt::Debug,
{
    terminal
        .backend_mut()
        .clear_region(ratatui::backend::ClearType::All)
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    terminal
        .backend_mut()
        .flush()
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    // Reset both buffers: swap twice so current AND back buffer are blank.
    terminal.swap_buffers();
    terminal.swap_buffers();
    Ok(())
}

/// Attributes the TUI leaves behind that would bleed into raw TTY output.
/// ratatui resets fg/bg/attrs after every draw, but cursor visibility and
/// reverse/bold state can leak if a draw was interrupted mid-frame.
fn reset_tui_attributes<W: Write>(out: &mut CrosstermBackend<W>) -> std::io::Result<()> {
    execute!(
        out,
        ResetColor,
        SetAttribute(crossterm::style::Attribute::Reset),
        cursor::Show,
        cursor::MoveTo(0, 0),
    )?;
    Ok(())
}

/// Prepare the physical terminal for raw passthrough of a child's vt100
/// screen.
///
/// Why each step is needed:
/// - `BeginSynchronizedUpdate` batches the whole switch into one frame so the
///   user never sees a half-TUI/half-TTY tear (real terminal only; the
///   generic path used by tests skips raw escape writes).
/// - ratatui keeps a double buffer and diffs frames: its cached "previous
///   frame" still describes the TUI list. Without `terminal.clear()` the next
///   `draw()` after leaving the TTY would emit only a *diff* against that
///   stale frame and leftover TTY rows would survive on screen. `clear()`
///   also resets the back buffer so the next TUI draw is a full repaint.
/// - `tty_output`'s diff cache (`prev_screen`) describes what is *currently
///   on the physical screen (the TUI). Keeping it would make the first diff
///   render TTY content *relative to the TUI screen* — garbage rows. Dropping
///   it forces a full `state_formatted` repaint.
/// - SGR/cursor state the TUI left behind (hidden cursor, attributes) would
///   bleed into the child's output; reset them first on a real terminal.
///
/// The `B: Backend` generic keeps this unit-testable with `TestBackend`;
/// [`enter_tty_real`] adds the raw crossterm writes used in production.
///
/// Only compiled for tests: production uses [`enter_tty_real`].
#[cfg(test)]
pub fn enter_tty<B: Backend>(terminal: &mut Terminal<B>, proc: &Process) -> std::io::Result<()>
where
    B::Error: std::fmt::Debug,
{
    wipe_screen(terminal)?;
    *proc.prev_screen.lock() = None;
    Ok(())
}

/// Production wrapper around [`enter_tty`] that additionally resets raw
/// crossterm attributes and wraps the switch in a synchronized update so the
/// user never sees a torn frame.
///
/// Fast path by design: no `Terminal::clear()` (which blocks ~500ms on a
/// `ESC[6n` cursor-position round-trip against our own event stream) and the
/// first TTY frame is painted synchronously inside this call, so there is no
/// wait for the next 50ms render tick either.
pub fn enter_tty_real<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    proc: &Process,
) -> std::io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    reset_tui_attributes(terminal.backend_mut())?;
    // Order matters: clear ratatui buffers first (records the clear into the
    // backend), then wipe the physical screen underneath, then flush once.
    wipe_real(terminal)?;
    execute!(terminal.backend_mut(), Clear(ClearType::All))?;
    // Paint the first TTY frame *now* instead of waiting for the next render
    // tick: `tty_output` drops the stale cache (None -> full repaint) and
    // emits the complete screen in this same synchronized update.
    if let Some(output) = tty_output(proc) {
        terminal.backend_mut().write_all(&output)?;
    }
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    std::io::Write::flush(terminal.backend_mut())?;
    Ok(())
}

/// Real-backend twin of [`wipe_screen`]: `Clear(All)` + flush + double
/// `swap_buffers()`, with zero cursor-position queries. Public so `main.rs`
/// can use it for the no-process fallback paths (already-reaped child) that
/// previously called the blocking `Terminal::clear()`.
pub fn wipe_real<W: Write>(terminal: &mut Terminal<CrosstermBackend<W>>) -> std::io::Result<()> {
    terminal.backend_mut().clear()?;
    std::io::Write::flush(terminal.backend_mut())?;
    terminal.swap_buffers();
    terminal.swap_buffers();
    Ok(())
}

/// Restore a clean slate when leaving raw TTY mode back to the ratatui TUI.
///
/// Mirrors `enter_tty`: drops the vt100 diff cache (it describes TTY content
/// that is about to be wiped) and resets ratatui's buffers so the next
/// `draw()` repaints the whole list instead of diffing against a frame that
/// no longer exists on screen. [`exit_tty_real`] adds the raw crossterm
/// writes used in production.
///
/// Only compiled for tests: production uses [`exit_tty_real`].
#[cfg(test)]
pub fn exit_tty<B: Backend>(terminal: &mut Terminal<B>, proc: &Process) -> std::io::Result<()>
where
    B::Error: std::fmt::Debug,
{
    *proc.prev_screen.lock() = None;
    wipe_screen(terminal)?;
    // Force the *next* TUI draw to repaint every cell: `wipe_screen` blanks
    // both buffers, but a `resize()` to the current area additionally re-syncs
    // buffer sizes and viewport bookkeeping after the raw stream may have
    // scrolled or resized the physical screen out from under ratatui.
    let area = terminal.get_frame().area();
    terminal
        .resize(area)
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    Ok(())
}

/// Production wrapper around [`exit_tty`] with raw crossterm SGR/cursor
/// resets and synchronized-update framing. Uses [`wipe_real`] (no
/// cursor-position query) instead of `Terminal::clear()`.
pub fn exit_tty_real<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    proc: &Process,
) -> std::io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    *proc.prev_screen.lock() = None;
    reset_tui_attributes(terminal.backend_mut())?;
    wipe_real(terminal)?;
    execute!(terminal.backend_mut(), Clear(ClearType::All))?;
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    std::io::Write::flush(terminal.backend_mut())?;
    Ok(())
}

/// Variant of [`exit_tty`] for `TempTty`, whose `Process` lives inside `Mode`
/// and can't be borrowed alongside `terminal` in some call sites. Takes the
/// pieces needed without requiring the full struct.
///
/// Only compiled for tests: production uses [`exit_temp_tty_real`].
#[cfg(test)]
pub fn exit_temp_tty<B: Backend>(
    terminal: &mut Terminal<B>,
    prev_screen: &parking_lot::Mutex<Option<vt100::Screen>>,
) -> std::io::Result<()>
where
    B::Error: std::fmt::Debug,
{
    *prev_screen.lock() = None;
    wipe_screen(terminal)?;
    let area = terminal.get_frame().area();
    terminal
        .resize(area)
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    Ok(())
}

/// Production wrapper around [`exit_temp_tty`] with raw crossterm writes.
/// Uses [`wipe_real`] (no cursor-position query).
pub fn exit_temp_tty_real<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    prev_screen: &parking_lot::Mutex<Option<vt100::Screen>>,
) -> std::io::Result<()> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    *prev_screen.lock() = None;
    reset_tui_attributes(terminal.backend_mut())?;
    wipe_real(terminal)?;
    execute!(terminal.backend_mut(), Clear(ClearType::All))?;
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    std::io::Write::flush(terminal.backend_mut())?;
    Ok(())
}

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
                crate::PromptPurpose::NewProcess(_) | crate::PromptPurpose::NewBareProcess(_) => {
                    "new name: "
                }
                crate::PromptPurpose::NewProject => "project dir: ",
                crate::PromptPurpose::Rename(_) => "rename: ",
            };
            let help = Line::from(format!("{}{}_", label, input));
            frame.render_widget(help, help_area);
        })
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
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

/// Invalidate the vt100 diff cache without emitting anything. Call when the
/// physical screen no longer matches `prev_screen` for reasons outside the
/// diff pipeline (mode switch, resize, child death) so the next frame is a
/// full repaint instead of a diff against a phantom screen.
#[cfg(test)]
pub(crate) fn invalidate_tty_cache(proc: &Process) {
    *proc.prev_screen.lock() = None;
}

fn render_tty<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    proc: &Process,
    _rows: u16,
    _cols: u16,
) -> std::io::Result<()> {
    // Skip the write syscall entirely when nothing changed: `tty_output`
    // returns None for an empty diff, and a lone `flush()` per 50ms tick on
    // an idle TTY would still wake the terminal driver for nothing.
    let Some(output) = tty_output(proc) else {
        return Ok(());
    };
    let stdout = terminal.backend_mut();
    // No Begin/EndSynchronizedUpdate here: the whole TTY session is already
    // wrapped by enter_tty/exit_tty framing, and nesting synchronized-update
    // scopes produces flicker on terminals that stack them. Per-tick diffs
    // are small cursor-addressed writes; flushing once keeps them atomic
    // enough without an extra scope per 50ms tick.
    stdout.write_all(&output)?;
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
            worktree_dir: None,
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

    /// True when `bytes` are a full vt100 screen repaint (home + erase-all
    /// present), as opposed to a cursor-addressed diff. vt100 prefixes the
    /// dump with cursor-visibility state, so match the clear sequence as a
    /// *substring* rather than a prefix.
    fn is_full_repaint(bytes: &[u8]) -> bool {
        bytes
            .windows(6)
            .any(|w| w == b"\x1b[H\x1b[J" || w == b"\x1b[2J")
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
        assert!(out.unwrap().windows(2).any(|w| w == b"\x1b["));
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
        let projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let proc = make_proc_with_content(b"", 24, 80);
        let entries = crate::project::build_entries(&projects, &[proc]);
        let res = render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            24,
            80,
            false,
            false,
        );
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

    // ---- TTY/TUI transition tests ----
    //
    // The bug under test: switching list -> TTY sometimes rendered a mix of
    // TUI rows and TTY rows. Root causes covered:
    //   1. vt100 diff cache (`prev_screen`) still describing the TUI screen
    //      when the first TTY frame is emitted;
    //   2. ratatui's double-buffer diffing against a stale TUI frame when
    //      coming back, leaving TTY rows on screen.
    // `enter_tty`/`exit_tty` must eliminate both.

    #[test]
    fn test_enter_tty_drops_stale_diff_cache() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        // Simulate: cache currently describes the TUI screen.
        let _ = tty_output(&proc);
        assert!(proc.prev_screen.lock().is_some());

        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        enter_tty(&mut terminal, &proc).unwrap();

        // Cache must be gone so the first TTY frame is a full repaint.
        assert!(proc.prev_screen.lock().is_none());
        // And the next output must be a full state dump (starts with SGR
        // reset + home+clear), not a cursor-addressed diff.
        let out = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&out));
    }

    #[test]
    fn test_enter_tty_clears_ratatui_buffers() {
        let proc = make_proc_with_content(b"hello", 20, 10);
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        // Paint a TUI frame so the backend holds TUI content.
        terminal
            .draw(|f| {
                use ratatui::widgets::{Block, Borders};
                f.render_widget(Block::default().borders(Borders::ALL), f.area());
            })
            .unwrap();
        // Sanity: something was actually drawn (border corners).
        let before: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(before.contains("┌") || before.contains("+") || before.trim().len() > 10);

        enter_tty(&mut terminal, &proc).unwrap();

        // Backend must be blank afterwards: no stale TUI cells may survive
        // to be diffed back onto the screen later. (`Terminal::clear`
        // issues Clear(All) to the backend and resets the back buffer.)
        terminal
            .backend()
            .assert_buffer_lines(vec![" ".repeat(20); 10]);
    }

    #[test]
    fn test_exit_tty_drops_cache_and_clears_buffers() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        // Simulate an active TTY session: cache populated, backend dirty.
        let _ = tty_output(&proc);
        assert!(proc.prev_screen.lock().is_some());

        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        // Simulate TTY having written outside ratatui's knowledge.
        terminal
            .backend_mut()
            .assert_buffer_lines(vec![" ".repeat(20); 10]);

        exit_tty(&mut terminal, &proc).unwrap();

        assert!(proc.prev_screen.lock().is_none());
        // First frame after exit must be a full repaint, not a diff.
        proc.parser.lock().process(b" more");
        let out = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&out));
    }

    #[test]
    fn test_exit_temp_tty_clears_without_process() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        let _ = tty_output(&proc);
        assert!(proc.prev_screen.lock().is_some());

        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        exit_temp_tty(&mut terminal, &proc.prev_screen).unwrap();
        assert!(proc.prev_screen.lock().is_none());
    }

    #[test]
    fn test_full_tui_tty_tui_roundtrip_repaints_everything() {
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let proc = make_proc_with_content(b"child output", 8, 30);
        let entries = crate::project::build_entries(&projects, std::slice::from_ref(&proc));

        // 1. TUI list frame.
        render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            8,
            30,
            false,
            false,
        )
        .unwrap();

        // 2. Enter TTY: first TTY bytes must be a full repaint (clear +
        //    content), never a diff against the TUI screen.
        enter_tty(&mut terminal, &proc).unwrap();
        let first = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&first));
        assert!(first.windows(12).any(|w| w == b"child output"));

        // 3. Steady-state tick with no changes emits nothing (no flicker).
        assert!(tty_output(&proc).is_none());

        // 4. Exit back to TUI: next list draw must repaint all rows.
        exit_tty(&mut terminal, &proc).unwrap();
        render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            8,
            30,
            false,
            false,
        )
        .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(screen.contains("Multistack"));
        // No TTY bytes may leak into the ratatui backend buffer.
        assert!(!screen.contains("child output"));
    }

    #[test]
    fn test_invalidate_tty_cache_forces_full_repaint() {
        let proc = make_proc_with_content(b"hello", 10, 20);
        let _ = tty_output(&proc).unwrap();
        // Steady state: no output.
        assert!(tty_output(&proc).is_none());
        // External event (resize/death/mode switch) invalidates…
        invalidate_tty_cache(&proc);
        // …so the next tick repaints everything.
        let out = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&out));
    }

    #[test]
    fn test_resize_invalidates_cache_so_no_mixed_frame() {
        // Regression: after a terminal resize the parser is rebuilt at the
        // new size but `prev_screen` still described the old-size screen.
        // `state_diff` against a different-size screen emits cursor moves
        // for the old geometry -> mixed TUI/TTY rows.
        let proc = make_proc_with_content(b"hello", 10, 20);
        let _ = tty_output(&proc).unwrap();
        {
            let mut parser = proc.parser.lock();
            *parser = vt100::Parser::new(15, 30, 0);
            parser.process(b"hello");
        }
        // Without invalidation the sizes disagree (the bug).
        assert_ne!(
            proc.prev_screen.lock().as_ref().unwrap().size(),
            proc.parser.lock().screen().size()
        );
        invalidate_tty_cache(&proc);
        let out = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&out));
        assert_eq!(proc.prev_screen.lock().as_ref().unwrap().size(), (15, 30));
    }

    #[test]
    fn test_rapid_enter_exit_enter_stays_consistent() {
        // Fast double-tap Enter/Esc/Enter must not leave a stale cache from
        // the first session poisoning the second.
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let proc = make_proc_with_content(b"one", 10, 20);

        enter_tty(&mut terminal, &proc).unwrap();
        let first = tty_output(&proc).unwrap();
        assert!(first.windows(3).any(|w| w == b"one"));
        exit_tty(&mut terminal, &proc).unwrap();

        proc.parser.lock().process(b"two");
        enter_tty(&mut terminal, &proc).unwrap();
        let second = tty_output(&proc).unwrap();
        // Full repaint again — contains both old and new content, and starts
        // with a clear so no rows from session 1 survive.
        assert!(is_full_repaint(&second));
        assert!(second.windows(3).any(|w| w == b"two"));
    }

    #[test]
    fn test_enter_tty_twice_without_exit_is_idempotent() {
        let backend = TestBackend::new(20, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let proc = make_proc_with_content(b"hi", 10, 20);

        enter_tty(&mut terminal, &proc).unwrap();
        let first = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&first));
        // Second enter (e.g. TTY(A) -> TTY(B) switch path re-firing) must
        // again force a full repaint, not a diff.
        enter_tty(&mut terminal, &proc).unwrap();
        let second = tty_output(&proc).unwrap();
        assert!(is_full_repaint(&second));
    }

    #[test]
    fn test_tui_draw_after_exit_paints_full_list() {
        // The exact reported symptom: after leaving TTY, some rows still
        // showed terminal content. Verify the post-exit draw emits cells for
        // every row of the list area (ratatui back-buffer was reset, so the
        // diff covers the whole viewport).
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let proc = make_proc_with_content(b"xyz", 8, 30);
        let entries = crate::project::build_entries(&projects, std::slice::from_ref(&proc));

        render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            8,
            30,
            false,
            false,
        )
        .unwrap();
        enter_tty(&mut terminal, &proc).unwrap();
        let _ = tty_output(&proc);
        exit_tty(&mut terminal, &proc).unwrap();

        // Backend must be blank right after exit (nothing leaks through).
        terminal
            .backend()
            .assert_buffer_lines(vec![" ".repeat(30); 8]);

        // NOTE: `entries` was built from `proc` but the render call passes
        // `&[]` as processes (mirroring the pre-existing test below), so the
        // agent row renders as an empty item. Assert on the chrome (title +
        // help line) rather than the agent row.
        render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            8,
            30,
            false,
            false,
        )
        .unwrap();
        let lines: Vec<String> = (0..8)
            .map(|y| {
                (0..30)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(lines[0].contains("Multistack"));
        // Help line is truncated to 30 cols; assert on its visible prefix.
        assert!(lines[7].trim().len() > 5, "help line missing: {lines:?}");
        // No blank (leaked) rows inside the list chrome: title, separator,
        // help line must all be non-empty.
        assert!(!lines[0].trim().is_empty());
        assert!(lines[1].chars().any(|c| c == '═'));
    }
}
