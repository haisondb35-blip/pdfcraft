//! pdfcraft-scan — Scan & OCR ▸ Scan (L4): pages from scanners and multifunction printers.
//!
//! The system's own scanning interface does the device work, run as a separate process so no
//! driver code is linked in and a misbehaving driver can't take the app down:
//! - **Windows:** Windows Image Acquisition through its Automation Layer (`wiaaut.dll`, part of
//!   Windows), driven by a PowerShell script ([`WIA_SCRIPT`]). Scanners and multifunction
//!   printers with a WIA driver appear, including network ones Windows found (WSD, eSCL).
//! - **Linux and BSD:** SANE's `scanimage` command-line front end, when installed.
//!
//! Each backend reports one result per line, parsed here ([`parse_wia_line`],
//! [`parse_sane_devices`], [`parse_sane_options`]) so the parsing is tested without a scanner.
//! [`Simulated`] stands in for a scanner in tests and demos.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Where a scanner takes the paper from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The glass: one page per scan.
    Flatbed,
    /// The automatic document feeder, front sides only.
    Feeder,
    /// The feeder, both sides of each sheet.
    Duplex,
}

impl Source {
    pub const ALL: [Source; 3] = [Source::Flatbed, Source::Feeder, Source::Duplex];

    pub fn id(self) -> &'static str {
        match self {
            Source::Flatbed => "flatbed",
            Source::Feeder => "feeder",
            Source::Duplex => "duplex",
        }
    }

    pub fn from_id(id: &str) -> Option<Source> {
        Source::ALL.into_iter().find(|s| s.id() == id)
    }

    /// The label in the Scan dialog.
    pub fn label(self) -> &'static str {
        match self {
            Source::Flatbed => "Flatbed",
            Source::Feeder => "Document feeder",
            Source::Duplex => "Document feeder, both sides",
        }
    }

    /// Does this source read every sheet loaded, rather than one page?
    pub fn feeds(self) -> bool {
        self != Source::Flatbed
    }
}

/// Colour mode of a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorMode {
    Color,
    Gray,
    /// Black and white (one bit per pixel): text documents.
    BlackWhite,
}

impl ColorMode {
    pub const ALL: [ColorMode; 3] = [ColorMode::Color, ColorMode::Gray, ColorMode::BlackWhite];

    pub fn id(self) -> &'static str {
        match self {
            ColorMode::Color => "color",
            ColorMode::Gray => "gray",
            ColorMode::BlackWhite => "bw",
        }
    }

    pub fn from_id(id: &str) -> Option<ColorMode> {
        ColorMode::ALL.into_iter().find(|m| m.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            ColorMode::Color => "Color",
            ColorMode::Gray => "Grayscale",
            ColorMode::BlackWhite => "Black and white",
        }
    }

    /// The image format pages come back in: JPEG keeps colour and grey scans small; black and
    /// white is PNG, which keeps the edges of text sharp and is small at one bit per pixel.
    pub fn extension(self) -> &'static str {
        if self == ColorMode::BlackWhite { "png" } else { "jpg" }
    }
}

/// Resolutions offered (dpi). Scanners round to the nearest they support.
pub const RESOLUTIONS: [u32; 5] = [150, 200, 300, 400, 600];

/// The resolution scans are clamped to.
pub const DPI_RANGE: std::ops::RangeInclusive<u32> = 50..=1200;

/// A scanner the system knows about.
#[derive(Clone, Debug, PartialEq)]
pub struct Scanner {
    /// The backend's device id (a WIA device id, or a SANE device name).
    pub id: String,
    pub name: String,
    pub maker: String,
    /// The paper sources it has; never empty.
    pub sources: Vec<Source>,
}

impl Scanner {
    /// "Maker Name", without repeating the maker when the name already starts with it.
    pub fn display_name(&self) -> String {
        let (maker, name) = (self.maker.trim(), self.name.trim());
        if maker.is_empty() || name.to_lowercase().starts_with(&maker.to_lowercase()) {
            if name.is_empty() { self.id.clone() } else { name.to_string() }
        } else {
            format!("{maker} {name}")
        }
    }
}

