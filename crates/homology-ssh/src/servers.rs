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
//!
//! Each change is made once, on what its user saw (#18): a record has a
//! `revision`, one up at each change, and a change names the revision it
//! was made on; when the record changed since (in another program),
//! `server_changed`, and nothing is written. The file has a revision too,
//! one up at each write, for a program to tell the list changed.

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
pub const LONGEST_NAME: usize = 120;

/// The macOS App Group the programs built on Homology share their servers,
/// login keys and known hosts in (#19, homology#29): the team's prefix, so it
/// needs no registration. A program declares it in its
/// `com.apple.security.application-groups` entitlement and passes it to
/// [`shared_dir_in`].
pub const APP_GROUP: &str = "9D45973HBS.io.github.seagochen.homology";

/// The file in the shared folder holding this computer's device id
/// ([`origin`]).
const DEVICE_FILE: &str = "device-id";

/// What is wrong with a server as filled in ([`ServerInput::check`]), in
/// the order a form checks it: the program words it, by the field it is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// No name.
    NameMissing,
    /// A name of more than [`LONGEST_NAME`] characters.
    NameTooLong,
    /// No host.
    HostMissing,
    /// A host with `@` in it: the user goes in its own field.
    HostHasAt,
    /// SSH port 0.
    PortZero,
    /// A host or user `ssh` cannot take.
    AddressInvalid,
}

impl Invalid {
    /// The form's field it is about: `name`, `host` or `port`.
    pub fn field(self) -> &'static str {
        match self {
            Invalid::NameMissing | Invalid::NameTooLong => "name",
            Invalid::HostMissing | Invalid::HostHasAt | Invalid::AddressInvalid => "host",
            Invalid::PortZero => "port",
        }
    }

    /// Whether it is about what was typed rather than a field left empty.
    pub fn typed(self) -> bool {
        !matches!(self, Invalid::NameMissing | Invalid::HostMissing)
    }
}

/// This computer's device id, as the saved servers record where they were
/// made and changed (`origin`; the programs decided, homology#29): kept in
/// the shared folder `shared`, made (a UUID) the first time. Every program
/// on this computer writes the same one; a hostname, which can change, is
/// not used.
pub fn origin(shared: &Path) -> SshResult<String> {
    let file = shared.join(DEVICE_FILE);
    let failed = |error: std::io::Error| SshError::new("server_store_write_failed", format!("{}: {error}", file.display()));
    let read = |file: &Path| {
        std::fs::read_to_string(file).ok().map(|text| text.trim().to_string()).filter(|id| !id.is_empty())
    };
    if let Some(id) = read(&file) {
        return Ok(id);
    }
    std::fs::create_dir_all(shared).map_err(failed)?;
    // Another program may make it at the same time: under the list's lock.
    let _lock = sys::LockFile::take(&self::file(shared)).map_err(failed)?;
    if let Some(id) = read(&file) {
        return Ok(id);
    }
    let id = uuid::Uuid::now_v7().to_string();
    sys::atomic_write_bytes(&file, id.as_bytes()).map_err(failed)?;
    Ok(id)
}

/// The folder the programs share: [`HOME_VARIABLE`] when it is an absolute
/// path, else `homology` in the user's configuration folder; none without
/// either. A program in an App Group asks [`shared_dir_in`] instead.
pub fn shared_dir() -> Option<PathBuf> {
    shared_dir_in(None)
}

/// [`shared_dir`] for a program in the macOS App Group `group` (#19,
/// `<team id>.<name>`, declared in its `com.apple.security.application-groups`
/// entitlement, every program of the group signed by that team): `homology`
/// in the group's container, which sandboxed programs and others share,
/// when it can be made and written; else as [`shared_dir`]. Elsewhere than
/// macOS the group is not asked about. [`HOME_VARIABLE`] still comes first.
pub fn shared_dir_in(group: Option<&str>) -> Option<PathBuf> {
    // Made, and its list's lock taken: written there, then.
    let usable = |folder: &Path| std::fs::create_dir_all(folder).is_ok() && sys::LockFile::take(&file(folder)).is_ok();
    shared_dir_with(group.filter(|_| cfg!(target_os = "macos")), &|name| std::env::var_os(name), &usable)
}

