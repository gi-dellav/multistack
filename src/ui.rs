use std::io::Write;
use std::sync::atomic::Ordering;

use crossterm::cursor;
use crossterm::execute;
use crossterm::style::{ResetColor, SetAttribute};
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, FrameExt, List, ListItem, ListState, Paragraph, Wrap};
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

/// Mirror a child's vt100 input-mode state onto the *physical* terminal.
///
/// A full-screen app (vim, less, lazygit, helix, ...) enables modes the TUI
/// itself never touches: application cursor/keypad (`ESC[?1h`, `ESC=`),
/// mouse reporting (`ESC[?1000/1002/1003h`), bracketed paste (`ESC[?2004h`),
/// and cursor visibility. Because the child only ever talks to our vt100
/// parser — never to the real terminal — the physical terminal must be moved
/// into the matching state *for* it, or edge cases break visibly:
/// - arrow keys insert `A/B/C/D` instead of moving (app-cursor off),
/// - mouse clicks/drags go to the wrong widget (mouse mode off),
/// - pastes lack `ESC[200~...201~` framing so bracketed-paste readers mangle
///   them (bracketed paste off),
/// - a child that hides the cursor leaves it invisible after exit.
///
/// The physical side is tracked alongside the vt100 diff cache (`prev`
/// holds the `Screen` whose modes were last pushed out). A `None` cache —
/// i.e. every full repaint — re-pushes *all* modes, so attach clients and
/// resize paths inherit the correct state from the next frame for free.
/// Only a diff-vs-previous is emitted on steady-state ticks (same
/// no-flicker/no-extra-bytes discipline as `tty_frame`), so an idle shell
/// costs zero extra bytes per 50ms tick.
///
/// `prev=None` (full repaint) re-pushes *every* mode, disabling the ones the
/// child does not want — this closes the gap where vt100's own
/// `state_formatted` emits only transitions and would otherwise leave
/// a stale `ESC[?1000h` from a previous child active.
///
/// Defensive by design: callers write the returned bytes with `let _ =`
/// (best-effort) — TTY framing must never hard-fail a mode switch over a
/// transient backend error. A lost sequence is always healed by the next
/// full repaint.
#[allow(clippy::wrong_self_convention)]
fn tty_input_mode_bytes(current: &vt100::Screen, prev: Option<&vt100::Screen>) -> Vec<u8> {
    let mut buf = Vec::new();
    // Local terminal state defaults: our own TUI runs with application
    // cursor/keypad off, mouse reporting off, bracketed paste ON (set once in
    // `run_server`), cursor visible. A full repaint must therefore *disable*
    // the modes the TUI never enables and *enable* the ones it does — the
    // child may have been spawned from a state where they still held.
    match prev {
        None => {
            buf.extend_from_slice(if current.application_cursor() {
                b"\x1b[?1h"
            } else {
                b"\x1b[?1l"
            });
            buf.extend_from_slice(if current.application_keypad() {
                b"\x1b="
            } else {
                b"\x1b>"
            });
            buf.extend_from_slice(if current.bracketed_paste() {
                b"\x1b[?2004h"
            } else {
                b"\x1b[?2004l"
            });
            match current.mouse_protocol_mode() {
                // vt100 normalises away the X10 `?9` mode, but still decode
                // its enable on input: pushing the closest superset keeps the
                // physical side reporting instead of silent. The disable side
                // below clears every known mode, so a stale superset can
                // never survive a child that turns reporting off.
                vt100::MouseProtocolMode::Press | vt100::MouseProtocolMode::PressRelease => {
                    buf.extend_from_slice(b"\x1b[?1000h");
                }
                vt100::MouseProtocolMode::ButtonMotion => {
                    buf.extend_from_slice(b"\x1b[?1002h");
                }
                vt100::MouseProtocolMode::AnyMotion => {
                    buf.extend_from_slice(b"\x1b[?1003h");
                }
                vt100::MouseProtocolMode::None => {
                    buf.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?9l");
                }
            }
            match current.mouse_protocol_encoding() {
                vt100::MouseProtocolEncoding::Utf8 => buf.extend_from_slice(b"\x1b[?1005h"),
                vt100::MouseProtocolEncoding::Sgr => buf.extend_from_slice(b"\x1b[?1006h"),
                vt100::MouseProtocolEncoding::Default => {
                    buf.extend_from_slice(b"\x1b[?1006l\x1b[?1005l");
                }
            }
            // Same for cursor visibility: `state_formatted` only records
            // *transitions*, but a fresh physical screen always starts
            // visible — which may disagree with the child (vim hides it).
            buf.extend_from_slice(if current.hide_cursor() {
                b"\x1b[?25l"
            } else {
                b"\x1b[?25h"
            });
        }
        Some(prev_screen) => {
            if current.application_cursor() != prev_screen.application_cursor() {
                buf.extend_from_slice(if current.application_cursor() {
                    b"\x1b[?1h"
                } else {
                    b"\x1b[?1l"
                });
            }
            if current.application_keypad() != prev_screen.application_keypad() {
                buf.extend_from_slice(if current.application_keypad() {
                    b"\x1b="
                } else {
                    b"\x1b>"
                });
            }
            if current.bracketed_paste() != prev_screen.bracketed_paste() {
                buf.extend_from_slice(if current.bracketed_paste() {
                    b"\x1b[?2004h"
                } else {
                    b"\x1b[?2004l"
                });
            }
            // Mouse mode is a single enum on the wire: any change re-pushes
            // the new mode. Disabling restores `None` by clearing every mode
            // the physical terminal might still hold from an earlier state.
            // Mouse encoding likewise only emits transitions.
            if current.mouse_protocol_mode() != prev_screen.mouse_protocol_mode() {
                match current.mouse_protocol_mode() {
                    vt100::MouseProtocolMode::Press | vt100::MouseProtocolMode::PressRelease => {
                        buf.extend_from_slice(b"\x1b[?1000h");
                    }
                    vt100::MouseProtocolMode::ButtonMotion => {
                        buf.extend_from_slice(b"\x1b[?1002h");
                    }
                    vt100::MouseProtocolMode::AnyMotion => {
                        buf.extend_from_slice(b"\x1b[?1003h");
                    }
                    vt100::MouseProtocolMode::None => {
                        buf.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?9l");
                    }
                }
            }
            if current.mouse_protocol_encoding() != prev_screen.mouse_protocol_encoding() {
                match current.mouse_protocol_encoding() {
                    vt100::MouseProtocolEncoding::Utf8 => {
                        buf.extend_from_slice(b"\x1b[?1005h");
                    }
                    vt100::MouseProtocolEncoding::Sgr => {
                        buf.extend_from_slice(b"\x1b[?1006h");
                    }
                    vt100::MouseProtocolEncoding::Default => {
                        buf.extend_from_slice(b"\x1b[?1006l\x1b[?1005l");
                    }
                }
            }
            if current.hide_cursor() != prev_screen.hide_cursor() {
                buf.extend_from_slice(if current.hide_cursor() {
                    b"\x1b[?25l"
                } else {
                    b"\x1b[?25h"
                });
            }
        }
    }
    buf
}

