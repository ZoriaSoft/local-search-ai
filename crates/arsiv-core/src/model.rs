//! Data types shared by extraction, indexing and the Tauri boundary.
//! Field names serialize as camelCase to match docs/API.md.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Pdf,
    Image,
    Word,
    Excel,
    Text,
    /// `.log` files: plain text like `Text`, but a kind of its own because
    /// logs can be huge and noisy; new folders leave it off.
    Log,
}

impl FileKind {
    /// Every kind, in the order the UI lists them.
    pub const ALL: [FileKind; 6] =
        [FileKind::Pdf, FileKind::Image, FileKind::Word, FileKind::Excel, FileKind::Text, FileKind::Log];

    /// The default selection: every kind except `Log`. A folder with no
    /// stored selection indexes these (so folders added before `Log` existed
    /// keep exactly what they had).
    pub const DEFAULT: [FileKind; 5] = [FileKind::Pdf, FileKind::Image, FileKind::Word, FileKind::Excel, FileKind::Text];

    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Pdf => "pdf",
            FileKind::Image => "image",
            FileKind::Word => "word",
            FileKind::Excel => "excel",
            FileKind::Text => "text",
            FileKind::Log => "log",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pdf" => Some(FileKind::Pdf),
            "image" => Some(FileKind::Image),
            "word" => Some(FileKind::Word),
            "excel" => Some(FileKind::Excel),
            "text" => Some(FileKind::Text),
            "log" => Some(FileKind::Log),
            _ => None,
        }
    }

    /// Supported extensions; legacy binary .doc/.xls are deliberately excluded
    /// (no reliable pure-Rust extractor) and are reported as unsupported.
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "pdf" => Some(FileKind::Pdf),
            "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" => Some(FileKind::Image),
            "docx" => Some(FileKind::Word),
            "xlsx" | "xlsm" => Some(FileKind::Excel),
            "txt" | "csv" | "md" | "json" | "xml" => Some(FileKind::Text),
            "log" => Some(FileKind::Log),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSource {
    TextLayer,
    Ocr,
    Native,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageText {
    pub index: usize,
    pub text: String,
    pub source: TextSource,
    /// Mean OCR word confidence 0..1, only for OCR pages.
    pub confidence: Option<f32>,
    /// Set when this page could not be read (render or OCR failure); the
    /// page then carries empty text and the rest of the document still counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PageText {
    pub fn new(index: usize, text: String, source: TextSource, confidence: Option<f32>) -> Self {
        Self { index, text, source, confidence, error: None }
    }

    /// A page that could not be read; kept so page numbering stays intact.
    pub fn failed(index: usize, error: String) -> Self {
        Self { index, text: String::new(), source: TextSource::Ocr, confidence: None, error: Some(error) }
    }

    pub fn is_failed(&self) -> bool {
        self.error.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct ExtractedDocument {
    pub kind: FileKind,
    pub pages: Vec<PageText>,
    /// Tesseract language its OCR pages were read with (`None` when no page
    /// went through OCR). A file read with another language than the
    /// current setting is read again (see `Catalog::is_current`).
    pub ocr_language: Option<String>,
}

impl ExtractedDocument {
    pub fn new(kind: FileKind, pages: Vec<PageText>) -> Self {
        Self { kind, pages, ocr_language: None }
    }

    /// Pages whose text came from OCR (failed pages produced no text).
    pub fn ocr_pages(&self) -> usize {
        self.pages.iter().filter(|p| p.source == TextSource::Ocr && !p.is_failed()).count()
    }

    pub fn failed_pages(&self) -> usize {
        self.pages.iter().filter(|p| p.is_failed()).count()
    }

    pub fn low_confidence_pages(&self, floor: f32) -> usize {
        self.pages
            .iter()
            .filter(|p| is_low_confidence(p, floor))
            .count()
    }
}

/// OCR pages under the confidence floor. Failed pages are reported as failed,
/// not as low-confidence.
pub fn is_low_confidence(page: &PageText, floor: f32) -> bool {
    page.source == TextSource::Ocr && !page.is_failed() && page.confidence.map_or(true, |c| c < floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_pages_are_neither_ocr_text_nor_low_confidence() {
        let document = ExtractedDocument {
            kind: FileKind::Pdf,
            pages: vec![
                PageText::new(0, "metin".into(), TextSource::Ocr, Some(0.3)),
                PageText::failed(1, "render failed".into()),
                PageText::new(2, "katman".into(), TextSource::TextLayer, None),
            ],
            ocr_language: None,
        };
        assert_eq!(document.ocr_pages(), 1);
        assert_eq!(document.failed_pages(), 1);
        assert_eq!(document.low_confidence_pages(0.6), 1);
        assert!(!is_low_confidence(&document.pages[1], 0.6));
    }

    #[test]
    fn plain_text_extensions_and_logs_have_their_kinds() {
        let kind = |name: &str| FileKind::from_path(std::path::Path::new(name));
        for name in ["a.txt", "b.CSV", "c.md", "d.json", "e.Xml"] {
            assert_eq!(kind(name), Some(FileKind::Text), "{name}");
        }
        assert_eq!(kind("app.log"), Some(FileKind::Log));
        assert_eq!(kind("APP.LOG"), Some(FileKind::Log));
        assert_eq!(kind("x.doc"), None);
        assert_eq!(FileKind::parse("log"), Some(FileKind::Log));
        assert_eq!(FileKind::Log.as_str(), "log");
        assert!(!FileKind::DEFAULT.contains(&FileKind::Log), "logs are opt-in");
        assert!(FileKind::DEFAULT.iter().all(|k| FileKind::ALL.contains(k)));
    }
}
