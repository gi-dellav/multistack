use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use crate::project::Project;
use crate::remote::{ProjectTarget, SshTarget};

fn projects_file_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zerostack")
        .join("multistack_projects")
}

fn first_launch_marker_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zerostack")
        .join("multistack_first_launch")
}

/// True when multistack has never been launched before (marker file absent).
pub fn is_first_launch() -> bool {
    !first_launch_marker_path().exists()
}

/// Record that the welcome screen was shown. Best-effort: callers ignore
/// errors (worst case the welcome screen reappears next launch).
pub fn mark_first_launch_seen() -> io::Result<()> {
    let path = first_launch_marker_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(&path)?;
    Ok(())
}

pub fn save_projects(projects: &[Project]) -> io::Result<()> {
    let path = projects_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(&path)?;
    for p in projects {
        writeln!(f, "{}", encode_target(&p.target))?;
    }
    Ok(())
}

pub fn load_project_targets() -> io::Result<Vec<ProjectTarget>> {
    let path = projects_file_path();
    let f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut targets = Vec::new();
    for line in io::BufReader::new(f).lines() {
        let line = line?;
        if let Some(target) = decode_target(&line) {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn encode_target(target: &ProjectTarget) -> String {
    match target {
        ProjectTarget::Local { dir } => format!("v2\tlocal\t{}", escape(dir)),
        ProjectTarget::Ssh { host, dir } => format!(
            "v2\tssh\t{}\t{}\t{}\t{}",
            escape(host.user.as_deref().unwrap_or("")),
            escape(&host.host),
            host.port.map(|p| p.to_string()).unwrap_or_default(),
            escape(dir)
        ),
    }
}

fn decode_target(line: &str) -> Option<ProjectTarget> {
    if let Some(rest) = line.strip_prefix("v2\t") {
        let fields: Vec<&str> = rest.split('\t').collect();
        return match fields.as_slice() {
            ["local", dir] => Some(ProjectTarget::Local { dir: unescape(dir) }),
            ["ssh", user, host, port, dir] => {
                let port = if port.is_empty() {
                    None
                } else {
                    Some(port.parse::<u16>().ok()?)
                };
                Some(ProjectTarget::Ssh {
                    host: SshTarget {
                        user: if user.is_empty() {
                            None
                        } else {
                            Some(unescape(user))
                        },
                        host: unescape(host),
                        port,
                    },
                    dir: unescape(dir),
                })
            }
            _ => None,
        };
    }

    // Legacy format: one local directory per line.
    let trimmed = line.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(ProjectTarget::local(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_local() {
        let target = ProjectTarget::local("/tmp/a b\tc");
        let encoded = encode_target(&target);
        assert_eq!(decode_target(&encoded), Some(target));
    }

    #[test]
    fn test_round_trip_ssh() {
        let target = ProjectTarget::ssh(
            SshTarget {
                user: Some("deploy".into()),
                host: "buildbox".into(),
                port: Some(2222),
            },
            "/srv/a b",
        );
        let encoded = encode_target(&target);
        assert_eq!(decode_target(&encoded), Some(target));
    }

    #[test]
    fn test_round_trip_without_user_or_port() {
        let target = ProjectTarget::ssh(
            SshTarget {
                user: None,
                host: "buildbox".into(),
                port: None,
            },
            "/srv/app",
        );
        let encoded = encode_target(&target);
        assert_eq!(decode_target(&encoded), Some(target));
    }

    #[test]
    fn test_legacy_line_is_local() {
        assert_eq!(
            decode_target("/home/me/proj"),
            Some(ProjectTarget::local("/home/me/proj"))
        );
        assert_eq!(decode_target("   "), None);
    }

    #[test]
    fn test_malformed_v2_is_ignored() {
        assert_eq!(decode_target("v2\tbogus"), None);
        assert_eq!(decode_target("v2\tssh\tu\th\tnotaport\t/d"), None);
    }
}