/// What to scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanSettings {
    pub dpi: u32,
    pub mode: ColorMode,
    pub source: Source,
    /// Stop after this many pages from the feeder (0 = every sheet loaded).
    pub max_pages: usize,
}

impl Default for ScanSettings {
    fn default() -> Self {
        ScanSettings { dpi: 300, mode: ColorMode::Color, source: Source::Flatbed, max_pages: 0 }
    }
}

/// A scanned page: an image file's bytes (JPEG or PNG) and its resolution.
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedPage {
    /// A file name for the page (`page-0001.jpg`); its extension names the format.
    pub name: String,
    pub bytes: Vec<u8>,
    pub dpi: u32,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ScanError {
    /// Shown to end users; says what to install.
    #[error("{0}")]
    Unavailable(String),
    #[error("no scanner was found; check that it is turned on and connected, and that its driver is installed")]
    NoScanner,
    #[error("the scanner is turned off or not connected")]
    Offline,
    #[error("the scanner is busy; try again when it has finished")]
    Busy,
    #[error("the paper is jammed in the scanner")]
    Jam,
    #[error("there is no paper in the document feeder")]
    FeederEmpty,
    #[error("the scanner's cover is open")]
    CoverOpen,
    #[error("scanning was cancelled")]
    Cancelled,
    #[error("the scanner did not respond in time")]
    TimedOut,
    #[error("scanning failed: {0}")]
    Failed(String),
}

/// A way to reach scanners. [`system`] is the real one; [`Simulated`] serves fixed pages.
pub trait Backend: Send + Sync {
    /// The scanners connected now. Slow (a second or more): call it when the user asks.
    fn scanners(&self) -> Result<Vec<Scanner>, ScanError>;

    /// Scan with `settings`. `progress(pages)` is called after each page; returning `false`
    /// stops the scan, keeping the pages so far (none yet: [`ScanError::Cancelled`]).
    fn scan(&self, scanner: &Scanner, settings: &ScanSettings, progress: &mut dyn FnMut(usize) -> bool) -> Result<Vec<ScannedPage>, ScanError>;
}

/// The system's scanning backend: WIA on Windows, SANE elsewhere.
pub fn system() -> std::sync::Arc<dyn Backend> {
    std::sync::Arc::new(System)
}

/// The system backend (see the crate docs).
#[derive(Clone, Copy, Debug, Default)]
pub struct System;

impl Backend for System {
    fn scanners(&self) -> Result<Vec<Scanner>, ScanError> {
        #[cfg(windows)]
        {
            wia_scanners()
        }
        #[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
        {
            sane_scanners()
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))))]
        {
            Err(unsupported())
        }
    }

    fn scan(&self, scanner: &Scanner, settings: &ScanSettings, progress: &mut dyn FnMut(usize) -> bool) -> Result<Vec<ScannedPage>, ScanError> {
        let settings = clamped(settings);
        #[cfg(windows)]
        {
            wia_scan(scanner, &settings, progress)
        }
        #[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
        {
            sane_scan(scanner, &settings, progress)
        }
        #[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))))]
        {
            let _ = (scanner, settings, progress);
            Err(unsupported())
        }
    }
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))))]
fn unsupported() -> ScanError {
    ScanError::Unavailable("scanning isn't available on this system yet".into())
}

/// Settings with the resolution inside [`DPI_RANGE`].
fn clamped(s: &ScanSettings) -> ScanSettings {
    ScanSettings { dpi: s.dpi.clamp(*DPI_RANGE.start(), *DPI_RANGE.end()), ..s.clone() }
}

/// A stand-in scanner serving fixed pages, for tests and demos.
#[derive(Clone, Debug, Default)]
pub struct Simulated {
    pub scanners: Vec<Scanner>,
    /// The pages one scan returns: the first only from the flatbed, all of them from the feeder
    /// (an empty list behaves like an empty feeder).
    pub pages: Vec<ScannedPage>,
    /// Fail every scan with this.
    pub error: Option<ScanError>,
}

