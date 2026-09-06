use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

#[cfg(not(test))]
use notify_rust::Notification;
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::Mode;
use crate::status;

pub struct Process {
    pub id: usize,
    pub project_id: usize,
    pub project_dir: String,
    /// Expected worktree directory for `--worktree` agents (sibling of
    /// `project_dir`). `None` for bare/parallel/temporary processes.
    pub worktree_dir: Option<String>,
    pub name: String,
    pub child: Option<Box<dyn portable_pty::Child + Send>>,
    pub master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub master_writer: Option<Box<dyn Write + Send>>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub alive: Arc<AtomicBool>,
    pub status: Arc<AtomicU8>,
    pub active_ms: Arc<AtomicU64>,
    pub cycle_start: Arc<Mutex<Option<Instant>>>,
    /// Set when the agent finishes working (`stop` signal) and cleared when
    /// its TTY is opened. Drives the unread-activity dot in the list view.
    pub has_unread: Arc<AtomicBool>,
    pub status_socket_path: Option<String>,
    pub shutdown_flag: Option<Arc<AtomicBool>>,
    pub listener_thread: Option<JoinHandle<()>>,
    pub kill_on_drop: bool,
    pub name_shared: Option<Arc<Mutex<String>>>,
    pub prev_screen: Arc<Mutex<Option<vt100::Screen>>>,
    /// Exit code captured via `try_wait()` polling in `sync_statuses`.
    /// `None` = still running (or unknown, e.g. killed on drop).
    pub exit_code: Arc<Mutex<Option<u32>>>,
    /// Signal name when the child died from a signal (unix), if known.
    pub exit_signal: Arc<Mutex<Option<String>>>,
    /// Raw PTY bytes (capped ring) used to show a log tail when the
    /// subprocess fails. Appended by the reader thread.
    pub log_buffer: Arc<Mutex<Vec<u8>>>,
}

/// Max raw bytes kept per process for failure diagnosis (~64 KiB).
pub const LOG_BUFFER_CAP: usize = 64 * 1024;
/// How many tail lines the TUI error pane shows.
pub const LOG_TAIL_LINES: usize = 6;

impl Process {
    /// Directory the agent actually works in: the worktree when one was
    /// requested (even if it hasn't been created yet), else the project dir.
    pub fn effective_dir(&self) -> String {
        self.worktree_dir
            .clone()
            .unwrap_or_else(|| self.project_dir.clone())
    }

    /// Clear the unread-activity dot (agent has been viewed).
    pub fn mark_seen(&self) {
        self.has_unread.store(false, Ordering::SeqCst);
    }
}

/// Clear the unread flag for the process currently shown in a TTY, if any.
/// Call before every render so a `stop` that lands while the user is watching
/// never produces a stale dot when they return to the list.
pub fn mark_tty_seen(mode: &Mode, processes: &[Process]) {
    if let Mode::Tty { process_id } = mode
        && let Some(proc) = processes.iter().find(|p| p.id == *process_id)
    {
        proc.mark_seen();
    }
}

/// Append raw PTY bytes to the capped log ring.
pub fn push_log(log: &Mutex<Vec<u8>>, bytes: &[u8]) {
    let mut guard = log.lock();
    if bytes.len() >= LOG_BUFFER_CAP {
        *guard = bytes[bytes.len() - LOG_BUFFER_CAP..].to_vec();
        return;
    }
    if guard.len() + bytes.len() > LOG_BUFFER_CAP {
        let overflow = guard.len() + bytes.len() - LOG_BUFFER_CAP;
        guard.drain(..overflow);
    }
    guard.extend_from_slice(bytes);
}

