use crate::process::Process;

pub struct Project {
    pub id: usize,
    pub name: String,
    pub directory: String,
}

pub enum ListEntry {
    ProjectHeader(usize),
    Agent(usize),
}

pub fn build_entries(projects: &[Project], processes: &[Process]) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    let show_headers = projects.len() > 1;

    for proj in projects {
        if show_headers {
            entries.push(ListEntry::ProjectHeader(proj.id));
        }
        for proc in processes {
            if proc.project_id == proj.id {
                entries.push(ListEntry::Agent(proc.id));
            }
        }
    }

    entries
}

pub fn resolve_project(
    entries: &[ListEntry],
    projects: &[Project],
    selected: usize,
) -> Option<usize> {
    match entries.get(selected) {
        Some(ListEntry::ProjectHeader(pid)) => Some(*pid),
        Some(ListEntry::Agent(_)) => entries[..selected]
            .iter()
            .rev()
            .find_map(|e| {
                if let ListEntry::ProjectHeader(pid) = e {
                    Some(*pid)
                } else {
                    None
                }
            })
            .or_else(|| projects.first().map(|p| p.id)),
        None => projects.first().map(|p| p.id),
    }
}

pub fn is_agent(entries: &[ListEntry], selected: usize) -> bool {
    matches!(entries.get(selected), Some(ListEntry::Agent(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Process;
    use parking_lot::Mutex;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64},
    };

    fn make_proc(id: usize, project_id: usize) -> Process {
        Process {
            id,
            project_id,
            project_dir: format!("/tmp/proj{project_id}"),
            worktree_dir: None,
            name: format!("agent{id}"),
            child: None,
            master: None,
            master_writer: None,
            parser: Arc::new(Mutex::new(vt100::Parser::new(10, 10, 0))),
            alive: Arc::new(AtomicBool::new(true)),
            status: Arc::new(AtomicU8::new(0)),
            active_ms: Arc::new(AtomicU64::new(0)),
            cycle_start: Arc::new(parking_lot::Mutex::new(None)),
            has_unread: Arc::new(AtomicBool::new(false)),
            status_socket_path: None,
            shutdown_flag: None,
            listener_thread: None,
            kill_on_drop: false,
            name_shared: None,
            prev_screen: Arc::new(Mutex::new(None)),
            exit_code: Arc::new(Mutex::new(None)),
            exit_signal: Arc::new(Mutex::new(None)),
            log_buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn make_proj(id: usize, name: &str) -> Project {
        Project {
            id,
            name: name.into(),
            directory: format!("/tmp/{name}"),
        }
    }

    #[test]
    fn test_build_entries_single_project_no_headers() {
        let projects = vec![make_proj(1, "proj1")];
        let processes = vec![make_proc(10, 1), make_proc(11, 1)];
        let entries = build_entries(&projects, &processes);
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], ListEntry::Agent(10)));
        assert!(matches!(entries[1], ListEntry::Agent(11)));
    }

    #[test]
    fn test_build_entries_multiple_projects_with_headers() {
        let projects = vec![make_proj(1, "a"), make_proj(2, "b")];
        let processes = vec![make_proc(1, 1), make_proc(2, 2), make_proc(3, 1)];
        let entries = build_entries(&projects, &processes);
        // Should interleave headers and agents sorted by project order
        assert_eq!(entries.len(), 5);
        assert!(matches!(entries[0], ListEntry::ProjectHeader(1)));
        assert!(matches!(entries[1], ListEntry::Agent(1)));
        assert!(matches!(entries[2], ListEntry::Agent(3)));
        assert!(matches!(entries[3], ListEntry::ProjectHeader(2)));
        assert!(matches!(entries[4], ListEntry::Agent(2)));
    }

    #[test]
    fn test_build_entries_empty() {
        let entries = build_entries(&[], &[]);
        assert!(entries.is_empty());
        let projects = vec![make_proj(1, "solo")];
        let entries = build_entries(&projects, &[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_resolve_project_on_header() {
        let projects = vec![make_proj(1, "a"), make_proj(2, "b")];
        let processes = vec![make_proc(10, 1)];
        let entries = build_entries(&projects, &processes);
        // entries: Header1, Agent10, Header2
        assert_eq!(resolve_project(&entries, &projects, 0), Some(1));
        assert_eq!(resolve_project(&entries, &projects, 2), Some(2));
    }

    #[test]
    fn test_resolve_project_on_agent() {
        let projects = vec![make_proj(1, "a"), make_proj(2, "b")];
        let processes = vec![make_proc(10, 1), make_proc(20, 2)];
        let entries = build_entries(&projects, &processes);
        // Header1, Agent10, Header2, Agent20
        assert_eq!(resolve_project(&entries, &projects, 1), Some(1));
        assert_eq!(resolve_project(&entries, &projects, 3), Some(2));
    }

    #[test]
    fn test_resolve_project_fallback() {
        let projects = vec![make_proj(5, "only")];
        let entries = vec![];
        assert_eq!(resolve_project(&entries, &projects, 0), Some(5));
        assert_eq!(resolve_project(&entries, &projects, 99), Some(5));
        let empty_proj: Vec<Project> = vec![];
        assert_eq!(resolve_project(&entries, &empty_proj, 0), None);
    }

    #[test]
    fn test_is_agent() {
        let entries = vec![ListEntry::ProjectHeader(1), ListEntry::Agent(10)];
        assert!(!is_agent(&entries, 0));
        assert!(is_agent(&entries, 1));
        assert!(!is_agent(&entries, 5));
    }
}
