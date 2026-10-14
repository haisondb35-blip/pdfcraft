//! Scan & OCR ▸ Scan: the Scan dialog (scanner, paper source, colour, resolution, recognise text
//! afterwards). Looking for scanners and scanning run on worker threads; the scanned pages
//! collect in the dialog (a flatbed scans one page at a time) until Done makes them a new PDF
//! or inserts them after the current page.

use std::sync::{Arc, Mutex};

use egui::{Align, Layout};
use pdfcraft_engine::DocId;
use pdfcraft_engine::scan::{Backend, ColorMode, RESOLUTIONS, ScanError, ScanSettings, ScannedPage, Scanner, Source};

use crate::theme::{self, Tokens};
use crate::{PdfCraftApp, widgets};

/// Where the scanned pages go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanTarget {
    /// A new document.
    NewDocument,
    /// Into `doc`, before page `at` (0-based).
    Insert { doc: DocId, at: usize },
}

/// The dialog's choices (kept between scans).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanDraft {
    /// The chosen scanner's id.
    pub scanner: Option<String>,
    pub settings: ScanSettings,
    /// Recognise text in the scanned pages once they are in the document.
    pub recognize: bool,
}

/// A scan in progress: pages so far, the result once finished, and a cancel request.
#[derive(Default)]
pub struct ScanProgress {
    pub pages: usize,
    pub result: Option<Result<Vec<ScannedPage>, ScanError>>,
    pub cancel: bool,
}

/// What looking for scanners found.
type Found = Result<Vec<Scanner>, ScanError>;

/// The open Scan dialog.
pub struct ScanSession {
    pub target: ScanTarget,
    /// The scanners found (`None` while looking).
    pub scanners: Option<Found>,
    looking: Option<Arc<Mutex<Option<Found>>>>,
    /// Pages scanned so far, in order.
    pub pages: Vec<ScannedPage>,
    pub running: Option<Arc<Mutex<ScanProgress>>>,
    /// The last scan's problem, shown in the dialog.
    pub error: Option<String>,
}

impl ScanSession {
    fn scanner<'a>(&'a self, draft: &ScanDraft) -> Option<&'a Scanner> {
        let found = self.scanners.as_ref()?.as_ref().ok()?;
        draft.scanner.as_ref().and_then(|id| found.iter().find(|s| &s.id == id)).or(found.first())
    }
}

fn source_label(s: Source) -> &'static str {
    match s {
        Source::Flatbed => tl!("Flatbed"),
        Source::Feeder => tl!("Document feeder"),
        Source::Duplex => tl!("Both sides"),
    }
}

fn mode_label(m: ColorMode) -> &'static str {
    match m {
        ColorMode::Color => tl!("Colour"),
        ColorMode::Gray => tl!("Greyscale"),
        ColorMode::BlackWhite => tl!("Black and white"),
    }
}

