use std::path::Path;
use std::sync::OnceLock;

/// Destination of an SSH project or process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// Where a project lives. Paths are always paths on the execution host:
/// local filesystem for `Local`, remote filesystem for `Ssh`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectTarget {
    Local { dir: String },
    Ssh { host: SshTarget, dir: String },
}

/// Host a process runs on. Stored separately from paths so the actual
/// working directory can be remote without pretending it is a local path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionHost {
    Local,
    Ssh(SshTarget),
}

impl ProjectTarget {
    pub fn local(dir: impl Into<String>) -> Self {
        ProjectTarget::Local { dir: dir.into() }
    }

    pub fn ssh(host: SshTarget, dir: impl Into<String>) -> Self {
        ProjectTarget::Ssh {
            host,
            dir: dir.into(),
        }
    }

    pub fn dir(&self) -> &str {
        match self {
            ProjectTarget::Local { dir } | ProjectTarget::Ssh { dir, .. } => dir,
        }
    }

    pub fn ssh_target(&self) -> Option<&SshTarget> {
        match self {
            ProjectTarget::Local { .. } => None,
            ProjectTarget::Ssh { host, .. } => Some(host),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, ProjectTarget::Ssh { .. })
    }

    pub fn execution_host(&self) -> ExecutionHost {
        match self {
            ProjectTarget::Local { .. } => ExecutionHost::Local,
            ProjectTarget::Ssh { host, .. } => ExecutionHost::Ssh(host.clone()),
        }
    }

    /// Human-readable location used in project headers and notifications.
    pub fn display_location(&self) -> String {
        match self {
            ProjectTarget::Local { dir } => dir.clone(),
            ProjectTarget::Ssh { host, dir } => {
                let mut out = String::from("ssh://");
                if let Some(user) = &host.user {
                    out.push_str(user);
                    out.push('@');
                }
                if host.host.contains(':') {
                    out.push('[');
                    out.push_str(&host.host);
                    out.push(']');
                } else {
                    out.push_str(&host.host);
                }
                if let Some(port) = host.port {
                    out.push(':');
                    out.push_str(&port.to_string());
                }
                out.push_str(dir);
                out
            }
        }
    }

    /// Project name derived from the directory, with the host as fallback
    /// for root directories.
    pub fn default_name(&self) -> String {
        Path::new(self.dir())
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| match self {
                ProjectTarget::Local { .. } => "root".to_string(),
                ProjectTarget::Ssh { host, .. } => host.host.clone(),
            })
    }

    /// Stable key for duplicate detection. Host names are case-insensitive;
    /// user names and paths are not. Trailing slashes are ignored.
    pub fn identity_key(&self) -> String {
        match self {
            ProjectTarget::Local { dir } => format!("local\t{}", normalize_dir(dir)),
            ProjectTarget::Ssh { host, dir } => format!(
                "ssh\t{}\t{}\t{}\t{}",
                host.user.as_deref().unwrap_or(""),
                host.host.to_ascii_lowercase(),
                host.port.map(|p| p.to_string()).unwrap_or_default(),
                normalize_dir(dir)
            ),
        }
    }
}

fn normalize_dir(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse `[user@]host[:port]/path` or `ssh://[user@]host[:port]/path`.
pub fn parse_target(input: &str) -> Result<ProjectTarget, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty remote target".to_string());
    }

    let (authority, dir) = if let Some(rest) = input.strip_prefix("ssh://") {
        let (authority, path) = rest
            .split_once('/')
            .ok_or_else(|| format!("missing absolute path in {input:?}"))?;
        (authority, format!("/{path}"))
    } else {
        let idx = input.find('/').ok_or_else(|| {
            format!("missing absolute path in {input:?} (expected [user@]host[:port]/path)")
        })?;
        (&input[..idx], input[idx..].to_string())
    };

    if authority.is_empty() {
        return Err(format!("missing host in {input:?}"));
    }
    if !dir.starts_with('/') {
        return Err(format!("remote path must be absolute: {dir:?}"));
    }
    if dir.chars().any(|c| c == '\0' || c.is_control()) {
        return Err("remote path contains control characters".to_string());
    }

    let host = parse_authority(authority)?;
    Ok(ProjectTarget::ssh(host, normalize_dir(&dir)))
}

