//! Scan & OCR ▸ Scan, Create PDF ▸ From scanner and Organize pages ▸ Insert from scanner: scanned
//! pages ([`pdfcraft_scan`]) become a new PDF, or pages inserted into an open one, each page the
//! size of the paper at the resolution it was scanned at.
//!
//! Scanning takes seconds a page, so the UI runs [`Backend::scan`] on a worker thread and hands
//! the pages to [`Session::create_from_scan`] or [`Session::insert_scan`] when it is done.

use std::sync::Arc;

pub use pdfcraft_scan::{Backend, ColorMode, DPI_RANGE, RESOLUTIONS, ScanError, ScanSettings, ScannedPage, Scanner, Simulated, Source, system};

use crate::{DocId, Edit, EditError, ImageResolution, Session};

/// Named image files, as Create PDF from images takes them.
type Images = Vec<(String, Vec<u8>)>;

/// The pages as images for Create PDF from images, with the resolution they were scanned at.
fn images(pages: &[ScannedPage]) -> Result<(Images, ImageResolution), EditError> {
    let first = pages.first().ok_or_else(|| EditError::Invalid("no pages were scanned".into()))?;
    let dpi = f64::from(first.dpi.clamp(*DPI_RANGE.start(), *DPI_RANGE.end()));
    Ok((pages.iter().map(|p| (p.name.clone(), p.bytes.clone())).collect(), ImageResolution::Dpi(dpi)))
}

impl Session {
    /// A new PDF of the scanned pages, one page per scan.
    pub fn create_from_scan(&self, pages: &[ScannedPage]) -> Result<Arc<Vec<u8>>, EditError> {
        let (images, resolution) = images(pages)?;
        self.create_from_images_with_resolution(&images, resolution)
    }

    /// Insert the scanned pages into document `id` before page `at` (0-based; the page count
    /// appends), as one undoable step. Returns the number of pages inserted.
    pub fn insert_scan(&mut self, id: DocId, pages: &[ScannedPage], at: usize) -> Result<usize, EditError> {
        let bytes = self.create_from_scan(pages)?;
        let count = self.get(id).map(|d| d.info.pages.len()).ok_or_else(|| EditError::Invalid("no such document".into()))?;
        self.apply(
            id,
            Edit::InsertPagesFrom {
                name: "Scan".into(),
                bytes,
                pages: None,
                at: at.min(count),
                field_conflicts: pdfcraft_organize::FieldConflicts::Rename,
            },
        )?;
        Ok(pages.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A white greyscale PNG, `w` × `h` pixels.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let img = image::GrayImage::from_pixel(w, h, image::Luma([255]));
        image::DynamicImage::ImageLuma8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn scanned(n: usize, dpi: u32) -> Vec<ScannedPage> {
        // A small page: 165 × 234 pixels.
        (0..n).map(|i| ScannedPage { name: format!("page-{i}.png"), bytes: png(165, 234), dpi }).collect()
    }

    #[test]
    fn scanned_pages_become_paper_sized_pages() {
        let mut s = Session::new();
        let bytes = s.create_from_scan(&scanned(2, 50)).unwrap();
        let id = s.open("scan.pdf", None, bytes, None).unwrap();
        let pages = &s.get(id).unwrap().info.pages;
        assert_eq!(pages.len(), 2);
        // 165 × 234 px at 50 dpi = 3.3 × 4.68 in = 237.6 × 337 pt.
        let (w, h) = (pages[0].width, pages[0].height);
        assert!((w - 237.6).abs() < 0.5 && (h - 336.96).abs() < 0.5, "{w} × {h}");
        assert!(s.create_from_scan(&[]).is_err());
    }

    #[test]
    fn scanned_pages_insert_as_one_undoable_step() {
        let mut s = Session::new();
        let base = s.create_from_scan(&scanned(1, 50)).unwrap();
        let id = s.open("doc.pdf", None, base, None).unwrap();
        assert_eq!(s.insert_scan(id, &scanned(2, 50), 99).unwrap(), 2);
        assert_eq!(s.get(id).unwrap().info.pages.len(), 3);
        s.undo(id).unwrap();
        assert_eq!(s.get(id).unwrap().info.pages.len(), 1);
    }
}