/// [`shared_dir_in`] with the environment read through `variable`, and
/// whether a folder can be used told by `usable`.
fn shared_dir_with(
    group: Option<&str>,
    variable: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    usable: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(named) = variable(HOME_VARIABLE).map(PathBuf::from).filter(|path| path.is_absolute()) {
        return Some(named);
    }
    let grouped = group.and_then(|group| group_container(group, variable)).map(|folder| folder.join("homology"));
    if let Some(grouped) = grouped.filter(|folder| usable(folder)) {
        return Some(grouped);
    }
    sys::config_home().map(|folder| folder.join("homology"))
}

/// The container of the App Group `group` (a name with no `/`): in the
/// user's own `Library/Group Containers`. A sandboxed program's `HOME` is
/// its own container (`~/Library/Containers/<id>/Data`), so the user's
/// home is what comes before that.
fn group_container(group: &str, variable: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if group.is_empty() || group.contains(['/', '\\']) || group.starts_with('.') {
        return None;
    }
    let home = variable("HOME").map(PathBuf::from).filter(|path| path.is_absolute())?;
    let text = home.to_string_lossy().into_owned();
    let user = match text.find("/Library/Containers/") {
        Some(at) => PathBuf::from(&text[..at]),
        None => home,
    };
    Some(user.join("Library").join("Group Containers").join(group))
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
/// `revision` is the record's when it was read (0 for a new one), which a
/// change is made on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerInput {
    pub id: String,
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub revision: u64,
}

impl ServerInput {
    /// The SSH target of `path` on this server: `invalid_server` without a
    /// name (up to 120 characters) or host, with a host holding `@`, or a
    /// port of 0.
    pub fn target(&self, path: &str) -> SshResult<SshTarget> {
        if self.filled().is_err() {
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

    /// The first thing wrong with the server as filled in, as a form
    /// checks it: the name, the host, the port, then whether `ssh` can take
    /// the address. The one rule the programs' forms check by.
    pub fn check(&self) -> Result<(), Invalid> {
        self.filled()?;
        self.target("~").map(drop).map_err(|_| Invalid::AddressInvalid)
    }

    /// [`Self::check`] without the address: every field filled in as it may be.
    fn filled(&self) -> Result<(), Invalid> {
        let name = self.name.trim();
        if name.is_empty() {
            Err(Invalid::NameMissing)
        } else if name.chars().count() > LONGEST_NAME {
            Err(Invalid::NameTooLong)
        } else if self.host.trim().is_empty() {
            Err(Invalid::HostMissing)
        } else if self.host.contains('@') {
            Err(Invalid::HostHasAt)
        } else if self.port == 0 {
            Err(Invalid::PortZero)
        } else {
            Ok(())
        }
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
const KNOWN: &[&str] =
    &["id", "name", "host", "user", "port", "revision", "created_at", "updated_at", "origin", "deleted_at"];

impl Server {
    /// The record as the file holds it.
    pub fn to_json(&self) -> Value {
        let input = &self.connection;
        let mut record = self.extra.clone();
        let known = json!({
            "id": input.id, "name": input.name, "host": input.host, "user": input.user, "port": input.port,
            "revision": input.revision, "created_at": self.created_at, "updated_at": self.updated_at, "origin": self.origin,
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
        let revision = value.get("revision").and_then(Value::as_u64).unwrap_or_default();
        let connection =
            ServerInput { id: text("id"), name: text("name"), host: text("host"), user: text("user"), port, revision };
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
    Ok(read(path)?.1)
}

/// The file's revision, one up at each write: 0 before the first.
pub fn revision(path: &Path) -> SshResult<u64> {
    Ok(read(path)?.0)
}

/// The file's revision and records.
fn read(path: &Path) -> SshResult<(u64, Vec<Server>)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, Vec::new())),
        Err(error) => return Err(SshError::new("server_store_read_failed", format!("{}: {error}", path.display()))),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| SshError::new("server_store_read_failed", format!("{}: {error}", path.display())))?;
    if value.get("schema_version").and_then(Value::as_u64) != Some(SCHEMA) {
        return Err(SshError::new("unsupported_server_schema", "unsupported server list schema"));
    }
    let records = value.get("servers").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let revision = value.get("revision").and_then(Value::as_u64).unwrap_or_default();
    Ok((revision, records.iter().filter_map(Server::from_json).collect()))
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

/// [`find`] among `servers`, to change it as it was at `revision`:
/// `server_changed` when it changed since, `server_not_found` when it is
/// gone (removed meanwhile too).
fn current<'a>(servers: &'a mut [Server], id: &str, revision: u64) -> SshResult<&'a mut Server> {
    let server = found(servers, id, false)?;
    if server.connection.revision != revision {
        return Err(SshError::new(
            "server_changed",
            format!("{:?} was changed in another program since it was read; read it again", server.connection.name),
        ));
    }
    server.connection.revision += 1;
    Ok(server)
}

/// [`find`] among `servers`.
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
/// `origin`; `server_name_exists` when another server has its name, and
/// `server_changed` when it changed since `input` was read.
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
            input.revision = 1;
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
        let server = current(servers, &input.id, input.revision)?;
        input.revision = server.connection.revision;
        server.connection = input.clone();
        server.updated_at = now;
        server.origin = origin.to_string();
        Ok(server.clone())
    })
}

