//! Where on a host: `[user@]host:/path` (as scp writes it) or
//! `ssh://[user@]host[:port]/path`.

use std::fmt;

use crate::{SshError, SshResult};

/// A directory on an SSH host: who to log in as, where, and the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    /// `user@host`, `host`, or a `Host` alias from `~/.ssh/config`.
    pub host: String,
    pub port: Option<u16>,
    /// Absolute (`/data/images`) or home-relative (`~/images`).
    pub path: String,
}

impl SshTarget {
    /// Read `[user@]host:/path` or `ssh://[user@]host[:port]/path`;
    /// `invalid_remote` for anything else.
    pub fn parse(text: &str) -> SshResult<Self> {
        let text = text.trim();
        let invalid = |why: &str| {
            SshError::new(
                "invalid_remote",
                format!("{why}: {text:?}; expected user@host:/path or ssh://user@host:port/path"),
            )
        };
        let (host, port, path) = if let Some(rest) = text.strip_prefix("ssh://") {
            let (authority, path) =
                rest.find('/').map(|slash| (&rest[..slash], &rest[slash..])).ok_or_else(|| invalid("missing path"))?;
            // `ssh://host/~/x` names a home-relative path.
            let path = path.strip_prefix("/~").map(|rest| format!("~{rest}")).unwrap_or_else(|| path.to_string());
            match authority.rsplit_once(':') {
                Some((host, port)) => {
                    (host.to_string(), Some(port.parse::<u16>().map_err(|_| invalid("invalid port"))?), path)
                }
                None => (authority.to_string(), None, path),
            }
        } else {
            let (host, path) = text.split_once(':').ok_or_else(|| invalid("missing `:` between host and path"))?;
            (host.to_string(), None, path.to_string())
        };
        if !valid_host(&host) {
            return Err(invalid("invalid host"));
        }
        let home = path == "~" || path.starts_with("~/");
        if !(path.starts_with('/') || home) || path.contains('\0') || path.contains('\n') {
            return Err(invalid("the path must be absolute or start with ~/"));
        }
        let path = if path.len() > 1 { path.trim_end_matches('/').to_string() } else { path };
        Ok(Self { host, port, path })
    }

    /// The path quoted for a POSIX shell, keeping a leading `~` expandable.
    pub fn quoted_path(&self) -> String {
        quoted_dir(&self.path)
    }
}

/// `user@host` with letters, digits and `._-`; never starting with `-`, so
/// it can not be read as an `ssh` option.
fn valid_host(host: &str) -> bool {
    let (user, name) = host.rsplit_once('@').unwrap_or(("", host));
    let ok = |part: &str| part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    !name.is_empty()
        && !host.starts_with('-')
        && !user.starts_with('-')
        && !name.starts_with('-')
        && ok(name)
        && ok(user)
        && host.matches('@').count() <= 1
}

/// `path` quoted for a POSIX shell, a leading `~` kept outside the quotes
/// so the shell expands it: `~` alone, a folder in the home as `~/…`; any
/// other `~` (another user's home, `~user`) is quoted whole, never expanded.
/// The one rule remote commands name folders by.
pub fn quoted_dir(path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~/{}", shell_quote(rest.trim_start_matches('/'))),
        _ => shell_quote(path),
    }
}

/// Single-quote `text` for a POSIX shell: the one implementation.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

impl fmt::Display for SshTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.port {
            Some(port) => {
                let path = if self.path.starts_with('~') { format!("/{}", self.path) } else { self.path.clone() };
                write!(formatter, "ssh://{}:{port}{path}", self.host)
            }
            None => write!(formatter, "{}:{}", self.host, self.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder in the home keeps its `~` for the shell; others are quoted whole.
    #[test]
    fn a_folder_in_the_home_is_gone_to_by_its_tilde() {
        assert_eq!(quoted_dir("~"), "~");
        assert_eq!(quoted_dir("~/work"), "~/'work'");
        assert_eq!(quoted_dir("~/my 'dir'"), r"~/'my '\''dir'\'''");
        assert_eq!(quoted_dir("/srv/a b"), "'/srv/a b'");
        // `~user` is not the home: quoted as it is.
        assert_eq!(quoted_dir("~other/x"), "'~other/x'");
    }

    #[test]
    fn shell_quoting_survives_empty_text_quotes_and_spaces() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote("$(rm -rf ~) `x` \\ \"q\""), "'$(rm -rf ~) `x` \\ \"q\"'");
    }

    #[test]
    fn scp_and_url_forms() {
        let scp = SshTarget::parse(" me@gpu1:/data/images/ ").unwrap();
        assert_eq!((scp.host.as_str(), scp.port, scp.path.as_str()), ("me@gpu1", None, "/data/images"));
        assert_eq!(scp.to_string(), "me@gpu1:/data/images");
        let url = SshTarget::parse("ssh://me@10.0.0.5:2222/srv/data").unwrap();
        assert_eq!((url.host.as_str(), url.port, url.path.as_str()), ("me@10.0.0.5", Some(2222), "/srv/data"));
        assert_eq!(url.to_string(), "ssh://me@10.0.0.5:2222/srv/data");
        let home = SshTarget::parse("ssh://gpu/~/images").unwrap();
        assert_eq!(home.path, "~/images");
        assert_eq!(SshTarget::parse(&home.to_string()).unwrap(), home);
        assert_eq!(SshTarget::parse("gpu:~").unwrap().quoted_path(), "~");
    }

    #[test]
    fn hosts_that_could_be_options_or_paths_that_are_relative_are_refused() {
        for text in
            ["-oProxyCommand=x:/a", "me@-x:/a", "gpu:data", "gpu", "a b:/x", "ssh://gpu:port/x", ":/x", "a@b@c:/x"]
        {
            assert_eq!(SshTarget::parse(text).unwrap_err().code, "invalid_remote", "{text}");
        }
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(SshTarget::parse("h:/a b/it's").unwrap().quoted_path(), r"'/a b/it'\''s'");
        assert_eq!(SshTarget::parse("h:~/my data").unwrap().quoted_path(), "~/'my data'");
    }
}
