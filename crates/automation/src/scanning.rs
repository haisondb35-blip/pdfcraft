//! Scanning tools: list the scanners the system knows, and scan into a new PDF or into an open
//! document (Scan & OCR ▸ Scan, Organize pages ▸ Insert from scanner).

use std::sync::Arc;

use pdfcraft_engine::scan::{Backend, ColorMode, ScanSettings, Scanner, Source};
use serde_json::{Value, json};

use crate::{Args, Automation, Result, ToolError, failed};

fn bad(m: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs(m.into())
}

fn scanner_json(s: &Scanner) -> Value {
    json!({ "id": s.id, "name": s.display_name(), "sources": s.sources.iter().map(|s| s.id()).collect::<Vec<_>>() })
}

impl Automation {
    /// Use `backend` instead of the system's scanners (tests, demos).
    pub fn with_scan_backend(mut self, backend: Arc<dyn Backend>) -> Self {
        self.scan_backend = backend;
        self
    }

    pub(crate) fn scanners(&self) -> Result<Value> {
        let found = self.scan_backend.scanners().map_err(failed)?;
        Ok(json!({ "count": found.len(), "scanners": found.iter().map(scanner_json).collect::<Vec<_>>() }))
    }

    pub(crate) fn scan(&mut self, a: &Args) -> Result<Value> {
        let found = self.scan_backend.scanners().map_err(failed)?;
        let scanner = match a.opt_str("scanner")? {
            Some(id) => found.iter().find(|s| s.id == id).ok_or_else(|| bad(format!("no scanner {id:?} (see scanners)")))?,
            None => found.first().ok_or_else(|| failed(pdfcraft_engine::scan::ScanError::NoScanner))?,
        };
        let source = match a.opt_str("source")? {
            None => scanner.sources.first().copied().unwrap_or(Source::Flatbed),
            Some(s) => Source::from_id(s).ok_or_else(|| bad(format!("source must be flatbed, feeder or duplex, not {s:?}")))?,
        };
        if !scanner.sources.contains(&source) {
            return Err(bad(format!("{} has no {} source", scanner.display_name(), source.id())));
        }
        let mode = match a.opt_str("mode")? {
            None => ColorMode::Color,
            Some(m) => ColorMode::from_id(m).ok_or_else(|| bad(format!("mode must be color, gray or bw, not {m:?}")))?,
        };
        let dpi = a.opt_int("dpi")?.unwrap_or(300);
        let range = pdfcraft_engine::scan::DPI_RANGE;
        let dpi =
            u32::try_from(dpi).ok().filter(|d| range.contains(d)).ok_or_else(|| bad(format!("dpi must be {} to {}", range.start(), range.end())))?;
        let max_pages = usize::try_from(a.opt_int("max_pages")?.unwrap_or(0).max(0)).unwrap_or(0);
        let settings = ScanSettings { dpi, mode, source, max_pages };
        let pages = self.scan_backend.scan(scanner, &settings, &mut |_| true).map_err(failed)?;
        let count = pages.len();
        let mut result = if a.opt_int("doc")?.is_some() {
            let at = match a.opt_int("at")? {
                Some(_) => self.position(a, "at")?,
                None => self.doc(a)?.info.pages.len(),
            };
            let id = self.doc(a)?.id;
            self.session.insert_scan(id, &pages, at).map_err(failed)?;
            json!({ "document": crate::summary(self.session.get(id).ok_or_else(|| failed("the document vanished"))?) })
        } else {
            let bytes = self.session.create_from_scan(&pages).map_err(failed)?;
            self.deliver(a, a.opt_str("name")?.unwrap_or("Scan.pdf"), bytes)?
        };
        result["scanner"] = json!(scanner.display_name());
        result["pages_scanned"] = json!(count);
        Ok(result)
    }
}
