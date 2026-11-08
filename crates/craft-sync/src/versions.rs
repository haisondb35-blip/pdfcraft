//! Earlier versions of saved files. Before a save replaces a file, the file as it was is copied
//! into a folder of its own under the versions folder, named after the time it was last saved.
//! AutoSave saves every few seconds, so a version is kept at most once per [`MIN_GAP`]; the
//! newest few are kept.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The shortest time between two kept versions of one file.
pub const MIN_GAP: Duration = Duration::from_secs(10 * 60);

/// One earlier version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub path: PathBuf,
    /// When the file was saved like this.
    pub modified: SystemTime,
    pub size: u64,
}

impl Version {
    /// "2026-11-08 14:05 · 34 KB", in this computer's time.
    pub fn label(&self) -> String {
        let when = chrono::DateTime::<chrono::Local>::from(self.modified).format("%Y-%m-%d %H:%M");
        let size = if self.size < 1024 {
            format!("{} B", self.size)
        } else if self.size < 1024 * 1024 {
            format!("{} KB", self.size / 1024)
        } else {
            format!("{:.1} MB", self.size as f64 / (1024.0 * 1024.0))
        };
        format!("{when} · {size}")
    }
}

/// 32-bit FNV-1a: tells apart two files with the same name in different folders.
fn fnv(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193))
}

/// The folder that holds the versions of `file`: its name and a short hash of where it is (the
/// [`crate::cloud::portable_key`] for a file in a cloud folder, so both computers use one folder).
pub fn folder_for(root: &Path, file: &Path) -> PathBuf {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let path = file.to_string_lossy();
    let key = crate::cloud::portable_key(&path).unwrap_or_else(|| path.replace('\\', "/").to_lowercase());
    root.join(format!("{name} [{:08x}]", fnv(&key)))
}

/// The versions of `file`, newest first.
pub fn list(root: &Path, file: &Path) -> Vec<Version> {
    list_in(&folder_for(root, file))
}

fn list_in(dir: &Path) -> Vec<Version> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Version> = entries
        .take(10_000)
        .flatten()
        .filter(|e| !crate::is_scratch(&e.file_name().to_string_lossy()))
        .filter_map(|e| {
            let m = e.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some(Version { path: e.path(), modified: m.modified().ok()?, size: m.len() })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.path.cmp(&a.path)));
    out
}

/// Whether `file` is itself a kept version (opened from the list): saving it keeps no version.
pub fn is_inside(root: &Path, file: &Path) -> bool {
    file.starts_with(root)
}

/// Keeps `file` as it is now among its versions, unless it was kept less than `gap` before its
/// last save, it is only in the cloud so far (it would be downloaded just for this), it is
/// larger than [`crate::MAX_COPY`], or it is itself a kept version. Then keeps only the newest
/// `keep`. Returns the version made.
pub fn snapshot(root: &Path, file: &Path, keep: usize, gap: Duration) -> Result<Option<PathBuf>, String> {
    if keep == 0 || is_inside(root, file) || crate::cloud::is_cloud_only(file) {
        return Ok(None);
    }
    let Ok(meta) = std::fs::metadata(file) else { return Ok(None) };
    if !meta.is_file() || meta.len() > crate::MAX_COPY {
        return Ok(None);
    }
    let saved = meta.modified().map_err(|e| format!("{}: {e}", file.display()))?;
    let dir = folder_for(root, file);
    let existing = list_in(&dir);
    if existing.first().is_some_and(|v| !saved.duration_since(v.modified).is_ok_and(|d| d >= gap)) {
        return Ok(None);
    }
    let ext = file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let name = format!("{}{ext}", chrono::DateTime::<chrono::Local>::from(saved).format("%Y-%m-%d %H.%M.%S"));
    let target = dir.join(name);
    if target.exists() {
        return Ok(None);
    }
    crate::copy_atomic(file, &target)?;
    for old in list_in(&dir).into_iter().skip(keep) {
        let _ = std::fs::remove_file(old.path);
    }
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(p: &Path, text: &str, secs: u64) {
        std::fs::write(p, text).unwrap();
        let t = std::time::UNIX_EPOCH + Duration::from_secs(secs);
        std::fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn versions_are_kept_once_per_gap_and_the_oldest_go() {
        let d = crate::test_dir::fresh("versions");
        let root = d.join("Versions");
        let file = d.join("Budget.xlsx");
        let base = 1_700_000_000;
        save(&file, "v1", base);
        let first = snapshot(&root, &file, 3, MIN_GAP).unwrap().expect("the first save keeps a version");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "v1");
        assert!(first.extension().is_some_and(|e| e == "xlsx"));
        // AutoSave a few seconds later: no new version.
        save(&file, "v2", base + 5);
        assert_eq!(snapshot(&root, &file, 3, MIN_GAP).unwrap(), None);
        for (i, text) in ["v3", "v4", "v5", "v6"].iter().enumerate() {
            save(&file, text, base + 700 * (i as u64 + 1));
            assert!(snapshot(&root, &file, 3, MIN_GAP).unwrap().is_some(), "{text}");
        }
        let kept: Vec<String> = list(&root, &file).iter().map(|v| std::fs::read_to_string(&v.path).unwrap()).collect();
        assert_eq!(kept, ["v6", "v5", "v4"], "newest first, three kept");
        assert!(list(&root, &file)[0].label().contains(" · 2 B"));
    }

    #[test]
    fn nothing_is_kept_of_a_missing_file_a_version_or_with_none_wanted() {
        let d = crate::test_dir::fresh("versions-none");
        let root = d.join("Versions");
        assert_eq!(snapshot(&root, &d.join("missing.xlsx"), 5, MIN_GAP).unwrap(), None);
        let file = d.join("a.docx");
        save(&file, "x", 1_700_000_000);
        assert_eq!(snapshot(&root, &file, 0, MIN_GAP).unwrap(), None);
        let v = snapshot(&root, &file, 5, MIN_GAP).unwrap().unwrap();
        assert_eq!(snapshot(&root, &v, 5, Duration::ZERO).unwrap(), None, "a version opened and saved again");
    }

    #[test]
    fn files_with_one_name_in_two_folders_keep_apart() {
        let root = Path::new("/v");
        assert_ne!(folder_for(root, Path::new("/a/Budget.xlsx")), folder_for(root, Path::new("/b/Budget.xlsx")));
        assert!(folder_for(root, Path::new("/a/Budget.xlsx")).to_string_lossy().contains("Budget.xlsx ["));
    }
}
