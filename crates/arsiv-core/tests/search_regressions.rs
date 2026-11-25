use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arsiv_core::engine::{Engine, EnginePaths, SearchRequest};
use arsiv_core::ocr::OcrConfig;

fn engine(data: &Path) -> Arc<Engine> {
    let engine = Engine::open(
        EnginePaths {
            data_dir: data.to_path_buf(),
            pdfium_dir: data.join("no-pdfium"),
            ocr: OcrConfig {
                binary: data.join("no-tesseract"),
                tessdata: data.join("no-tessdata"),
                language: "tur".into(),
                timeout: Duration::from_secs(1),
                threads: 1,
            },
            ocr_jobs: 2,
            deskew: false,
            embed: None,
        },
        Arc::new(|_| {}),
        Arc::new(|_| {}),
    ).unwrap();
    engine.wait_ready();
    engine
}

fn request(query: &str) -> SearchRequest {
    SearchRequest { query: query.into(), kinds: vec![], modified_after: None, modified_before: None, limit: 20, offset: 0 }
}

#[test]
fn filename_search_folds_apostrophes_and_enforces_multipart_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("documents");
    std::fs::create_dir(&root).unwrap();
    for name in ["Ankara'da.txt", "report_8_foo_158_bar_00.txt", "report_8.158,00.txt", "IMG_20231117.txt"] {
        std::fs::write(root.join(name), "").unwrap();
    }
    let e = engine(&dir.path().join("data"));
    e.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    for query in ["Ankara'da", "ankarada"] {
        let response = e.search(&request(query)).unwrap();
        assert_eq!(response.total, 1, "{query}");
        assert_eq!(response.hits[0].file_name, "Ankara'da.txt");
        assert!(response.hits[0].name_only);
    }
    let numeric = e.search(&request("8.158,00")).unwrap();
    assert_eq!(numeric.total, 1);
    assert_eq!(numeric.hits[0].file_name, "report_8.158,00.txt");
    assert_eq!(e.search(&request("2023")).unwrap().hits[0].file_name, "IMG_20231117.txt");
}

#[test]
fn engine_snippets_find_tail_matches_with_bounded_windows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("documents");
    std::fs::create_dir(&root).unwrap();
    let text = "filler ".repeat(110_000) + "quartzneedle end";
    std::fs::write(root.join("tail.txt"), text).unwrap();
    let e = engine(&dir.path().join("data"));
    e.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    let response = e.search(&request("quartzneedle")).unwrap();
    assert_eq!(response.total, 1);
    let snippets = &response.hits[0].snippets;
    assert!(snippets.iter().any(|s| s.text.contains("quartzneedle") && !s.highlights.is_empty()));
}

#[test]
fn engine_tail_windows_keep_unicode_and_boundary_tokens_intact() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("documents");
    std::fs::create_dir(&root).unwrap();
    // Exercise the 512 KiB read boundary with non-ASCII context and a match
    // spanning it. A second match verifies progress to a later window.
    let mut text = "ğ ".repeat(174_760);
    text.push_str("quartzneedle ");
    text.push_str(&"ğ ".repeat(180_000));
    text.push_str("finaluniqueneedle");
    std::fs::write(root.join("unicode.txt"), text).unwrap();
    let e = engine(&dir.path().join("data"));
    e.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    for query in ["quartzneedle", "finaluniqueneedle"] {
        let response = e.search(&request(query)).unwrap();
        assert_eq!(response.total, 1);
        assert!(response.hits[0].snippets.iter().any(|s| s.text.contains(query) && !s.highlights.is_empty()), "{query}");
    }
}
