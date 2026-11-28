//! Formats beyond PDF/images/DOCX/XLSX/plain text: RTF, HTML, OpenDocument
//! (ODT/ODS/ODP), PowerPoint (PPTX), legacy Excel (XLS), Word/PowerPoint
//! 97-2003 (DOC/PPT), UYAP documents (UDF), e-Yazışma packages (EYP) and saved
//! e-mails (EML, Outlook MSG).
//!
//! Containers (EYP, EML, MSG) carry other documents. Each carried file is written
//! to the scratch folder and read with the normal per-kind reader, one level
//! deep: a package inside a package is not opened.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use super::{check_cancel, docx_xml_text, office_member, open_office_zip, read_bounded, unique, Extractor, OcrSetup, TextOutput, OFFICE_LIMITS};
use crate::model::{FileKind, PageText, TextSource};

/// EYP packages and e-mails carry whole documents (scans, attachments).
pub(crate) const MAX_CONTAINER_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// One carried file is read into memory before it is written to scratch.
const MAX_CARRIED_FILE_BYTES: usize = 128 * 1024 * 1024;
/// All carried files of one container together.
const MAX_CARRIED_TOTAL_BYTES: usize = 384 * 1024 * 1024;
/// Carried files read per container; the rest are skipped.
const MAX_CARRIED_FILES: usize = 64;
/// Package metadata XML (Ustveri, BelgeHedef, docProps) per part.
const MAX_METADATA_XML_BYTES: usize = 4 * 1024 * 1024;
/// A metadata value longer than this is a digest or signature, not text.
const MAX_METADATA_VALUE_CHARS: usize = 2000;

fn file_bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let size = std::fs::metadata(path).with_context(|| format!("Cannot read {}", path.display()))?.len();
    if size > limit {
        bail!("File exceeds the {} MiB input limit", limit / (1024 * 1024));
    }
    std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))
}

fn one_page(text: String) -> Vec<PageText> {
    vec![PageText::new(0, text, TextSource::Native, None)]
}

/// Collapses runs of blank lines and trims line ends.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------- RTF

pub(super) fn rtf(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    check_cancel(cancel)?;
    let bytes = file_bytes(path, super::MAX_OFFICE_FILE_BYTES)?;
    if !bytes.starts_with(b"{\\rtf") {
        bail!("Not an RTF document");
    }
    let text = rtf_to_text(&bytes);
    check_cancel(cancel)?;
    Ok(one_page(tidy(&text)))
}

#[derive(Clone, Copy)]
struct RtfGroup {
    skip: bool,
    uc: usize,
}

struct RtfText {
    out: String,
    pending: Vec<u8>,
    encoding: &'static encoding_rs::Encoding,
    /// Fallback characters still to drop after a `\uN`.
    fallback: usize,
}

impl RtfText {
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            self.out.push_str(&self.encoding.decode_without_bom_handling(&self.pending).0);
            self.pending.clear();
        }
    }

    fn text(&mut self, text: &str) {
        self.flush();
        self.out.push_str(text);
    }

    /// One character of the document's code page (a plain byte or `\'hh`).
    fn byte(&mut self, byte: u8) {
        if self.fallback > 0 {
            self.fallback -= 1;
        } else {
            self.pending.push(byte);
        }
    }
}

/// Destinations whose text is not document text.
const RTF_SKIPPED: &[&[u8]] = &[
    b"fonttbl", b"colortbl", b"stylesheet", b"info", b"pict", b"object", b"themedata", b"colorschememapping",
    b"latentstyles", b"datastore", b"listtable", b"listoverridetable", b"revtbl", b"rsidtbl", b"generator",
    b"xmlnstbl", b"filetbl", b"fldinst", b"bkmkstart", b"bkmkend", b"datafield", b"objdata", b"shppict",
    b"nonshppict", b"blipuid", b"mmathPr", b"pgdsctbl", b"protusertbl", b"wgrffmtfilter", b"defchp", b"defpap",
];

/// RTF to plain text: groups, destinations, `\'hh` bytes in the document's
/// ANSI code page (Windows-1254 unless `\ansicpg` says otherwise), `\uN`
/// characters with their fallback, paragraph and cell breaks.
pub(crate) fn rtf_to_text(data: &[u8]) -> String {
    const TEXT_LIMIT: usize = 16 * 1024 * 1024;
    let mut state = RtfText { out: String::new(), pending: Vec::new(), encoding: encoding_rs::WINDOWS_1254, fallback: 0 };
    let mut stack: Vec<RtfGroup> = Vec::new();
    let mut group = RtfGroup { skip: false, uc: 1 };
    let mut i = 0;
    while i < data.len() && state.out.len() < TEXT_LIMIT {
        match data[i] {
            b'{' => {
                state.flush();
                stack.push(group);
                i += 1;
                if data[i..].starts_with(b"\\*") {
                    group.skip = true;
                }
            }
            b'}' => {
                state.flush();
                group = stack.pop().unwrap_or(RtfGroup { skip: false, uc: 1 });
                state.fallback = 0;
                i += 1;
            }
            b'\\' => {
                i += 1;
                let Some(&next) = data.get(i) else { break };
                if next.is_ascii_alphabetic() {
                    let start = i;
                    while i < data.len() && data[i].is_ascii_alphabetic() && i - start < 32 {
                        i += 1;
                    }
                    let word = &data[start..i];
                    let negative = data.get(i) == Some(&b'-');
                    if negative {
                        i += 1;
                    }
                    let digits = i;
                    while i < data.len() && data[i].is_ascii_digit() && i - digits < 10 {
                        i += 1;
                    }
                    let param = if i > digits {
                        let value: i64 = std::str::from_utf8(&data[digits..i]).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                        Some(if negative { -value } else { value })
                    } else {
                        if negative {
                            i -= 1;
                        }
                        None
                    };
                    if data.get(i) == Some(&b' ') {
                        i += 1;
                    }
                    if word == b"bin" {
                        i = i.saturating_add(param.unwrap_or(0).max(0) as usize);
                        continue;
                    }
                    if word == b"ansicpg" {
                        if let Some(page) = param {
                            if let Some(encoding) = encoding_rs::Encoding::for_label(format!("windows-{page}").as_bytes()) {
                                state.flush();
                                state.encoding = encoding;
                            }
                        }
                        continue;
                    }
                    if word == b"uc" {
                        group.uc = param.unwrap_or(1).clamp(0, 8) as usize;
                        continue;
                    }
                    if RTF_SKIPPED.contains(&word) {
                        group.skip = true;
                        continue;
                    }
                    if group.skip {
                        continue;
                    }
                    match word {
                        b"par" | b"line" | b"sect" | b"page" | b"row" => state.text("\n"),
                        b"tab" | b"cell" => state.text("\t"),
                        b"emdash" => state.text("\u{2014}"),
                        b"endash" => state.text("\u{2013}"),
                        b"bullet" => state.text("\u{2022}"),
                        b"lquote" => state.text("\u{2018}"),
                        b"rquote" => state.text("\u{2019}"),
                        b"ldblquote" => state.text("\u{201C}"),
                        b"rdblquote" => state.text("\u{201D}"),
                        b"u" => {
                            if let Some(value) = param {
                                let code = if value < 0 { value + 65536 } else { value };
                                if let Some(ch) = u32::try_from(code).ok().and_then(char::from_u32) {
                                    let mut buffer = [0; 4];
                                    state.text(ch.encode_utf8(&mut buffer));
                                }
                                state.fallback = group.uc;
                            }
                        }
                        _ => {}
                    }
                } else {
                    i += 1;
                    match next {
                        b'\'' => {
                            let hex = data.get(i..i + 2).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
                            i += 2;
                            if let (Some(byte), false) = (hex, group.skip) {
                                state.byte(byte);
                            }
                        }
                        b'\\' | b'{' | b'}' if !group.skip => state.byte(next),
                        b'~' if !group.skip => state.text("\u{00A0}"),
                        b'_' if !group.skip => state.text("-"),
                        b'\n' | b'\r' if !group.skip => state.text("\n"),
                        b'*' => group.skip = true,
                        _ => {}
                    }
                }
            }
            b'\r' | b'\n' => i += 1,
            byte => {
                if !group.skip {
                    state.byte(byte);
                }
                i += 1;
            }
        }
    }
    state.flush();
    state.out
}