fn parse_authority(authority: &str) -> Result<SshTarget, String> {
    let (user, hostport) = match authority.rsplit_once('@') {
        Some((user, host)) => {
            if user.is_empty() {
                return Err("empty user name".to_string());
            }
            if user
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '@' || c == '/')
            {
                return Err(format!("invalid user name {user:?}"));
            }
            (Some(user.to_string()), host)
        }
        None => (None, authority),
    };

    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| "unterminated IPv6 address".to_string())?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let port = if tail.is_empty() {
            None
        } else {
            let port = tail
                .strip_prefix(':')
                .ok_or_else(|| format!("invalid host {hostport:?}"))?;
            Some(parse_port(port)?)
        };
        (host.to_string(), port)
    } else if let Some((host, port)) = hostport.rsplit_once(':') {
        if host.contains(':') {
            return Err("IPv6 addresses must be enclosed in brackets".to_string());
        }
        (host.to_string(), Some(parse_port(port)?))
    } else {
        (hostport.to_string(), None)
    };

    if host.is_empty() {
        return Err("empty host".to_string());
    }
    if host.chars().any(|c| {
        c.is_control() || c.is_whitespace() || c == '/' || c == '@' || c == '[' || c == ']'
    }) {
        return Err(format!("invalid host {host:?}"));
    }

    Ok(SshTarget { user, host, port })
}

fn parse_port(port: &str) -> Result<u16, String> {
    let parsed: u16 = port.parse().map_err(|_| format!("invalid port {port:?}"))?;
    if parsed == 0 {
        return Err("port must be between 1 and 65535".to_string());
    }
    Ok(parsed)
}

/// POSIX shell single-quote escaping.
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Remote command that changes into `dir` and execs a fixed program.
pub fn remote_command(dir: &str, program: &str, args: &[String]) -> String {
    let mut command = format!(
        "cd -- {} && exec {}",
        shell_quote(dir),
        shell_quote(program)
    );
    for arg in args {
        command.push(' ');
        command.push_str(&shell_quote(arg));
    }
    command
}

/// Remote command opening the user's login shell in `dir`.
pub fn remote_shell_command(dir: &str) -> String {
    format!("cd -- {} && exec \"${{SHELL:-sh}}\"", shell_quote(dir))
}

/// SSH client argv for an interactive session with a remote PTY.
pub fn ssh_argv(
    target: &SshTarget,
    remote_command: &str,
    forwards: &[(String, String)],
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "-tt".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=15".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ];
    for (remote_socket, local_socket) in forwards {
        args.push("-R".to_string());
        args.push(format!("{remote_socket}:{local_socket}"));
    }
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(user) = &target.user {
        args.push("-l".to_string());
        args.push(user.clone());
    }
    args.extend(extra_args.iter().cloned());
    args.push(target.host.clone());
    args.push(remote_command.to_string());
    args
}

/// SSH client argv for a non-interactive best-effort command (probes,
/// speck apply). Never prompts: failure is silently ignored by callers.
pub fn ssh_probe_argv(
    target: &SshTarget,
    remote_command: &str,
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(user) = &target.user {
        args.push("-l".to_string());
        args.push(user.clone());
    }
    args.extend(extra_args.iter().cloned());
    args.push(target.host.clone());
    args.push(remote_command.to_string());
    args
}

/// Sibling directory for a worktree. String-based so it also works for
/// remote POSIX paths that do not exist on the local filesystem.
pub fn sibling_dir(dir: &str, name: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        return format!("./{name}");
    }
    match trimmed.rfind('/') {
        Some(0) => format!("/{name}"),
        Some(idx) => format!("{}/{}", &trimmed[..idx], name),
        None => format!("./{name}"),
    }
}

static SSH_SUFFIX: OnceLock<Vec<String>> = OnceLock::new();

/// Parse the `--ssh-suffix` CLI values into individual ssh argv tokens.
/// Each occurrence is split with shell-like quoting, so
/// `--ssh-suffix "-o 'ProxyCommand=ssh jump nc %h %p'"` works as expected.
pub fn parse_ssh_suffix(values: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for value in values {
        out.extend(split_shell_words(value)?);
    }
    Ok(out)
}

pub fn set_ssh_suffix(values: &[String]) -> Result<(), String> {
    let parsed = parse_ssh_suffix(values)?;
    let _ = SSH_SUFFIX.set(parsed);
    Ok(())
}

/// Extra arguments appended to every ssh invocation.
pub fn ssh_suffix() -> &'static [String] {
    SSH_SUFFIX.get().map(Vec::as_slice).unwrap_or(&[])
}

