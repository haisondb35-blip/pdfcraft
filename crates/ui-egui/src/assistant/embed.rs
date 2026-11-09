//! Save chat with file: the assistant's conversation about a PDF, written into the PDF itself
//! when the person turns it on (off by default), so it travels with the file and comes back when
//! the file is opened on another computer.
//!
//! The conversation (portable form, without this computer's conversation id, see
//! `Memory::to_json`) is a Flate stream of JSON that the document catalog names under
//! `/ArtCraftAssistant`. Other PDF readers ignore a catalog key they don't know. It is added as an
//! incremental update after the saved file, so nothing already in the file is rewritten; saving
//! with the option off takes the key out the same way (earlier revisions keep the old stream until
//! the file is saved as a clean copy). Signed and encrypted files are left as they are: an update
//! after a signature shows as a change to the signed document, and an encrypted file would need
//! its keys.

use std::sync::Arc;

use pdfcraft_cos::{Dict, Document, Object, SaveOptions, Stream};
use serde_json::Value;

/// The catalog key that holds the conversation.
const KEY: &[u8] = b"ArtCraftAssistant";
/// The largest conversation read back.
const MAX_CHAT: usize = 8 << 20;

/// Files the conversation can be saved in.
pub fn can_embed(path: &str) -> bool {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// The conversation saved in the file at `path`, if it has one.
pub fn read(path: &str) -> Option<Value> {
    if !can_embed(path) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let doc = Document::open(Arc::new(bytes)).ok()?;
    if doc.security().is_some() {
        return None;
    }
    chat_in(&doc)
}

/// The conversation `doc`'s catalog names, if any.
fn chat_in(doc: &Document) -> Option<Value> {
    let catalog = doc.dict(&Object::Ref(doc.root()?))?;
    let held = doc.resolve(catalog.get(KEY)?);
    let Object::Stream(s) = &*held else { return None };
    serde_json::from_slice(&s.decoded_within(MAX_CHAT).ok()?).ok()
}

/// The bytes to write for the PDF at `path`: `bytes` with the conversation `shelf` has for it
/// (or without one, the option off). None keeps `bytes` as they are: nothing changes, the file is
/// signed (`signed`) or encrypted, or it can't be read.
pub fn hook(shelf: &std::sync::Mutex<super::memory::Shelf>, path: &str, bytes: &Arc<Vec<u8>>, signed: bool) -> Option<Vec<u8>> {
    if signed || !can_embed(path) {
        return None;
    }
    let chat = shelf.lock().ok().and_then(|s| s.chat_for(path));
    embed(bytes, chat.as_ref())
}

/// `bytes` with `chat` as the catalog's conversation (replacing one already there), or without
/// one when `chat` is None, as an incremental update. None when nothing changes or the file
/// can't take one (encrypted, unreadable).
pub fn embed(bytes: &Arc<Vec<u8>>, chat: Option<&Value>) -> Option<Vec<u8>> {
    let mut doc = Document::open(bytes.clone()).ok()?;
    if doc.security().is_some() {
        return None;
    }
    let root = doc.root()?;
    let has = doc.dict(&Object::Ref(root))?.get(KEY).is_some();
    match chat {
        None if !has => return None,
        Some(c) if chat_in(&doc).as_ref() == Some(c) => return None,
        _ => {}
    }
    let stream = chat.map(|c| doc.add(Object::Stream(Stream::flate(Dict::new(), c.to_string().as_bytes()))));
    doc.update_dict(root, |d| match stream {
        Some(r) => d.set(KEY.to_vec(), Object::Ref(r)),
        None => {
            d.remove(KEY);
        }
    })
    .ok()?;
    pdfcraft_cos::write_incremental(&doc, &SaveOptions::default()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A one-page PDF with a classic cross-reference table.
    fn pdf() -> Arc<Vec<u8>> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        Arc::new(out)
    }

    fn chat_of(bytes: &[u8]) -> Option<Value> {
        chat_in(&Document::open(Arc::new(bytes.to_vec())).unwrap())
    }

    #[test]
    fn a_chat_goes_into_the_pdf_and_comes_out_again() {
        let original = pdf();
        let chat = json!({"version": 1, "messages": [{"role": "user", "text": "Tóm tắt trang 1"}]});
        let with = embed(&original, Some(&chat)).unwrap();
        assert!(with.starts_with(&original), "an incremental update: the saved file is kept as it was");
        assert_eq!(chat_of(&with), Some(chat.clone()));
        let with = Arc::new(with);
        assert_eq!(embed(&with, Some(&chat)), None, "the same conversation again: nothing to add");
        let newer = json!({"messages": [{"role": "assistant", "text": "Xong."}]});
        let again = embed(&with, Some(&newer)).unwrap();
        assert_eq!(chat_of(&again), Some(newer));
        let without = embed(&with, None).unwrap();
        assert_eq!(chat_of(&without), None, "with the option off it is taken out");
        assert_eq!(embed(&original, None), None, "nothing to do");
        assert_eq!(embed(&Arc::new(b"not a pdf".to_vec()), Some(&chat)), None);
        assert!(can_embed("C:/Tài liệu/Hợp đồng.PDF") && !can_embed("a.docx"));
    }
}
