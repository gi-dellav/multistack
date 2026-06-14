use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::project::Project;

fn projects_file_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zerostack")
        .join("multistack_projects")
}

pub fn save_projects(projects: &[Project]) -> io::Result<()> {
    let path = projects_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(&path)?;
    for p in projects {
        writeln!(f, "{}", p.directory)?;
    }
    Ok(())
}

pub fn load_project_dirs() -> io::Result<Vec<String>> {
    let path = projects_file_path();
    let f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut dirs = Vec::new();
    for line in io::BufReader::new(f).lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            dirs.push(trimmed.to_string());
        }
    }
    Ok(dirs)
}
