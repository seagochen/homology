//! The SSH servers the user saved, shared by every program built on
//! Homology (#17): a server added in one is there in the others, and so is
//! the login key a program set up for it ([`keys_dir`]).
//!
//! A server is what the user fills in (a name, the host, the user, the
//! port) with the fields a list kept on several computers needs: when it
//! was made and changed, on which computer (`origin`, the caller's), and
//! when it was removed (a removed server stays, as a tombstone). What only
//! one program keeps of a server (AnnotaryCat's data directory) is that
//! program's, kept by the server's id. Fields a record has that this does
//! not know (a newer program's) are kept as they are.
//!
//! The list is `servers.json` in the shared folder ([`shared_dir`]),
//! changed under a [`LockFile`](homology_sys::LockFile) so that two programs
//! saving at once both keep their change, and written whole or not at all.
//! No password or key is ever in it.

use std::path::{Path, PathBuf};

use homology_sys as sys;
use serde_json::{json, Map, Value};

use crate::target::SshTarget;
use crate::{SshError, SshResult};

/// The variable naming the shared folder instead of the user's own (a
/// test's, a sandboxed program's).
pub const HOME_VARIABLE: &str = "HOMOLOGY_HOME";

/// The list's file in the shared folder, and its schema.
const FILE: &str = "servers.json";
const SCHEMA: u64 = 1;

/// The longest name a server may have, in characters.
const LONGEST_NAME: usize = 120;

/// The folder the programs share: [`HOME_VARIABLE`] when it is an absolute
/// path, else `homology` in the user's configuration folder; none without
/// either.
pub fn shared_dir() -> Option<PathBuf> {
    let named = std::env::var_os(HOME_VARIABLE).map(PathBuf::from).filter(|path| path.is_absolute());
    named.or_else(|| sys::config_home().map(|folder| folder.join("homology")))
}

/// The saved servers' file in the shared folder `shared`.
pub fn file(shared: &Path) -> PathBuf {
    shared.join(FILE)
}

/// The login keys' folder in the shared folder `shared`, for
/// [`Context::set_keys_dir`](crate::Context::set_keys_dir).
pub fn keys_dir(shared: &Path) -> PathBuf {
    shared.join("ssh-keys")
}

/// A server as the user fills it in: no password or key, ever. `id` is
/// empty for one not saved yet; `user` is empty to leave it to `ssh`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerInput {
    pub id: String,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
}

impl ServerInput {
    /// The SSH target of `path` on this server: `invalid_server` without a
    /// name (up to 120 characters) or host, with a host holding `@`, or a
    /// port of 0.
    pub fn target(&self, path: &str) -> SshResult<SshTarget> {
        let named = !self.name.trim().is_empty() && self.name.chars().count() <= LONGEST_NAME;
        if !named || self.host.trim().is_empty() || self.host.contains('@') || self.port == 0 {
            return Err(SshError::new(
                "invalid_server",
                format!("a name (up to {LONGEST_NAME} characters), host and nonzero SSH port are required"),
            ));
        }
        let host = if self.user.is_empty() { self.host.clone() } else { format!("{}@{}", self.user, self.host) };
        let path = if path.starts_with('~') { format!("/{path}") } else { path.to_string() };
        SshTarget::parse(&format!("ssh://{host}:{}{path}", self.port)).map_err(|_| {
            SshError::new("invalid_server", "invalid SSH host, user or path; use an absolute path or ~/folder")
        })
    }

    /// Whether this is the server `target` names: the same host and port,
    /// and the same user unless either leaves it to `ssh`.
    pub fn serves(&self, target: &SshTarget) -> bool {
        let (user, host) = target.host.rsplit_once('@').unwrap_or(("", &target.host));
        self.host.eq_ignore_ascii_case(host)
            && (self.user.is_empty() || user.is_empty() || self.user == user)
            && self.port == target.port.unwrap_or(22)
    }

    /// Whether `target` names this server's user as well, not only its host.
    fn names_user(&self, target: &SshTarget) -> bool {
        target.host.rsplit_once('@').map_or(self.user.is_empty(), |(user, _)| user == self.user)
    }
}

/// A saved server: what the user filled in and the fields every record
/// has; `deleted_at` once removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub connection: ServerInput,
    pub created_at: String,
    pub updated_at: String,
    /// The computer it was last saved on, as the program names it.
    pub origin: String,
    pub deleted_at: Option<String>,
    /// The fields of the record this does not know, kept as they are.
    pub extra: Map<String, Value>,
}

/// The fields of a record [`Server`] knows.
const KNOWN: &[&str] = &["id", "name", "host", "user", "port", "created_at", "updated_at", "origin", "deleted_at"];

