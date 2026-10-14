//! Scan & OCR ▸ Scan in the real shell (egui_kittest), against a simulated scanner: the dialog,
//! scanning page after page from the flatbed, the feeder, and where the pages go.

use std::sync::Arc;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use pdfcraft_engine::scan::{ScanError, ScannedPage, Simulated, Source};
use pdfcraft_ui_egui::{Dialog, PdfCraftApp};

/// White PNG pages, 425 × 550 pixels (8.5 × 11 in at 50 dpi).
fn pages(n: usize) -> Vec<ScannedPage> {
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(425, 550, image::Luma([255])))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    (0..n).map(|i| ScannedPage { name: format!("page-{i}.png"), bytes: png.clone(), dpi: 50 }).collect()
}

fn harness(sim: Simulated, open: bool) -> Harness<'static, PdfCraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.run_inline = true;
        app.set_scan_backend(Arc::new(sim));
        if open {
            let s = pdfcraft_engine::Session::new();
            let doc = s.create_blank(612.0, 792.0, 2).unwrap();
            app.open_bytes("doc.pdf", None, doc.to_vec()).unwrap();
        }
        app
    });
    h.run_steps(4);
    h
}

fn page_count(app: &PdfCraftApp) -> usize {
    let id = app.active_ids().unwrap().1;
    app.session.get(id).unwrap().info.pages.len()
}

#[test]
fn flatbed_pages_collect_until_done_makes_a_new_pdf() {
    let mut h = harness(Simulated::with_pages(pages(3)), false);
    // The pages come back at the resolution asked for: 425 × 550 px at 50 dpi is US Letter.
    h.state_mut().scan_draft.settings.dpi = 50;
    assert!(h.state_mut().execute("scan.create"));
    h.run_steps(2);
    assert_eq!(h.state().dialog, Some(Dialog::Scan));
    h.get_by_label("Scan to PDF");
    let found = h.state().scan.as_ref().and_then(|s| s.scanners.clone()).unwrap().unwrap();
    assert_eq!(found.iter().map(|s| s.display_name()).collect::<Vec<_>>(), vec!["Simulated Scanner"]);
    // Two flatbed scans: one page each.
    for _ in 0..2 {
        h.get_by_label("Scan").click();
        h.run_steps(2);
    }
    h.get_by_label("2 pages");
    h.get_by_label("Done").click();
    h.run_steps(2);
    assert_eq!(h.state().dialog, None);
    let app = h.state();
    assert_eq!(page_count(app), 2);
    let id = app.active_ids().unwrap().1;
    let page = &app.session.get(id).unwrap().info.pages[0];
    assert!((page.width - 612.0).abs() < 0.5 && (page.height - 792.0).abs() < 0.5, "{} × {}", page.width, page.height);
}

#[test]
fn the_feeder_inserts_every_sheet_after_the_current_page_in_one_step() {
    let mut h = harness(Simulated::with_pages(pages(3)), true);
    h.state_mut().scan_draft.settings.source = Source::Feeder;
    assert!(h.state_mut().execute("page.insert_scan"));
    h.run_steps(2);
    h.get_by_label("Insert from scanner");
    h.get_by_label("Scan").click();
    h.run_steps(2);
    h.get_by_label("3 pages");
    h.get_by_label("Done").click();
    h.run_steps(2);
    let app = h.state_mut();
    assert_eq!(page_count(app), 5);
    let id = app.active_ids().unwrap().1;
    app.session.undo(id).unwrap();
    assert_eq!(page_count(app), 2, "one undoable step");
}

#[test]
fn cancel_drops_the_scanned_pages() {
    let mut h = harness(Simulated::with_pages(pages(1)), false);
    assert!(h.state_mut().execute("scan.create"));
    h.run_steps(2);
    h.get_by_label("Scan").click();
    h.run_steps(2);
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert_eq!(h.state().dialog, None);
    assert!(h.state().active_ids().is_none(), "no document was made");
    // Opening the dialog again starts with no pages.
    assert!(h.state_mut().execute("scan.create"));
    h.run_steps(2);
    assert!(h.state().scan.as_ref().unwrap().pages.is_empty());
}

#[test]
fn scanner_problems_show_in_the_dialog() {
    let mut sim = Simulated::with_pages(pages(1));
    sim.error = Some(ScanError::Jam);
    let mut h = harness(sim, false);
    assert!(h.state_mut().execute("scan.create"));
    h.run_steps(2);
    h.get_by_label("Scan").click();
    h.run_steps(2);
    h.get_by_label(&ScanError::Jam.to_string());
    // No scanner at all: the dialog says so and offers to look again.
    let mut h = harness(Simulated::default(), false);
    assert!(h.state_mut().execute("scan.create"));
    h.run_steps(2);
    h.get_by_label(&ScanError::NoScanner.to_string());
    h.get_by_label("Search again");
}