/// The dialog. Returns `(scan, done, cancel)`.
pub(crate) fn body(ui: &mut egui::Ui, app: &mut PdfCraftApp, t: &Tokens) -> (bool, bool, bool) {
    app.poll_scan();
    let ocr = pdfcraft_engine::ocr::available();
    let Some(session) = app.scan.as_ref() else { return (false, false, true) };
    let insert = matches!(session.target, ScanTarget::Insert { .. });
    ui.label(egui::RichText::new(if insert { tl!("Insert from scanner") } else { tl!("Scan to PDF") }).font(theme::semibold(18.0)));
    ui.add_space(8.0);
    let busy = session.running.is_some();
    let mut search = false;
    let draft = &mut app.scan_draft;
    egui::Grid::new("scan-settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label(tl!("Scanner"));
        match &session.scanners {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(tl!("Looking for scanners…"));
                });
            }
            Some(Ok(found)) if !found.is_empty() => {
                let current = session.scanner(draft).cloned();
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("scan-device")
                        .selected_text(current.as_ref().map(Scanner::display_name).unwrap_or_default())
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for s in found {
                                if ui.selectable_label(current.as_ref().is_some_and(|c| c.id == s.id), s.display_name()).clicked() {
                                    draft.scanner = Some(s.id.clone());
                                }
                            }
                        });
                });
            }
            Some(result) => {
                let why = match result {
                    Err(e) => e.to_string(),
                    Ok(_) => ScanError::NoScanner.to_string(),
                };
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(why).small().color(t.text_muted));
                    if widgets::pill_button(ui, tl!("Search again"), false).clicked() {
                        search = true;
                    }
                });
            }
        }
        ui.end_row();
        let sources = session.scanner(draft).map_or_else(|| vec![Source::Flatbed], |s| s.sources.clone());
        if !sources.contains(&draft.settings.source) {
            draft.settings.source = sources.first().copied().unwrap_or(Source::Flatbed);
        }
        ui.label(tl!("Source"));
        ui.add_enabled_ui(!busy, |ui| {
            ui.vertical(|ui| {
                for s in &sources {
                    widgets::radio_value(ui, &mut draft.settings.source, *s, source_label(*s));
                }
            });
        });
        ui.end_row();
        ui.label(tl!("Colour"));
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("scan-mode").selected_text(mode_label(draft.settings.mode)).width(260.0).show_ui(ui, |ui| {
                for m in ColorMode::ALL {
                    ui.selectable_value(&mut draft.settings.mode, m, mode_label(m));
                }
            });
        });
        ui.end_row();
        ui.label(tl!("Resolution"));
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("scan-dpi").selected_text(format!("{} dpi", draft.settings.dpi)).width(260.0).show_ui(ui, |ui| {
                for v in RESOLUTIONS {
                    ui.selectable_value(&mut draft.settings.dpi, v, format!("{v} dpi"));
                }
            });
        });
        ui.end_row();
    });
    ui.add_space(6.0);
    ui.add_enabled(ocr && !busy, widgets::checkbox(&mut draft.recognize, tl!("Recognize text after scanning")));
    ui.add_space(8.0);
    let pages = session.pages.len();
    match &session.running {
        Some(p) => {
            let n = p.lock().map_or(0, |s| s.pages);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(crate::i18n::fmt(tl!("Scanning… page {n}"), &[("n", &(n + 1).to_string())]));
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
        None if pages > 0 => {
            let label = if pages == 1 { tl!("1 page").to_string() } else { crate::i18n::fmt(tl!("{n} pages"), &[("n", &pages.to_string())]) };
            ui.label(label);
        }
        None => {}
    }
    if let Some(e) = &session.error {
        ui.label(egui::RichText::new(e).small().color(t.text_muted));
    }
    if session.scanners.is_none() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    }
    let can_scan = !busy && session.scanner(draft).is_some();
    ui.add_space(6.0);
    let (mut scan, mut done, mut cancel) = (false, false, false);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add_enabled_ui(pages > 0 && !busy, |ui| widgets::pill_button(ui, tl!("Done"), true)).inner.clicked() {
                done = true;
            }
            if ui.add_enabled_ui(can_scan, |ui| widgets::pill_button(ui, tl!("Scan"), pages == 0)).inner.clicked() {
                scan = true;
            }
            if widgets::pill_button(ui, tl!("Cancel"), false).clicked() {
                cancel = true;
            }
        })
    });
    if search {
        app.find_scanners();
    }
    (scan, done, cancel)
}

impl PdfCraftApp {
    /// Open the Scan dialog for pages that go to `target`, looking for scanners.
    pub fn open_scan(&mut self, target: ScanTarget) {
        if let ScanTarget::Insert { doc, .. } = target
            && let Some(why) = self.session.get(doc).and_then(|d| d.read_only_reason.clone())
        {
            self.notify(why);
            return;
        }
        // Scanners found earlier are kept; the list is refreshed on every opening.
        let scanners = self.scan.take().and_then(|s| s.scanners);
        self.scan = Some(ScanSession { target, scanners, looking: None, pages: Vec::new(), running: None, error: None });
        self.dialog = Some(crate::Dialog::Scan);
        self.find_scanners();
    }

    /// Create PDF ▸ Scanner, Scan & OCR ▸ Scan to PDF.
    pub(crate) fn scan_to_new_document(&mut self) {
        self.open_scan(ScanTarget::NewDocument);
    }

    /// Organize pages ▸ Insert from scanner: after the current page.
    pub(crate) fn scan_into_document(&mut self) {
        let Some((vi, id)) = self.active_ids() else { return };
        let at = self.views.get(vi).map_or(0, |v| v.current.saturating_add(1));
        self.open_scan(ScanTarget::Insert { doc: id, at });
    }

    /// Look for scanners again, in the background.
    pub fn find_scanners(&mut self) {
        let Some(session) = self.scan.as_mut() else { return };
        if session.looking.is_some() {
            return;
        }
        if session.scanners.as_ref().is_some_and(|s| s.as_ref().is_ok_and(|v| v.is_empty()) || s.is_err()) {
            session.scanners = None;
        }
        let slot = Arc::new(Mutex::new(None));
        let (out, backend) = (slot.clone(), self.scan_backend.clone());
        let work = move || {
            let found = backend.scanners();
            if let Ok(mut s) = out.lock() {
                *s = Some(found);
            }
        };
        session.looking = Some(slot);
        self.run_scan_work("pdfcraft-scan-list", work);
        self.poll_scan();
    }

