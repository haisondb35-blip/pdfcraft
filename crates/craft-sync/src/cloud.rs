//! The cloud folders on this computer, and what a cloud does to the files in them.

use std::path::{Path, PathBuf};

/// Whose cloud a folder belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    ICloud,
    OneDrive,
    GoogleDrive,
    Dropbox,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::ICloud => "iCloud Drive",
            Kind::OneDrive => "OneDrive",
            Kind::GoogleDrive => "Google Drive",
            Kind::Dropbox => "Dropbox",
        }
    }

    /// The prefix of a [`portable_key`].
    fn key(self) -> &'static str {
        match self {
            Kind::ICloud => "icloud",
            Kind::OneDrive => "onedrive",
            Kind::GoogleDrive => "googledrive",
            Kind::Dropbox => "dropbox",
        }
    }
}

/// A cloud folder: whose, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub kind: Kind,
    pub path: PathBuf,
}

/// The cloud folders that exist on this computer, iCloud Drive first.
///
/// Windows: iCloud Drive in the user's folder or at the top of any drive (`D:\iCloudDrive`, where
/// iCloud for Windows is told to keep it), OneDrive where its own environment variables say,
/// Google Drive's `My Drive` at the top of a drive, Dropbox in the user's folder. macOS: iCloud
/// Drive's `Mobile Documents` folder and the File Provider folders in `Library/CloudStorage`.
/// Looked for once per run (a drive letter that is a slow network drive costs once, not per file).
pub fn roots() -> Vec<Root> {
    static ROOTS: std::sync::OnceLock<Vec<Root>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(find_roots).clone()
}

fn find_roots() -> Vec<Root> {
    let mut out = Vec::new();
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut add = |kind: Kind, path: PathBuf| {
        if path.is_dir() && !out.iter().any(|r: &Root| r.path == path) {
            out.push(Root { kind, path });
        }
    };
    if cfg!(windows) {
        let drives = || ('C'..='Z').map(|d| PathBuf::from(format!("{d}:\\")));
        if let Some(home) = var("USERPROFILE") {
            add(Kind::ICloud, home.join("iCloudDrive"));
        }
        for d in drives() {
            add(Kind::ICloud, d.join("iCloudDrive"));
        }
        for k in ["OneDriveConsumer", "OneDrive", "OneDriveCommercial"] {
            if let Some(p) = var(k) {
                add(Kind::OneDrive, p);
            }
        }
        for d in drives() {
            add(Kind::GoogleDrive, d.join("My Drive"));
        }
        if let Some(home) = var("USERPROFILE") {
            add(Kind::Dropbox, home.join("Dropbox"));
        }
    } else if let Some(home) = var("HOME") {
        add(Kind::ICloud, home.join("Library").join("Mobile Documents").join("com~apple~CloudDocs"));
        if let Ok(entries) = std::fs::read_dir(home.join("Library").join("CloudStorage")) {
            let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            found.sort();
            for p in found {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if name.starts_with("OneDrive") {
                    add(Kind::OneDrive, p);
                } else if name.starts_with("GoogleDrive") {
                    add(Kind::GoogleDrive, p.join("My Drive"));
                } else if name.starts_with("Dropbox") {
                    add(Kind::Dropbox, p);
                }
            }
        }
        add(Kind::Dropbox, home.join("Dropbox"));
    }
    out
}

/// The folder the Craft apps sync through when none was chosen: `ArtCraft` in iCloud Drive, else
/// in the first other cloud folder there is.
pub fn default_folder() -> Option<PathBuf> {
    let roots = roots();
    roots.iter().find(|r| r.kind == Kind::ICloud).or_else(|| roots.first()).map(|r| r.path.join("ArtCraft"))
}

/// A path written the same way on every system: `/` between names, no trailing `/`, lower case
/// (iCloud Drive, OneDrive and Google Drive don't tell names apart by case).
fn folded(path: &str) -> String {
    let p = path.replace('\\', "/").to_lowercase();
    p.trim_end_matches('/').to_string()
}

/// The name of a file in a cloud folder that is the same on every computer that has the folder,
/// wherever the folder is there: `icloud:/work/budget.xlsx` for `D:\iCloudDrive\Work\Budget.xlsx`
/// on one computer and `~/Library/Mobile Documents/com~apple~CloudDocs/Work/Budget.xlsx` on a Mac.
/// None for a file in no cloud folder.
pub fn portable_key(path: &str) -> Option<String> {
    key_in(path, &roots())
}

/// [`portable_key`] against the given cloud folders (the deepest one that holds `path`).
pub fn key_in(path: &str, roots: &[Root]) -> Option<String> {
    let p = folded(path);
    roots
        .iter()
        .filter_map(|r| {
            let root = folded(&r.path.to_string_lossy());
            if root.is_empty() {
                return None;
            }
            let rest = p.strip_prefix(&root)?.strip_prefix('/')?;
            (!rest.is_empty()).then(|| (root.len(), format!("{}:/{rest}", r.kind.key())))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, key)| key)
}