/// Last `n` non-empty plain-text lines from raw PTY bytes: strips ANSI/CSI
/// via a scratch `vt100::Parser` sized to the current screen so wrapped
/// output reflows instead of truncating.
pub fn log_tail_lines(log: &[u8], rows: u16, cols: u16, n: usize) -> Vec<String> {
    if log.is_empty() || n == 0 {
        return Vec::new();
    }
    let rows = rows.max(1);
    let cols = cols.max(1);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(log);
    let contents = parser.screen().contents();
    let lines: Vec<String> = contents
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut s: String = l.chars().take(cols as usize).collect();
            if l.chars().count() > cols as usize {
                // `cols` is u16 >= 1 so saturating_sub is safe.
                while s.chars().count() > (cols as usize).saturating_sub(1) {
                    s.pop();
                }
                s.push('…');
            }
            s
        })
        .collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

/// True when the process exited with a non-zero code or terminating signal.
pub fn failed(proc: &Process) -> bool {
    if proc.exit_signal.lock().is_some() {
        return true;
    }
    matches!(*proc.exit_code.lock(), Some(code) if code != 0)
}

/// One-line human-readable exit reason, e.g. `exit code 1` or
/// `killed by SIGKILL`. Empty when the child is still running.
pub fn exit_reason(proc: &Process) -> Option<String> {
    if let Some(sig) = proc.exit_signal.lock().clone() {
        return Some(format!("killed by {sig}"));
    }
    proc.exit_code
        .lock()
        .map(|code| format!("exit code {code}"))
}

/// Poll a live child's `try_wait()` once and stash the result. Returns the
/// exit status when the child has terminated.
fn poll_exit(proc: &mut Process) -> Option<portable_pty::ExitStatus> {
    let child = proc.child.as_mut()?;
    // `try_wait` succeeds exactly once: after it returns `Some`, the child
    // is reaped and later calls may error — hence stash in Arcs.
    if proc.exit_code.lock().is_some() || proc.exit_signal.lock().is_some() {
        return None;
    }
    match child.try_wait() {
        Ok(Some(status)) => {
            *proc.exit_code.lock() = Some(status.exit_code());
            if let Some(sig) = status.signal() {
                *proc.exit_signal.lock() = Some(sig.to_string());
            }
            Some(status)
        }
        Ok(None) | Err(_) => None,
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(ref flag) = self.shutdown_flag {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(handle) = self.listener_thread.take() {
            // The listener polls the shutdown flag every 100ms; give it a
            // bounded grace period so a wedged socket can't freeze the UI.
            let deadline = Instant::now() + std::time::Duration::from_secs(1);
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
            // If still running, detach: the thread owns only Arcs + the
            // socket path and exits on its next poll.
        }
        if let Some(ref path) = self.status_socket_path {
            let _ = std::fs::remove_file(path);
        }
        // Always terminate the child before waiting: a `d`/`l`/quit on a
        // still-running agent must not orphan it, and an unconditional
        // blocking `wait()` on a live interactive child would freeze the UI.
        // `kill` on an already-dead child is a harmless no-op error.
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
        drop(self.master_writer.take());
        drop(self.master.take());
        if let Some(ref mut child) = self.child {
            // Bounded reap: `wait()` blocks until the child exits, so poll
            // `try_wait()` first and only block briefly.
            let deadline = Instant::now() + std::time::Duration::from_millis(500);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            // The child is dead or reaped above; `wait()` now returns
            // immediately. If the deadline expired with a live child (e.g.
            // ignoring SIGKILL), detach it rather than hanging the TUI.
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                let _ = child.wait();
            }
        }
    }
}