// ---------------------------------------------------------------- HTML

pub(super) fn html(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    check_cancel(cancel)?;
    let bytes = file_bytes(path, super::MAX_TEXT_FILE_BYTES)?;
    Ok(one_page(html_to_text(&super::decode_text(&bytes))))
}

const HTML_BLOCKS: &[&str] = &[
    "p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "table", "section", "article", "header",
    "footer", "ul", "ol", "title", "blockquote", "pre", "hr", "dt", "dd", "dl", "address", "main", "nav", "aside",
    "form", "fieldset", "figure", "figcaption", "caption", "thead", "tbody", "tfoot",
];

fn html_entity(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ccedil" => 'ç',
        "Ccedil" => 'Ç',
        "ouml" => 'ö',
        "Ouml" => 'Ö',
        "uuml" => 'ü',
        "Uuml" => 'Ü',
        "scedil" => 'ş',
        "Scedil" => 'Ş',
        "gbreve" => 'ğ',
        "Gbreve" => 'Ğ',
        "imath" | "inodot" => 'ı',
        "Idot" => 'İ',
        "acirc" => 'â',
        "icirc" => 'î',
        "ucirc" => 'û',
        "euro" => '€',
        "copy" => '©',
        "reg" => '®',
        "deg" => '°',
        "middot" => '·',
        "hellip" => '…',
        "ndash" => '–',
        "mdash" => '—',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "times" => '×',
        _ => return None,
    })
}

/// HTML to text: script/style/comments dropped, block elements become line
/// breaks, table cells tabs, entities decoded, whitespace collapsed.
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut space = false;
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < html.len() {
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |end| end + 3);
            continue;
        }
        if bytes[i] == b'<' {
            let Some(end) = rest.find('>') else { break };
            let tag = rest[1..end].trim_start_matches('/').trim_start();
            let name: String = tag
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect();
            i += end + 1;
            if (name == "script" || name == "style" || name == "noscript") && !rest[1..].starts_with('/') {
                let close = format!("</{name}");
                i = lower[i..].find(&close).map_or(html.len(), |at| i + at);
                continue;
            }
            if name == "td" || name == "th" {
                if !rest[1..].starts_with('/') && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\t');
                }
                space = false;
            } else if HTML_BLOCKS.contains(&name.as_str()) {
                out.push('\n');
                space = false;
            } else if !name.is_empty() {
                space = !out.is_empty() && !out.ends_with(['\n', '\t', ' ']) && space;
            }
            continue;
        }
        let ch = rest.chars().next().unwrap();
        if ch == '&' {
            if let Some(end) = rest[1..].find(';').filter(|&end| end <= 10) {
                if let Some(decoded) = html_entity(&rest[1..1 + end]) {
                    if decoded == ' ' {
                        space = true;
                    } else {
                        if space && !out.is_empty() && !out.ends_with(['\n', '\t', ' ']) {
                            out.push(' ');
                        }
                        space = false;
                        out.push(decoded);
                    }
                    i += end + 2;
                    continue;
                }
            }
        }
        if ch.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() && !out.ends_with(['\n', '\t', ' ']) {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
        i += ch.len_utf8();
    }
    tidy(&out)
}

// ---------------------------------------------------------------- OpenDocument

/// ODT (one page) and ODP (one page per slide) from `content.xml`.
pub(super) fn odf(path: &Path, cancel: &AtomicBool, slides: bool) -> Result<Vec<PageText>> {
    let mut archive = open_office_zip(path, cancel)?;
    let member = archive.by_name("content.xml").context("OpenDocument file has no content.xml")?;
    let size = member.size();
    let mut remaining = OFFICE_LIMITS.expanded_bytes;
    let bytes = office_member(member, size, &mut remaining, OFFICE_LIMITS, cancel)?;
    let xml = std::str::from_utf8(&bytes).context("OpenDocument XML is not UTF-8")?;
    let pages = odf_xml_pages(xml, slides, cancel)?;
    Ok(pages.into_iter().enumerate().map(|(i, text)| PageText::new(i, tidy(&text), TextSource::Native, None)).collect())
}

fn push_general_ref(reference: &quick_xml::events::BytesRef<'_>, out: &mut TextOutput) -> Result<()> {
    if let Some(ch) = reference.resolve_char_ref()? {
        return out.push(ch.encode_utf8(&mut [0; 4]));
    }
    match reference.as_ref() {
        "amp" => out.push("&"),
        "lt" => out.push("<"),
        "gt" => out.push(">"),
        "quot" => out.push("\""),
        "apos" => out.push("'"),
        _ => Ok(()),
    }
}

