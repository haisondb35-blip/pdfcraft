//! Keeps a folder of the app's own and its copy in the cloud folder the same: each file goes the
//! way of its newest copy. Nothing is deleted on either side (a file forgotten on one computer is
//! not taken from the others), and only what `include` names travels (never keys or the running
//! apps' ports).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The deepest folder looked into.
const MAX_DEPTH: usize = 6;
/// The most files looked at on one side.
const MAX_FILES: usize = 20_000;
/// Times closer than this are the same (FAT, network and cloud folders round them).
const SLACK: Duration = Duration::from_secs(2);

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Files copied to the cloud folder.
    pub up: usize,
    /// Files copied from it.
    pub down: usize,
    pub errors: Vec<String>,
}

impl Report {
    /// In words, for the settings page.
    pub fn summary(&self) -> String {
        let mut s = match (self.up, self.down) {
            (0, 0) => "Up to date".to_string(),
            (u, 0) => format!("Sent {u} file(s)"),
            (0, d) => format!("Received {d} file(s)"),
            (u, d) => format!("Sent {u} and received {d} file(s)"),
        };
        if let Some(e) = self.errors.first() {
            s.push_str(&format!("; {} couldn't be copied ({e})", self.errors.len()));
        }
        s
    }
}

/// The files under `dir` (relative, `/` between names) with their times.
fn files(dir: &Path, include: &dyn Fn(&str) -> bool) -> BTreeMap<String, (PathBuf, Option<SystemTime>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(dir.to_path_buf(), String::new(), 0usize)];
    while let Some((d, prefix, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            if out.len() >= MAX_FILES {
                return out;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if crate::is_scratch(&name) {
                continue;
            }
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((e.path(), rel, depth + 1));
                }
            } else if kind.is_file() && include(&rel) {
                let time = e.metadata().ok().and_then(|m| m.modified().ok());
                out.insert(rel, (e.path(), time));
            }
        }
    }
    out
}

/// Whether `a` is newer than `b` by more than the slack.
fn newer(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.duration_since(b).is_ok_and(|d| d > SLACK),
        (Some(_), None) => true,
        _ => false,
    }
}

/// Makes `local` and `remote` hold the same files (those `include` takes), each the newest copy.
pub fn mirror(local: &Path, remote: &Path, include: &dyn Fn(&str) -> bool) -> Report {
    let mut report = Report::default();
    let mine = files(local, include);
    let theirs = files(remote, include);
    for (rel, (src, time)) in &mine {
        let other = theirs.get(rel);
        if other.is_none_or(|(_, t)| newer(*time, *t)) {
            match crate::copy_atomic(src, &remote.join(rel)) {
                Ok(()) => report.up += 1,
                Err(e) => report.errors.push(e),
            }
        }
    }
    for (rel, (src, time)) in &theirs {
        let own = mine.get(rel);
        if own.is_none_or(|(_, t)| newer(*time, *t)) {
            match crate::copy_atomic(src, &local.join(rel)) {
                Ok(()) => report.down += 1,
                Err(e) => report.errors.push(e),
            }
        }
    }
    report
}

/// What of the AI folder (`ArtCraft/AI`) travels: each file's conversation and the shared
/// choices. Not the running apps' ports and tokens (`apps/`), not the assistant's working folder
/// (`work/`), not the keys people gave for their own AI accounts (`providers.json`).
pub fn ai_files(rel: &str) -> bool {
    (rel.starts_with("memory/") && rel.ends_with(".json")) || rel == "settings.json"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(p: &Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + Duration::from_secs(secs);
        std::fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn write(p: &Path, text: &str, secs: u64) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        at(p, secs);
    }

    #[test]
    fn the_newest_copy_wins_both_ways_and_nothing_is_deleted() {
        let d = crate::test_dir::fresh("mirror");
        let (local, remote) = (d.join("local"), d.join("cloud"));
        write(&local.join("memory/a.json"), "local a", 2_000);
        write(&remote.join("memory/a.json"), "cloud a", 1_000);
        write(&local.join("memory/b.json"), "local b", 1_000);
        write(&remote.join("memory/b.json"), "cloud b", 2_000);
        write(&remote.join("memory/c.json"), "only in the cloud", 1_000);
        write(&local.join("settings.json"), "{}", 1_000);
        write(&local.join("providers.json"), "secret", 1_000);
        write(&local.join("apps/gridcraft.json"), "port", 1_000);
        write(&local.join("memory/.x.json.artcraft-tmp"), "half", 1_000);
        let r = mirror(&local, &remote, &ai_files);
        assert_eq!((r.up, r.down), (2, 2), "{r:?}");
        assert!(r.errors.is_empty());
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(remote.join("memory/a.json")), "local a");
        assert_eq!(read(local.join("memory/b.json")), "cloud b");
        assert_eq!(read(local.join("memory/c.json")), "only in the cloud");
        assert_eq!(read(remote.join("settings.json")), "{}");
        assert!(!remote.join("providers.json").exists(), "keys stay on this computer");
        assert!(!remote.join("apps").exists());
        assert!(!remote.join("memory/.x.json.artcraft-tmp").exists());
        // Copies keep their times: a second run has nothing to do.
        assert_eq!(mirror(&local, &remote, &ai_files), Report::default());
        assert_eq!(Report::default().summary(), "Up to date");
    }

    #[test]
    fn a_missing_cloud_folder_is_made_and_a_missing_local_one_is_nothing() {
        let d = crate::test_dir::fresh("mirror-new");
        write(&d.join("local/memory/a.json"), "a", 1_000);
        let r = mirror(&d.join("local"), &d.join("new/cloud"), &ai_files);
        assert_eq!(r.up, 1);
        let r = mirror(&d.join("nowhere"), &d.join("empty"), &ai_files);
        assert_eq!(r, Report::default());
    }
}