impl Server {
    /// The record as the file holds it.
    pub fn to_json(&self) -> Value {
        let input = &self.connection;
        let mut record = self.extra.clone();
        let known = json!({
            "id": input.id, "name": input.name, "host": input.host, "user": input.user, "port": input.port,
            "created_at": self.created_at, "updated_at": self.updated_at, "origin": self.origin,
            "deleted_at": self.deleted_at,
        });
        if let Value::Object(known) = known {
            record.extend(known);
        }
        Value::Object(record)
    }

    /// A record of the file; none without an id, host or port.
    pub fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let port = value.get("port").and_then(Value::as_u64).and_then(|port| u16::try_from(port).ok())?;
        let connection =
            ServerInput { id: text("id"), name: text("name"), host: text("host"), user: text("user"), port };
        if connection.id.is_empty() || connection.host.is_empty() {
            return None;
        }
        let extra = value
            .as_object()
            .map(|record| record.iter().filter(|(key, _)| !KNOWN.contains(&key.as_str())))
            .into_iter()
            .flatten()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Some(Self {
            connection,
            created_at: text("created_at"),
            updated_at: text("updated_at"),
            origin: text("origin"),
            deleted_at: value.get("deleted_at").and_then(Value::as_str).map(str::to_string),
            extra,
        })
    }
}

/// Every record of the file at `path`, tombstones too; none when there is
/// no file yet (`server_store_read_failed` when it cannot be read,
/// `unsupported_server_schema` for a schema this does not know).
pub fn all(path: &Path) -> SshResult<Vec<Server>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(SshError::new("server_store_read_failed", format!("{}: {error}", path.display()))),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| SshError::new("server_store_read_failed", format!("{}: {error}", path.display())))?;
    if value.get("schema_version").and_then(Value::as_u64) != Some(SCHEMA) {
        return Err(SshError::new("unsupported_server_schema", "unsupported server list schema"));
    }
    let records = value.get("servers").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    Ok(records.iter().filter_map(Server::from_json).collect())
}

/// The saved servers, removed ones left out.
pub fn list(path: &Path) -> SshResult<Vec<Server>> {
    Ok(all(path)?.into_iter().filter(|server| server.deleted_at.is_none()).collect())
}

/// The saved server `wanted` names: its id, or with `by_name` its name in
/// any case too; a removed one is not found (`server_not_found`).
pub fn find(path: &Path, wanted: &str, by_name: bool) -> SshResult<Server> {
    Ok(found(&mut all(path)?, wanted, by_name)?.clone())
}

/// [`find`] among `servers`, to change it.
fn found<'a>(servers: &'a mut [Server], wanted: &str, by_name: bool) -> SshResult<&'a mut Server> {
    servers
        .iter_mut()
        .find(|server| {
            server.deleted_at.is_none()
                && (server.connection.id == wanted || by_name && server.connection.name.eq_ignore_ascii_case(wanted))
        })
        .ok_or_else(|| SshError::new("server_not_found", format!("no saved server {wanted:?}")))
}

/// The saved server `target` is on, if any: the one that names its user
/// too when several serve the host; still more than one,
/// `server_ambiguous`, rather than one picked silently.
pub fn serving(servers: Vec<Server>, target: &SshTarget) -> SshResult<Option<Server>> {
    let mut found: Vec<Server> = servers.into_iter().filter(|server| server.connection.serves(target)).collect();
    if found.len() > 1 {
        let exact: Vec<Server> = found.iter().filter(|server| server.connection.names_user(target)).cloned().collect();
        if exact.len() == 1 {
            return Ok(exact.into_iter().next());
        }
        let names: Vec<&str> = found.iter().map(|server| server.connection.name.as_str()).collect();
        return Err(SshError::new(
            "server_ambiguous",
            format!("several saved servers are {}: {}; choose one", target.host, names.join(", ")),
        ));
    }
    Ok(found.pop())
}

/// `input` as it is saved: trimmed, and a server [`ServerInput::target`]
/// takes.
pub fn normalized(mut input: ServerInput) -> SshResult<ServerInput> {
    input.name = input.name.trim().to_string();
    input.host = input.host.trim().to_string();
    input.user = input.user.trim().to_string();
    input.target("~")?;
    Ok(input)
}