fn split_shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => current.push(ch),
                        None => return Err("unterminated single quote in --ssh-suffix".to_string()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch) => current.push(ch),
                            None => return Err("unterminated escape in --ssh-suffix".to_string()),
                        },
                        Some(ch) => current.push(ch),
                        None => return Err("unterminated double quote in --ssh-suffix".to_string()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(ch) => current.push(ch),
                    None => return Err("unterminated escape in --ssh-suffix".to_string()),
                }
            }
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                started = true;
                current.push(c);
            }
        }
    }
    if started {
        words.push(current);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bare_target() {
        assert_eq!(
            parse_target("buildbox/srv/app").unwrap(),
            ProjectTarget::ssh(
                SshTarget {
                    user: None,
                    host: "buildbox".into(),
                    port: None,
                },
                "/srv/app"
            )
        );
    }

    #[test]
    fn test_parse_user_port_target() {
        assert_eq!(
            parse_target("deploy@buildbox:2222/srv/app").unwrap(),
            ProjectTarget::ssh(
                SshTarget {
                    user: Some("deploy".into()),
                    host: "buildbox".into(),
                    port: Some(2222),
                },
                "/srv/app"
            )
        );
    }

    #[test]
    fn test_parse_ssh_uri() {
        assert_eq!(
            parse_target("ssh://deploy@buildbox:2222/srv/app").unwrap(),
            parse_target("deploy@buildbox:2222/srv/app").unwrap()
        );
    }

    #[test]
    fn test_parse_ipv6() {
        assert_eq!(
            parse_target("deploy@[::1]:22/srv/app").unwrap(),
            ProjectTarget::ssh(
                SshTarget {
                    user: Some("deploy".into()),
                    host: "::1".into(),
                    port: Some(22),
                },
                "/srv/app"
            )
        );
        assert!(parse_target("deploy@::1/srv/app").is_err());
    }

    #[test]
    fn test_parse_rejects_bad_targets() {
        assert!(parse_target("").is_err());
        assert!(parse_target("host").is_err());
        assert!(parse_target("hostrelative").is_err());
        assert!(parse_target("host:0/srv").is_err());
        assert!(parse_target("host:70000/srv").is_err());
        assert!(parse_target("@host/srv").is_err());
        assert!(parse_target("host:/srv").is_err());
        assert!(parse_target("ho st/srv").is_err());
    }

    #[test]
    fn test_parse_trailing_slash_and_identity() {
        let a = parse_target("host/srv/app/").unwrap();
        let b = parse_target("HOST/srv/app").unwrap();
        assert_eq!(a.dir(), "/srv/app");
        assert_eq!(a.identity_key(), b.identity_key());
    }

    #[test]
    fn test_display_location() {
        let t = parse_target("deploy@[::1]:2222/srv/app").unwrap();
        assert_eq!(t.display_location(), "ssh://deploy@[::1]:2222/srv/app");
        assert_eq!(ProjectTarget::local("/tmp/x").display_location(), "/tmp/x");
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    #[test]
    fn test_remote_command_quotes_everything() {
        let command = remote_command(
            "/srv/my project",
            "zerostack",
            &["--worktree".into(), "wt-a'b".into()],
        );
        assert_eq!(
            command,
            "cd -- '/srv/my project' && exec 'zerostack' '--worktree' 'wt-a'\\''b'"
        );
    }

    #[test]
    fn test_ssh_argv_order_and_forwarding() {
        let target = SshTarget {
            user: Some("deploy".into()),
            host: "buildbox".into(),
            port: Some(2222),
        };
        let args = ssh_argv(
            &target,
            "echo hi",
            &[("/tmp/r.sock".into(), "/tmp/l.sock".into())],
            &["-o".into(), "Foo=bar".into()],
        );
        let pos = |needle: &str| args.iter().position(|a| a == needle).unwrap();
        assert!(pos("-R") < pos("-p"));
        assert!(pos("-p") < pos("-l"));
        assert!(pos("-l") < pos("buildbox"));
        assert_eq!(args[pos("-R") + 1], "/tmp/r.sock:/tmp/l.sock");
        assert_eq!(args[pos("-l") + 1], "deploy");
        assert_eq!(args[pos("buildbox") + 1], "echo hi");
        assert!(args.windows(2).any(|w| w == ["-o", "Foo=bar"]));
    }

    #[test]
    fn test_ssh_probe_argv_is_batch_mode() {
        let target = SshTarget {
            user: None,
            host: "buildbox".into(),
            port: None,
        };
        let args = ssh_probe_argv(&target, "true", &[]);
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(!args.contains(&"-tt".to_string()));
    }

    #[test]
    fn test_sibling_dir() {
        assert_eq!(sibling_dir("/tmp/myproj", "wt-foo"), "/tmp/wt-foo");
        assert_eq!(sibling_dir("/myproj/", "wt-foo"), "/wt-foo");
        assert_eq!(sibling_dir("project", "wt-foo"), "./wt-foo");
        assert_eq!(sibling_dir("/", "wt-foo"), "./wt-foo");
    }

    #[test]
    fn test_split_shell_words() {
        assert_eq!(
            split_shell_words("-o Foo=bar").unwrap(),
            vec!["-o", "Foo=bar"]
        );
        assert_eq!(
            split_shell_words("-o 'A B' \"C D\"").unwrap(),
            vec!["-o", "A B", "C D"]
        );
        assert_eq!(split_shell_words("").unwrap(), Vec::<String>::new());
        assert_eq!(split_shell_words("''").unwrap(), vec![""]);
        assert!(split_shell_words("'oops").is_err());
    }

    #[test]
    fn test_parse_ssh_suffix_combines_occurrences() {
        assert_eq!(
            parse_ssh_suffix(&["-o A=1".into(), "-v -o B=2".into()]).unwrap(),
            vec!["-o", "A=1", "-v", "-o", "B=2"]
        );
    }
}
