//! Verified, bounded tool archives installed transactionally into a caller-owned private cache.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Archive formats used by standalone desktop tools.
#[derive(Clone, Copy)]
pub enum Format {
    TarGzip,
    Zip,
}

/// Immutable distribution policy supplied by the caller, never by a project repository.
pub struct Artifact<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
    pub format: Format,
    pub entry: &'a str,
    pub maximum_download: u64,
    pub maximum_unpacked: u64,
}

/// Completion metadata lives beside the files and catches incomplete or modified cache entries.
const MANIFEST: &str = ".homology-artifact.json";
/// Bound directory and metadata growth independently of uncompressed bytes.
const MAXIMUM_ENTRIES: usize = 20_000;

/// One regular file produced by an archive.
#[derive(Serialize, Deserialize)]
struct Record {
    path: String,
    bytes: u64,
    sha256: String,
}

/// Only complete, verified installations receive a manifest.
#[derive(Serialize, Deserialize)]
struct Manifest {
    archive: String,
    files: Vec<Record>,
}

/// Prepare a pinned HTTPS artifact without scripts or globally installed tools.
/// A cache hit verifies its files; a miss notifies once, then publishes a complete directory.
pub fn prepare(spec: &Artifact<'_>, destination: &Path, preparing: impl FnOnce()) -> Result<PathBuf, String> {
    prepare_with(spec, destination, |path| {
        preparing();
        download(spec, path)
    })
    .map_err(|error| error.to_string())
}

/// Apply the same transaction to a real download or an isolated test producer.
fn prepare_with(
    spec: &Artifact<'_>,
    destination: &Path,
    produce: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    if !spec.url.starts_with("https://") || spec.sha256.len() != 64 || hex::decode(spec.sha256).is_err() {
        return Err(invalid("Tool source must be HTTPS with a SHA-256 pin"));
    }
    relative(Path::new(spec.entry))?;
    let parent = destination
        .parent()
        .filter(|_| destination.file_name().is_some())
        .ok_or_else(|| invalid("Invalid tool cache destination"))?;
    fs::create_dir_all(parent)?;
    let _lock = homology_sys::LockFile::take(destination)?;
    if cached(spec, destination) {
        return Ok(destination.join(spec.entry));
    }
    let temporary = tempfile::Builder::new().prefix("artifact-").tempdir_in(parent)?;
    let archive = temporary.path().join("download");
    produce(&archive)?;
    if fs::metadata(&archive)?.len() > spec.maximum_download || hash(&archive)? != spec.sha256.to_ascii_lowercase() {
        return Err(invalid("Tool archive size or SHA-256 verification failed"));
    }
    let payload = temporary.path().join("payload");
    fs::create_dir(&payload)?;
    let files = unpack(spec, &archive, &payload)?;
    if !files.iter().any(|record| record.path == spec.entry) {
        return Err(invalid("Tool archive is missing its entry point"));
    }
    let manifest = Manifest { archive: spec.sha256.to_ascii_lowercase(), files };
    let bytes = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
    homology_sys::atomic_write_bytes(&payload.join(MANIFEST), &bytes)?;
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.is_dir() && !metadata.is_symlink() {
            fs::remove_dir_all(destination)?;
        } else {
            fs::remove_file(destination)?;
        }
    }
    fs::rename(payload, destination)?;
    Ok(destination.join(spec.entry))
}

/// Verify cache contents without trusting a marker alone or following cached symbolic links.
fn cached(spec: &Artifact<'_>, destination: &Path) -> bool {
    let result = (|| -> io::Result<bool> {
        let metadata = fs::symlink_metadata(destination)?;
        if !metadata.is_dir() || metadata.is_symlink() {
            return Ok(false);
        }
        let manifest_path = destination.join(MANIFEST);
        let metadata = fs::symlink_metadata(&manifest_path)?;
        if !metadata.is_file() || metadata.is_symlink() || metadata.len() > 4 * 1024 * 1024 {
            return Ok(false);
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?).map_err(io::Error::other)?;
        if manifest.archive != spec.sha256.to_ascii_lowercase() || manifest.files.len() > MAXIMUM_ENTRIES {
            return Ok(false);
        }
        let mut total = 0_u64;
        for record in &manifest.files {
            let path = relative(Path::new(&record.path))?;
            // Reject a changed directory component as well as a changed final file.
            let mut current = destination.to_path_buf();
            for component in path.components() {
                current.push(component);
                if fs::symlink_metadata(&current)?.is_symlink() {
                    return Ok(false);
                }
            }
            let metadata = fs::metadata(&current)?;
            total = total.checked_add(metadata.len()).ok_or_else(|| invalid("Tool cache size overflow"))?;
            if !metadata.is_file()
                || metadata.len() != record.bytes
                || total > spec.maximum_unpacked
                || hash(&current)? != record.sha256
            {
                return Ok(false);
            }
        }
        Ok(manifest.files.iter().any(|record| record.path == spec.entry))
    })();
    result.unwrap_or(false)
}