fn odf_xml_pages(xml: &str, slides: bool, cancel: &AtomicBool) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    let mut out = TextOutput::new(OFFICE_LIMITS.text_bytes);
    let mut pages = Vec::new();
    let mut in_body = false;
    loop {
        check_cancel(cancel)?;
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == "body" => in_body = true,
            Event::End(e) if e.local_name().as_ref() == "body" => in_body = false,
            Event::Text(t) if in_body => out.push(&t.xml10_content())?,
            Event::CData(t) if in_body => out.push(&t.into_inner())?,
            Event::GeneralRef(r) if in_body => push_general_ref(&r, &mut out)?,
            Event::Empty(e) | Event::Start(e) if in_body && matches!(e.local_name().as_ref(), "s" | "tab" | "line-break") => {
                match e.local_name().as_ref() {
                    "s" => {
                        let count = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == "c")
                            .and_then(|a| a.value.parse::<usize>().ok())
                            .unwrap_or(1)
                            .min(64);
                        out.push(&" ".repeat(count))?;
                    }
                    "tab" => out.push("\t")?,
                    _ => out.push("\n")?,
                }
            }
            Event::End(e) if in_body => match e.local_name().as_ref() {
                "p" | "h" | "table-row" => out.push("\n")?,
                "table-cell" => out.push("\t")?,
                "page" if slides => pages.push(std::mem::take(&mut out.text)),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    if !slides || !out.text.trim().is_empty() || pages.is_empty() {
        pages.push(out.text);
    }
    Ok(pages)
}

// ---------------------------------------------------------------- PPTX