pub fn check_tty_alive(mode: &Mode, processes: &mut Vec<Process>) -> Option<usize> {
    match mode {
        Mode::Tty { process_id } => {
            let pid = *process_id;
            let Some(idx) = processes.iter().position(|p| p.id == pid) else {
                processes.retain(|p| p.id != pid);
                return Some(0);
            };
            if processes[idx].alive.load(Ordering::SeqCst) {
                return None;
            }
            // Reap the exit status now so the dead agent keeps its
            // failure reason for the TUI error view.
            poll_exit(&mut processes[idx]);
            let dir = processes[idx].project_dir.clone();
            let was_failed = failed(&processes[idx]);
            run_speck_apply_if_present(&dir);
            // Keep failed agents in the list so the user can see the
            // exit code + log tail (`d` dismisses explicitly). Clean
            // exits are still pruned immediately.
            if was_failed {
                return Some(0);
            }
            processes.retain(|p| p.id != pid);
            Some(0)
        }
        Mode::TempTty {
            process,
            previous_selected,
        } => {
            if !process.alive.load(Ordering::SeqCst) {
                run_speck_apply_if_present(&process.project_dir);
                Some(*previous_selected)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn spawn_pty(
    pty_system: &NativePtySystem,
    cmd: &str,
    args: &[&str],
    title: Option<&str>,
    rows: u16,
    cols: u16,
    cwd: &str,
) -> std::io::Result<Process> {
    // portable-pty rejects 0x0 sizes; clamp to vt100-compatible minimums.
    let rows = rows.max(1);
    let cols = cols.max(1);
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(std::io::Error::other)?;

    let mut cmd_builder = CommandBuilder::new(cmd);
    for arg in args {
        cmd_builder.arg(arg);
    }
    cmd_builder.cwd(cwd);
    let child = pair
        .slave
        .spawn_command(cmd_builder)
        .map_err(std::io::Error::other)?;

    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(std::io::Error::other)?;
    let writer = pair.master.take_writer().map_err(std::io::Error::other)?;

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let parser_clone = parser.clone();
    let alive = Arc::new(AtomicBool::new(true));
    let alive_clone = alive.clone();
    let log_buffer = Arc::new(Mutex::new(Vec::new()));
    let log_clone = log_buffer.clone();

    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    push_log(&log_clone, &buf[..n]);
                    parser_clone.lock().process(&buf[..n]);
                }
                Err(_) => break,
            }
        }
        alive_clone.store(false, Ordering::SeqCst);
    });

    let name = if let Some(title) = title {
        title.to_string()
    } else if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, args.join(" "))
    };

    Ok(Process {
        id: 0,
        project_id: 0,
        project_dir: String::new(),
        worktree_dir: None,
        name,
        child: Some(child),
        master: Some(pair.master),
        master_writer: Some(writer),
        parser,
        alive,
        status: Arc::new(AtomicU8::new(status::STATUS_NOT_YET)),
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
        log_buffer,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_process(
    pty_system: &NativePtySystem,
    next_id: &mut usize,
    project_id: usize,
    cmd: &str,
    args: &[&str],
    title: Option<&str>,
    rows: u16,
    cols: u16,
    status_socket: Option<&str>,
    cwd: &str,
    worktree_dir: Option<&str>,
    activity_dot_enabled: bool,
) -> std::io::Result<Process> {
    let id = *next_id;
    *next_id += 1;

    let mut proc = spawn_pty(pty_system, cmd, args, title, rows, cols, cwd)?;
    proc.id = id;
    proc.project_id = project_id;
    proc.project_dir = cwd.to_string();
    proc.worktree_dir = worktree_dir.map(|s| s.to_string());

    let (status_socket_path, shutdown_flag, listener_thread) = if let Some(path) = status_socket {
        let name_shared = Arc::new(Mutex::new(proc.name.clone()));
        let (flag, handle) = status::spawn_status_listener(
            proc.status.clone(),
            proc.active_ms.clone(),
            proc.cycle_start.clone(),
            proc.has_unread.clone(),
            activity_dot_enabled,
            path.to_string(),
            name_shared.clone(),
            proc.project_dir.clone(),
        );
        proc.name_shared = Some(name_shared);
        (Some(path.to_string()), Some(flag), Some(handle))
    } else {
        (None, None, None)
    };

    proc.status_socket_path = status_socket_path;
    proc.shutdown_flag = shutdown_flag;
    proc.listener_thread = listener_thread;

    Ok(proc)
}

pub fn resize_parsers(processes: &mut [Process], rows: u16, cols: u16) {
    for proc in processes.iter_mut() {
        let mut parser = proc.parser.lock();
        let old_screen = parser.screen().clone();
        let rows = if rows == 0 { 24 } else { rows };
        let cols = if cols == 0 { 80 } else { cols };
        let mut new_parser = vt100::Parser::new(rows, cols, old_screen.scrollback());
        new_parser.process(&old_screen.contents_formatted());
        *parser = new_parser;
        // Invalidate cached previous screen after resize to force full redraw
        *proc.prev_screen.lock() = None;
    }
}

pub fn run_speck_apply_if_present(dir: &str) {
    let speck_toml = std::path::Path::new(dir).join("Speck.toml");
    if speck_toml.exists() {
        let _ = std::process::Command::new("speck")
            .arg("apply")
            .current_dir(dir)
            .status();
    }
}

pub fn sync_statuses(processes: &mut [Process]) {
    for p in processes.iter_mut() {
        // Reap the exit status as soon as the child terminates, even while
        // its PTY reader thread is still draining output. `failed()` then
        // knows the exit code without waiting for EOF.
        poll_exit(p);
        if p.alive.load(Ordering::SeqCst) {
            continue;
        }
        let status = p.status.load(Ordering::SeqCst);
        // Anything that never reported `stop` still holds an open cycle;
        // credit it before freezing the timer so elapsed time isn't lost.
        // NOT_YET (never started) means "dead on arrival" -> [X].
        if status == status::STATUS_WORKING
            || status == status::STATUS_NOT_YET
            || status == status::STATUS_GIT_CONFLICT
        {
            if let Some(start) = p.cycle_start.lock().take() {
                let elapsed = start.elapsed();
                p.active_ms
                    .fetch_add(elapsed.as_millis() as u64, Ordering::SeqCst);
            }
            if status != status::STATUS_GIT_CONFLICT {
                p.status.store(status::STATUS_DEAD, Ordering::SeqCst);
            }
            if status == status::STATUS_GIT_CONFLICT {
                continue;
            }
            run_speck_apply_if_present(&p.project_dir);
            if p.worktree_dir
                .as_deref()
                .is_some_and(|d| d != p.project_dir)
            {
                run_speck_apply_if_present(&p.effective_dir());
            }
            #[cfg(not(test))]
            {
                if failed(p) {
                    let reason = exit_reason(p).unwrap_or_else(|| "failed".to_string());
                    let _ = Notification::new()
                        .summary("Agent failed")
                        .body(&format!("{} {reason}", &p.name))
                        .show();
                } else {
                    let _ = Notification::new()
                        .summary("Agent died")
                        .body(&format!("{} has terminated unexpectedly", &p.name))
                        .show();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn make_test_process(alive: bool, status: u8, active_ms: u64, with_cycle: bool) -> Process {
        let cycle_start = if with_cycle {
            Some(
                std::time::Instant::now()
                    .checked_sub(std::time::Duration::from_millis(500))
                    .unwrap(),
            )
        } else {
            None
        };
        Process {
            id: 1,
            project_id: 1,
            project_dir: String::new(),
            worktree_dir: None,
            name: "test [1]".into(),
            child: None,
            master: None,
            master_writer: None,
            parser: Arc::new(parking_lot::Mutex::new(vt100::Parser::new(1, 1, 0))),
            alive: Arc::new(AtomicBool::new(alive)),
            status: Arc::new(AtomicU8::new(status)),
            active_ms: Arc::new(AtomicU64::new(active_ms)),
            cycle_start: Arc::new(parking_lot::Mutex::new(cycle_start)),
            has_unread: Arc::new(AtomicBool::new(false)),
            status_socket_path: None,
            shutdown_flag: None,
            listener_thread: None,
            kill_on_drop: false,
            name_shared: None,
            prev_screen: Arc::new(parking_lot::Mutex::new(None)),
            exit_code: Arc::new(Mutex::new(None)),
            exit_signal: Arc::new(Mutex::new(None)),
            log_buffer: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn test_sync_statuses_alive_working_unchanged() {
        let mut p = make_test_process(true, status::STATUS_WORKING, 5000, true);
        sync_statuses(std::slice::from_mut(&mut p));
    }

    #[test]
    fn test_sync_statuses_dead_marks_dead_and_credits_time() {
        let mut p = make_test_process(false, status::STATUS_WORKING, 5000, true);
        sync_statuses(std::slice::from_mut(&mut p));
        assert_eq!(p.status.load(Ordering::SeqCst), status::STATUS_DEAD);
        assert!(p.active_ms.load(Ordering::SeqCst) >= 5000);
        assert!(p.cycle_start.lock().is_none());
    }

    #[test]
    fn test_sync_statuses_dead_no_cycle_still_marks_dead() {
        let mut p = make_test_process(false, status::STATUS_WORKING, 5000, false);
        sync_statuses(std::slice::from_mut(&mut p));
        assert_eq!(p.status.load(Ordering::SeqCst), status::STATUS_DEAD);
        assert_eq!(p.active_ms.load(Ordering::SeqCst), 5000);
    }

    #[test]
    fn test_sync_statuses_already_finished_unchanged() {
        let mut p = make_test_process(false, status::STATUS_FINISHED, 10000, false);
        sync_statuses(std::slice::from_mut(&mut p));
        assert_eq!(p.status.load(Ordering::SeqCst), status::STATUS_FINISHED);
        assert_eq!(p.active_ms.load(Ordering::SeqCst), 10000);
    }

    #[test]
    fn test_sync_statuses_not_yet_alive_unchanged() {
        let mut p = make_test_process(true, status::STATUS_NOT_YET, 0, false);
        sync_statuses(std::slice::from_mut(&mut p));
        assert_eq!(p.status.load(Ordering::SeqCst), status::STATUS_NOT_YET);
    }

    #[test]
    fn test_sync_statuses_git_conflict_preserved() {
        let mut p = make_test_process(false, status::STATUS_GIT_CONFLICT, 10000, false);
        sync_statuses(std::slice::from_mut(&mut p));
        assert_eq!(p.status.load(Ordering::SeqCst), status::STATUS_GIT_CONFLICT);
        assert_eq!(p.active_ms.load(Ordering::SeqCst), 10000);
        assert!(p.cycle_start.lock().is_none());
    }

    #[test]
    fn test_resize_parsers_clears_prev_screen() {
        // Use a reasonably sized parser to avoid vt100 overflow on tiny 1x1 grids
        let p = {
            let mut base = make_test_process(true, status::STATUS_WORKING, 0, false);
            // Replace 1x1 parser with 10x20 for realistic content
            base.parser = Arc::new(parking_lot::Mutex::new(vt100::Parser::new(10, 20, 0)));
            base
        };
        // Fill parser with content and set prev_screen
        {
            let mut parser = p.parser.lock();
            parser.process(b"hello world");
        }
        let screen = p.parser.lock().screen().clone();
        *p.prev_screen.lock() = Some(screen);
        assert!(p.prev_screen.lock().is_some());
        let mut processes = vec![p];
        resize_parsers(&mut processes, 30, 100);
        assert!(processes[0].prev_screen.lock().is_none());
        let (rows, cols) = processes[0].parser.lock().screen().size();
        assert_eq!((rows, cols), (30, 100));
    }

    #[test]
    fn test_resize_parsers_zero_defaults() {
        let p = make_test_process(true, status::STATUS_NOT_YET, 0, false);
        let mut processes = vec![p];
        resize_parsers(&mut processes, 0, 0);
        let (rows, cols) = processes[0].parser.lock().screen().size();
        assert_eq!((rows, cols), (24, 80));
        assert!(processes[0].prev_screen.lock().is_none());
    }

    #[test]
    fn test_resize_parsers_preserves_visible_content() {
        let p = make_test_process(true, status::STATUS_NOT_YET, 0, false);
        // Give parser a larger initial size
        {
            let mut parser = p.parser.lock();
            *parser = vt100::Parser::new(10, 20, 0);
            parser.process(b"test content");
        }
        let _before = p.parser.lock().screen().contents();
        let mut processes = vec![p];
        resize_parsers(&mut processes, 10, 20);
        let after = processes[0].parser.lock().screen().contents();
        assert!(after.contains("test content") || after.contains("test"));
        // Ensure scrollback preserved
        assert!(processes[0].parser.lock().screen().size() == (10, 20));
    }

    #[test]
    fn test_prev_screen_initially_none() {
        let p = make_test_process(true, status::STATUS_NOT_YET, 0, false);
        assert!(p.prev_screen.lock().is_none());
    }

    #[test]
    fn test_check_tty_alive_tty_dead_removes() {
        let p = make_test_process(false, status::STATUS_WORKING, 0, false);
        let pid = p.id;
        let mut processes = vec![p];
        let mode = crate::Mode::Tty { process_id: pid };
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, Some(0));
        assert!(processes.is_empty());
    }

    #[test]
    fn test_check_tty_alive_tty_alive_noop() {
        let p = make_test_process(true, status::STATUS_WORKING, 0, false);
        let pid = p.id;
        let mut processes = vec![p];
        let mode = crate::Mode::Tty { process_id: pid };
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, None);
        assert_eq!(processes.len(), 1);
    }

    #[test]
    fn test_check_tty_alive_tty_not_found() {
        let p = make_test_process(true, status::STATUS_NOT_YET, 0, false);
        let mut processes = vec![p];
        let mode = crate::Mode::Tty { process_id: 999 };
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, Some(0));
        // Should retain existing process since pid not found? Actually code retains only p.id != 999, so keeps p
        assert_eq!(processes.len(), 1);
    }

    #[test]
    fn test_check_tty_alive_temptty_dead() {
        let proc = make_test_process(false, status::STATUS_NOT_YET, 0, false);
        proc.alive.store(false, Ordering::SeqCst);
        let mode = crate::Mode::TempTty {
            process: Box::new(proc),
            previous_selected: 3,
        };
        let mut processes = vec![];
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, Some(3));
    }

    #[test]
    fn test_check_tty_alive_temptty_alive() {
        let proc = make_test_process(true, status::STATUS_NOT_YET, 0, false);
        proc.alive.store(true, Ordering::SeqCst);
        let mode = crate::Mode::TempTty {
            process: Box::new(proc),
            previous_selected: 2,
        };
        let mut processes = vec![];
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, None);
    }

    #[test]
    fn test_check_tty_alive_normal_is_none() {
        let mode = crate::Mode::Normal { selected: 0 };
        let mut processes = vec![];
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parser_processes_ansi_and_diff_logic() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"hello");
        let screen1 = parser.screen().clone();
        let bytes1 = screen1.state_formatted();
        assert!(!bytes1.is_empty());
        // Store as prev
        let prev: Option<vt100::Screen> = Some(screen1.clone());
        parser.process(b" world");
        let screen2 = parser.screen().clone();
        let diff = screen2.state_diff(prev.as_ref().unwrap());
        assert!(!diff.is_empty());
        // Diff should be smaller than full
        let full = screen2.state_formatted();
        assert!(diff.len() < full.len());
        // Same screen diff should be empty
        let diff2 = screen2.state_diff(&screen2);
        assert!(diff2.is_empty());
        // Different size forces full redraw
        let mut parser2 = vt100::Parser::new(10, 30, 0);
        parser2.process(b"hello");
        let screen3 = parser2.screen().clone();
        assert_ne!(screen2.size(), screen3.size());
    }

    #[test]
    fn test_vt100_contents_and_state() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"\x1b[31mred\x1b[0m");
        let screen = parser.screen();
        assert!(screen.contents().contains("red"));
        let formatted = screen.contents_formatted();
        // Should contain color escape or at least content
        assert!(formatted.windows(3).any(|w| w == b"red"));
        let state = screen.state_formatted();
        assert!(!state.is_empty());
    }

    #[test]
    fn test_failed_none_when_running() {
        let p = make_test_process(true, status::STATUS_WORKING, 0, false);
        assert!(!failed(&p));
        assert!(exit_reason(&p).is_none());
    }

    #[test]
    fn test_failed_nonzero_exit_code() {
        let p = make_test_process(false, status::STATUS_DEAD, 0, false);
        *p.exit_code.lock() = Some(1);
        assert!(failed(&p));
        assert_eq!(exit_reason(&p).as_deref(), Some("exit code 1"));
    }

    #[test]
    fn test_failed_zero_exit_code_is_clean() {
        let p = make_test_process(false, status::STATUS_FINISHED, 0, false);
        *p.exit_code.lock() = Some(0);
        assert!(!failed(&p));
        assert_eq!(exit_reason(&p).as_deref(), Some("exit code 0"));
    }

    #[test]
    fn test_failed_signal() {
        let p = make_test_process(false, status::STATUS_DEAD, 0, false);
        *p.exit_signal.lock() = Some("SIGKILL".to_string());
        assert!(failed(&p));
        assert_eq!(exit_reason(&p).as_deref(), Some("killed by SIGKILL"));
    }

    #[test]
    fn test_check_tty_alive_failed_kept_for_inspection() {
        let p = make_test_process(false, status::STATUS_DEAD, 0, false);
        *p.exit_code.lock() = Some(2);
        let pid = p.id;
        let mut processes = vec![p];
        let mode = crate::Mode::Tty { process_id: pid };
        let result = check_tty_alive(&mode, &mut processes);
        assert_eq!(result, Some(0));
        // Failed agent stays in the list so its logs can be inspected.
        assert_eq!(processes.len(), 1);
        assert!(failed(&processes[0]));
    }

    #[test]
    fn test_push_log_caps_at_64k() {
        let log = parking_lot::Mutex::new(Vec::new());
        push_log(&log, &vec![b'x'; LOG_BUFFER_CAP + 100]);
        assert_eq!(log.lock().len(), LOG_BUFFER_CAP);
        push_log(&log, b"tail");
        assert_eq!(log.lock().len(), LOG_BUFFER_CAP);
        assert!(log.lock().ends_with(b"tail"));
    }

    #[test]
    fn test_log_tail_lines_strips_ansi_and_takes_tail() {
        let mut raw = Vec::new();
        for i in 0..10 {
            raw.extend_from_slice(format!("\x1b[31mline{i}\x1b[0m\r\n").as_bytes());
        }
        let tail = log_tail_lines(&raw, 24, 80, 3);
        assert_eq!(tail, vec!["line7", "line8", "line9"]);
    }

    #[test]
    fn test_log_tail_lines_empty() {
        assert!(log_tail_lines(&[], 24, 80, 6).is_empty());
        assert!(log_tail_lines(b"hi", 24, 80, 0).is_empty());
    }

    #[test]
    fn test_mark_seen_clears_unread() {
        let p = make_test_process(true, status::STATUS_FINISHED, 0, false);
        p.has_unread.store(true, Ordering::SeqCst);
        p.mark_seen();
        assert!(!p.has_unread.load(Ordering::SeqCst));
    }

    #[test]
    fn test_mark_tty_seen_clears_viewed_process_only() {
        let p1 = make_test_process(true, status::STATUS_FINISHED, 0, false);
        let mut p2_mut = make_test_process(true, status::STATUS_FINISHED, 0, false);
        p2_mut.id = 2;
        let p2 = p2_mut;
        p1.has_unread.store(true, Ordering::SeqCst);
        p2.has_unread.store(true, Ordering::SeqCst);
        let processes = vec![p1, p2];
        mark_tty_seen(&crate::Mode::Tty { process_id: 1 }, &processes);
        assert!(!processes[0].has_unread.load(Ordering::SeqCst));
        assert!(processes[1].has_unread.load(Ordering::SeqCst));
    }

    #[test]
    fn test_mark_tty_seen_noop_outside_tty() {
        let p = make_test_process(true, status::STATUS_FINISHED, 0, false);
        p.has_unread.store(true, Ordering::SeqCst);
        let processes = vec![p];
        mark_tty_seen(&crate::Mode::Normal { selected: 0 }, &processes);
        assert!(processes[0].has_unread.load(Ordering::SeqCst));
    }
}
