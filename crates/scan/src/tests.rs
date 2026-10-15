use super::*;

fn page(n: u8) -> ScannedPage {
    ScannedPage { name: format!("page-{n}.png"), bytes: vec![n; 4], dpi: 72 }
}

#[test]
fn wia_device_lines_become_scanners() {
    let l =
        r#"{"device":{"id":"{6BDD1FC6-810F-11D0-BEC7-08002BE2092F}\\0001","name":"MFC-L2700DW","maker":"Brother","sources":["flatbed","feeder"]}}"#;
    let Some(WiaLine::Device(s)) = parse_wia_line(l) else { panic!("{l}") };
    assert_eq!(s.id, r"{6BDD1FC6-810F-11D0-BEC7-08002BE2092F}\0001");
    assert_eq!(s.sources, vec![Source::Flatbed, Source::Feeder]);
    assert_eq!(s.display_name(), "Brother MFC-L2700DW");
    // ConvertTo-Json writes a one-item array as a bare string.
    let one = r#"{"device":{"id":"x","name":"Brother MFC-L2700DW","maker":"Brother","sources":"feeder"}}"#;
    let Some(WiaLine::Device(s)) = parse_wia_line(one) else { panic!("{one}") };
    assert_eq!(s.sources, vec![Source::Feeder]);
    assert_eq!(s.display_name(), "Brother MFC-L2700DW", "the maker isn't repeated");
    // No sources at all: the glass.
    let none = r#"{"device":{"id":"x","name":"","maker":""}}"#;
    let Some(WiaLine::Device(s)) = parse_wia_line(none) else { panic!("{none}") };
    assert_eq!((s.display_name(), s.sources), ("x".to_string(), vec![Source::Flatbed]));
}