impl Simulated {
    /// One scanner with every source, serving `pages`.
    pub fn with_pages(pages: Vec<ScannedPage>) -> Simulated {
        let scanner = Scanner { id: "simulated".into(), name: "Simulated Scanner".into(), maker: String::new(), sources: Source::ALL.to_vec() };
        Simulated { scanners: vec![scanner], pages, error: None }
    }
}

impl Backend for Simulated {
    fn scanners(&self) -> Result<Vec<Scanner>, ScanError> {
        Ok(self.scanners.clone())
    }

    fn scan(&self, scanner: &Scanner, settings: &ScanSettings, progress: &mut dyn FnMut(usize) -> bool) -> Result<Vec<ScannedPage>, ScanError> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        if !self.scanners.iter().any(|s| s.id == scanner.id) {
            return Err(ScanError::Offline);
        }
        if !scanner.sources.contains(&settings.source) {
            return Err(ScanError::Failed(format!("the scanner has no {}", settings.source.label().to_lowercase())));
        }
        let wanted = if !settings.source.feeds() {
            1
        } else if settings.max_pages == 0 {
            usize::MAX
        } else {
            settings.max_pages
        };
        let mut out = Vec::new();
        for page in self.pages.iter().take(wanted) {
            out.push(ScannedPage { dpi: settings.dpi, ..page.clone() });
            if !progress(out.len()) {
                break;
            }
        }
        if out.is_empty() {
            return Err(if settings.source.feeds() { ScanError::FeederEmpty } else { ScanError::Failed("no image came back".into()) });
        }
        Ok(out)
    }
}

/// No page for this long means the scanner has stopped answering (a feeder page takes seconds;
/// a 600 dpi colour flatbed scan on a slow USB scanner can take a minute or two).
const PAGE_TIMEOUT: Duration = Duration::from_secs(300);
/// Listing devices is quicker.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// A page image larger than this is refused (a 1200 dpi colour A3 PNG is about 300 MB).
const MAX_PAGE_BYTES: u64 = 512 << 20;
/// At most this many pages per scan (a feeder holds 50-100 sheets).
const MAX_PAGES: usize = 2000;
/// Lines from a backend longer than this are cut (a device list or a page path is short).
const MAX_LINE: usize = 64 << 10;

/// One result line from a running backend process, or the end.
enum Out {
    Line(String),
    Done(String),
}

/// Run `cmd`, passing each stdout line to `line` as it arrives (it returns `false` to stop the
/// process). Returns the process's standard error. `timeout` bounds the wait for each line.
fn run_lines(mut cmd: Command, timeout: Duration, mut line: impl FnMut(&str) -> bool) -> Result<String, ScanError> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_window(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| ScanError::Failed(e.to_string()))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        return Err(ScanError::Failed("no output from the scanning process".into()));
    };
    let (tx, rx) = mpsc::channel();
    let err_tx = tx.clone();
    // Standard error is read on its own thread so a chatty backend can't block on a full pipe.
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.take(MAX_LINE as u64).read_to_string(&mut text);
        let _ = err_tx.send(Out::Done(text));
    });
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut buf = Vec::new();
            match reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // A line cut at the cap: drop the rest of it.
                    if !buf.ends_with(b"\n") {
                        let _ = reader.by_ref().skip_until(b'\n');
                    }
                    if tx.send(Out::Line(String::from_utf8_lossy(&buf).trim().to_string())).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut stopped = false;
    let mut stderr_text = None;
    loop {
        match rx.recv_timeout(timeout) {
            Ok(Out::Line(l)) => {
                if !l.is_empty() && !line(&l) {
                    stopped = true;
                    let _ = child.kill();
                    break;
                }
            }
            Ok(Out::Done(e)) => stderr_text = Some(e),
            // Both readers have finished: the process closed its output.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ScanError::TimedOut);
            }
        }
    }
    let _ = child.wait();
    if stopped {
        return Ok(String::new());
    }
    Ok(stderr_text.unwrap_or_default())
}

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    /// CREATE_NO_WINDOW: no console window flashes up for the helper process.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_window(_: &mut Command) {}