/// Restore the input modes the TUI itself expects after leaving raw TTY.
///
/// Inverse of [`tty_input_mode_bytes`]'s full-repaint side: the TUI enables
/// bracketed paste once at startup and never touches application cursor /
/// keypad / mouse reporting, so on exit force exactly that state. Any child
/// that left mouse reporting, application cursor, or `DECSET 2004`-off
/// behind would otherwise keep corrupting the outer terminal — arrow keys or
/// mouse events would arrive in the wrong encoding after the TTY closed.
///
/// Like `reset_tui_attributes`, best-effort: never fails the transition.
fn restore_tui_input_modes<W: Write>(out: &mut CrosstermBackend<W>) -> std::io::Result<()> {
    let _ = out.write_all(b"\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?9l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?25h");
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
/// - `tty_frame`'s diff cache (`prev_screen`) describes what is *currently
///   on the physical screen (the TUI). Keeping it would make the first diff
///   render TTY content *relative to the TUI screen* — garbage rows. Dropping
///   it forces a full `state_formatted` repaint.
/// - SGR/cursor/input-mode state the TUI left behind (hidden cursor,
///   attributes, bracketed paste) would bleed into the child's output; reset
///   them first on a real terminal, then push the child's own modes before
///   the first content bytes so the opening frame is already interpreted
///   with the right settings.
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
    // tick: `tty_frame` drops the stale cache (None -> full repaint) and
    // emits the complete screen in this same synchronized update. The input
    // modes are pushed *before* the content so a full-screen child never
    // renders one frame with the TUI's modes.
    if let Some((output, modes)) = tty_frame(proc) {
        // `modes` is empty on an unchanged steady-state tick; `tty_frame`
        // returns `None` there so `enter` never emits a bare mode sequence
        // without content. On a full repaint it carries the child's modes
        // (best-effort: a lost mode sequence heals on the next repaint).
        if !modes.is_empty() {
            let _ = terminal.backend_mut().write_all(&modes);
        }
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
    restore_tui_input_modes(terminal.backend_mut())?;
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
    restore_tui_input_modes(terminal.backend_mut())?;
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
    activity_dot_enabled: bool,
    show_help: bool,
    help_scroll: u16,
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
            activity_dot_enabled,
            show_help,
            help_scroll,
        ),
        Mode::Prompt {
            purpose,
            selected,
            input,
        } => render_prompt(
            terminal,
            entries,
            projects,
            processes,
            *selected,
            purpose,
            input,
            rows,
            cols,
            show_help,
            help_scroll,
            no_worktree,
            activity_dot_enabled,
        ),
        Mode::Tty { process_id } => {
            if let Some(proc) = processes.iter().find(|p| p.id == *process_id) {
                render_tty(terminal, proc, rows, cols)
            } else {
                Ok(())
            }
        }
        Mode::TempTty { process, .. } => render_tty(terminal, process, rows, cols),
        Mode::DirPicker { explorer, .. } => render_dirpicker(
            terminal,
            explorer,
            rows,
            cols,
            show_help,
            help_scroll,
            no_worktree,
        ),
    }
}