    /// Scan with the dialog's settings, in the background.
    pub fn start_scan(&mut self) {
        let Some(session) = self.scan.as_mut() else { return };
        if session.running.is_some() {
            return;
        }
        let Some(scanner) = session.scanner(&self.scan_draft).cloned() else { return };
        session.error = None;
        let settings = self.scan_draft.settings.clone();
        let progress = Arc::new(Mutex::new(ScanProgress::default()));
        let (p, backend) = (progress.clone(), self.scan_backend.clone());
        let work = move || {
            let result = backend.scan(&scanner, &settings, &mut |n| {
                let Ok(mut s) = p.lock() else { return false };
                s.pages = n;
                !s.cancel
            });
            if let Ok(mut s) = p.lock() {
                s.result = Some(result);
            }
        };
        session.running = Some(progress);
        self.run_scan_work("pdfcraft-scan", work);
        self.poll_scan();
    }

    fn run_scan_work(&mut self, name: &str, work: impl FnOnce() + Send + 'static) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.run_inline {
            work();
        } else {
            std::thread::Builder::new().name(name.into()).spawn(work).ok();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = name;
            work();
        }
    }

    /// Stop a running scan (the pages scanned so far are kept).
    pub fn cancel_scan(&mut self) {
        if let Some(p) = self.scan.as_ref().and_then(|s| s.running.as_ref())
            && let Ok(mut s) = p.lock()
        {
            s.cancel = true;
        }
    }

    /// Collect what the workers have finished.
    pub(crate) fn poll_scan(&mut self) {
        let Some(session) = self.scan.as_mut() else { return };
        if let Some(found) = session.looking.as_ref().and_then(|l| l.lock().ok().and_then(|mut s| s.take())) {
            session.looking = None;
            session.scanners = Some(found);
        }
        let Some(result) = session.running.as_ref().and_then(|p| p.lock().ok().and_then(|mut s| s.result.take())) else { return };
        session.running = None;
        match result {
            Ok(pages) => session.pages.extend(pages),
            Err(ScanError::Cancelled) => {}
            Err(e) => session.error = Some(e.to_string()),
        }
    }

    /// Done: the scanned pages become a new document or go into the target one, then (if asked)
    /// their text is recognised.
    pub fn finish_scan(&mut self) {
        let Some(session) = self.scan.as_mut() else { return };
        let pages = std::mem::take(&mut session.pages);
        let target = session.target;
        session.error = None;
        if pages.is_empty() {
            return;
        }
        let count = pages.len();
        let range = match target {
            ScanTarget::NewDocument => {
                let created = self.session.create_from_scan(&pages).map_err(|e| e.to_string());
                if let Err(e) = self.open_created_bytes("Scan.pdf", created) {
                    self.notify_fmt("Couldn't create a PDF: {e}", &[("e", &e.to_string())]);
                    return;
                }
                0..count
            }
            ScanTarget::Insert { doc, at } => {
                let at = self.session.get(doc).map_or(at, |d| at.min(d.info.pages.len()));
                if let Err(e) = self.session.insert_scan(doc, &pages, at) {
                    self.notify_error(e);
                    return;
                }
                if let Some(info) = self.session.get(doc).map(|d| d.info.clone())
                    && let Some(view) = self.views.iter_mut().find(|v| v.id == doc)
                {
                    view.document_changed(&info);
                }
                if let Some(i) = self.views.iter().position(|v| v.id == doc) {
                    self.active = Some(i);
                }
                at..at + count
            }
        };
        // The new pages show in the document; recognising their text reports its own progress.
        if self.scan_draft.recognize && pdfcraft_engine::ocr::available() {
            self.ocr_draft.pages = crate::ocr_ui::OcrPages::Range;
            self.ocr_draft.from = range.start + 1;
            self.ocr_draft.to = range.end;
            self.start_ocr();
        }
    }

    /// The dialog closed: stop any scan and drop its pages.
    pub(crate) fn close_scan(&mut self) {
        self.cancel_scan();
        if let Some(s) = self.scan.as_mut() {
            s.pages.clear();
            s.running = None;
            s.error = None;
        }
    }

    /// Use `backend` instead of the system's scanners (tests, demos).
    pub fn set_scan_backend(&mut self, backend: Arc<dyn Backend>) {
        self.scan_backend = backend;
        if let Some(s) = self.scan.as_mut() {
            s.scanners = None;
            s.looking = None;
        }
    }
}