/// Read a page image the backend wrote, within [`MAX_PAGE_BYTES`].
fn read_page(path: &Path) -> Result<Vec<u8>, ScanError> {
    let f = std::fs::File::open(path).map_err(|e| ScanError::Failed(format!("reading the scanned page: {e}")))?;
    let mut bytes = Vec::new();
    f.take(MAX_PAGE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| ScanError::Failed(format!("reading the scanned page: {e}")))?;
    if bytes.len() as u64 > MAX_PAGE_BYTES {
        return Err(ScanError::Failed("the scanned page is too large".into()));
    }
    if bytes.is_empty() {
        return Err(ScanError::Failed("the scanner returned an empty page".into()));
    }
    Ok(bytes)
}

/// A private folder for one scan's page files, removed when dropped.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> Result<WorkDir, ScanError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("pdfcraft-scan-{}-{stamp}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).map_err(|e| ScanError::Failed(format!("creating a folder for the scan: {e}")))?;
        Ok(WorkDir(dir))
    }

    /// Is `path` a file directly inside this folder (so a backend can't point us elsewhere)?
    fn holds(&self, path: &Path) -> bool {
        path.parent().is_some_and(|p| p == self.0) && path.file_name().is_some()
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Windows: WIA

/// The PowerShell script that drives WIA. Settings come in `PDFCRAFT_SCAN_*` environment
/// variables (never spliced into the script); results come out as JSON lines ([`parse_wia_line`]).
pub const WIA_SCRIPT: &str = include_str!("wia.ps1");

/// One line of [`WIA_SCRIPT`]'s output.
#[derive(Clone, Debug, PartialEq)]
pub enum WiaLine {
    Device(Scanner),
    Page { file: PathBuf },
    Error(ScanError),
}

/// Parse a line of [`WIA_SCRIPT`]'s output; `None` for anything else.
pub fn parse_wia_line(line: &str) -> Option<WiaLine> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();
    if let Some(d) = v.get("device") {
        let id = text(d, "id");
        if id.is_empty() {
            return None;
        }
        // ConvertTo-Json writes a one-item array as a bare string.
        let sources: Vec<Source> = match d.get("sources") {
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|s| s.as_str().and_then(Source::from_id)).collect(),
            Some(serde_json::Value::String(s)) => Source::from_id(s).into_iter().collect(),
            _ => Vec::new(),
        };
        let sources = if sources.is_empty() { vec![Source::Flatbed] } else { sources };
        return Some(WiaLine::Device(Scanner { id, name: text(d, "name"), maker: text(d, "maker"), sources }));
    }
    if let Some(kind) = v.get("error").and_then(|e| e.as_str()) {
        let message = text(&v, "message");
        let e = match kind {
            "offline" => ScanError::Offline,
            "busy" => ScanError::Busy,
            "jam" => ScanError::Jam,
            "empty" => ScanError::FeederEmpty,
            "cover" => ScanError::CoverOpen,
            "cancelled" => ScanError::Cancelled,
            "nowia" => ScanError::Unavailable("Windows Image Acquisition isn't available on this computer".into()),
            _ => ScanError::Failed(if message.is_empty() { "the scanner reported an error".into() } else { message }),
        };
        return Some(WiaLine::Error(e));
    }
    let file = text(&v, "file");
    (v.get("page").is_some() && !file.is_empty()).then(|| WiaLine::Page { file: PathBuf::from(file) })
}

#[cfg(windows)]
fn powershell(env: &[(&str, &str)]) -> Command {
    let exe = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join(r"System32\WindowsPowerShell\v1.0\powershell.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"));
    let mut cmd = Command::new(exe);
    // -EncodedCommand takes the script as Base64 UTF-16LE: no temporary script file, no quoting.
    let utf16: Vec<u8> = WIA_SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"]).arg(base64(&utf16));
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

