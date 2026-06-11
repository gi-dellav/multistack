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