/// Whether the file is only in the cloud so far (a placeholder that downloads when it is read).
/// Windows marks those with "recall on data access", "recall on open" or "offline"; elsewhere this
/// is never known, so false.
pub fn is_cloud_only(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const OFFLINE: u32 = 0x0000_1000;
        const RECALL_ON_OPEN: u32 = 0x0004_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        std::fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// The most entries of one folder read when looking for copies.
const MAX_ENTRIES: usize = 20_000;

/// The other copies of `path` a cloud left beside it when two computers changed the file before
/// they caught up with each other: `Budget 2.xlsx` (iCloud Drive), `Budget (1).xlsx` (Google
/// Drive), `Budget-LAPTOP.xlsx` (OneDrive) or `Budget (Lan's conflicted copy 2026-11-08).xlsx`
/// (Dropbox). Sorted by name.
pub fn conflict_copies(path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(own)) = (path.parent(), path.file_name().map(|n| n.to_string_lossy().into_owned())) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries
        .take(MAX_ENTRIES)
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name != own && is_conflict_name(&own, &name) && e.path().is_file()).then(|| e.path())
        })
        .collect();
    out.sort();
    out
}

/// Whether `name` is the name a cloud gives another copy of the file named `own`.
pub fn is_conflict_name(own: &str, name: &str) -> bool {
    let split = |n: &str| -> (String, String) {
        match n.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), ext.to_lowercase()),
            _ => (n.to_string(), String::new()),
        }
    };
    let (stem, ext) = split(own);
    let (other, other_ext) = split(name);
    if ext != other_ext || crate::is_scratch(name) {
        return false;
    }
    let Some(rest) = other.strip_prefix(&stem) else { return false };
    let digits = |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit());
    // iCloud Drive: "Budget 2".
    if let Some(n) = rest.strip_prefix(' ')
        && digits(n)
    {
        return n != "0" && n != "1";
    }
    // Google Drive and Windows: "Budget (1)".
    if let Some(n) = rest.strip_prefix(" (").and_then(|r| r.strip_suffix(')'))
        && digits(n)
    {
        return true;
    }
    // Dropbox: "Budget (Lan's conflicted copy 2026-11-08)".
    if rest.starts_with(" (") && rest.to_lowercase().contains("conflicted copy") {
        return true;
    }
    // OneDrive: "Budget-LAPTOP" (the computer's name, as Windows writes it: capitals, digits, '-').
    if let Some(pc) = rest.strip_prefix('-') {
        return (2..=40).contains(&pc.len())
            && pc.bytes().any(|b| b.is_ascii_uppercase())
            && pc.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<Root> {
        vec![
            Root { kind: Kind::ICloud, path: PathBuf::from("D:\\iCloudDrive") },
            Root { kind: Kind::ICloud, path: PathBuf::from("/Users/lan/Library/Mobile Documents/com~apple~CloudDocs") },
            Root { kind: Kind::OneDrive, path: PathBuf::from("C:\\Users\\lan\\OneDrive") },
        ]
    }

    #[test]
    fn a_file_in_a_cloud_has_the_same_key_on_every_computer() {
        let pc = key_in("D:\\iCloudDrive\\Work\\Bảng lương.xlsx", &roots());
        let mac = key_in("/Users/lan/Library/Mobile Documents/com~apple~CloudDocs/Work/Bảng lương.xlsx", &roots());
        assert_eq!(pc.as_deref(), Some("icloud:/work/bảng lương.xlsx"));
        assert_eq!(pc, mac);
        assert_eq!(key_in("d:/icloudDRIVE/work/BẢNG LƯƠNG.xlsx", &roots()), pc, "case and separators don't matter");
        assert_eq!(key_in("C:\\Users\\lan\\OneDrive\\a.docx", &roots()).as_deref(), Some("onedrive:/a.docx"));
    }

    #[test]
    fn a_file_outside_the_clouds_has_no_key() {
        assert_eq!(key_in("C:\\Users\\lan\\Documents\\a.xlsx", &roots()), None);
        assert_eq!(key_in("D:\\iCloudDrive", &roots()), None, "the folder itself is no file");
        assert_eq!(key_in("D:\\iCloudDriveOld\\a.xlsx", &roots()), None, "a folder that only starts the same");
        assert_eq!(key_in("", &[Root { kind: Kind::ICloud, path: PathBuf::new() }]), None);
    }

    #[test]
    fn the_copies_clouds_make_are_recognised() {
        for name in [
            "Budget 2.xlsx",
            "Budget 13.xlsx",
            "Budget (1).xlsx",
            "Budget (Lan's conflicted copy 2026-11-08).xlsx",
            "Budget-LAPTOP.xlsx",
            "Budget-DESKTOP-7Q2K.XLSX",
        ] {
            assert!(is_conflict_name("Budget.xlsx", name), "{name}");
        }
        for name in [
            "Budget.xlsx",
            "Budget final.xlsx",
            "Budget 2.docx",
            "Budget-final.xlsx",
            "Budget 1.xlsx",
            "Budget 2024.xlsx",
            "Other 2.xlsx",
            "~$Budget 2.xlsx",
        ] {
            assert!(!is_conflict_name("Budget.xlsx", name), "{name}");
        }
        assert!(is_conflict_name("Notes", "Notes 2"), "a name without an extension");
    }

    #[test]
    fn copies_are_found_beside_the_file() {
        let d = crate::test_dir::fresh("conflicts");
        for n in ["Budget.xlsx", "Budget 2.xlsx", "Budget (1).xlsx", "Other.xlsx", "Budget 2.docx"] {
            std::fs::write(d.join(n), "x").unwrap();
        }
        std::fs::create_dir(d.join("Budget 3.xlsx")).unwrap();
        let found: Vec<String> =
            conflict_copies(&d.join("Budget.xlsx")).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(found, ["Budget (1).xlsx", "Budget 2.xlsx"]);
        assert!(conflict_copies(&d.join("missing").join("x.xlsx")).is_empty());
        assert!(!is_cloud_only(&d.join("Budget.xlsx")));
    }
}