#[cfg(windows)]
fn wia_scanners() -> Result<Vec<Scanner>, ScanError> {
    let mut found = Vec::new();
    let mut error = None;
    let stderr = run_lines(powershell(&[("PDFCRAFT_SCAN_OP", "list")]), LIST_TIMEOUT, |l| {
        match parse_wia_line(l) {
            Some(WiaLine::Device(s)) => found.push(s),
            Some(WiaLine::Error(e)) => error = Some(e),
            _ => {}
        }
        true
    })?;
    if let Some(e) = error {
        return Err(e);
    }
    if found.is_empty() && !readable(&stderr).is_empty() {
        return Err(ScanError::Failed(first_line(&stderr)));
    }
    Ok(found)
}

#[cfg(windows)]
fn wia_scan(scanner: &Scanner, settings: &ScanSettings, progress: &mut dyn FnMut(usize) -> bool) -> Result<Vec<ScannedPage>, ScanError> {
    let dir = WorkDir::new()?;
    let out = dir.0.to_string_lossy().to_string();
    let (dpi, max) = (settings.dpi.to_string(), settings.max_pages.min(MAX_PAGES).to_string());
    let env = [
        ("PDFCRAFT_SCAN_OP", "scan"),
        ("PDFCRAFT_SCAN_DEVICE", scanner.id.as_str()),
        ("PDFCRAFT_SCAN_SOURCE", settings.source.id()),
        ("PDFCRAFT_SCAN_MODE", settings.mode.id()),
        ("PDFCRAFT_SCAN_DPI", dpi.as_str()),
        ("PDFCRAFT_SCAN_MAX_PAGES", max.as_str()),
        ("PDFCRAFT_SCAN_OUT", out.as_str()),
    ];
    let mut pages = Vec::new();
    let mut error = None;
    let mut cancelled = false;
    let stderr = run_lines(powershell(&env), PAGE_TIMEOUT, |l| match parse_wia_line(l) {
        Some(WiaLine::Page { file }) => {
            if !dir.holds(&file) {
                error = Some(ScanError::Failed("the scanner wrote its page somewhere unexpected".into()));
                return false;
            }
            match read_page(&file) {
                Ok(bytes) => {
                    let name = file
                        .file_name()
                        .map_or_else(|| format!("page-{}.{}", pages.len() + 1, settings.mode.extension()), |n| n.to_string_lossy().to_string());
                    pages.push(ScannedPage { name, bytes, dpi: settings.dpi });
                }
                Err(e) => {
                    error = Some(e);
                    return false;
                }
            }
            let go = pages.len() < MAX_PAGES && progress(pages.len());
            cancelled = !go;
            go
        }
        Some(WiaLine::Error(e)) => {
            error = Some(e);
            true
        }
        _ => true,
    })?;
    finish(pages, error, cancelled, &stderr)
}

/// The result of a scan from what came back.
fn finish(pages: Vec<ScannedPage>, error: Option<ScanError>, cancelled: bool, stderr: &str) -> Result<Vec<ScannedPage>, ScanError> {
    match (pages.is_empty(), error) {
        // Pages and then a problem (a jam halfway through the feeder): keep what was scanned.
        (false, _) => Ok(pages),
        (true, Some(e)) => Err(e),
        (true, None) if cancelled => Err(ScanError::Cancelled),
        (true, None) if readable(stderr).is_empty() => Err(ScanError::Failed("no image came back".into())),
        (true, None) => Err(ScanError::Failed(first_line(stderr))),
    }
}

/// Standard error worth showing: PowerShell serialises its other streams there as CLIXML when
/// its output is redirected, which is not a message.
fn readable(stderr: &str) -> &str {
    let s = stderr.trim();
    if s.starts_with("#< CLIXML") { "" } else { s }
}

fn first_line(text: &str) -> String {
    let line = readable(text).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("the scanner reported an error");
    line.chars().take(300).collect()
}

