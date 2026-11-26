//! Scan & OCR ▸ Scan: the Scan dialog (scanner, paper source, colour, resolution, recognise text
//! afterwards). Looking for scanners and scanning run on worker threads; the scanned pages
//! collect while the dialog is open (a flatbed scans one page at a time) until Done makes them a
//! new PDF or inserts them after the current page.
//!
//! The dialog is a stack dialog on the craft UI framework: it shows what [`PdfCraftApp::scan_pass`]
//! copies into it each frame and asks for a scan or a new search through `action`; the app runs
//! the work and keeps the pages.

use std::sync::{Arc, Mutex};

use craft::ui::buttons::Button;
use craft::ui::choice::Checkbox;
use craft::ui::contract::{Bind, Entry, Props as _, Show as _};
use craft::ui::dialog::{DialogId, DialogResult, DialogStack, StackDialog, dialog_id};
use craft::ui::display::{Shade, Text};
use craft::ui::form::Field;
use craft::ui::patterns::form_dialogs::FormDialog;
use craft::ui::pickers::Select;
use craft::ui::sections::{DialogRole, FooterButton};
use egui::Ui;
use pdfcraft_engine::DocId;
use pdfcraft_engine::scan::{Backend, ColorMode, RESOLUTIONS, ScanError, ScanSettings, ScannedPage, Scanner, Source};

use crate::PdfCraftApp;

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

/// The open Scan dialog's work.
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

/// A scanner as the dialog lists it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Listed {
    id: String,
    name: String,
    sources: Vec<Source>,
}

/// What the dialog shows of the work, copied in by [`PdfCraftApp::scan_pass`].
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    insert: bool,
    /// The scanners found; `None` while looking; the reason when there are none.
    scanners: Option<Result<Vec<Listed>, String>>,
    /// Pages scanned so far.
    pages: usize,
    /// The page being scanned (1-based) while a scan runs.
    scanning: Option<usize>,
    error: Option<String>,
    /// Text recognition is available.
    ocr: bool,
}

/// What a button in the dialog asked for; `scan_pass` runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Scan,
    Search,
}

/// Scan & OCR ▸ Scan to PDF, Organize pages ▸ Insert from scanner. Done answers `Ok(true)`.
pub(crate) struct ScanDialog {
    id: DialogId,
    form: ScanDraft,
    shown: Shown,
    action: Option<Action>,
}

impl StackDialog for ScanDialog {
    type Result = bool;

    fn id(&self) -> DialogId {
        self.id
    }

    fn show(&mut self, ui: &mut Ui, _: &DialogStack) -> Option<DialogResult<bool>> {
        let ScanDialog { form, shown, action, .. } = self;
        let title = if shown.insert { tl!("Insert from scanner") } else { tl!("Scan to PDF") };
        let out = FormDialog::new(title, Bind::bound(&mut *form))
            .ok_label(tl!("Done").to_string())
            .cancel_label(tl!("Cancel").to_string())
            .alt_resets(false)
            .extra_buttons([FooterButton::new(tl!("Scan").to_string(), DialogRole::Other)])
            .show(ui, |ui, _, f| {
                body(ui, shown, action, f);
                true
            });
        if out.picked == Some(2) {
            *action = Some(Action::Scan);
        }
        if out.accepted.is_some() {
            return Some(DialogResult::Ok(true));
        }
        out.closed.map(|_| DialogResult::Cancel)
    }
}