/// Save `input` (normalized), new when it has no id, saved on the computer
/// `origin`; `server_name_exists` when another server has its name.
pub fn save(path: &Path, input: ServerInput, origin: &str) -> SshResult<Server> {
    let mut input = normalized(input)?;
    changing(path, |servers| {
        if servers.iter().any(|server| {
            server.deleted_at.is_none()
                && server.connection.id != input.id
                && server.connection.name.eq_ignore_ascii_case(&input.name)
        }) {
            return Err(SshError::new("server_name_exists", "a server with this name already exists"));
        }
        let now = crate::keys::now();
        if input.id.is_empty() {
            input.id = uuid::Uuid::now_v7().to_string();
            let server = Server {
                connection: input.clone(),
                created_at: now.clone(),
                updated_at: now,
                origin: origin.to_string(),
                deleted_at: None,
                extra: Map::new(),
            };
            servers.push(server.clone());
            return Ok(server);
        }
        let server = found(servers, &input.id, false)?;
        server.connection = input.clone();
        server.updated_at = now;
        server.origin = origin.to_string();
        Ok(server.clone())
    })
}

/// Remove a saved server: a tombstone, `deleted_at` set.
pub fn remove(path: &Path, id: &str) -> SshResult<()> {
    changing(path, |servers| {
        let server = found(servers, id, false)?;
        let now = crate::keys::now();
        server.updated_at = now.clone();
        server.deleted_at = Some(now);
        Ok(())
    })
}

/// Add `servers` (a program's own list, moved here) as they are, ids and
/// tombstones kept: those whose id the file has already are left out, so
/// moving them again changes nothing. How many were added.
pub fn import(path: &Path, servers: Vec<Server>) -> SshResult<usize> {
    changing(path, |saved| {
        let before = saved.len();
        for server in servers {
            if !saved.iter().any(|other| other.connection.id == server.connection.id) {
                saved.push(server);
            }
        }
        Ok(saved.len() - before)
    })
}