/// Use platform certificate validation, proxy discovery and an absolute transfer deadline.
fn download(spec: &Artifact<'_>, path: &Path) -> io::Result<()> {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(8)
        .timeout_global(Some(Duration::from_secs(300)))
        .tls_config(tls)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent.get(spec.url).call().map_err(|_| {
        io::Error::other("Tool download failed; check the network, proxy and TLS configuration, then retry")
    })?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    let bytes =
        io::copy(&mut response.body_mut().as_reader().take(spec.maximum_download.saturating_add(1)), &mut output)?;
    if bytes > spec.maximum_download {
        return Err(invalid("Tool download exceeds its size limit"));
    }
    output.sync_all()
}

/// Reject cross-platform traversal, drive prefixes and the reserved completion manifest.
fn relative(path: &Path) -> io::Result<PathBuf> {
    let name = path.to_str().ok_or_else(|| invalid("Tool archive paths must be UTF-8"))?;
    if name.is_empty()
        || name.contains(['\\', ':'])
        || path == Path::new(MANIFEST)
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid("Unsafe tool archive path"));
    }
    for component in path.components() {
        let name = component.as_os_str().to_str().ok_or_else(|| invalid("Invalid archive component"))?;
        let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
                })
            });
        if name.ends_with(['.', ' ']) || name.chars().any(char::is_control) || device {
            return Err(invalid("Unsafe cross-platform archive component"));
        }
    }
    Ok(path.to_path_buf())
}

/// Extract only directories and regular files, never archive links or device entries.
fn unpack(spec: &Artifact<'_>, archive: &Path, destination: &Path) -> io::Result<Vec<Record>> {
    let mut output =
        Extraction { root: destination, remaining: spec.maximum_unpacked, names: HashSet::new(), files: Vec::new() };
    match spec.format {
        Format::TarGzip => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
            for entry in tar.entries()?.raw(true) {
                let mut entry = entry?;
                let path = relative(&entry.path()?)?;
                let kind = entry.header().entry_type();
                if kind.is_dir() {
                    output.directory(&path)?;
                } else if kind.is_file() {
                    let size = entry.header().size()?;
                    let mode = entry.header().mode()?;
                    output.file(&path, &mut entry, size, mode)?;
                } else {
                    return Err(invalid("Tool archives cannot contain links or special files"));
                }
            }
        }
        Format::Zip => {
            let mut zip = zip::ZipArchive::new(File::open(archive)?).map_err(io::Error::other)?;
            if zip.len() > MAXIMUM_ENTRIES {
                return Err(invalid("Too many tool archive entries"));
            }
            for index in 0..zip.len() {
                let mut entry = zip.by_index(index).map_err(io::Error::other)?;
                let path = relative(Path::new(entry.name()))?;
                let mode = entry.unix_mode().unwrap_or(0o644);
                let kind = mode & 0o170000;
                if entry.is_dir() && (kind == 0 || kind == 0o040000) {
                    output.directory(&path)?;
                } else if !entry.is_dir() && (kind == 0 || kind == 0o100000) {
                    let size = entry.size();
                    output.file(&path, &mut entry, size, mode)?;
                } else {
                    return Err(invalid("Tool archives cannot contain links or special files"));
                }
            }
        }
    }
    Ok(output.files)
}

/// One extraction's shared byte, entry and path budgets.
struct Extraction<'a> {
    root: &'a Path,
    remaining: u64,
    names: HashSet<PathBuf>,
    files: Vec<Record>,
}

impl Extraction<'_> {
    /// Reserve each name once, including directory-only archives.
    fn reserve(&mut self, path: &Path) -> io::Result<()> {
        if self.names.len() >= MAXIMUM_ENTRIES || !self.names.insert(path.to_path_buf()) {
            return Err(invalid("Duplicate or excessive tool archive entries"));
        }
        Ok(())
    }
    /// Directory permissions are owned by the private temporary parent, not archive metadata.
    fn directory(&mut self, path: &Path) -> io::Result<()> {
        self.reserve(path)?;
        fs::create_dir_all(self.root.join(path))
    }
    /// Create a file exclusively, enforcing declared and actual expanded sizes.
    fn file(&mut self, path: &Path, source: &mut impl Read, size: u64, mode: u32) -> io::Result<()> {
        self.reserve(path)?;
        if size > self.remaining {
            return Err(invalid("Tool archive exceeds its expanded size limit"));
        }
        let destination = self.root.join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new().write(true).create_new(true).open(&destination)?;
        let copied = io::copy(&mut source.take(size.saturating_add(1)), &mut output)?;
        if copied != size {
            return Err(invalid("Truncated or oversized tool archive file"));
        }
        output.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode & 0o755))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        self.remaining -= copied;
        self.files.push(Record { path: path.to_string_lossy().into(), bytes: copied, sha256: hash(&destination)? });
        Ok(())
    }
}