/// The dialog's rows.
fn body(ui: &mut Ui, shown: &Shown, action: &mut Option<Action>, f: &mut ScanDraft) {
    let busy = shown.scanning.is_some();
    let mut sources = vec![Source::Flatbed];
    match &shown.scanners {
        None => {
            Text::new(tl!("Looking for scanners…").to_string()).shade(Shade::Muted).show(ui);
        }
        Some(Ok(found)) if !found.is_empty() => {
            let current = found.iter().find(|s| f.scanner.as_ref() == Some(&s.id)).or(found.first());
            if let Some(c) = current {
                sources = c.sources.clone();
            }
            let entries: Vec<Entry<String>> = found.iter().map(|s| Entry::new(s.id.clone(), s.name.clone())).collect();
            let caption = tl!("Scanner").to_string();
            let given = current.map(|c| c.id.clone()).unwrap_or_default();
            Field::new(caption.clone()).show(ui, |ui, _| {
                if let Some(id) = Select::new(caption.clone(), Bind::given(given.clone()), &entries).show(ui).value().cloned()
                    && !busy
                {
                    f.scanner = Some(id);
                }
            });
        }
        Some(result) => {
            let why = match result {
                Err(e) => e.clone(),
                Ok(_) => ScanError::NoScanner.to_string(),
            };
            Text::new(why).shade(Shade::Muted).show(ui);
            if Button::new(tl!("Search again").to_string()).show(ui).clicks > 0 {
                *action = Some(Action::Search);
            }
        }
    }
    if !sources.contains(&f.settings.source) {
        f.settings.source = sources.first().copied().unwrap_or(Source::Flatbed);
    }
    let caption = tl!("Source").to_string();
    let entries: Vec<Entry<Source>> = sources.iter().map(|s| Entry::new(*s, source_label(*s).to_string())).collect();
    Field::new(caption.clone()).show(ui, |ui, _| {
        if let Some(s) = Select::new(caption.clone(), Bind::given(f.settings.source), &entries).show(ui).value().copied()
            && !busy
        {
            f.settings.source = s;
        }
    });
    let caption = tl!("Colour").to_string();
    let entries: Vec<Entry<ColorMode>> = ColorMode::ALL.iter().map(|m| Entry::new(*m, mode_label(*m).to_string())).collect();
    Field::new(caption.clone()).show(ui, |ui, _| {
        if let Some(m) = Select::new(caption.clone(), Bind::given(f.settings.mode), &entries).show(ui).value().copied()
            && !busy
        {
            f.settings.mode = m;
        }
    });
    let caption = tl!("Resolution").to_string();
    let entries: Vec<Entry<u32>> = RESOLUTIONS.iter().map(|v| Entry::new(*v, format!("{v} dpi"))).collect();
    Field::new(caption.clone()).show(ui, |ui, _| {
        if let Some(v) = Select::new(caption.clone(), Bind::given(f.settings.dpi), &entries).show(ui).value().copied()
            && !busy
        {
            f.settings.dpi = v;
        }
    });
    if shown.ocr {
        Checkbox::new(Bind::bound(&mut f.recognize)).label(tl!("Recognize text after scanning")).show(ui);
    }
    match shown.scanning {
        Some(n) => {
            Text::new(crate::i18n::fmt(tl!("Scanning… page {n}"), &[("n", &n.to_string())])).show(ui);
        }
        None if shown.pages == 1 => {
            Text::new(tl!("1 page").to_string()).show(ui);
        }
        None if shown.pages > 1 => {
            Text::new(crate::i18n::fmt(tl!("{n} pages"), &[("n", &shown.pages.to_string())])).show(ui);
        }
        None => {}
    }
    if let Some(e) = &shown.error {
        Text::new(e.clone()).shade(Shade::Muted).show(ui);
    }
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
        let scanners = self.scan.take().and_then(|s| s.scanners).or_else(|| self.scan_found.take());
        self.scan = Some(ScanSession { target, scanners, looking: None, pages: Vec::new(), running: None, error: None });
        self.find_scanners();
        let dialog = ScanDialog { id: dialog_id("scan"), form: self.scan_draft.clone(), shown: self.scan_shown(), action: None };
        crate::dialogs::host::open(self, dialog);
    }

    /// Create PDF ▸ Scanner, Scan & OCR ▸ Scan to PDF.
    pub(crate) fn scan_to_new_document(&mut self) {
        self.open_scan(ScanTarget::NewDocument);
    }

    /// Organize pages ▸ Insert from scanner: after the current page.
    pub(crate) fn scan_into_document(&mut self) {
        let Some((vi, id)) = self.active_ids() else { return };
        let at = self.views.get(vi).map_or(0, |v| v.current().saturating_add(1));
        self.open_scan(ScanTarget::Insert { doc: id, at });
    }

    /// What the dialog shows of the work now.
    fn scan_shown(&self) -> Shown {
        let Some(s) = self.scan.as_ref() else { return Shown::default() };
        let scanners = s.scanners.as_ref().map(|found| match found {
            Ok(v) => Ok(v.iter().map(|x| Listed { id: x.id.clone(), name: x.display_name(), sources: x.sources.clone() }).collect()),
            Err(e) => Err(e.to_string()),
        });
        let scanning = s.running.as_ref().map(|p| p.lock().map_or(0, |x| x.pages) + 1);
        Shown {
            insert: matches!(s.target, ScanTarget::Insert { .. }),
            scanners,
            pages: s.pages.len(),
            scanning,
            error: s.error.clone(),
            ocr: pdfcraft_engine::ocr::available(),
        }
    }

    /// Each pass: collects the workers' results, runs what the dialog asked for, refreshes what
    /// it shows, and once it closes either makes the pages a document (Done) or drops them.
    pub(crate) fn scan_pass(&mut self, ctx: &egui::Context) {
        if self.scan.is_none() {
            return;
        }
        self.poll_scan();
        if let Some(result) = self.dialogs.take::<ScanDialog>() {
            if let DialogResult::Ok(true) = result {
                self.finish_scan(ctx);
            }
            self.close_scan();
            return;
        }
        let Some(d) = self.dialogs.get_mut::<ScanDialog>() else { return };
        let action = d.action.take();
        self.scan_draft = d.form.clone();
        match action {
            Some(Action::Scan) => self.start_scan(),
            Some(Action::Search) => self.find_scanners(),
            None => {}
        }
        let shown = self.scan_shown();
        let waiting = shown.scanners.is_none() || shown.scanning.is_some();
        if let Some(d) = self.dialogs.get_mut::<ScanDialog>() {
            d.shown = shown;
        }
        if waiting {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
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
        if self.scan_inline {
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
    /// the Recognize Text dialog opens on them.
    pub fn finish_scan(&mut self, ctx: &egui::Context) {
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
                let opened = match self.session.create_from_scan(&pages) {
                    Ok(bytes) => self.open_bytes(ctx, "Scan.pdf", None, bytes.to_vec()),
                    Err(e) => Err(e.to_string()),
                };
                if let Err(e) = opened {
                    self.notify_fmt("Couldn't create a PDF: {e}", &[("e", e.as_str())]);
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
                    view.document_changed(ctx, &info);
                }
                if let Some(i) = self.views.iter().position(|v| v.id == doc) {
                    self.active = Some(i);
                }
                at..at + count
            }
        };
        // Recognize Text on the new pages, which reports its own progress.
        if self.scan_draft.recognize && pdfcraft_engine::ocr::available() {
            self.ocr_draft.pages = crate::ocr_ui::OcrPages::Range;
            self.ocr_draft.from = range.start + 1;
            self.ocr_draft.to = range.end;
            self.open_recognize_text(ctx);
        }
    }

    /// The dialog closed: stop any scan and drop its pages (the scanners found are kept).
    pub(crate) fn close_scan(&mut self) {
        self.cancel_scan();
        if let Some(s) = self.scan.as_mut() {
            s.pages.clear();
            s.running = None;
            s.error = None;
            s.looking = None;
        }
        if let Some(s) = self.scan.take() {
            self.scan_found = s.scanners;
        }
    }

    /// Use `backend` instead of the system's scanners (tests, demos).
    pub fn set_scan_backend(&mut self, backend: Arc<dyn Backend>) {
        self.scan_backend = backend;
        self.scan_found = None;
        if let Some(s) = self.scan.as_mut() {
            s.scanners = None;
            s.looking = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfcraft_engine::scan::Simulated;

    /// White PNG pages, 425 × 550 pixels (8.5 × 11 in at 50 dpi).
    fn pages(n: usize) -> Vec<ScannedPage> {
        let img = craft_image_codecs::Image::from_u8(425, 550, craft_image_codecs::ChannelLayout::Rgb, vec![255; 425 * 550 * 3]).expect("an image");
        let png = craft_image_codecs::encode(&img, craft_image_codecs::Format::Png, &craft_image_codecs::EncodeOptions::default()).expect("a PNG");
        (0..n).map(|i| ScannedPage { name: format!("page-{i}.png"), bytes: png.clone(), dpi: 50 }).collect()
    }

    fn app(sim: Simulated) -> (PdfCraftApp, egui::Context) {
        let ctx = egui::Context::default();
        let mut app = PdfCraftApp::new(&ctx);
        app.scan_inline = true;
        app.set_scan_backend(Arc::new(sim));
        (app, ctx)
    }

    #[test]
    fn scanned_pages_collect_until_done_makes_a_new_pdf() {
        let (mut app, ctx) = app(Simulated::with_pages(pages(3)));
        app.scan_draft.settings.dpi = 50;
        // The feeder scans every page at once (the flatbed one per scan).
        app.scan_draft.settings.source = Source::Feeder;
        app.scan_to_new_document();
        assert!(app.dialogs.get_mut::<ScanDialog>().is_some(), "the dialog is open");
        let shown = app.scan_shown();
        assert!(matches!(&shown.scanners, Some(Ok(v)) if !v.is_empty()), "the simulated scanner is found: {shown:?}");
        app.start_scan();
        assert_eq!(app.scan.as_ref().map(|s| s.pages.len()), Some(3), "the pages are kept while the dialog is open");
        let before = app.views.len();
        app.finish_scan(&ctx);
        assert_eq!(app.views.len(), before + 1, "Done opens the new PDF");
        let id = app.active_ids().map(|(_, id)| id).expect("an active document");
        assert_eq!(app.session.get(id).map(|d| d.info.pages.len()), Some(3));
    }

    #[test]
    fn a_closed_dialog_drops_its_pages_but_keeps_the_scanners() {
        let (mut app, _) = app(Simulated::with_pages(pages(1)));
        app.scan_to_new_document();
        app.start_scan();
        app.close_scan();
        assert!(app.scan.is_none());
        assert!(matches!(&app.scan_found, Some(Ok(v)) if !v.is_empty()));
        // Opened again: the scanners are there at once, the pages are gone.
        app.scan_to_new_document();
        assert_eq!(app.scan.as_ref().map(|s| s.pages.len()), Some(0));
    }
}
