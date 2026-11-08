//! A synced cloud folder as every Craft app uses it.
//!
//! On Windows iCloud Drive, OneDrive, Google Drive and Dropbox are ordinary folders that their own
//! programs keep in step with the cloud, and on macOS iCloud Drive is one too. So the apps need no
//! account, sign-in or token of their own: they read and write a folder, carefully.
//!
//! - [`cloud`]: where those folders are on this computer, a name for a file in one that is the same
//!   on every computer ([`cloud::portable_key`]), whether a file is only in the cloud so far, and
//!   the copies a cloud keeps when two computers changed the same file ("Report 2.xlsx").
//! - [`mirror`]: keeps a folder of the app's own (the AI conversations) and its copy in the cloud
//!   folder the same, newest file wins, nothing deleted.
//! - [`versions`]: before a save replaces a file, the file as it was is kept among its earlier
//!   versions (at most one every few minutes, the newest few).
//! - [`background`]: the thread that runs the mirror every few minutes while an app runs.
//! - [`settings`]: whether syncing is on, the folder, how many versions to keep (`ArtCraft/sync.json`
//!   in the system's settings folder, shared by every Craft app).
//!
//! Every write goes to a temporary file beside its target and is renamed over it, so a cloud never
//! uploads half a file. Nothing here reads the clock: times come from the files themselves.
//!
//! This crate is the same in every Craft app (`crates/craft-sync`); change it in one place and copy
//! it to the others.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod background;
pub mod cloud;
pub mod mirror;
pub mod settings;
pub mod versions;

use std::path::Path;

pub use settings::Settings;

/// The largest file copied (a version or a mirrored file): larger ones are left alone.
pub const MAX_COPY: u64 = 512 << 20;

/// Copies `src` to `dst` through a temporary file beside `dst`, keeping `src`'s modification time
/// (so the next comparison sees the two as the same). Makes `dst`'s folder.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if meta.len() > MAX_COPY {
        return Err(format!("{}: larger than {} MB", src.display(), MAX_COPY >> 20));
    }
    let dir = dst.parent().ok_or_else(|| format!("{}: no folder", dst.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.artcraft-tmp"));
    let result = std::fs::copy(src, &tmp)
        .map_err(|e| format!("{}: {e}", src.display()))
        .and_then(|_| {
            let f = std::fs::OpenOptions::new().write(true).open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
            if let Ok(t) = meta.modified() {
                let _ = f.set_modified(t);
            }
            Ok(())
        })
        .and_then(|()| std::fs::rename(&tmp, dst).map_err(|e| format!("{}: {e}", dst.display())));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A temporary or hidden name this crate (or a save in progress) uses: never synced or listed.
pub fn is_scratch(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with('.')
        || name.starts_with("~$")
        || [".tmp", ".artcraft-tmp", ".sctmp", ".part", ".partial", ".crdownload"].iter().any(|s| lower.ends_with(s))
}

#[cfg(test)]
pub(crate) mod test_dir {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// An empty folder of the test's own.
    pub fn fresh(name: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("craft-sync-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_lands_whole_with_the_source_time() {
        let d = test_dir::fresh("copy");
        let src = d.join("a.txt");
        std::fs::write(&src, "hello").unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::OpenOptions::new().write(true).open(&src).unwrap().set_modified(t).unwrap();
        let dst = d.join("deep").join("b.txt");
        copy_atomic(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "hello");
        assert_eq!(std::fs::metadata(&dst).unwrap().modified().unwrap(), t);
        let names: Vec<_> = std::fs::read_dir(d.join("deep")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "no temporary file left: {names:?}");
        assert!(copy_atomic(&d.join("missing"), &dst).is_err());
    }

    #[test]
    fn scratch_names_are_recognised() {
        for n in [".DS_Store", "~$Book.xlsx", "a.tmp", "Book.xlsx.sctmp", "x.part", ".b.json.artcraft-tmp"] {
            assert!(is_scratch(n), "{n}");
        }
        for n in ["Book.xlsx", "memory", "a.json"] {
            assert!(!is_scratch(n), "{n}");
        }
    }
}