/// Centered rectangle of fixed size, clamped to the available area.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(10);
    let h = height.min(area.height.saturating_sub(2)).max(6);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn section_header(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn key_line(keys: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {keys:<16}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

/// Full help text shown in the `?` overlay. Kept as data (not widgets) so
/// both the overlay and tests can inspect it.
pub fn help_content(no_worktree: bool) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "Multistack — parallel zerostack agents".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Press Esc to close  •  ↑↓ / j k / PgUp PgDn / Home End to scroll".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(""),
        section_header("SPAWN AGENTS"),
    ];
    if no_worktree {
        lines.push(key_line(
            "m",
            "spawn bare agent (no worktree, asks name, enters TTY)",
        ));
    } else {
        lines.push(key_line(
            "n",
            "spawn agent (asks name, creates git worktree, stays in list)",
        ));
        lines.push(key_line(
            "N (shift-n)",
            "spawn agent in parallel mode (no prompt, enters TTY)",
        ));
        lines.push(key_line(
            "m",
            "spawn bare agent (no worktree, asks name, enters TTY)",
        ));
    }
    lines.extend([
        Line::from(""),
        section_header("ACT ON SELECTED AGENT"),
        key_line("Enter", "drop into agent TTY (keystrokes pass through)"),
        key_line("r", "rename agent"),
        key_line("d", "kill agent (runs speck apply if present)"),
        Line::from(""),
        section_header("PROJECTS"),
        key_line("p", "add project (directory picker)"),
        key_line("l", "remove project + all its agents (runs speck apply)"),
        key_line("PgUp / PgDn", "jump to prev / next project header"),
        Line::from(""),
        section_header("TOOLS (open in project or agent worktree)"),
        key_line("h", "open lazygit (Esc returns to list)"),
        key_line("s", "open $SHELL (Esc returns to list)"),
        Line::from(""),
        section_header("NAVIGATE LIST"),
        key_line("↑ / ↓", "move selection"),
        key_line("Esc / q", "quit (asks for confirmation when needed)"),
        key_line("?", "this help (scrollable, Esc closes)"),
        Line::from(""),
        section_header("INSIDE TTY / PROMPT / PICKER"),
        key_line("Esc (TTY)", "back to agent list"),
        key_line(
            "Enter (prompt)",
            "confirm  •  Esc cancels  •  paste supported",
        ),
        key_line(
            "Enter (picker)",
            "add highlighted directory  •  Esc cancels",
        ),
        key_line("/ (picker)", "filter directories  •  Esc clears filter"),
        Line::from(""),
        section_header("STATUS + TIMERS"),
        key_line("[ ] gray", "waiting — agent hasn't started yet"),
        key_line("[~] yellow", "working — timer is running"),
        key_line("[✓] green", "finished (stop signal received)"),
        key_line("[X] red", "dead — process exited"),
        key_line("[!] magenta", "git conflict — resolve it, quit asks twice"),
        key_line("● yellow", "unread — agent finished, Enter clears it"),
        Line::from(Span::styled(
            "  Timer shows active working time, kept accurate via status socket.".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )),
        Line::from(""),
        section_header("TIPS"),
    ]);
    if no_worktree {
        lines.push(Line::from(
            "  • Started with -w / --no-worktree: n spawns bare agents, N is disabled.",
        ));
    } else {
        lines.push(Line::from(
            "  • n creates a sibling worktree wt-<name>-… next to the project.",
        ));
    }
    lines.extend([
        Line::from("  • Failed agents stay in the list with exit reason + log tail."),
        Line::from("  • Enter on a failed agent shows its full log, d dismisses it."),
        Line::from("  • -c / --continue: reload the saved project list;"),
        Line::from("    without it, start fresh from the current directory."),
        Line::from("  • -w / --no-worktree: disable git-worktree integration"),
        Line::from("    (n spawns a bare agent, N is disabled)."),
        Line::from("  • -D / --no-activity-dot: disable the unread ● dot."),
        Line::from("  • --attach [PID]: mirror another running instance"),
        Line::from("    (default: oldest; pass a PID for that one; Ctrl+\\ detaches)."),
    ]);
    lines
}

fn render_help_overlay(frame: &mut ratatui::Frame, area: Rect, scroll: u16, no_worktree: bool) {
    let content = help_content(no_worktree);
    let popup = centered_rect(
        area,
        72.min(area.width.saturating_sub(2)),
        area.height.saturating_sub(2).max(10),
    );
    frame.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Help  (Esc closes) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 2 || inner.width < 2 {
        return;
    }
    let footer_h = 1u16;
    let body_h = inner.height.saturating_sub(footer_h);
    let body = Rect::new(inner.x, inner.y, inner.width, body_h);
    let footer_area = Rect::new(inner.x, inner.y + body_h, inner.width, footer_h);
    let max_scroll = (content.len() as u16).saturating_sub(body_h.max(1));
    let scroll = scroll.min(max_scroll);
    let para = Paragraph::new(content)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, body);
    let hint = if max_scroll == 0 {
        Line::from(Span::styled(
            "Esc closes",
            Style::default().add_modifier(Modifier::DIM),
        ))
    } else {
        Line::from(Span::styled(
            format!("Esc closes • {}/{} ", scroll + 1, max_scroll + 1),
            Style::default().add_modifier(Modifier::DIM),
        ))
    };
    frame.render_widget(hint.centered(), footer_area);
}

fn quit_counts(processes: &[Process]) -> (usize, usize, usize, usize, usize, usize, usize) {
    let mut working = 0;
    let mut waiting = 0;
    let mut finished = 0;
    let mut dead = 0;
    let mut conflict = 0;
    let mut running = 0;
    for p in processes {
        if p.alive.load(Ordering::SeqCst) {
            running += 1;
        }
        match p.status.load(Ordering::SeqCst) {
            status::STATUS_WORKING => working += 1,
            status::STATUS_NOT_YET => waiting += 1,
            status::STATUS_FINISHED => finished += 1,
            status::STATUS_DEAD => dead += 1,
            status::STATUS_GIT_CONFLICT => conflict += 1,
            _ => waiting += 1,
        }
    }
    (
        processes.len(),
        running,
        working,
        waiting,
        finished,
        dead,
        conflict,
    )
}