/// Remove the saved server `id` as it was at `revision`: a tombstone,
/// `deleted_at` set; `server_changed` when it changed since.
pub fn remove(path: &Path, id: &str, revision: u64) -> SshResult<()> {
    changing(path, |servers| {
        let server = current(servers, id, revision)?;
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

/// What moving a program's earlier shared folder into another did: the
/// servers added, the login keys moved, the known hosts added.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Moved {
    pub servers: usize,
    pub keys: crate::keys::Adopted,
    pub known_hosts: usize,
}

/// Move what the shared folder `from` holds into `to` (#19: a sandboxed
/// program's own folder, from before its App Group's): its servers, ids
/// and tombstones kept, those `to` has already left out; its login keys
/// ([`crate::keys::adopt`]: `cx`'s keys folder is `to`'s); the hosts its
/// sandbox known hosts name that `to`'s lack. `from` is left as it was, so
/// moving again changes nothing.
pub fn adopt(cx: &crate::Context, from: &Path, to: &Path) -> SshResult<Moved> {
    if from == to || !from.is_dir() {
        return Ok(Moved::default());
    }
    let known: std::collections::HashSet<String> =
        all(&file(to))?.into_iter().map(|server| server.connection.id).collect();
    let new: Vec<Server> =
        all(&file(from))?.into_iter().filter(|server| !known.contains(&server.connection.id)).collect();
    let servers = if new.is_empty() { 0 } else { import(&file(to), new)? };
    let keys = crate::keys::adopt(cx, &keys_dir(from))?;
    let known_hosts =
        crate::sandbox::adopt_known_hosts(&crate::sandbox::known_hosts(from), &crate::sandbox::known_hosts(to))?;
    Ok(Moved { servers, keys, known_hosts })
}

/// Change the records of the file at `path` with `change`, holding its
/// lock across programs and reading them under it, and write them, one
/// revision up, when it succeeds.
fn changing<T>(path: &Path, change: impl FnOnce(&mut Vec<Server>) -> SshResult<T>) -> SshResult<T> {
    let failed =
        |error: std::io::Error| SshError::new("server_store_write_failed", format!("{}: {error}", path.display()));
    let _lock = sys::LockFile::take(path).map_err(failed)?;
    let (revision, mut servers) = read(path)?;
    let answer = change(&mut servers)?;
    let records: Vec<Value> = servers.iter().map(Server::to_json).collect();
    let file = json!({"schema_version": SCHEMA, "revision": revision + 1, "servers": records});
    let text = serde_json::to_string_pretty(&file).unwrap_or_default();
    sys::atomic_write_bytes(path, (text + "\n").as_bytes()).map_err(failed)?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server named `name` on `host`, port 22.
    /// The form's rule (homology#29): the first thing wrong, in order, by
    /// its field; a name counted in characters, not bytes.
    #[test]
    fn a_server_filled_in_is_checked_field_by_field() {
        let checked = |name: &str, host: &str, port: u16| {
            ServerInput { name: name.into(), host: host.into(), port, ..ServerInput::default() }.check()
        };
        assert_eq!(checked("", "", 0), Err(Invalid::NameMissing));
        assert_eq!(checked(&"名".repeat(LONGEST_NAME), "gpu", 22), Ok(()));
        assert_eq!(checked(&"名".repeat(LONGEST_NAME + 1), "gpu", 22), Err(Invalid::NameTooLong));
        assert_eq!(checked("gpu", " ", 22), Err(Invalid::HostMissing));
        assert_eq!(checked("gpu", "me@gpu", 22), Err(Invalid::HostHasAt));
        assert_eq!(checked("gpu", "gpu", 0), Err(Invalid::PortZero));
        assert_eq!(checked("gpu", "-oProxyCommand=x", 22), Err(Invalid::AddressInvalid));
        assert_eq!(Invalid::HostHasAt.field(), "host");
        assert!(Invalid::HostHasAt.typed() && !Invalid::HostMissing.typed());
    }

    /// Every program on the computer records the same device id, made once
    /// and kept in the shared folder.
    #[test]
    fn the_device_id_is_made_once_and_kept() {
        let shared = tempfile::tempdir().unwrap();
        let first = origin(shared.path()).unwrap();
        assert!(uuid::Uuid::parse_str(&first).is_ok(), "{first}");
        assert_eq!(origin(shared.path()).unwrap(), first);
        std::fs::write(shared.path().join(DEVICE_FILE), "kept-id\n").unwrap();
        assert_eq!(origin(shared.path()).unwrap(), "kept-id");
    }

    fn input(name: &str, host: &str, user: &str) -> ServerInput {
        ServerInput {
            id: String::new(),
            name: name.into(),
            host: host.into(),
            user: user.into(),
            port: 22,
            revision: 0,
        }
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
        remove(&path, &gpu.connection.id, changed.connection.revision).unwrap();
        assert!(list(&path).unwrap().is_empty());
        let tombstones = all(&path).unwrap();
        assert!(tombstones.len() == 1 && tombstones[0].deleted_at.is_some());
        let removed = remove(&path, &gpu.connection.id, tombstones[0].connection.revision);
        assert_eq!(removed.unwrap_err().code, "server_not_found");
        // Its name is free again.
        save(&path, input("GPU", "new.lan", ""), "pc-1").unwrap();
    }

    /// A change is made on the revision its user saw, once: one made on an
    /// older one (another program changed the server meanwhile) is refused
    /// and writes nothing (#18).
    #[test]
    fn a_change_made_on_an_older_revision_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let path = file(folder.path());
        assert_eq!(revision(&path).unwrap(), 0);
        let gpu = save(&path, input("GPU", "gpu", ""), "pc").unwrap();
        assert_eq!((gpu.connection.revision, revision(&path).unwrap()), (1, 1));
        // Two programs read it; the first saves its change.
        let (mut first, mut second) = (gpu.connection.clone(), gpu.connection.clone());
        first.port = 2222;
        let saved = save(&path, first, "pc-1").unwrap();
        assert_eq!((saved.connection.revision, revision(&path).unwrap()), (2, 2));
        // The second, on what it read before, is told and changes nothing.
        let before = std::fs::read_to_string(&path).unwrap();
        second.user = "me".into();
        assert_eq!(save(&path, second, "pc-2").unwrap_err().code, "server_changed");
        assert_eq!(remove(&path, &gpu.connection.id, 1).unwrap_err().code, "server_changed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "nothing written");
        // Read again, its change is made.
        let mut again = find(&path, &gpu.connection.id, false).unwrap().connection;
        again.user = "me".into();
        let saved = save(&path, again, "pc-2").unwrap();
        assert_eq!((saved.connection.port, saved.connection.user.as_str(), saved.connection.revision), (2222, "me", 3));
        remove(&path, &gpu.connection.id, 3).unwrap();
        assert_eq!((all(&path).unwrap()[0].connection.revision, revision(&path).unwrap()), (4, 4));
    }

    /// The variable that makes this test program another program saving
    /// servers: the file, and the names to save, comma-separated.
    const SAVER: &str = "HOMOLOGY_TEST_SERVER_SAVER";

    /// Run as another program ([`SAVER`] set): save its servers.
    #[test]
    fn saver() {
        let Some(task) = std::env::var(SAVER).ok() else { return };
        let (path, names) = task.split_once('|').unwrap();
        for name in names.split(',') {
            save(Path::new(path), input(name, "h", ""), "pc").unwrap();
        }
    }

    /// Programs saving at once keep every server (#18).
    #[test]
    fn programs_saving_at_once_keep_every_server() {
        let folder = tempfile::tempdir().unwrap();
        let path = file(folder.path());
        let programs: Vec<_> = (0..4)
            .map(|program| {
                let names: Vec<String> = (0..10).map(|at| format!("p{program}-{at}")).collect();
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "servers::tests::saver", "--test-threads=1"])
                    .env(SAVER, format!("{}|{}", path.display(), names.join(",")))
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut program in programs {
            assert!(program.wait().unwrap().success());
        }
        assert_eq!(list(&path).unwrap().len(), 40);
        assert_eq!(revision(&path).unwrap(), 40, "one write each");
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
        std::fs::write(&path, "{\"schema_version\": 2, \"servers\": []}").unwrap();
        assert_eq!(list(&path).unwrap_err().code, "unsupported_server_schema");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(list(&path).unwrap_err().code, "server_store_read_failed");
    }

    /// A program in an App Group shares the group's container (#19), from
    /// inside its sandbox too; one that cannot use it shares the usual
    /// folder; `HOMOLOGY_HOME` comes first.
    #[test]
    fn a_program_in_an_app_group_shares_the_group_s_container() {
        let group = APP_GROUP;
        let environment = |home: &'static str, named: Option<&'static str>| {
            move |name: &str| match name {
                "HOME" => Some(home.into()),
                HOME_VARIABLE => named.map(Into::into),
                _ => None,
            }
        };
        let container = PathBuf::from(format!("/Users/me/Library/Group Containers/{group}/homology"));
        let yes = |_: &Path| true;
        let sandboxed = environment("/Users/me/Library/Containers/io.github.seagochen.annotarycat/Data", None);
        assert_eq!(
            shared_dir_with(Some(group), &sandboxed, &yes),
            Some(container.clone()),
            "the user's, not the app's"
        );
        assert_eq!(shared_dir_with(Some(group), &environment("/Users/me", None), &yes), Some(container));
        let no = |_: &Path| false;
        let usual = sys::config_home().map(|folder| folder.join("homology"));
        assert_eq!(shared_dir_with(Some(group), &environment("/Users/me", None), &no), usual, "not usable: the usual");
        assert_eq!(shared_dir_with(None, &environment("/Users/me", None), &yes), usual, "no group: the usual");
        let named = environment("/Users/me", Some("/tmp/shared"));
        assert_eq!(shared_dir_with(Some(group), &named, &yes), Some(PathBuf::from("/tmp/shared")));
        for wrong in ["", "../x", "a/b", ".hidden"] {
            assert_eq!(group_container(wrong, &environment("/Users/me", None)), None, "{wrong:?}");
        }
    }

    /// A program's earlier folder moves into its App Group's once: servers
    /// the group's lacks, the login keys, the known hosts (#19).
    #[test]
    fn an_earlier_shared_folder_moves_into_the_group_s_once() {
        let folder = tempfile::tempdir().unwrap();
        let (from, to) = (folder.path().join("own"), folder.path().join("group"));
        let mine = save(&file(&from), input("Mine", "gpu", ""), "pc").unwrap();
        save(&file(&to), input("Theirs", "lab", ""), "pc").unwrap();
        let hosts = crate::sandbox::known_hosts(&from);
        std::fs::create_dir_all(hosts.parent().unwrap()).unwrap();
        std::fs::write(&hosts, "gpu ssh-ed25519 AAAA1\n").unwrap();
        let cx = crate::Context::new(crate::App { name: "tool", env: "TOOL" });
        cx.set_keys_dir(keys_dir(&to));
        let moved = adopt(&cx, &from, &to).unwrap();
        assert_eq!((moved.servers, moved.known_hosts), (1, 1));
        let names: Vec<String> = list(&file(&to)).unwrap().into_iter().map(|server| server.connection.name).collect();
        assert_eq!(names, ["Theirs", "Mine"]);
        assert_eq!(
            find(&file(&to), &mine.connection.id, false).unwrap().connection.revision,
            1,
            "its id and revision kept"
        );
        let before = std::fs::read_to_string(file(&to)).unwrap();
        assert_eq!(adopt(&cx, &from, &to).unwrap(), Moved::default(), "moved again: nothing");
        assert_eq!(std::fs::read_to_string(file(&to)).unwrap(), before, "not even written");
        assert!(file(&from).is_file(), "the earlier folder stays");
        assert_eq!(adopt(&cx, &to, &to).unwrap(), Moved::default());
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