/// PPTX: one page per slide, in slide order.
pub(super) fn pptx(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    let mut archive = open_office_zip(path, cancel)?;
    let mut slides: Vec<(u32, String)> = Vec::new();
    for name in archive.file_names() {
        check_cancel(cancel)?;
        let name = name?;
        if let Some(number) = name
            .strip_prefix("ppt/slides/slide")
            .and_then(|rest| rest.strip_suffix(".xml"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            slides.push((number, name.into_owned()));
        }
    }
    if slides.is_empty() {
        bail!("PPTX has no slides");
    }
    slides.sort();
    let mut out = TextOutput::new(OFFICE_LIMITS.text_bytes);
    let mut remaining = OFFICE_LIMITS.expanded_bytes;
    let mut pages = Vec::new();
    for (index, (_, name)) in slides.into_iter().enumerate() {
        let member = archive.by_name(&name)?;
        let size = member.size();
        let bytes = office_member(member, size, &mut remaining, OFFICE_LIMITS, cancel)
            .with_context(|| format!("Cannot read PPTX part {name}"))?;
        let xml = std::str::from_utf8(&bytes).context("PPTX XML is not UTF-8")?;
        docx_xml_text(xml, &mut out, cancel)?;
        pages.push(PageText::new(index, tidy(&std::mem::take(&mut out.text)), TextSource::Native, None));
    }
    Ok(pages)
}

// ---------------------------------------------------------------- DOC / PPT (97-2003)

const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Word 97-2003 (.doc, one page) and PowerPoint 97-2003 (.ppt, one page per
/// slide, speaker notes included) through the pure-Rust `undoc` crate. A
/// ".doc" that is really RTF or HTML (old "save as" and web exports do this)
/// goes to that reader instead. The parser runs isolated: a malformed file
/// fails alone and never unwinds into the indexing worker.
pub(super) fn legacy_office(path: &Path, cancel: &AtomicBool, slides: bool) -> Result<Vec<PageText>> {
    check_cancel(cancel)?;
    let bytes = file_bytes(path, super::MAX_OFFICE_FILE_BYTES)?;
    if !slides {
        if bytes.starts_with(b"{\\rtf") {
            return rtf(path, cancel);
        }
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
        if head.contains("<html") || head.contains("<!doctype html") {
            return html(path, cancel);
        }
    }
    if !bytes.starts_with(&OLE_MAGIC) {
        bail!(if slides { "Not a PowerPoint 97-2003 file" } else { "Not a Word 97-2003 file" });
    }
    let document = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| undoc::parse_bytes(&bytes))) {
        Ok(Ok(document)) => document,
        Ok(Err(undoc::Error::Encrypted)) => bail!("Password-protected document: its content cannot be read"),
        Ok(Err(error)) => bail!("Cannot read the document: {error}"),
        Err(_) => bail!("Document parser panicked on malformed input"),
    };
    check_cancel(cancel)?;
    let mut budget = super::OFFICE_LIMITS.text_bytes;
    let mut pages = Vec::new();
    for section in &document.sections {
        check_cancel(cancel)?;
        let mut text = section_text(section, slides);
        if text.len() > budget {
            let mut cut = budget;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        budget -= text.len();
        pages.push(text);
        if budget == 0 {
            break;
        }
    }
    let pages: Vec<String> = if slides { pages } else { vec![pages.join("\n\n")] };
    if pages.is_empty() {
        bail!(if slides { "PPT has no slides" } else { "DOC has no text" });
    }
    Ok(pages
        .into_iter()
        .enumerate()
        .map(|(index, text)| PageText::new(index, tidy(&text), TextSource::Native, None))
        .collect())
}

fn section_text(section: &undoc::Section, slides: bool) -> String {
    let mut lines = Vec::new();
    for block in &section.content {
        match block {
            undoc::Block::Paragraph(paragraph) => lines.push(paragraph.plain_text()),
            undoc::Block::Table(table) => lines.push(table.plain_text()),
            _ => {}
        }
    }
    if slides {
        if let Some(notes) = &section.notes {
            let notes: Vec<String> = notes.iter().map(|p| p.plain_text()).filter(|t| !t.trim().is_empty()).collect();
            if !notes.is_empty() {
                lines.push(String::new());
                lines.extend(notes);
            }
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------- XLS / ODS

/// Legacy Excel (.xls) and OpenDocument spreadsheets (.ods): one page per sheet.
pub(super) fn workbook(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    use calamine::Reader as _;
    check_cancel(cancel)?;
    let size = std::fs::metadata(path)?.len();
    if size > super::MAX_OFFICE_FILE_BYTES {
        bail!("Spreadsheet exceeds the {} MiB input limit", super::MAX_OFFICE_FILE_BYTES / (1024 * 1024));
    }
    let is_ods = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ods"));
    if is_ods {
        // ODS is a ZIP: refuse an expansion bomb before calamine inflates it.
        let mut archive = open_office_zip(path, cancel)?;
        let member = archive.by_name("content.xml").context("ODS has no content.xml")?;
        if member.size() > OFFICE_LIMITS.expanded_bytes as u64 {
            bail!("ODS content exceeds the expansion budget");
        }
    }
    let mut workbook = calamine::open_workbook_auto(path).map_err(|e| anyhow!("Cannot read spreadsheet: {e}"))?;
    let mut out = TextOutput::new(OFFICE_LIMITS.text_bytes);
    let mut cells: u64 = 0;
    let mut pages = Vec::new();
    for (index, name) in workbook.sheet_names().to_vec().into_iter().enumerate() {
        check_cancel(cancel)?;
        let range = workbook.worksheet_range(&name).map_err(|e| anyhow!("Cannot read sheet {name}: {e}"))?;
        let (rows, columns) = range.get_size();
        cells += rows as u64 * columns as u64;
        if cells > super::MAX_WORKBOOK_CELLS {
            bail!("Spreadsheet exceeds the {}-cell limit", super::MAX_WORKBOOK_CELLS);
        }
        out.push(&name)?;
        out.push("\n")?;
        for row in range.rows() {
            check_cancel(cancel)?;
            if let Some(last) = row.iter().rposition(|cell| !matches!(cell, calamine::Data::Empty)) {
                for (column, cell) in row[..=last].iter().enumerate() {
                    if column != 0 {
                        out.push("\t")?;
                    }
                    if !matches!(cell, calamine::Data::Empty) {
                        out.push(&cell.to_string())?;
                    }
                }
                out.push("\n")?;
            }
        }
        pages.push(PageText::new(index, std::mem::take(&mut out.text), TextSource::Native, None));
    }
    if pages.is_empty() {
        bail!("Spreadsheet has no sheets");
    }
    Ok(pages)
}

// ---------------------------------------------------------------- UDF

/// UYAP UDF: a ZIP whose `content.xml` holds the whole text in the CDATA of
/// `template/content`; the rest of the XML is layout over offsets into it.
pub(super) fn udf(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    let mut archive = open_office_zip(path, cancel)?;
    let member = archive.by_name("content.xml").context("UDF has no content.xml")?;
    let size = member.size();
    let mut remaining = OFFICE_LIMITS.expanded_bytes;
    let bytes = office_member(member, size, &mut remaining, OFFICE_LIMITS, cancel)?;
    let xml = String::from_utf8_lossy(&bytes);
    let text = udf_text(&xml)?;
    check_cancel(cancel)?;
    Ok(one_page(tidy(&text.replace("\r\n", "\n").replace('\r', "\n"))))
}

pub(crate) fn udf_text(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut in_main = false;
    let mut text = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                depth += 1;
                if depth == 2 && e.local_name().as_ref() == "content" {
                    in_main = true;
                }
            }
            Event::End(_) => {
                if in_main && depth == 2 {
                    return Ok(text);
                }
                depth = depth.saturating_sub(1);
            }
            Event::CData(t) if in_main => text.push_str(&t.into_inner()),
            Event::Text(t) if in_main => text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if in_main => {
                let mut out = TextOutput::new(16);
                push_general_ref(&r, &mut out)?;
                text.push_str(&out.text);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if text.is_empty() {
        bail!("UDF has no document text");
    }
    Ok(text)
}

// ---------------------------------------------------------------- metadata XML

/// Leaf elements of a metadata XML as "Name: value" lines (Ustveri's Konu,
/// Tarih, BelgeNo; BelgeHedef's recipients; docProps' creator and date).
pub(crate) fn metadata_lines(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    let mut names: Vec<String> = Vec::new();
    let mut lines = Vec::new();
    let mut value = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                names.push(e.local_name().as_ref().to_string());
                value.clear();
            }
            Event::Text(t) => value.push_str(&t.xml10_content()),
            Event::CData(t) => value.push_str(&t.into_inner()),
            Event::GeneralRef(r) => {
                let mut out = TextOutput::new(16);
                push_general_ref(&r, &mut out)?;
                value.push_str(&out.text);
            }
            Event::End(_) => {
                let name = names.pop().unwrap_or_default();
                let trimmed = value.trim();
                if !trimmed.is_empty() && trimmed.chars().count() <= MAX_METADATA_VALUE_CHARS {
                    lines.push(format!("{name}: {trimmed}"));
                }
                value.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(lines)
}

// ---------------------------------------------------------------- containers

/// A file carried inside a container, to be read with its own reader.
struct Carried {
    name: String,
    bytes: Vec<u8>,
}

/// Removes a scratch copy however the read ends.
struct ScratchFile(std::path::PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn file_name_of(path_in_container: &str) -> &str {
    path_in_container.rsplit(['/', '\\']).next().unwrap_or(path_in_container)
}

impl Extractor {
    /// Reads one carried file with the reader of its kind. Containers are not
    /// opened inside containers. The file's name heads its first page.
    fn carried_pages(&self, carried: &Carried, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<Vec<PageText>> {
        let name = file_name_of(&carried.name);
        let path_like = Path::new(name);
        let kind = FileKind::from_path(path_like).ok_or_else(|| anyhow!("Unsupported file type"))?;
        if carried.bytes.is_empty() {
            return Ok(vec![PageText::new(0, String::new(), TextSource::Native, None)]);
        }
        let ext = path_like.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_ascii_lowercase();
        std::fs::create_dir_all(&self.scratch)?;
        let scratch = ScratchFile(self.scratch.join(format!("carried-{}-{}.{ext}", std::process::id(), unique())));
        std::fs::write(&scratch.0, &carried.bytes).context("Cannot write scratch copy")?;
        self.read_pages(kind, &scratch.0, ocr, cancel, true)
    }

    /// Appends each carried file's pages, its name heading its first page. A
    /// file that cannot be read becomes one failed page; cancellation stops all.
    fn append_carried(&self, pages: &mut Vec<PageText>, carried: Vec<Carried>, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<()> {
        for item in carried {
            check_cancel(cancel)?;
            let name = file_name_of(&item.name).to_string();
            match self.carried_pages(&item, ocr, cancel) {
                Ok(mut read) => {
                    if let Some(first) = read.first_mut() {
                        if first.is_failed() {
                            first.error = first.error.take().map(|e| format!("{name}: {e}"));
                        } else {
                            first.text = format!("{name}\n\n{}", first.text);
                        }
                    }
                    pages.extend(read);
                }
                Err(error) if crate::ocr::is_cancelled(&error) => return Err(error),
                Err(error) => pages.push(PageText::failed(0, format!("{name}: {error:#}"))),
            }
        }
        Ok(())
    }

    /// e-Yazışma package (EYP): page 1 is the package metadata (Ustveri,
    /// recipients, creator and date), then the cover letter (UstYazi) and the
    /// attachments (Ekler), each read with its own reader. An encrypted
    /// package (SifreliIcerik) opens only with the recipient's e-signature
    /// card, so only its metadata is read and its content is a failed page.
    pub(super) fn eyp(&self, path: &Path, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<Vec<PageText>> {
        check_cancel(cancel)?;
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() > MAX_CONTAINER_FILE_BYTES {
            bail!("EYP exceeds the {} MiB input limit", MAX_CONTAINER_FILE_BYTES / (1024 * 1024));
        }
        let mut archive = zip::ZipArchive::new(file).context("Not a valid EYP package")?;
        if archive.len() > super::MAX_OFFICE_ENTRIES {
            bail!("EYP exceeds the {}-member limit", super::MAX_OFFICE_ENTRIES);
        }
        let names: Vec<String> = archive.file_names().map(|n| n.map(|n| n.into_owned())).collect::<std::result::Result<_, _>>()?;
        let folder = |name: &str| name.split(['/', '\\']).next().unwrap_or("").to_lowercase();
        let is_file = |name: &str| !name.ends_with('/') && !name.ends_with('\\');
        let has = |wanted: &str| names.iter().any(|n| folder(n) == wanted && is_file(n));
        let metadata_folder = if has("ustveri") { "ustveri" } else { "nihaiustveri" };
        let encrypted = has("sifreliicerik");

        let mut metadata = Vec::new();
        for name in names.iter().filter(|n| is_file(n) && n.to_lowercase().ends_with(".xml")) {
            let top = folder(name);
            let is_core = name.eq_ignore_ascii_case("docProps/core.xml");
            if top != metadata_folder && top != "belgehedef" && !is_core {
                continue;
            }
            check_cancel(cancel)?;
            let member = archive.by_name(name)?;
            let bytes = read_bounded(member, MAX_METADATA_XML_BYTES, Some(cancel)).with_context(|| format!("Cannot read EYP part {name}"))?;
            let xml = String::from_utf8_lossy(&bytes);
            match metadata_lines(&xml) {
                Ok(lines) => metadata.extend(lines),
                Err(error) => metadata.push(format!("{name}: {error}")),
            }
        }

        let mut carried_names: Vec<&String> =
            names.iter().filter(|n| is_file(n) && matches!(folder(n).as_str(), "ustyazi" | "ekler" | "ek")).collect();
        carried_names.sort_by_key(|n| (folder(n) != "ustyazi", n.to_lowercase()));
        let mut carried = Vec::new();
        let mut total = 0usize;
        for name in carried_names.into_iter().take(MAX_CARRIED_FILES) {
            check_cancel(cancel)?;
            let member = archive.by_name(name)?;
            let budget = MAX_CARRIED_FILE_BYTES.min(MAX_CARRIED_TOTAL_BYTES.saturating_sub(total));
            let bytes = read_bounded(member, budget, Some(cancel)).with_context(|| format!("Cannot read EYP part {name}"))?;
            total += bytes.len();
            carried.push(Carried { name: name.clone(), bytes });
        }
        drop(archive);

        let mut pages = vec![PageText::new(0, metadata.join("\n"), TextSource::Native, None)];
        if encrypted && carried.is_empty() {
            pages.push(PageText::failed(1, "Encrypted e-Yazışma package: its content opens only with the recipient's e-signature card".into()));
        }
        self.append_carried(&mut pages, carried, ocr, cancel)?;
        Ok(renumber(pages))
    }

    /// Saved e-mail (EML): page 1 is the headers and body, then each
    /// attachment read with its own reader (an attached e-mail as its text).
    pub(super) fn eml(&self, path: &Path, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<Vec<PageText>> {
        use mail_parser::MimeHeaders as _;
        check_cancel(cancel)?;
        let bytes = file_bytes(path, MAX_CONTAINER_FILE_BYTES)?;
        let message = mail_parser::MessageParser::default().parse(&bytes).ok_or_else(|| anyhow!("Not a readable e-mail message"))?;
        let mut pages = vec![PageText::new(0, message_text(&message), TextSource::Native, None)];
        let mut carried = Vec::new();
        let mut nested = Vec::new();
        let mut total = 0usize;
        for (index, part) in message.attachments().enumerate().take(MAX_CARRIED_FILES) {
            check_cancel(cancel)?;
            let name = part.attachment_name().map(str::to_string).unwrap_or_else(|| format!("attachment-{}", index + 1));
            if let Some(inner) = part.message() {
                nested.push(format!("{name}\n\n{}", message_text(inner)));
                continue;
            }
            let contents = part.contents();
            if contents.len() > MAX_CARRIED_FILE_BYTES || total + contents.len() > MAX_CARRIED_TOTAL_BYTES {
                pages.push(PageText::failed(0, format!("{name}: attachment exceeds the size limit")));
                continue;
            }
            total += contents.len();
            if FileKind::from_path(Path::new(&name)).is_none() {
                continue;
            }
            carried.push(Carried { name, bytes: contents.to_vec() });
        }
        for text in nested {
            pages.push(PageText::new(0, text, TextSource::Native, None));
        }
        self.append_carried(&mut pages, carried, ocr, cancel)?;
        Ok(renumber(pages))
    }

    /// Outlook message (MSG): page 1 is the headers and body, then each
    /// attachment read with its own reader. An attached message (embedded or
    /// a ".msg" file) is read as its text; inline pictures of the body
    /// (signature logos, cid: images) are skipped like OLE objects.
    pub(super) fn msg(&self, path: &Path, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<Vec<PageText>> {
        check_cancel(cancel)?;
        let bytes = file_bytes(path, MAX_CONTAINER_FILE_BYTES)?;
        let message = outlook_message(&bytes)?;
        let mut pages = vec![PageText::new(0, outlook_text(&message), TextSource::Native, None)];
        let mut carried = Vec::new();
        let mut nested = Vec::new();
        let mut total = 0usize;
        for (index, attachment) in message.attachments.iter().enumerate().take(MAX_CARRIED_FILES) {
            check_cancel(cancel)?;
            let name = [&attachment.long_file_name, &attachment.file_name, &attachment.display_name]
                .into_iter()
                .map(|n| n.trim())
                .find(|n| !n.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("attachment-{}", index + 1));
            if attachment.is_embedded_message() {
                match attachment.as_message() {
                    Some(Ok(inner)) => nested.push(format!("{name}\n\n{}", outlook_text(&inner))),
                    _ => nested.push(format!("{name}\n\n(attached message)")),
                }
                continue;
            }
            if attachment.attach_method == 6 {
                continue;
            }
            let contents = &attachment.payload_bytes;
            if contents.len() > MAX_CARRIED_FILE_BYTES || total + contents.len() > MAX_CARRIED_TOTAL_BYTES {
                pages.push(PageText::failed(0, format!("{name}: attachment exceeds the size limit")));
                continue;
            }
            let kind = FileKind::from_path(Path::new(&name));
            if kind == Some(FileKind::Email) && name.to_ascii_lowercase().ends_with(".msg") {
                match outlook_message(contents) {
                    Ok(inner) => nested.push(format!("{name}\n\n{}", outlook_text(&inner))),
                    Err(error) => pages.push(PageText::failed(0, format!("{name}: {error:#}"))),
                }
                continue;
            }
            if kind == Some(FileKind::Image) && !attachment.content_id.trim().is_empty() {
                continue;
            }
            if kind.is_none() {
                continue;
            }
            total += contents.len();
            carried.push(Carried { name, bytes: contents.clone() });
        }
        for text in nested {
            pages.push(PageText::new(0, text, TextSource::Native, None));
        }
        self.append_carried(&mut pages, carried, ocr, cancel)?;
        Ok(renumber(pages))
    }
}

fn renumber(mut pages: Vec<PageText>) -> Vec<PageText> {
    for (index, page) in pages.iter_mut().enumerate() {
        page.index = index;
    }
    pages
}

/// Parses an Outlook message in isolation: a malformed file fails alone.
fn outlook_message(bytes: &[u8]) -> Result<msg_parser::Outlook> {
    if !bytes.starts_with(&OLE_MAGIC) {
        bail!("Not an Outlook message");
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| msg_parser::Outlook::from_slice(bytes))) {
        Ok(Ok(message)) => Ok(message),
        Ok(Err(error)) => bail!("Not a readable Outlook message: {error}"),
        Err(_) => bail!("Outlook message parser panicked on malformed input"),
    }
}

fn person_text(person: &msg_parser::Person) -> String {
    let name = person.name.trim();
    // Exchange-internal senders carry an X.500 path, not an e-mail address.
    let email = person.email.trim();
    let email = if email.starts_with('/') { "" } else { email };
    match (name.is_empty(), email.is_empty()) {
        (false, false) if !name.eq_ignore_ascii_case(email) => format!("{name} <{email}>"),
        (false, _) => name.to_string(),
        (true, false) => email.to_string(),
        (true, true) => String::new(),
    }
}

fn outlook_text(message: &msg_parser::Outlook) -> String {
    let people = |list: &[msg_parser::Person]| list.iter().map(person_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
    let mut lines = Vec::new();
    let from = person_text(&message.sender);
    if !from.is_empty() {
        lines.push(format!("From: {from}"));
    }
    for (label, list) in [("To", &message.to), ("Cc", &message.cc)] {
        let text = people(list);
        if !text.is_empty() {
            lines.push(format!("{label}: {text}"));
        }
    }
    let date = [&message.client_submit_time, &message.message_delivery_time, &message.headers.date]
        .into_iter()
        .map(|d| d.trim())
        .find(|d| !d.is_empty());
    if let Some(date) = date {
        lines.push(format!("Date: {date}"));
    }
    if !message.subject.trim().is_empty() {
        lines.push(format!("Subject: {}", message.subject.trim()));
    }
    let body = if !message.body.trim().is_empty() {
        message.body.clone()
    } else if !message.html.trim().is_empty() {
        html_to_text(&message.html)
    } else if let Some(html) = message.html_from_rtf() {
        html_to_text(&html)
    } else if let Some(rtf) = message.rtf_decompressed() {
        rtf_to_text(&rtf)
    } else {
        String::new()
    };
    format!("{}\n\n{}", lines.join("\n"), tidy(&body))
}

fn address_text(address: &mail_parser::Address<'_>) -> String {
    let one = |a: &mail_parser::Addr<'_>| match (a.name(), a.address()) {
        (Some(name), Some(address)) => format!("{name} <{address}>"),
        (Some(name), None) => name.to_string(),
        (None, Some(address)) => address.to_string(),
        (None, None) => String::new(),
    };
    if let Some(list) = address.as_list() {
        return list.iter().map(one).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
    }
    if let Some(groups) = address.as_group() {
        return groups
            .iter()
            .flat_map(|g| g.addresses.iter().map(one))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
    }
    String::new()
}

fn message_text(message: &mail_parser::Message<'_>) -> String {
    let mut lines = Vec::new();
    if let Some(from) = message.from() {
        lines.push(format!("From: {}", address_text(from)));
    }
    if let Some(to) = message.to() {
        lines.push(format!("To: {}", address_text(to)));
    }
    if let Some(cc) = message.cc() {
        lines.push(format!("Cc: {}", address_text(cc)));
    }
    if let Some(date) = message.date() {
        lines.push(format!("Date: {}", date.to_rfc3339()));
    }
    if let Some(subject) = message.subject() {
        lines.push(format!("Subject: {subject}"));
    }
    let body = message
        .body_text(0)
        .map(|b| b.into_owned())
        .or_else(|| message.body_html(0).map(|h| html_to_text(&h)))
        .unwrap_or_default();
    format!("{}\n\n{}", lines.join("\n"), tidy(&body))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ocr::OcrConfig;

    fn extractor(dir: &Path) -> Extractor {
        let ocr = OcrConfig {
            binary: dir.join("missing-tesseract"),
            tessdata: dir.to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(5),
            threads: 1,
        };
        Extractor::new(&dir.join("no-pdfium"), ocr, dir.join("scratch"), 1)
    }

    fn write_zip_bytes(path: &Path, members: &[(&str, &[u8])]) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in members {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn docx_bytes(dir: &Path, text: &str) -> Vec<u8> {
        let path = dir.join(format!("tmp-{}.docx", unique()));
        let xml = format!(r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#);
        write_zip_bytes(&path, &[("word/document.xml", xml.as_bytes())]);
        std::fs::read(&path).unwrap()
    }

    /// Manual check on a real file: `ZA_SAMPLE=<path> cargo test -p arsiv-core
    /// real_sample -- --ignored --nocapture`. Prints shape only, never text.
    #[test]
    #[ignore]
    fn real_sample() {
        let path = std::path::PathBuf::from(std::env::var("ZA_SAMPLE").expect("ZA_SAMPLE"));
        let dir = tempfile::tempdir().unwrap();
        let document = extractor(dir.path()).extract(&path).unwrap();
        println!("kind={:?} pages={}", document.kind, document.pages.len());
        for page in &document.pages {
            let words = page.text.split_whitespace().count();
            let lines: Vec<&str> = page.text.lines().filter(|l| l.contains(':')).map(|l| l.split(':').next().unwrap_or("")).take(12).collect();
            println!("page {} words={} failed={:?} labels={:?}", page.index, words, page.error, lines);
        }
    }

    #[test]
    fn rtf_reads_turkish_code_page_unicode_and_skips_tables() {
        let rtf = br"{\rtf1\ansi\ansicpg1254\deff0{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}{\*\generator Word;}
\pard Kira s\'f6zle\'femesi\par
{\info{\title gizli ba\'fel\'fdk}}Tutar: 5.000 TL\tab \u305?\u351?k\par
Kar\u351\'3f\'fd taraf \{parantez\}}";
        let text = rtf_to_text(rtf);
        assert!(text.contains("Kira sözleşmesi"), "{text}");
        assert!(text.contains("Tutar: 5.000 TL\tış"), "{text}");
        assert!(text.contains("Karşı taraf {parantez}"), "{text}");
        assert!(!text.contains("Arial") && !text.contains("gizli") && !text.contains("Word"), "{text}");
    }

    #[test]
    fn html_drops_scripts_and_keeps_blocks_and_entities() {
        let html = "<html><head><title>Dilek&ccedil;e</title><style>p{color:red}</style><script>var x='<p>';</script></head>\
                    <body><!-- not --><p>Say&#305;n   <b>Müdürlük</b></p><table><tr><td>A</td><td>B&amp;C</td></tr></table>Son&nbsp;satır</body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Dilekçe"), "{text}");
        assert!(text.contains("Sayın Müdürlük"), "{text}");
        assert!(text.contains("A\tB&C"), "{text}");
        assert!(text.contains("Son satır"), "{text}");
        assert!(!text.contains("color") && !text.contains("var x") && !text.contains("not"), "{text}");
    }

    #[test]
    fn udf_text_is_the_template_content_cdata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("karar.udf");
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<template format_id=\"1.8\">\n<content><![CDATA[ANKARA 15. ASLİYE HUKUK MAHKEMESİNE\nDava konusu: kira alacağı\n]]></content>\
                   <properties><pageFormat leftMargin=\"42\"/></properties><elements resolver=\"hvl-default\"><paragraph><content startOffset=\"0\" length=\"10\"/></paragraph></elements></template>";
        write_zip_bytes(&path, &[("content.xml", xml.as_bytes()), ("documentproperties.xml", b"<properties/>")]);
        let pages = udf(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].text.starts_with("ANKARA 15. ASLİYE HUKUK MAHKEMESİNE\nDava konusu: kira alacağı"), "{}", pages[0].text);
    }

    #[test]
    fn odt_and_odp_read_paragraphs_spaces_and_slides() {
        let dir = tempfile::tempdir().unwrap();
        let odt = dir.path().join("a.odt");
        let xml = r#"<office:document-content xmlns:office="o" xmlns:text="t" xmlns:table="tb"><office:body><office:text>
<text:p>Birinci<text:s text:c="2"/>satır<text:tab/>sekme</text:p><text:h>Başlık</text:h>
<table:table><table:table-row><table:table-cell><text:p>x</text:p></table:table-cell></table:table-row></table:table>
</office:text></office:body></office:document-content>"#;
        write_zip_bytes(&odt, &[("content.xml", xml.as_bytes())]);
        let pages = odf(&odt, &AtomicBool::new(false), false).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages[0].text.contains("Birinci  satır\tsekme\nBaşlık"), "{:?}", pages[0].text);

        let odp = dir.path().join("b.odp");
        let xml = r#"<office:document-content xmlns:office="o" xmlns:draw="d" xmlns:text="t"><office:body><office:presentation>
<draw:page><draw:frame><draw:text-box><text:p>Slayt bir</text:p></draw:text-box></draw:frame></draw:page>
<draw:page><draw:frame><draw:text-box><text:p>Slayt iki</text:p></draw:text-box></draw:frame></draw:page>
</office:presentation></office:body></office:document-content>"#;
        write_zip_bytes(&odp, &[("content.xml", xml.as_bytes())]);
        let pages = odf(&odp, &AtomicBool::new(false), true).unwrap();
        assert_eq!(pages.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Slayt bir", "Slayt iki"]);
    }

    #[test]
    fn pptx_slides_are_pages_in_numeric_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunum.pptx");
        let slide = |text: &str| format!(r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#);
        let (s1, s2, s10) = (slide("Giriş"), slide("İkinci"), slide("Onuncu"));
        write_zip_bytes(
            &path,
            &[("ppt/slides/slide10.xml", s10.as_bytes()), ("ppt/slides/slide2.xml", s2.as_bytes()), ("ppt/slides/slide1.xml", s1.as_bytes())],
        );
        let pages = pptx(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(pages.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Giriş", "İkinci", "Onuncu"]);
        assert_eq!(pages[2].index, 2);
    }

    #[test]
    fn eyp_reads_metadata_cover_letter_and_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yazi.eyp");
        let ustveri = r#"<?xml version="1.0" encoding="UTF-8"?><Ustveri xmlns="urn:eyazisma"><Konu>Kira sözleşmesi hakkında</Konu><Tarih>2025-12-10T15:03:08</Tarih><BelgeNo>E-12345</BelgeNo><Olusturan><Adi>Örnek Müdürlüğü</Adi></Olusturan></Ustveri>"#;
        let cover = docx_bytes(dir.path(), "Üst yazı metni: kira bedeli");
        write_zip_bytes(
            &path,
            &[
                ("[Content_Types].xml", b"<Types/>"),
                ("Ustveri/Ustveri.xml", ustveri.as_bytes()),
                ("UstYazi/ustyazi.docx", &cover),
                ("Ekler/ek-1.txt", "Ek liste: demirbaş".as_bytes()),
                ("Ekler/resim.xyz", b"?"),
            ],
        );
        let extractor = extractor(dir.path());
        let document = extractor.extract(&path).unwrap();
        assert_eq!(document.kind, FileKind::Official);
        let texts: Vec<&str> = document.pages.iter().map(|p| p.text.as_str()).collect();
        assert!(texts[0].contains("Konu: Kira sözleşmesi hakkında") && texts[0].contains("Adi: Örnek Müdürlüğü"), "{texts:?}");
        assert!(texts[1].starts_with("ustyazi.docx\n\n") && texts[1].contains("kira bedeli"), "{texts:?}");
        assert!(texts[2].starts_with("ek-1.txt\n\n") && texts[2].contains("demirbaş"), "{texts:?}");
        assert_eq!(document.pages.len(), 4, "the unsupported attachment is a failed page");
        assert!(document.pages[3].is_failed());
        assert!(document.pages.iter().enumerate().all(|(i, p)| p.index == i));
        assert!(std::fs::read_dir(dir.path().join("scratch")).map_or(true, |mut d| d.next().is_none()), "scratch copies removed");
    }

    #[test]
    fn encrypted_eyp_keeps_its_metadata_and_reports_the_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sifreli.eyp");
        let hedef = r#"<ns6:BelgeHedef xmlns:ns6="x"><HedefListesi><Hedef><KurumKurulus><KKK>123</KKK><Adi>Örnek Valiliği</Adi></KurumKurulus></Hedef></HedefListesi></ns6:BelgeHedef>"#;
        let core = r#"<cp:coreProperties xmlns:cp="c" xmlns:dc="d"><dc:creator>EBYS</dc:creator><dc:identifier>abc-1</dc:identifier></cp:coreProperties>"#;
        write_zip_bytes(
            &path,
            &[
                ("BelgeHedef/BelgeHedef.xml", hedef.as_bytes()),
                ("docProps/core.xml", core.as_bytes()),
                ("SifreliIcerik/0862B377", b"\x30\x82binary"),
                ("NihaiOzet/NihaiOzet.xml", br#"<NihaiOzet><DigestValue>AAAA</DigestValue></NihaiOzet>"#),
            ],
        );
        let document = extractor(dir.path()).extract(&path).unwrap();
        assert_eq!(document.pages.len(), 2);
        assert!(document.pages[0].text.contains("Adi: Örnek Valiliği"), "{:?}", document.pages[0].text);
        assert!(document.pages[0].text.contains("creator: EBYS"));
        assert!(!document.pages[0].text.contains("DigestValue"), "digests are not metadata");
        assert!(document.pages[1].error.as_deref().unwrap().contains("Encrypted"));
    }

    #[test]
    fn eml_reads_headers_body_and_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posta.eml");
        let eml = concat!(
            "From: Ayşe Yılmaz <ayse@example.com>\r\n",
            "To: Birim <birim@example.com>\r\n",
            "Subject: =?UTF-8?B?S2lyYSBzw7Z6bGXFn21lc2k=?=\r\n",
            "Date: Tue, 25 Nov 2025 09:44:00 +0300\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"b\"\r\n",
            "\r\n",
            "--b\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Ekte tutanak var.\r\n",
            "--b\r\n",
            "Content-Type: text/plain; name=\"tutanak.txt\"\r\n",
            "Content-Disposition: attachment; filename=\"tutanak.txt\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "VHV0YW5hayBtZXRuaTogZGVtaXJiYcWf\r\n",
            "--b--\r\n",
        );
        std::fs::write(&path, eml).unwrap();
        let document = extractor(dir.path()).extract(&path).unwrap();
        assert_eq!(document.kind, FileKind::Email);
        let first = &document.pages[0].text;
        assert!(first.contains("Subject: Kira sözleşmesi") && first.contains("ayse@example.com") && first.contains("Ekte tutanak var."), "{first}");
        assert_eq!(document.pages.len(), 2, "{:?}", document.pages);
        assert!(document.pages[1].text.starts_with("tutanak.txt\n\n") && document.pages[1].text.contains("demirbaş"));
    }

    fn legacy_fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy").join(name)
    }

    #[test]
    fn doc_97_2003_is_read_without_libreoffice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eski.doc");
        std::fs::copy(legacy_fixture("sample.doc"), &path).unwrap();
        let document = extractor(dir.path()).extract(&path).unwrap();
        assert_eq!(document.kind, FileKind::Word);
        assert_eq!(document.pages.len(), 1);
        let text = &document.pages[0].text;
        assert!(text.contains("This is Heading1 Text") && text.contains("default style of Normal"), "{text}");
    }

    #[test]
    fn ppt_97_2003_gives_one_page_per_slide() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunum.ppt");
        std::fs::copy(legacy_fixture("sample.ppt"), &path).unwrap();
        let document = extractor(dir.path()).extract(&path).unwrap();
        assert_eq!(document.kind, FileKind::Presentation);
        assert_eq!(document.pages.len(), 34);
        assert!(document.pages[0].text.contains("Research talk 101"), "{}", document.pages[0].text);
        assert!(document.pages[1].text.contains("Purpose of a research talk"));
    }

    #[test]
    fn doc_that_is_really_rtf_or_html_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let rtf = dir.path().join("kayit.doc");
        std::fs::write(&rtf, r"{\rtf1\ansi Kira s\'f6zle\'femesi}").unwrap();
        let document = extractor(dir.path()).extract(&rtf).unwrap();
        assert!(document.pages[0].text.contains("Kira"), "{:?}", document.pages);
        let html = dir.path().join("web.doc");
        std::fs::write(&html, "<html><body><p>Tutanak metni</p></body></html>").unwrap();
        let document = extractor(dir.path()).extract(&html).unwrap();
        assert!(document.pages[0].text.contains("Tutanak metni"));
    }

    #[test]
    fn broken_doc_and_msg_fail_alone() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [("bozuk.doc", &b"not a document"[..]), ("bozuk.ppt", &OLE_MAGIC[..]), ("bozuk.msg", &b"not a message"[..])] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(extractor(dir.path()).extract(&path).is_err(), "{name}");
        }
    }

    #[test]
    fn msg_reads_headers_body_and_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posta.msg");
        std::fs::copy(legacy_fixture("attachment.msg"), &path).unwrap();
        let document = extractor(dir.path()).extract(&path).unwrap();
        assert_eq!(document.kind, FileKind::Email);
        let first = &document.pages[0].text;
        assert!(first.contains("Subject: FW:") && first.contains("From: Nagisetti, Satya") && first.contains("looks to be a suspicious"), "{first}");
        assert!(!first.contains("/O=EXCHANGELABS"), "X.500 sender paths are not addresses: {first}");
        // The attached .doc is read with the DOC reader; the two inline
        // signature pictures (cid:) are skipped, so no OCR is attempted.
        assert_eq!(document.pages.len(), 2, "{:?}", document.pages);
        let attached = &document.pages[1];
        assert!(!attached.is_failed(), "{:?}", attached.error);
        assert!(attached.text.starts_with("loan_proposal.doc\n\n"), "{}", attached.text);
        assert!(attached.text.contains("fake phishing email"), "{}", attached.text);
    }
}