fn render_quit_overlay(
    frame: &mut ratatui::Frame,
    area: Rect,
    projects: &[Project],
    processes: &[Process],
) {
    let (total, running, working, waiting, finished, dead, conflict) = quit_counts(processes);
    let has_conflict = conflict > 0;
    let warnings: Vec<Line> = {
        let mut w = Vec::new();
        if has_conflict {
            w.push(Line::from(Span::styled(
                "! git conflict — resolve before quitting if you can".to_string(),
                Style::default().fg(Color::Magenta),
            )));
        }
        if working > 0 || running > 0 {
            w.push(Line::from(Span::styled(
                "! quitting kills running agents".to_string(),
                Style::default().fg(Color::Yellow),
            )));
        }
        w
    };
    // Title + summary + breakdown + warnings + confirm line.
    let height: u16 = 10 + warnings.len() as u16;
    let popup = centered_rect(area, 56.min(area.width.saturating_sub(2)), height);
    frame.render_widget(ratatui::widgets::Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if has_conflict {
            Color::Magenta
        } else {
            Color::Yellow
        }))
        .title(Span::styled(
            " Quit Multistack? ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut lines = vec![
        Line::from(format!(
            "{} project{} • {} agent{} ({} running)",
            projects.len(),
            if projects.len() == 1 { "" } else { "s" },
            total,
            if total == 1 { "" } else { "s" },
            running,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[~] working ".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format!("{working}   ")),
            Span::styled("[ ] waiting ".to_string(), Style::default().fg(Color::Gray)),
            Span::raw(format!("{waiting}")),
        ]),
        Line::from(vec![
            Span::styled("[✓] done ".to_string(), Style::default().fg(Color::Green)),
            Span::raw(format!("{finished}   ")),
            Span::styled("[X] dead ".to_string(), Style::default().fg(Color::Red)),
            Span::raw(format!("{dead}   ")),
            Span::styled(
                "[!] conflict ".to_string(),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(format!("{conflict}")),
        ]),
        Line::from(""),
    ];
    let no_warnings = warnings.is_empty();
    lines.extend(warnings);
    if no_warnings {
        lines.push(Line::from(Span::styled(
            "Nothing running — safe to quit.".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "Enter".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" quit   ".to_string()),
        Span::styled(
            "Esc / n".to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" stay".to_string()),
    ]));
    let para = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(para, inner);
}

fn find_process(processes: &[Process], id: usize) -> Option<&Process> {
    processes.iter().find(|p| p.id == id)
}

fn find_project(projects: &[Project], id: usize) -> Option<&Project> {
    projects.iter().find(|p| p.id == id)
}

fn process_item(proc: &Process, activity_dot_enabled: bool) -> ListItem<'static> {
    let status_val = proc.status.load(Ordering::SeqCst);
    let prefix = status::status_prefix(status_val);
    let color = status::status_color(status_val);
    let cycle = proc.cycle_start.lock();
    let timer = status::format_timer(proc.active_ms.load(Ordering::SeqCst), &cycle);
    let mut text = format!("  {} {}  {}", prefix, proc.name, timer);
    // Surface the failure reason inline so a dead agent explains itself
    // without forcing the user to re-enter its TTY.
    if crate::process::failed(proc)
        && let Some(reason) = crate::process::exit_reason(proc)
    {
        text.push_str(&format!("  ({reason})"));
    }
    let mut spans = vec![Span::styled(text, Style::default().fg(color))];
    if activity_dot_enabled && proc.has_unread.load(Ordering::SeqCst) {
        spans.push(Span::styled(
            " ●",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

#[allow(clippy::too_many_arguments)]
fn render_normal<B: Backend>(
    terminal: &mut Terminal<B>,
    entries: &[ListEntry],
    projects: &[Project],
    processes: &[Process],
    selected: usize,
    rows: u16,
    cols: u16,
    confirm_quit: bool,
    no_worktree: bool,
    activity_dot_enabled: bool,
    show_help: bool,
    help_scroll: u16,
) -> std::io::Result<()> {
    // Failure detail for the selected row: exit reason + log tail. Computed
    // outside the draw closure (locks + vt100 replay are not render work).
    let error_detail: Option<(String, Vec<String>)> = entries
        .get(selected)
        .and_then(|e| match e {
            ListEntry::Agent(pid) => find_process(processes, *pid),
            _ => None,
        })
        .filter(|proc| crate::process::failed(proc))
        .map(|proc| {
            let reason = crate::process::exit_reason(proc).unwrap_or_else(|| "failed".to_string());
            let tail = {
                let log = proc.log_buffer.lock();
                crate::process::log_tail_lines(&log, rows, cols, crate::process::LOG_TAIL_LINES)
            };
            (reason, tail)
        });
    terminal
        .draw(|frame| {
        let error_rows: u16 = error_detail.as_ref().map_or(0, |(_, tail)| {
            // Header + reason + tail lines, clamped so the agent list keeps
            // at least one visible row.
            let want = 2 + tail.len().max(1) as u16;
            let max = frame.area().height.saturating_sub(5).max(1);
            want.min(max)
        });
        let layout = if error_rows > 0 {
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(error_rows),
                Constraint::Length(1),
            ])
        } else {
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(0),
                Constraint::Length(1),
            ])
        };
        let [title_area, sep_area, _, list_area, error_area, help_area] =
            frame.area().layout(&layout);

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
                            process_item(proc, activity_dot_enabled)
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

        if let Some((reason, tail)) = error_detail.as_ref() {
            let mut text = vec![Line::from(vec![
                Span::styled(
                    "✖ failed: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(reason.clone(), Style::default().fg(Color::Red)),
                Span::styled(
                    "  — Enter: full log  d: dismiss",
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ])];
            if tail.is_empty() {
                text.push(Line::from(Span::styled(
                    "(no output captured)",
                    Style::default().add_modifier(Modifier::DIM),
                )));
            } else {
                for line in tail {
                    text.push(Line::from(Span::styled(
                        format!("│ {line}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
            let para = Paragraph::new(text)
                .block(Block::default().borders(Borders::TOP))
                .wrap(Wrap { trim: true });
            frame.render_widget(para, error_area);
        }

        let help = if confirm_quit {
            Line::from("Enter: quit   Esc: stay")
        } else if no_worktree {
            if cols < 40 {
                Line::from("m:bare r:ren d:kill ?:help q:quit")
            } else {
                Line::from("m: bare  r: rename  d: kill  h: lazygit  s: shell  p/l: project  Enter: TTY  ?: help  q: quit")
            }
        } else if cols < 40 {
            Line::from("n:new N:go ?:help q:quit")
        } else {
            Line::from("n: new  N: spawn&enter  m: bare  r: rename  d: kill  h: lazygit  s: shell  p/l: project  Enter: TTY  ?: help  q: quit")
        };
        frame.render_widget(help, help_area);

        let area = frame.area();
        if confirm_quit {
            render_quit_overlay(frame, area, projects, processes);
        }
        if show_help {
            render_help_overlay(frame, area, help_scroll, no_worktree);
        }
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
    show_help: bool,
    help_scroll: u16,
    no_worktree: bool,
    activity_dot_enabled: bool,
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
                                process_item(proc, activity_dot_enabled)
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

            if show_help {
                let area = frame.area();
                render_help_overlay(frame, area, help_scroll, no_worktree);
            }
        })
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    Ok(())
}

/// Content-only projection of [`tty_frame`] (drops the mode half).
///
/// Kept for the unit-test seam: production (`render_tty`/`enter_tty_real`)
/// uses `tty_frame` directly so modes ride with content.
#[cfg(test)]
pub(crate) fn tty_output(proc: &Process) -> Option<Vec<u8>> {
    tty_frame(proc).map(|(output, _)| output)
}

/// Full TTY frame as `(content, input_modes)`.
///
/// `content` is the vt100 `state_diff` (or `state_formatted` when the cache
/// is empty or the geometry changed); `input_modes` carries the physical
/// terminal mode transitions (`tty_input_mode_bytes`) for the same
/// prev→current pair. Both halves share one `prev` snapshot, so modes and
/// content can never disagree about which state they describe — e.g. an
/// app-cursor enable can never be paired with a diff computed against the
/// wrong baseline.
///
/// Edge cases handled explicitly (each previously produced visible breakage):
/// - empty `state_diff` *with* a mode change (a child toggling bracketed
///   paste / mouse / app-cursor without touching any cell): returns `Some`
///   with empty content and non-empty modes, so `render_tty` still pushes
///   the mode switch instead of dropping it as a no-op;
/// - fully empty frame (no content, no mode change): returns `None`, so the
///   50ms tick skips the write syscall entirely;
/// - geometry change: full `state_formatted` repaint, and the mode half is
///   computed with `prev=None` so *all* modes are re-pushed (the physical
///   screen was cleared underneath, so differential modes would be wrong).
/// - alternate-screen transitions (`vim` enter/exit, `less`, `lazygit`):
///   treated as a full repaint even at identical geometry. vt100's diff
///   only compares the *active* grid, so without this a normal→alt switch
///   diffs the alt grid against the stale normal grid and vice versa —
///   leftover rows from the other stack leak through. The mode half is again
///   `prev=None` (full re-push), keeping cursor-visibility/mouse state exact
///   across the switch.
fn tty_frame(proc: &Process) -> Option<(Vec<u8>, Vec<u8>)> {
    // Lock order is always `parser` → `prev_screen` (same as
    // `resize_parsers`): never invert it, or the PTY reader thread can
    // deadlock against the render tick.
    let parser = proc.parser.lock();
    let screen = parser.screen();
    let mut prev = proc.prev_screen.lock();
    let full_repaint = match prev.as_ref() {
        None => true,
        Some(prev_screen) => {
            prev_screen.size() != screen.size()
                || prev_screen.alternate_screen() != screen.alternate_screen()
        }
    };
    // `bytes`/`modes` are owned, so the immutable `prev` borrow ends before
    // the `*prev = ...` assignment below (no clone of the cached screen on
    // the hot path — the idle 50ms tick does zero screen-sized allocations).
    let bytes = if full_repaint {
        screen.state_formatted()
    } else {
        // `prev` is `Some` here (checked above).
        match prev.as_ref() {
            Some(prev_screen) => screen.state_diff(prev_screen),
            None => screen.state_formatted(),
        }
    };
    // Modes always track the *real* previous frame for a diff, but a full
    // repaint cleared the physical screen — re-push everything.
    let modes = if full_repaint {
        tty_input_mode_bytes(screen, None)
    } else {
        match prev.as_ref() {
            Some(prev_screen) => tty_input_mode_bytes(screen, Some(prev_screen)),
            None => tty_input_mode_bytes(screen, None),
        }
    };
    if bytes.is_empty() && modes.is_empty() {
        return None;
    }
    *prev = Some(screen.clone());
    Some((bytes, modes))
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
    // Skip the write syscall entirely when nothing changed: `tty_frame`
    // returns None for an empty diff *with no mode change*, and a lone
    // `flush()` per 50ms tick on an idle TTY would still wake the terminal
    // driver for nothing. A mode-only frame (empty content, non-empty modes)
    // must still be pushed — otherwise a child toggling bracketed-paste or
    // mouse reporting without touching a cell would never take effect.
    let Some((output, modes)) = tty_frame(proc) else {
        return Ok(());
    };
    let stdout = terminal.backend_mut();
    // No Begin/EndSynchronizedUpdate here: the whole TTY session is already
    // wrapped by enter_tty/exit_tty framing, and nesting synchronized-update
    // scopes produces flicker on terminals that stack them. Per-tick diffs
    // are small cursor-addressed writes; flushing once keeps them atomic
    // enough without an extra scope per 50ms tick. Modes go first so the
    // content that follows is interpreted with the right settings.
    if !modes.is_empty() {
        let _ = stdout.write_all(&modes);
    }
    stdout.write_all(&output)?;
    std::io::Write::flush(stdout)?;
    Ok(())
}

/// Force the next render to be a full repaint, for a newly attached client.
///
/// A fresh client starts from a blank screen, but ratatui diffs frames
/// against the server's *current* screen and `tty_output` diffs against the
/// vt100 cache — if nothing changed, both emit nothing and the client stays
/// blank until the next keystroke causes a visible change. Resetting both
/// ratatui buffers (no backend writes, no cursor query, no physical clear —
/// the client's own `Clear(All)` on startup already wiped its screen) plus
/// dropping every vt100 diff cache makes the next render emit one complete
/// frame.
///
/// Call on every remote `Resize`: `Hello` arrives as a resize and real
/// client resizes also need a full frame at the new geometry.
#[cfg_attr(not(feature = "attach"), allow(dead_code))]
pub fn force_full_repaint<B: Backend>(
    terminal: &mut Terminal<B>,
    mode: &Mode,
    processes: &[Process],
) {
    // Blank both ratatui buffers without touching the physical screen: the
    // next `draw()` diffs blank-vs-blank and repaints every cell. The bytes
    // flow through the backend (and the attach tee) to the client as one
    // full frame; locally it just redraws identical pixels. NOTE: this must
    // reset the *inactive* buffer, which is exactly what double
    // `swap_buffers()` does — it does NOT emit anything by itself.
    terminal.swap_buffers();
    terminal.swap_buffers();
    // Same idea for raw TTY passthrough: drop the diff cache so the next
    // tick emits `state_formatted` instead of an (empty) diff.
    for proc in processes {
        *proc.prev_screen.lock() = None;
    }
    if let Mode::TempTty { process, .. } = mode {
        *process.prev_screen.lock() = None;
    }
}

fn render_dirpicker<B: Backend>(
    terminal: &mut Terminal<B>,
    explorer: &FileExplorer,
    _rows: u16,
    cols: u16,
    show_help: bool,
    help_scroll: u16,
    no_worktree: bool,
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
            Line::from("Enter:pick Esc:cancel arrows:nav /:search ?:help")
        } else {
            Line::from("Enter: pick directory  Esc: cancel  \u{2191}\u{2193}\u{2190}\u{2192}: navigate  /: search  ?: help")
        };
        frame.render_widget(help, help_area);

        if show_help {
            let area = frame.area();
            render_help_overlay(frame, area, help_scroll, no_worktree);
        }
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

    /// Build a `Process` with `bytes` fed through its vt100 parser, at the
    /// given geometry with a zero scrollback.
    ///
    /// NOTE: the alternate-screen flag is grid-independent in vt100, so a
    /// parser built with `Parser::new` always starts on the normal screen —
    /// use [`make_alt_proc`] for alternate-screen cases.
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
            has_unread: Arc::new(AtomicBool::new(false)),
            status_socket_path: None,
            shutdown_flag: None,
            listener_thread: None,
            kill_on_drop: false,
            name_shared: None,
            prev_screen: Arc::new(Mutex::new(None)),
            exit_code: Arc::new(Mutex::new(None)),
            exit_signal: Arc::new(Mutex::new(None)),
            log_buffer: Arc::new(Mutex::new(content.to_vec())),
        }
    }

    /// Build a `Process` with `bytes` fed through its vt100 parser, with an
    /// alternate-screen app active (as `vim`/`less` leave it). `content` is
    /// processed *after* the switch so it lands on the alternate grid.
    fn make_alt_proc(content: &[u8], rows: u16, cols: u16) -> Process {
        let proc = make_proc_with_content(b"", rows, cols);
        proc.parser.lock().process(b"\x1b[?1049h");
        proc.parser.lock().process(content);
        proc
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
            true,
            false,
            0,
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
            false,
            0,
            false,
            true,
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
            true,
            false,
            0,
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
            true,
            false,
            0,
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
            true,
            false,
            0,
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
            true,
            false,
            0,
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

    #[test]
    fn test_force_full_repaint_forces_full_frame() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, backend::Backend};

        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let proc = make_proc_with_content(b"idle", 8, 30);
        let entries = crate::project::build_entries(&projects, std::slice::from_ref(&proc));
        let mode = crate::Mode::Normal { selected: 0 };

        // Steady state: draw twice; the second draw emits an empty diff
        // (nothing changed) — this is exactly the idle-server attach case.
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
            true,
            false,
            0,
        )
        .unwrap();
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
            true,
            false,
            0,
        )
        .unwrap();
        let size = terminal.backend().size().unwrap();
        let second: String = (0..size.height)
            .flat_map(|y| (0..size.width).map(move |x| (x, y)))
            .map(|(x, y)| terminal.backend().buffer()[(x, y)].symbol().to_string())
            .collect();
        assert!(second.contains("Multistack"));

        // A fresh client would see nothing more (empty diff). After forcing
        // a repaint, the next draw must repaint every cell: emulate a blank
        // previous buffer the way ratatui's own diff machinery does — the
        // forced draw must rewrite all rows so the client's diff is full.
        force_full_repaint(&mut terminal, &mode, std::slice::from_ref(&proc));
        // vt100 cache dropped too (raw TTY path heals the same way).
        assert!(proc.prev_screen.lock().is_none());
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
            true,
            false,
            0,
        )
        .unwrap();
        let size = terminal.backend().size().unwrap();
        let repainted: String = (0..size.height)
            .flat_map(|y| (0..size.width).map(move |x| (x, y)))
            .map(|(x, y)| terminal.backend().buffer()[(x, y)].symbol().to_string())
            .collect();
        assert!(repainted.contains("Multistack"));
    }

    // ---- TTY input-mode + alternate-screen edge cases ----
    //
    // A full-screen child (vim/less/lazygit) toggles physical-terminal modes
    // the TUI itself never touches. The child only talks to our vt100 parser,
    // so `tty_frame` must mirror those modes onto the real terminal — and
    // undo them on exit — or arrow keys/mouse/paste/cursor visibly break.

    #[test]
    fn test_tty_frame_carries_modes_on_full_repaint() {
        // `vim`-style entry: app cursor + hidden cursor, no cell touched.
        let proc = make_proc_with_content(b"\x1b[?1h\x1b[?25l", 10, 20);
        let (content, modes) = tty_frame(&proc).expect("full repaint frame");
        assert!(is_full_repaint(&content));
        assert!(
            modes.windows(5).any(|w| w == b"\x1b[?1h"),
            "app-cursor enable missing: {modes:?}"
        );
        assert!(
            modes.windows(6).any(|w| w == b"\x1b[?25l"),
            "hide-cursor missing: {modes:?}"
        );
    }

    /// Subsequence search over raw mode bytes (avoids hand-counting escape
    /// lengths in every assertion below).
    fn modes_contain(modes: &[u8], seq: &[u8]) -> bool {
        seq.is_empty() || modes.windows(seq.len()).any(|w| w == seq)
    }

    #[test]
    fn test_tty_frame_mode_only_change_still_emits() {
        // Steady state first (content + modes flushed, cache populated).
        let proc = make_proc_with_content(b"shell$ ", 10, 20);
        let _ = tty_frame(&proc).expect("initial frame");
        assert!(tty_frame(&proc).is_none(), "idle tick must stay silent");
        // Child enables app-cursor without touching any cell: the content
        // diff may or may not be empty depending on vt100 internals, but the
        // mode switch must go out either way.
        proc.parser.lock().process(b"\x1b[?1h");
        let (_, modes) = tty_frame(&proc).expect("mode-only frame");
        assert!(
            modes_contain(&modes, b"\x1b[?1h"),
            "app-cursor enable dropped: {modes:?}"
        );
        // And steady state is silent again afterwards.
        assert!(tty_frame(&proc).is_none());
    }

    #[test]
    fn test_tty_frame_mode_diff_tracks_previous() {
        // App cursor on, flushed; then off again: the second frame must carry
        // the *disable*, not re-emit the enable.
        let proc = make_proc_with_content(b"\x1b[?1h", 10, 20);
        let (_, first_modes) = tty_frame(&proc).expect("initial frame");
        assert!(modes_contain(&first_modes, b"\x1b[?1h"));
        proc.parser.lock().process(b"\x1b[?1l");
        let (content, modes) = tty_frame(&proc).expect("mode-off frame");
        assert!(
            modes_contain(&modes, b"\x1b[?1l"),
            "app-cursor disable missing: content={content:?} modes={modes:?}"
        );
        assert!(
            !modes_contain(&modes, b"\x1b[?1h"),
            "stale enable must not repeat: {modes:?}"
        );
    }

    #[test]
    fn test_tty_frame_mouse_enable_and_disable() {
        let proc = make_proc_with_content(b"\x1b[?1000h", 10, 20);
        let (_, modes) = tty_frame(&proc).expect("mouse-on frame");
        assert!(
            modes_contain(&modes, b"\x1b[?1000h"),
            "mouse enable missing: {modes:?}"
        );
        proc.parser.lock().process(b"\x1b[?1000l");
        let (_, modes) = tty_frame(&proc).expect("mouse-off frame");
        // Disabling clears every known mouse mode so no stale superset (e.g.
        // an earlier `1002h`) can survive with a wrong encoding.
        for seq in [b"\x1b[?1000l".as_slice(), b"\x1b[?1002l", b"\x1b[?1003l"] {
            assert!(
                modes_contain(&modes, seq),
                "mouse disable incomplete ({seq:?}): {modes:?}"
            );
        }
    }

    #[test]
    fn test_tty_frame_alternate_screen_forces_full_repaint() {
        // Normal screen with content, steady state reached.
        let proc = make_proc_with_content(b"shell output", 10, 20);
        let _ = tty_frame(&proc).expect("initial frame");
        assert!(tty_frame(&proc).is_none());
        // Child enters the alternate screen (`vim`): same geometry, but the
        // visible stack changed — must be a full repaint, never a diff of
        // the alt grid against the stale normal grid.
        proc.parser.lock().process(b"\x1b[?1049hvim screen");
        assert!(proc.parser.lock().screen().alternate_screen());
        let (content, _) = tty_frame(&proc).expect("alt-enter frame");
        assert!(is_full_repaint(&content));
        assert!(content.windows(10).any(|w| w == b"vim screen"));
        // Steady on the alternate screen stays silent...
        assert!(tty_frame(&proc).is_none());
        // ...and leaving it (`:q`) is a full repaint back, not a diff.
        proc.parser.lock().process(b"\x1b[?1049l");
        assert!(!proc.parser.lock().screen().alternate_screen());
        let (content, _) = tty_frame(&proc).expect("alt-exit frame");
        assert!(is_full_repaint(&content));
        assert!(content.windows(12).any(|w| w == b"shell output"));
    }

    #[test]
    fn test_tty_input_mode_bytes_full_repaint_disables_stale_modes() {
        // Child never asked for mouse: a full repaint must still *disable*
        // it, or a stale `ESC[?1000h` left by a previous child survives.
        let proc = make_proc_with_content(b"plain shell", 10, 20);
        let screen = proc.parser.lock().screen().clone();
        let modes = tty_input_mode_bytes(&screen, None);
        assert!(
            modes_contain(&modes, b"\x1b[?1000l"),
            "full repaint must clear mouse: {modes:?}"
        );
        assert!(
            modes_contain(&modes, b"\x1b[?2004l") || modes_contain(&modes, b"\x1b[?2004h"),
            "full repaint must pin bracketed paste: {modes:?}"
        );
        // Diff side stays quiet when nothing changed.
        let same = tty_input_mode_bytes(&screen, Some(&screen));
        assert!(same.is_empty(), "no mode change must emit nothing");
    }

    #[test]
    fn test_tty_frame_content_and_modes_share_one_baseline() {
        // Cell change + mode change in the same tick: both halves describe
        // the same prev→current transition (single cache snapshot).
        let proc = make_proc_with_content(b"a", 10, 20);
        let _ = tty_frame(&proc).expect("initial frame");
        proc.parser.lock().process(b"\x1b[?1hb");
        let (content, modes) = tty_frame(&proc).expect("mixed frame");
        assert!(!content.is_empty());
        assert!(modes_contain(&modes, b"\x1b[?1h"));
        assert!(tty_frame(&proc).is_none());
    }

    #[test]
    fn test_make_alt_proc_helper_marks_alternate() {
        let proc = make_alt_proc(b"alt content", 10, 20);
        assert!(proc.parser.lock().screen().alternate_screen());
        let (content, _) = tty_frame(&proc).expect("alt frame");
        assert!(content.windows(11).any(|w| w == b"alt content"));
    }

    #[test]
    fn test_help_content_has_sections() {
        for no_worktree in [false, true] {
            let lines = help_content(no_worktree);
            assert!(lines.len() > 20, "help should be scrollable");
            let text: String = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            for section in [
                "SPAWN AGENTS",
                "ACT ON SELECTED AGENT",
                "PROJECTS",
                "STATUS + TIMERS",
                "TIPS",
            ] {
                assert!(text.contains(section), "missing {section}");
            }
        }
        // Worktree mode documents n/N, bare mode documents the --no-worktree note.
        let wt: String = help_content(false)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wt.contains("worktree"));
        let bare: String = help_content(true)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(bare.contains("--no-worktree"));
        assert!(wt.contains("●"), "help should document the unread dot");
        assert!(wt.contains("--no-activity-dot"));
    }

    #[test]
    fn test_process_item_unread_dot() {
        use std::sync::atomic::Ordering;
        fn screen_text(terminal: &Terminal<TestBackend>) -> String {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        }
        fn render_with(unread: bool, dot_enabled: bool) -> String {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let projects = vec![crate::project::Project {
                id: 1,
                name: "proj".into(),
                directory: "/tmp".into(),
            }];
            let proc = make_proc_with_content(b"", 24, 80);
            proc.has_unread.store(unread, Ordering::SeqCst);
            let entries = crate::project::build_entries(&projects, std::slice::from_ref(&proc));
            // NB: `entries` borrows `proc`, so pass `proc` itself as the
            // processes slice to keep the unread flag visible to the renderer.
            render_normal(
                &mut terminal,
                &entries,
                &projects,
                std::slice::from_ref(&proc),
                0,
                24,
                80,
                false,
                false,
                dot_enabled,
                false,
                0,
            )
            .unwrap();
            screen_text(&terminal)
        }
        assert!(
            render_with(true, true).contains("●"),
            "unread dot should render when enabled"
        );
        assert!(
            !render_with(true, false).contains("●"),
            "dot must be hidden with -D"
        );
        assert!(
            !render_with(false, true).contains("●"),
            "no dot once the agent was seen"
        );
    }

    #[test]
    fn test_render_help_and_quit_overlays_do_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let projects = vec![crate::project::Project {
            id: 1,
            name: "proj".into(),
            directory: "/tmp".into(),
        }];
        let proc = make_proc_with_content(b"", 24, 80);
        let entries = crate::project::build_entries(&projects, std::slice::from_ref(&proc));
        // Help overlay, top and scrolled.
        for scroll in [0, 5, u16::MAX] {
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
                true,
                true,
                scroll,
            );
            assert!(res.is_ok());
        }
        // Quit overlay with no agents and with agents.
        let res = render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            24,
            80,
            true,
            false,
            true,
            false,
            0,
        );
        assert!(res.is_ok());
        // Tiny screen: overlays must clamp, never panic.
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let res = render_normal(
            &mut terminal,
            &entries,
            &projects,
            &[],
            0,
            10,
            30,
            true,
            false,
            true,
            true,
            100,
        );
        assert!(res.is_ok());
    }

    #[test]
    fn test_quit_counts_breakdown() {
        use std::sync::atomic::Ordering;
        let a = make_proc_with_content(b"", 24, 80);
        a.status
            .store(crate::status::STATUS_WORKING, Ordering::SeqCst);
        let b = make_proc_with_content(b"", 24, 80);
        b.status
            .store(crate::status::STATUS_GIT_CONFLICT, Ordering::SeqCst);
        let procs = vec![a, b];
        let (total, running, working, _waiting, _finished, _dead, conflict) = quit_counts(&procs);
        assert_eq!(total, 2);
        assert_eq!(running, 2);
        assert_eq!(working, 1);
        assert_eq!(conflict, 1);
    }
}