/// Standard Base64 (RFC 4648), for `-EncodedCommand`.
#[cfg_attr(not(windows), allow(dead_code))]
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sym = |i: u32| char::from(A[(i & 63) as usize]);
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| chunk.get(i).copied().map_or(0, u32::from);
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        out.push(if chunk.len() > 1 { sym(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(n) } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Linux and BSD: SANE

/// The `--formatted-device-list` format asked of `scanimage -L`: id, vendor, model, type.
pub const SANE_LIST_FORMAT: &str = "%d\t%v\t%m\t%t%n";

/// Parse `scanimage -L -f` output ([`SANE_LIST_FORMAT`]); sources are filled in later.
pub fn parse_sane_devices(text: &str) -> Vec<Scanner> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim();
            if id.is_empty() {
                return None;
            }
            let maker = f.next().unwrap_or_default().trim().to_string();
            let name = f.next().unwrap_or_default().trim().to_string();
            Some(Scanner { id: id.to_string(), name, maker, sources: vec![Source::Flatbed] })
        })
        .collect()
}

/// The choices a device offers for its `--source` and `--mode` options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SaneOptions {
    pub sources: Vec<String>,
    pub modes: Vec<String>,
}

/// Parse `scanimage -d DEVICE -A` (the device's options), lines such as
/// `    --source Flatbed|ADF|ADF Duplex [Flatbed]`.
pub fn parse_sane_options(text: &str) -> SaneOptions {
    let choices = |l: &str, opt: &str| -> Option<Vec<String>> {
        let rest = l.trim_start().strip_prefix(opt)?.strip_prefix(' ')?;
        // The current value follows in brackets; the choices are before it.
        let list = rest.rfind(" [").map_or(rest, |i| rest.get(..i).unwrap_or(rest));
        Some(list.split('|').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect())
    };
    let mut o = SaneOptions::default();
    for l in text.lines() {
        if let Some(c) = choices(l, "--source") {
            o.sources = c;
        } else if let Some(c) = choices(l, "--mode") {
            o.modes = c;
        }
    }
    o
}

impl SaneOptions {
    /// The device's name for `source`, if it has one.
    pub fn source_value(&self, source: Source) -> Option<&str> {
        let is_duplex = |c: &str| c.to_lowercase().contains("duplex");
        let is_feeder = |c: &str| {
            let c = c.to_lowercase();
            c.contains("adf") || c.contains("feeder") || c.contains("automatic document")
        };
        self.sources.iter().map(String::as_str).find(|c| match source {
            Source::Duplex => is_duplex(c),
            Source::Feeder => is_feeder(c) && !is_duplex(c),
            Source::Flatbed => !is_feeder(c) && !is_duplex(c),
        })
    }

    /// The paper sources the device has.
    pub fn sources(&self) -> Vec<Source> {
        let s: Vec<Source> = Source::ALL.into_iter().filter(|s| self.source_value(*s).is_some()).collect();
        if s.is_empty() { vec![Source::Flatbed] } else { s }
    }

    /// The device's name for `mode`, if it has one.
    pub fn mode_value(&self, mode: ColorMode) -> Option<&str> {
        let wanted: &[&str] = match mode {
            ColorMode::Color => &["color", "colour", "24bit color"],
            ColorMode::Gray => &["gray", "grey", "grayscale", "greyscale", "8bit gray"],
            ColorMode::BlackWhite => &["lineart", "binary", "black & white", "black and white", "halftone"],
        };
        self.modes.iter().map(String::as_str).find(|m| wanted.contains(&m.to_lowercase().as_str()))
    }
}

/// A SANE device name is passed as one argument, but refuse ones that look like options.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn sane_device_ok(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('-') && id.len() <= 512 && !id.contains(['\n', '\r', '\0'])
}

#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn scanimage() -> Command {
    Command::new(std::env::var_os("PDFCRAFT_SCANIMAGE").unwrap_or_else(|| "scanimage".into()))
}

#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn sane_missing(e: &ScanError) -> ScanError {
    match e {
        ScanError::Failed(m) if m.contains("No such file") || m.contains("not found") => ScanError::Unavailable(
            "scanning needs SANE: install the sane-utils (Debian, Ubuntu) or sane-backends (Fedora, Arch, FreeBSD) package".into(),
        ),
        other => other.clone(),
    }
}