/// Hash a file using fixed memory, including large standalone runtimes.
fn hash(path: &Path) -> io::Result<String> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let bytes = input.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        hash.update(&buffer[..bytes]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// A validation error contains policy information, never URLs with proxy credentials.
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};

    /// A tiny distribution with a companion file, as used by standalone vendor tools.
    fn archive(format: Format) -> Vec<u8> {
        let files = [("agent", b"standalone".as_slice()), ("companion", b"runtime".as_slice())];
        match format {
            Format::TarGzip => {
                let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
                for (name, bytes) in files {
                    let mut header = tar::Header::new_gnu();
                    header.set_size(bytes.len() as u64);
                    header.set_mode(0o755);
                    header.set_cksum();
                    tar.append_data(&mut header, name, bytes).unwrap();
                }
                tar.into_inner().unwrap().finish().unwrap()
            }
            Format::Zip => {
                let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
                for (name, bytes) in files {
                    zip.start_file(name, zip::write::SimpleFileOptions::default().unix_permissions(0o755)).unwrap();
                    zip.write_all(bytes).unwrap();
                }
                zip.finish().unwrap().into_inner()
            }
        }
    }

    /// Test sources never contact a server or execute archive contents.
    fn spec<'a>(bytes: &[u8], format: Format) -> Artifact<'a> {
        Artifact {
            url: "https://fixture.invalid/agent",
            sha256: "",
            format,
            entry: "agent",
            maximum_download: bytes.len() as u64,
            maximum_unpacked: 1024,
        }
    }

    #[test]
    fn both_formats_reuse_verified_caches_and_rebuild_a_damaged_companion() {
        for format in [Format::TarGzip, Format::Zip] {
            let folder = tempfile::tempdir().unwrap();
            let destination = folder.path().join("version");
            let bytes = archive(format);
            let digest = hex::encode(Sha256::digest(&bytes));
            let mut spec = spec(&bytes, format);
            spec.sha256 = &digest;
            let entry = prepare_with(&spec, &destination, |path| fs::write(path, &bytes)).unwrap();
            assert_eq!(fs::read(entry).unwrap(), b"standalone");
            assert!(prepare_with(&spec, &destination, |_| panic!("cache hit must not download")).is_ok());
            fs::write(destination.join("companion"), b"damaged").unwrap();
            prepare_with(&spec, &destination, |path| fs::write(path, &bytes)).unwrap();
            assert_eq!(fs::read(destination.join("companion")).unwrap(), b"runtime");
        }
    }

    #[test]
    fn digest_size_and_download_failures_never_publish_partial_tools() {
        let folder = tempfile::tempdir().unwrap();
        let destination = folder.path().join("version");
        let bytes = archive(Format::TarGzip);
        let digest = hex::encode(Sha256::digest(&bytes));
        let mut spec = spec(&bytes, Format::TarGzip);
        spec.sha256 = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(prepare_with(&spec, &destination, |path| fs::write(path, &bytes)).is_err());
        spec.sha256 = &digest;
        spec.maximum_unpacked = 1;
        assert!(prepare_with(&spec, &destination, |path| fs::write(path, &bytes)).is_err());
        spec.maximum_unpacked = 1024;
        spec.maximum_download = 1;
        assert!(prepare_with(&spec, &destination, |path| fs::write(path, &bytes)).is_err());
        assert!(prepare_with(&spec, &destination, |path| {
            fs::write(path, b"partial")?;
            Err(io::Error::other("interrupted"))
        })
        .is_err());
        assert!(!destination.exists());
        assert!(fs::read_dir(folder.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("artifact-")));
    }

    #[test]
    fn traversal_links_and_reserved_paths_are_rejected() {
        for path in
            ["../escape", "/absolute", "C:/drive", "dir\\escape", "NUL", "dir/COM1.txt", "agent.", "dir/LPT¹", MANIFEST]
        {
            assert!(relative(Path::new(path)).is_err(), "{path}");
        }
        let folder = tempfile::tempdir().unwrap();
        for symlink in [false, true] {
            let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
            let mut header = tar::Header::new_gnu();
            // Write raw header names so the producer itself does not sanitize traversal.
            let name = if symlink { "agent" } else { "../escape" };
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_size(0);
            header.set_mode(0o755);
            if symlink {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name("../escape").unwrap();
            }
            header.set_cksum();
            builder.append(&header, io::empty()).unwrap();
            let bytes = builder.into_inner().unwrap().finish().unwrap();
            let digest = hex::encode(Sha256::digest(&bytes));
            let mut spec = spec(&bytes, Format::TarGzip);
            spec.sha256 = &digest;
            assert!(prepare_with(&spec, &folder.path().join("tool"), |path| fs::write(path, &bytes)).is_err());
            assert!(!folder.path().join("escape").exists());
        }
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.start_file("../escape", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"bad").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let digest = hex::encode(Sha256::digest(&bytes));
        let mut spec = spec(&bytes, Format::Zip);
        spec.sha256 = &digest;
        assert!(prepare_with(&spec, &folder.path().join("tool"), |path| fs::write(path, &bytes)).is_err());
    }
    proptest::proptest! {
        #[test]
        fn every_accepted_path_stays_under_its_destination(name in ".{0,256}") {
            if let Ok(path) = relative(Path::new(&name)) {
                let root = Path::new("private-cache");
                proptest::prop_assert!(root.join(path).starts_with(root));
            }
        }
    }
}