#[test]
fn wia_page_and_error_lines() {
    assert_eq!(
        parse_wia_line(r#"{"page":1,"file":"C:\\t\\page-0001.jpg","dpi":300}"#),
        Some(WiaLine::Page { file: PathBuf::from(r"C:\t\page-0001.jpg") })
    );
    let err = |kind: &str| parse_wia_line(&format!(r#"{{"error":"{kind}","code":"80210005","message":"m"}}"#));
    assert_eq!(err("offline"), Some(WiaLine::Error(ScanError::Offline)));
    assert_eq!(err("busy"), Some(WiaLine::Error(ScanError::Busy)));
    assert_eq!(err("jam"), Some(WiaLine::Error(ScanError::Jam)));
    assert_eq!(err("empty"), Some(WiaLine::Error(ScanError::FeederEmpty)));
    assert_eq!(err("cover"), Some(WiaLine::Error(ScanError::CoverOpen)));
    assert_eq!(err("cancelled"), Some(WiaLine::Error(ScanError::Cancelled)));
    assert!(matches!(err("nowia"), Some(WiaLine::Error(ScanError::Unavailable(_)))));
    assert_eq!(err("failed"), Some(WiaLine::Error(ScanError::Failed("m".into()))));
}

#[test]
fn hostile_wia_lines_are_ignored() {
    for l in ["", "not json", "{}", "[]", "null", r#"{"device":{}}"#, r#"{"device":{"id":""}}"#, r#"{"page":1}"#, r#"{"file":"x"}"#, "{\"device\":"] {
        assert_eq!(parse_wia_line(l), None, "{l}");
    }
    assert_eq!(parse_wia_line(&"[".repeat(10_000)), None, "deep nesting is refused, not recursed");
}

#[test]
fn the_wia_script_takes_its_settings_from_the_environment() {
    for var in [
        "PDFCRAFT_SCAN_OP",
        "PDFCRAFT_SCAN_DEVICE",
        "PDFCRAFT_SCAN_SOURCE",
        "PDFCRAFT_SCAN_MODE",
        "PDFCRAFT_SCAN_DPI",
        "PDFCRAFT_SCAN_MAX_PAGES",
        "PDFCRAFT_SCAN_OUT",
    ] {
        assert!(WIA_SCRIPT.contains(&format!("$env:{var}")), "{var}");
    }
    // It fits on a command line once compacted and encoded (Windows allows 32,767 characters).
    let encoded = base64(&compact_script(WIA_SCRIPT).encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
    assert!(encoded.len() < 30_000, "{}", encoded.len());
}

#[test]
fn base64_matches_rfc_4648() {
    for (input, out) in
        [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
    {
        assert_eq!(base64(input.as_bytes()), out, "{input}");
    }
}

#[test]
fn sane_device_lists_and_options() {
    let list = "escl:https://192.168.1.20:443\tBrother\tMFC-L2700DW\tflatbed scanner\nairscan:e0:Canon\tCanon\tMG3600\tip scanner\n\n";
    let found = parse_sane_devices(list);
    assert_eq!(found.len(), 2);
    assert_eq!((found[0].id.as_str(), found[0].maker.as_str(), found[0].name.as_str()), ("escl:https://192.168.1.20:443", "Brother", "MFC-L2700DW"));
    let options = parse_sane_options(
        "All options specific to device `escl:https://192.168.1.20:443':\n  Scan mode:\n    --mode Lineart|Gray|Color [Color]\n        Selects the scan mode.\n    --resolution 75|100|150|200|300|600dpi [75]\n    --source Flatbed|ADF|ADF Duplex [Flatbed]\n",
    );
    assert_eq!(options.sources, vec!["Flatbed", "ADF", "ADF Duplex"]);
    assert_eq!(options.sources(), vec![Source::Flatbed, Source::Feeder, Source::Duplex]);
    assert_eq!(options.source_value(Source::Duplex), Some("ADF Duplex"));
    assert_eq!(options.source_value(Source::Feeder), Some("ADF"));
    assert_eq!(options.mode_value(ColorMode::BlackWhite), Some("Lineart"));
    assert_eq!(options.mode_value(ColorMode::Gray), Some("Gray"));
    // Other backends' wording.
    let hp = parse_sane_options("    --source Flatbed|Automatic Document Feeder|Duplex [Flatbed]\n    --mode Black & White|Grayscale|Color\n");
    assert_eq!(hp.source_value(Source::Feeder), Some("Automatic Document Feeder"));
    assert_eq!(hp.source_value(Source::Duplex), Some("Duplex"));
    assert_eq!(hp.mode_value(ColorMode::BlackWhite), Some("Black & White"));
    assert_eq!(hp.mode_value(ColorMode::Gray), Some("Grayscale"));
    // A flatbed-only scanner lists no source option.
    assert_eq!(parse_sane_options("    --mode Color\n").sources(), vec![Source::Flatbed]);
}

#[test]
fn sane_errors_are_named() {
    assert_eq!(sane_error("scanimage: sane_start: Document feeder out of documents"), Some(ScanError::FeederEmpty));
    assert_eq!(sane_error("scanimage: sane_read: Document feeder jammed"), Some(ScanError::Jam));
    assert_eq!(sane_error("scanimage: sane_start: Scanner cover is open"), Some(ScanError::CoverOpen));
    assert_eq!(sane_error("scanimage: sane_start: Device busy"), Some(ScanError::Busy));
    assert_eq!(sane_error("scanimage: open of device x failed: Invalid argument"), Some(ScanError::Offline));
    assert_eq!(sane_error("Scanned page 1. (scanner status = 5)"), None);
}

#[test]
fn sane_device_names_that_look_like_options_are_refused() {
    assert!(sane_device_ok("escl:https://10.0.0.2:443"));
    for bad in ["", "-o/tmp/x", "a\nb", &"x".repeat(600)] {
        assert!(!sane_device_ok(bad), "{bad:?}");
    }
}

#[test]
fn simulated_flatbed_scans_one_page_and_the_feeder_all() {
    let sim = Simulated::with_pages(vec![page(1), page(2), page(3)]);
    let scanner = sim.scanners().unwrap().remove(0);
    let mut seen = Vec::new();
    let flat = sim.scan(&scanner, &ScanSettings { dpi: 300, ..Default::default() }, &mut |n| {
        seen.push(n);
        true
    });
    assert_eq!(flat.unwrap().iter().map(|p| (p.bytes[0], p.dpi)).collect::<Vec<_>>(), vec![(1, 300)]);
    assert_eq!(seen, vec![1]);
    let feed = ScanSettings { source: Source::Feeder, ..Default::default() };
    assert_eq!(sim.scan(&scanner, &feed, &mut |_| true).unwrap().len(), 3);
    assert_eq!(sim.scan(&scanner, &ScanSettings { max_pages: 2, ..feed.clone() }, &mut |_| true).unwrap().len(), 2);
    // Stopping keeps the pages so far.
    assert_eq!(sim.scan(&scanner, &feed, &mut |n| n < 2).unwrap().len(), 2);
    let empty = Simulated::with_pages(Vec::new());
    assert_eq!(empty.scan(&scanner, &feed, &mut |_| true), Err(ScanError::FeederEmpty));
    let gone = Scanner { id: "elsewhere".into(), ..scanner.clone() };
    assert_eq!(sim.scan(&gone, &feed, &mut |_| true), Err(ScanError::Offline));
}

#[test]
fn a_scan_keeps_its_pages_when_the_feeder_jams_halfway() {
    assert_eq!(finish(vec![page(1)], Some(ScanError::Jam), false, ""), Ok(vec![page(1)]));
    assert_eq!(finish(Vec::new(), Some(ScanError::Jam), false, ""), Err(ScanError::Jam));
    assert_eq!(finish(Vec::new(), None, true, ""), Err(ScanError::Cancelled));
    assert_eq!(finish(Vec::new(), None, false, "\n  driver said no\nmore"), Err(ScanError::Failed("driver said no".into())));
    // PowerShell's serialised progress stream is not a message.
    let clixml = "#< CLIXML\n<Objs Version=\"1.1.0.1\"><Obj S=\"progress\" RefId=\"0\"/></Objs>";
    assert_eq!(finish(Vec::new(), None, false, clixml), Err(ScanError::Failed("no image came back".into())));
}

#[test]
fn settings_and_ids_round_trip() {
    for s in Source::ALL {
        assert_eq!(Source::from_id(s.id()), Some(s));
    }
    for m in ColorMode::ALL {
        assert_eq!(ColorMode::from_id(m.id()), Some(m));
    }
    assert_eq!(clamped(&ScanSettings { dpi: 0, ..Default::default() }).dpi, 50);
    assert_eq!(clamped(&ScanSettings { dpi: u32::MAX, ..Default::default() }).dpi, 1200);
}

#[test]
fn work_folders_hold_only_their_own_files() {
    let dir = WorkDir::new().unwrap();
    let path = dir.0.clone();
    assert!(dir.holds(&path.join("page-0001.jpg")));
    assert!(!dir.holds(&path.join("sub").join("page.jpg")));
    assert!(!dir.holds(&std::env::temp_dir().join("page.jpg")));
    std::fs::write(path.join("page-0001.jpg"), b"x").unwrap();
    assert_eq!(read_page(&path.join("page-0001.jpg")).unwrap(), b"x");
    std::fs::write(path.join("empty.jpg"), b"").unwrap();
    assert!(read_page(&path.join("empty.jpg")).is_err());
    drop(dir);
    assert!(!path.exists(), "removed when done");
}

/// The process runner streams lines, stops the process when asked and reports a missing program.
#[test]
fn backend_processes_stream_their_lines() {
    #[cfg(windows)]
    let (prog, args): (&str, &[&str]) = ("cmd", &["/C", "echo one& echo two& echo three"]);
    #[cfg(not(windows))]
    let (prog, args): (&str, &[&str]) = ("sh", &["-c", "echo one; echo two; echo three"]);
    let mut cmd = Command::new(prog);
    cmd.args(args);
    let mut lines = Vec::new();
    run_lines(cmd, Duration::from_secs(30), |l| {
        lines.push(l.to_string());
        true
    })
    .unwrap();
    assert_eq!(lines, vec!["one", "two", "three"]);
    let mut cmd = Command::new(prog);
    cmd.args(args);
    let mut first = Vec::new();
    run_lines(cmd, Duration::from_secs(30), |l| {
        first.push(l.to_string());
        false
    })
    .unwrap();
    assert_eq!(first, vec!["one"]);
    assert!(matches!(run_lines(Command::new("pdfcraft-no-such-program"), Duration::from_secs(5), |_| true), Err(ScanError::Failed(_))));
}

/// On Windows the real script runs: listing scanners works on a machine without any.
#[cfg(windows)]
#[test]
fn the_wia_script_runs() {
    match System.scanners() {
        Ok(found) => assert!(found.iter().all(|s| !s.id.is_empty())),
        Err(e) => assert!(matches!(e, ScanError::Unavailable(_)), "{e}"),
    }
    let ghost = Scanner { id: "pdfcraft-no-such-device".into(), name: String::new(), maker: String::new(), sources: vec![Source::Flatbed] };
    assert!(matches!(System.scan(&ghost, &ScanSettings::default(), &mut |_| true), Err(ScanError::Offline | ScanError::Unavailable(_))));
}