/// Change the records of the file at `path` with `change`, holding its
/// lock across programs, and write them when it succeeds.
fn changing<T>(path: &Path, change: impl FnOnce(&mut Vec<Server>) -> SshResult<T>) -> SshResult<T> {
    let failed =
        |error: std::io::Error| SshError::new("server_store_write_failed", format!("{}: {error}", path.display()));
    let _lock = sys::LockFile::take(path).map_err(failed)?;
    let mut servers = all(path)?;
    let answer = change(&mut servers)?;
    let records: Vec<Value> = servers.iter().map(Server::to_json).collect();
    let text = serde_json::to_string_pretty(&json!({"schema_version": SCHEMA, "servers": records})).unwrap_or_default();
    sys::atomic_write_bytes(path, (text + "\n").as_bytes()).map_err(failed)?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server named `name` on `host`, port 22.
    fn input(name: &str, host: &str, user: &str) -> ServerInput {
        ServerInput { id: String::new(), name: name.into(), host: host.into(), user: user.into(), port: 22 }
    }

    #[test]
    fn servers_are_saved_changed_found_and_removed_as_tombstones() {
        let folder = tempfile::tempdir().unwrap();
        let path = file(folder.path());
        assert!(list(&path).unwrap().is_empty(), "no file yet: none");
        let gpu = save(&path, input("  GPU  ", " gpu.lan ", "me"), "pc-1").unwrap();
        assert_eq!((gpu.connection.name.as_str(), gpu.connection.host.as_str()), ("GPU", "gpu.lan"));
        assert!(!gpu.connection.id.is_empty() && gpu.deleted_at.is_none() && gpu.origin == "pc-1");
        let error = save(&path, input("gpu", "other", ""), "pc-1").unwrap_err();
        assert_eq!(error.code, "server_name_exists", "names are told apart in any case");
        let mut changed = gpu.connection.clone();
        changed.port = 2222;
        let changed = save(&path, changed, "pc-2").unwrap();
        assert_eq!((changed.connection.port, changed.created_at.as_str()), (2222, gpu.created_at.as_str()));
        assert_eq!(changed.origin, "pc-2");
        assert_eq!(find(&path, "gpu", true).unwrap().connection.id, gpu.connection.id);
        assert_eq!(find(&path, "gpu", false).unwrap_err().code, "server_not_found", "by id only");
        remove(&path, &gpu.connection.id).unwrap();
        assert!(list(&path).unwrap().is_empty());
        let tombstones = all(&path).unwrap();
        assert!(tombstones.len() == 1 && tombstones[0].deleted_at.is_some());
        assert_eq!(remove(&path, &gpu.connection.id).unwrap_err().code, "server_not_found");
        // Its name is free again.
        save(&path, input("GPU", "new.lan", ""), "pc-1").unwrap();
    }

    #[test]
    fn what_a_server_lacks_is_said() {
        for (name, host, port) in [("", "h", 22), ("n", "", 22), ("n", "me@h", 22), ("n", "h", 0)] {
            let server = ServerInput { port, ..input(name, host, "") };
            assert_eq!(normalized(server).unwrap_err().code, "invalid_server", "{name:?} {host:?} {port}");
        }
        let long = input(&"x".repeat(LONGEST_NAME + 1), "h", "");
        assert_eq!(normalized(long).unwrap_err().code, "invalid_server");
        let target = input("n", "gpu", "me").target("~/data").unwrap();
        assert_eq!((target.host.as_str(), target.port, target.path.as_str()), ("me@gpu", Some(22), "~/data"));
    }

    #[test]
    fn a_target_finds_its_server_and_several_are_never_picked_from_silently() {
        let server = |name: &str, user: &str, port: u16| Server {
            connection: ServerInput { id: name.into(), port, ..input(name, "GPU.lan", user) },
            created_at: String::new(),
            updated_at: String::new(),
            origin: String::new(),
            deleted_at: None,
            extra: Map::new(),
        };
        let target = |text: &str| SshTarget::parse(text).unwrap();
        let one = vec![server("a", "me", 22)];
        assert_eq!(serving(one.clone(), &target("gpu.lan:/x")).unwrap().unwrap().connection.id, "a", "no user: ssh's");
        assert!(serving(one.clone(), &target("you@gpu.lan:/x")).unwrap().is_none(), "another user");
        assert!(serving(one, &target("ssh://me@gpu.lan:2222/x")).unwrap().is_none(), "another port");
        let two = vec![server("a", "me", 22), server("b", "", 22)];
        assert_eq!(serving(two.clone(), &target("me@gpu.lan:/x")).unwrap().unwrap().connection.id, "a");
        assert_eq!(serving(two, &target("gpu.lan:/x")).unwrap().unwrap().connection.id, "b", "neither names a user");
        let users = vec![server("a", "me", 22), server("c", "you", 22)];
        assert_eq!(serving(users, &target("gpu.lan:/x")).unwrap_err().code, "server_ambiguous");
    }

    #[test]
    fn another_program_s_fields_and_moved_servers_are_kept() {
        let folder = tempfile::tempdir().unwrap();
        let path = file(folder.path());
        let record = json!({"schema_version": 1, "servers": [{
            "id": "old-1", "name": "Lab", "host": "lab", "user": "", "port": 22, "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z", "origin": "pc", "deleted_at": null, "colour": "teal"
        }]});
        std::fs::write(&path, record.to_string()).unwrap();
        let mut lab = find(&path, "old-1", false).unwrap();
        lab.connection.port = 2200;
        save(&path, lab.connection.clone(), "pc").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"colour\": \"teal\""), "a newer program's field survives: {text}");
        // A program's own list moved here: added once, ids and tombstones kept.
        let moved = |id: &str, deleted: Option<&str>| Server {
            connection: ServerInput { id: id.into(), ..input(id, "h", "") },
            created_at: "c".into(),
            updated_at: "u".into(),
            origin: "o".into(),
            deleted_at: deleted.map(str::to_string),
            extra: Map::new(),
        };
        let list_moved = vec![moved("old-1", None), moved("x", None), moved("gone", Some("d"))];
        assert_eq!(import(&path, list_moved.clone()).unwrap(), 2, "old-1 is there already");
        assert_eq!(import(&path, list_moved).unwrap(), 0, "moved again: nothing changes");
        assert_eq!(find(&path, "old-1", false).unwrap().connection.port, 2200, "the shared one is kept");
        assert_eq!(list(&path).unwrap().len(), 2);
        assert_eq!(all(&path).unwrap().len(), 3);
    }

    #[test]
    fn saves_at_once_keep_every_server_and_an_unknown_schema_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let path = file(folder.path());
        let saving: Vec<_> = (0..8)
            .map(|at| {
                let path = path.clone();
                std::thread::spawn(move || save(&path, input(&format!("s{at}"), "h", ""), "pc").map(|_| ()))
            })
            .collect();
        for one in saving {
            one.join().unwrap().unwrap();
        }
        assert_eq!(list(&path).unwrap().len(), 8);
        assert!(!path.with_file_name("servers.json.lock").exists());
        std::fs::write(&path, "{\"schema_version\": 2, \"servers\": []}").unwrap();
        assert_eq!(list(&path).unwrap_err().code, "unsupported_server_schema");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(list(&path).unwrap_err().code, "server_store_read_failed");
    }

    #[test]
    fn the_shared_folder_holds_the_list_and_the_keys() {
        let shared = Path::new("/shared");
        assert_eq!(file(shared), PathBuf::from("/shared/servers.json"));
        assert_eq!(keys_dir(shared), PathBuf::from("/shared/ssh-keys"));
        if std::env::var_os(HOME_VARIABLE).is_none() {
            assert_eq!(shared_dir(), sys::config_home().map(|folder| folder.join("homology")));
        }
    }
}
