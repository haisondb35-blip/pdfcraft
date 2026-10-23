//! Document-level file attachments (Edit a PDF ▸ Attach file): files embedded in the
//! `/Names /EmbeddedFiles` name tree (ISO 32000-2 §7.7.4, §7.11.4), which the Attachments panel
//! lists. Attachments on a page are file attachment annotations and live in the annot crate.

use pdfcraft_cos::{Dict, Document, Object, PdfString, Stream};

use crate::OrganizeError;
use crate::import::name_tree_entries;

/// The largest file Attach file embeds (the whole file is held in memory and written into the PDF).
pub const MAX_ATTACHMENT_BYTES: usize = 512 * 1024 * 1024;

/// One file to attach to the document.
#[derive(Clone, Debug, PartialEq)]
pub struct NewAttachment<'a> {
    /// The file name shown in the Attachments panel.
    pub name: &'a str,
    pub data: &'a [u8],
    /// The description (`/Desc`), if any.
    pub description: Option<&'a str>,
    /// The modification date as a PDF date string (`D:2026…`), if known.
    pub date: Option<&'a str>,
}

/// The document's `/EmbeddedFiles` entries, flattened in tree order (a tree deeper than the
/// reader's depth limit is cut off there).
fn entries(doc: &Document) -> Vec<(Vec<u8>, Object)> {
    let Some(tree) = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().cloned())
        .and_then(|c| c.get(b"Names").map(|n| doc.resolve(n)))
        .and_then(|n| n.as_dict().and_then(|d| d.get(b"EmbeddedFiles").cloned()))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    name_tree_entries(doc, &tree, 0, &mut std::collections::HashSet::new(), &mut out);
    out
}

/// The name an entry's key reads as (keys are PDF text strings).
fn key_text(key: &[u8]) -> String {
    PdfString::literal(key.to_vec()).to_text()
}

/// Write `entries` back as one flat, sorted `/EmbeddedFiles` name tree, or drop the tree (and an
/// emptied `/Names`) when none are left. Other name trees in `/Names` are kept.
fn write(doc: &mut Document, mut entries: Vec<(Vec<u8>, Object)>) -> Result<(), OrganizeError> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let tree = if entries.is_empty() {
        None
    } else {
        let mut names = Vec::with_capacity(entries.len().saturating_mul(2));
        for (k, v) in entries {
            names.push(Object::String(PdfString::literal(k)));
            names.push(v);
        }
        let mut tree = Dict::new();
        tree.set(b"Names".to_vec(), Object::Array(names));
        Some(doc.add(tree))
    };
    let root = doc.root().ok_or(OrganizeError::NoPageTree)?;
    let names = match doc.get(root).as_dict().and_then(|c| c.get(b"Names").cloned()) {
        Some(n) => doc.resolve(&n).as_dict().cloned().unwrap_or_default(),
        None => Dict::new(),
    };
    let mut names = names;
    match tree {
        Some(t) => names.set(b"EmbeddedFiles".to_vec(), Object::Ref(t)),
        None => {
            names.remove(b"EmbeddedFiles");
        }
    }
    doc.update_dict(root, |c| {
        if names.is_empty() {
            c.remove(b"Names");
        } else {
            c.set(b"Names".to_vec(), Object::Dict(names));
        }
    })?;
    Ok(())
}

/// Embed a file in the document. A name already in use gets " (2)", " (3)"… before its
/// extension, as Acrobat does. Returns the name it was attached under.
pub fn add_attachment(doc: &mut Document, new: &NewAttachment) -> Result<String, OrganizeError> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(OrganizeError::Invalid("the attached file needs a name".into()));
    }
    if new.data.len() > MAX_ATTACHMENT_BYTES {
        return Err(OrganizeError::Invalid(format!(
            "the file is larger than {} MB, the most Attach file embeds",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        )));
    }
    let mut list = entries(doc);
    let taken: std::collections::HashSet<String> = list.iter().map(|(k, _)| key_text(k)).collect();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let mut unique = name.to_string();
    let mut n = 2u32;
    while taken.contains(&unique) {
        unique = format!("{stem} ({n}){ext}");
        n = n.saturating_add(1);
    }
    // An embedded file stream inside a file specification (§7.11.4).
    let mut params = Dict::new();
    params.set(b"Size".to_vec(), Object::Int(i64::try_from(new.data.len()).unwrap_or(i64::MAX)));
    if let Some(date) = new.date {
        params.set(b"ModDate".to_vec(), Object::String(PdfString::literal(date.as_bytes().to_vec())));
    }
    let mut ef = Dict::new();
    ef.set(b"Type".to_vec(), Object::name("EmbeddedFile"));
    ef.set(b"Params".to_vec(), Object::Dict(params));
    let ef = doc.add(Object::Stream(Stream::flate(ef, new.data)));
    let mut efd = Dict::new();
    efd.set(b"F".to_vec(), Object::Ref(ef));
    efd.set(b"UF".to_vec(), Object::Ref(ef));
    let mut fs = Dict::new();
    fs.set(b"Type".to_vec(), Object::name("Filespec"));
    fs.set(b"F".to_vec(), Object::String(PdfString::text(&unique)));
    fs.set(b"UF".to_vec(), Object::String(PdfString::text(&unique)));
    fs.set(b"EF".to_vec(), Object::Dict(efd));
    if let Some(d) = new.description.map(str::trim).filter(|d| !d.is_empty()) {
        fs.set(b"Desc".to_vec(), Object::String(PdfString::text(d)));
    }
    let fs = doc.add(Object::Dict(fs));
    list.push((PdfString::text(&unique).bytes, Object::Ref(fs)));
    write(doc, list)?;
    Ok(unique)
}

/// Remove the document-level attachment named `name`. The file's objects are left for a full
/// save to drop. Returns whether there was one.
pub fn remove_attachment(doc: &mut Document, name: &str) -> Result<bool, OrganizeError> {
    let mut list = entries(doc);
    let before = list.len();
    list.retain(|(k, _)| key_text(k) != name);
    if list.len() == before {
        return Ok(false);
    }
    write(doc, list)?;
    Ok(true)
}