#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn sane_device_options(id: &str) -> SaneOptions {
    let mut cmd = scanimage();
    cmd.args(["-d", id, "-A"]);
    let mut text = String::new();
    let _ = run_lines(cmd, LIST_TIMEOUT, |l| {
        text.push_str(l);
        text.push('\n');
        true
    });
    parse_sane_options(&text)
}

#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn sane_scanners() -> Result<Vec<Scanner>, ScanError> {
    let mut cmd = scanimage();
    cmd.args(["-L", "-f", SANE_LIST_FORMAT]);
    let mut text = String::new();
    run_lines(cmd, LIST_TIMEOUT, |l| {
        text.push_str(l);
        text.push('\n');
        true
    })
    .map_err(|e| sane_missing(&e))?;
    let mut found = parse_sane_devices(&text);
    found.retain(|s| sane_device_ok(&s.id));
    for s in &mut found {
        s.sources = sane_device_options(&s.id).sources();
    }
    Ok(found)
}

#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn sane_scan(scanner: &Scanner, settings: &ScanSettings, progress: &mut dyn FnMut(usize) -> bool) -> Result<Vec<ScannedPage>, ScanError> {
    if !sane_device_ok(&scanner.id) {
        return Err(ScanError::Offline);
    }
    let options = sane_device_options(&scanner.id);
    let dir = WorkDir::new()?;
    let ext = settings.mode.extension();
    let format = if ext == "png" { "png" } else { "jpeg" };
    let mut cmd = scanimage();
    cmd.arg("-d").arg(&scanner.id);
    cmd.arg(format!("--format={format}")).arg(format!("--resolution={}", settings.dpi));
    if let Some(m) = options.mode_value(settings.mode) {
        cmd.arg("--mode").arg(m);
    }
    match options.source_value(settings.source) {
        Some(s) => {
            cmd.arg("--source").arg(s);
        }
        None if settings.source.feeds() => return Err(ScanError::Failed(format!("the scanner has no {}", settings.source.label().to_lowercase()))),
        None => {}
    }
    // --batch-print names each page file on stdout once it is written.
    cmd.arg(format!("--batch={}", dir.0.join(format!("page-%04d.{ext}")).to_string_lossy())).arg("--batch-print");
    let count = if settings.source.feeds() { settings.max_pages.min(MAX_PAGES) } else { 1 };
    if count > 0 {
        cmd.arg(format!("--batch-count={count}"));
    }
    let mut pages = Vec::new();
    let mut error = None;
    let mut cancelled = false;
    let stderr = run_lines(cmd, PAGE_TIMEOUT, |l| {
        let file = PathBuf::from(l);
        if !dir.holds(&file) {
            return true;
        }
        match read_page(&file) {
            Ok(bytes) => pages.push(ScannedPage { name: format!("page-{:04}.{ext}", pages.len() + 1), bytes, dpi: settings.dpi }),
            Err(e) => {
                error = Some(e);
                return false;
            }
        }
        let go = pages.len() < MAX_PAGES && progress(pages.len());
        cancelled = !go;
        go
    })
    .map_err(|e| sane_missing(&e))?;
    let error = error.or_else(|| sane_error(&stderr));
    finish(pages, error, cancelled, &stderr)
}

/// The error in `scanimage`'s standard error, if it names one SANE reports.
pub fn sane_error(stderr: &str) -> Option<ScanError> {
    let s = stderr.to_lowercase();
    if s.contains("out of documents") || s.contains("no documents") {
        Some(ScanError::FeederEmpty)
    } else if s.contains("jammed") {
        Some(ScanError::Jam)
    } else if s.contains("cover is open") {
        Some(ScanError::CoverOpen)
    } else if s.contains("device busy") {
        Some(ScanError::Busy)
    } else if (s.contains("invalid argument") && s.contains("open of device")) || s.contains("no scanners were identified") {
        Some(ScanError::Offline)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
