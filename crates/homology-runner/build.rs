//! Lists the files of `python/` (the code check, the completion and the
//! vendored pyflakes, jedi and parso) for `src/python.rs`: each with its
//! path and its bytes, `include_bytes!`, so they are compiled into the
//! runner and none can be left out by hand (homology#14: jedi brings
//! hundreds of stub files).

use std::path::{Path, PathBuf};

/// Every file under `dir`, but `README.md` at its top and `__pycache__`.
fn files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder)? {
            let path = entry?.path();
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            if path.is_dir() {
                if name != "__pycache__" {
                    folders.push(path);
                }
            } else if !(name == "README.md" && folder == root) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Write `$OUT_DIR/python_files.rs`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("python");
    println!("cargo:rerun-if-changed=python");
    let mut text = String::from(
        "/// Homology's own files (`python/`, listed by `build.rs`): its `/`-separated\n\
         /// path in the folder, and its bytes.\n\
         pub const FILES: &[File] = &[\n",
    );
    for path in files(&root)? {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(&root)?
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        text.push_str(&format!("    ({relative:?}, include_bytes!({:?})),\n", path.display().to_string()));
    }
    text.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR")?).join("python_files.rs"), text)?;
    Ok(())
}
