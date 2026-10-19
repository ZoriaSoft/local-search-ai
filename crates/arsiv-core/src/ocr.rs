//! Bundled Tesseract OCR, invoked as a subprocess exactly like the measured lab
//! configuration: `--oem 1`, tessdata_best models (`tur`, `eng` or `tur+eng`,
//! the user's document-language setting), TSV output, and a
//! pixel-layout PSM choice (3 for tables / two columns, 6 for uniform rows).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use image::GrayImage;

#[derive(Clone, Debug)]
pub struct OcrConfig {
    pub binary: PathBuf,
    pub tessdata: PathBuf,
    pub language: String,
    pub timeout: Duration,
    pub threads: usize,
}

/// The run was cancelled by the user (`Engine::cancel_indexing`); a
/// Tesseract process still running was killed. Not a file failure: nothing
/// about the file is recorded.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

/// True when `error` (or anything it wraps) is `Cancelled`.
pub fn is_cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Cancelled>())
}

#[derive(Clone, Debug)]
pub struct OcrOutput {
    pub text: String,
    /// Mean word confidence 0..1; None when no words were recognised.
    pub confidence: Option<f32>,
    pub psm: u8,
}

/// "ARŞİV 2026 OCR" in Liberation Sans 40 px on a 420×80 grayscale canvas.
/// Read through the real `recognize()` path by `check()`.
const PROBE_PNG: &[u8] = include_bytes!("../assets/ocr-probe.png");
/// Token the probe must yield; digits are font- and model-robust.
const PROBE_TOKEN: &str = "2026";

impl OcrConfig {
    /// Proves OCR works end to end: the binary starts, the model file exists
    /// AND the binary can load it from `tessdata` (same `--tessdata-dir`
    /// argument as real pages) and read the embedded probe image. A green
    /// `--version` alone hid the Windows `\\?\` tessdata bug.
    /// `scratch` is where the probe image is written, like real page renders.
    pub fn check(&self, scratch: &Path) -> Result<String> {
        if !self.binary.is_file() {
            bail!("Tesseract binary not found: {}", self.binary.display());
        }
        if let Some(missing) = self.missing_models().into_iter().next() {
            bail!("Tesseract model not found: {}", missing.display());
        }
        let output = self
            .command()
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Tesseract could not be started")?;
        if !output.status.success() {
            bail!("Tesseract --version exited with {}", output.status);
        }
        let banner = String::from_utf8_lossy(&output.stdout);
        let banner = banner.lines().next().unwrap_or("tesseract").trim().to_string();

        std::fs::create_dir_all(scratch)
            .with_context(|| format!("Cannot create OCR scratch folder {}", scratch.display()))?;
        let probe = scratch.join(format!("ocr-probe-{}.png", std::process::id()));
        std::fs::write(&probe, PROBE_PNG).context("Cannot write OCR probe image")?;
        let result = self.recognize(&probe);
        let _ = std::fs::remove_file(&probe);
        let result = result.with_context(|| {
            format!("Tesseract could not read a test image with the model in {}", self.tessdata.display())
        })?;
        if !result.text.contains(PROBE_TOKEN) {
            bail!(
                "Tesseract ran but misread its test image (expected \"{PROBE_TOKEN}\", got {:?})",
                result.text.trim()
            );
        }
        Ok(format!("{banner} · {} model verified", self.language))
    }

    /// Model files of `language` (`tur`, `eng`, `tur+eng`) absent from
    /// `tessdata`. Every part of a combined language needs its own model.
    pub fn missing_models(&self) -> Vec<PathBuf> {
        self.language
            .split('+')
            .map(|part| self.tessdata.join(format!("{part}.traineddata")))
            .filter(|model| !model.is_file())
            .collect()
    }

    fn command(&self) -> Command {
        #[cfg(windows)]
        let mut command = {
            // OCR is background work: below-normal priority keeps the
            // user's PC responsive while a scan runs (Windows only — Unix
            // Tesseract goes through `nice` below).
            let mut command = Command::new(&self.binary);
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            // `nice -n10` lowers the scheduler priority so indexing never
            // starves the user's other work; absent on minimal images, the
            // binary is invoked directly.
            ["/usr/bin/nice", "/bin/nice", "/usr/local/bin/nice"]
                .iter()
                .find(|p| std::path::Path::new(p).is_file())
                .map(|nice| {
                    let mut command = Command::new(nice);
                    command.arg("-n").arg("10").arg(&self.binary);
                    command
                })
                .unwrap_or_else(|| Command::new(&self.binary))
        };
        let threads = self.threads.max(1).to_string();
        command
            .env("OMP_THREAD_LIMIT", &threads)
            .env("OMP_NUM_THREADS", &threads)
            .stdin(Stdio::null());
        // Tesseract opens files with the C runtime's ANSI `fopen` on Windows:
        // any character outside the system code page (Arabic folder names on
        // a Turkish PC, `ş` in "Zoria Arşiv" on an English one) turned into
        // `?` and the open failed. Images therefore go in through stdin, and
        // the model folder is passed relative to the working directory, which
        // Windows resolves with the full Unicode path.
        if let Some(parent) = self.tessdata.parent().filter(|p| !p.as_os_str().is_empty()) {
            command.current_dir(parent);
        }
        if let Some(dir) = self.binary.parent() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(&path));
            if let Ok(joined) = std::env::join_paths(paths) {
                command.env("PATH", joined);
            }
        }
        command
    }

    /// `--tessdata-dir` as Tesseract must see it: the folder's own name,
    /// resolved against the working directory `command` sets (its parent).
    fn tessdata_arg(&self) -> PathBuf {
        match (self.tessdata.parent(), self.tessdata.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => PathBuf::from(name),
            _ => self.tessdata.clone(),
        }
    }

    /// OCR one image file. The image is read once for layout analysis.
    pub fn recognize(&self, image_path: &Path) -> Result<OcrOutput> {
        self.recognize_with_psm(image_path, None)
    }

    /// OCR with a page-segmentation mode chosen by the caller; `None` picks
    /// it from this image's pixels (`select_psm`).
    pub fn recognize_with_psm(&self, image_path: &Path, psm: Option<u8>) -> Result<OcrOutput> {
        self.recognize_cancellable(image_path, psm, None)
    }

    /// `recognize_with_psm` that kills Tesseract and fails with `Cancelled`
    /// as soon as `cancel` is set (checked every 25 ms while it runs).
    pub fn recognize_cancellable(
        &self,
        image_path: &Path,
        psm: Option<u8>,
        cancel: Option<&AtomicBool>,
    ) -> Result<OcrOutput> {
        let cancelled = || cancel.is_some_and(|flag| flag.load(Ordering::Relaxed));
        if cancelled() {
            return Err(Cancelled.into());
        }
        // Rust reads the file (Unicode-safe on every OS); Tesseract gets the
        // bytes on stdin, never the path (see `command`).
        let bytes = std::fs::read(image_path).with_context(|| format!("Cannot read image {}", image_path.display()))?;
        let psm = match psm {
            Some(psm) => psm,
            None => select_psm(
                &image::load_from_memory(&bytes)
                    .with_context(|| format!("Cannot decode image {}", image_path.display()))?
                    .into_luma8(),
            ),
        };
        let mut child = self
            .command()
            .arg("stdin")
            .arg("stdout")
            .arg("--tessdata-dir")
            .arg(self.tessdata_arg())
            .args(["-l", &self.language, "--psm", &psm.to_string(), "--oem", "1"])
            .args(["-c", "tessedit_create_tsv=1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Tesseract could not be started")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        // A writer thread: a large image fills the pipe before Tesseract
        // reads it, and a killed Tesseract (cancel) just ends the write.
        let in_writer = std::thread::spawn(move || {
            use std::io::Write;
            let mut stdin = stdin;
            let _ = stdin.write_all(&bytes);
        });
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
        let out_reader = std::thread::spawn(move || read_all(stdout));
        let err_reader = std::thread::spawn(move || read_all(stderr));
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Cancelled.into());
            }
            if started.elapsed() > self.timeout {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Tesseract timed out after {} s", self.timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        let _ = in_writer.join();
        let stdout = out_reader.join().map_err(|_| anyhow!("stdout reader panicked"))??;
        let stderr = err_reader.join().map_err(|_| anyhow!("stderr reader panicked"))??;
        if !status.success() {
            bail!(
                "Tesseract exited with {}: {}",
                status,
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        let tsv = String::from_utf8_lossy(&stdout);
        let (text, confidence) = parse_tsv(tsv.trim_start_matches('\u{feff}'))?;
        Ok(OcrOutput { text, confidence, psm })
    }
}

fn read_all(mut reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Parse Tesseract TSV (literal text, not CSV-quoted) into reading-order lines.
pub fn parse_tsv(tsv: &str) -> Result<(String, Option<f32>)> {
    let mut lines = tsv.lines();
    let header: Vec<&str> = lines.next().unwrap_or("").split('\t').collect();
    let column = |name: &str| header.iter().position(|h| *h == name);
    let (level, conf, text, page, block, par, line) = match (
        column("level"),
        column("conf"),
        column("text"),
        column("page_num"),
        column("block_num"),
        column("par_num"),
        column("line_num"),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(g)) => (a, b, c, d, e, f, g),
        _ => bail!("Tesseract output is not native TSV"),
    };
    let mut out = String::new();
    let mut previous: Option<[String; 4]> = None;
    let (mut conf_sum, mut words) = (0.0f64, 0usize);
    for row in lines {
        let fields: Vec<&str> = row.split('\t').collect();
        if fields.len() < header.len() || fields[level] != "5" {
            continue;
        }
        let word = fields[text].trim();
        if word.is_empty() {
            continue;
        }
        let key = [
            fields[page].to_string(),
            fields[block].to_string(),
            fields[par].to_string(),
            fields[line].to_string(),
        ];
        if previous.as_ref() == Some(&key) {
            out.push(' ');
        } else if previous.is_some() {
            out.push('\n');
        }
        out.push_str(word);
        previous = Some(key);
        if let Ok(value) = fields[conf].trim().parse::<f64>() {
            if value.is_finite() && value >= 0.0 {
                conf_sum += value.min(100.0) / 100.0;
                words += 1;
            }
        }
    }
    if previous.is_some() {
        out.push('\n');
    }
    let confidence = (words > 0).then(|| (conf_sum / words as f64) as f32);
    Ok((out, confidence))
}

/// Port of the lab's pixel-layout-v2 policy: table grid => 3, horizontal form
/// rows => 6, center gutter (two columns) => 3, otherwise 6.
pub fn select_psm(source: &GrayImage) -> u8 {
    let small = shrink(source, 1000, 2000);
    let (width, height) = small.dimensions();
    let threshold = otsu(&small);
    let x0 = (width as f32 * 0.07) as u32;
    let x1 = (width as f32 * 0.93) as u32;
    let y0 = (height as f32 * 0.15) as u32;
    let y1 = (height as f32 * 0.87) as u32;
    if x1 <= x0 || y1 <= y0 {
        return 3;
    }
    let (bw, bh) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let mut ink = vec![false; bw * bh];
    for y in 0..bh {
        for x in 0..bw {
            let value = small.get_pixel(x0 + x as u32, y0 + y as u32)[0];
            ink[y * bw + x] = value <= threshold;
        }
    }
    let horizontal_len = (bw / 12).max(40);
    let vertical_len = (bh / 12).max(40);
    let mut rule_pixels = 0usize;
    for y in 0..bh {
        let mut run = 0usize;
        for x in 0..=bw {
            if x < bw && ink[y * bw + x] {
                run += 1;
            } else {
                if run >= horizontal_len {
                    rule_pixels += run;
                }
                run = 0;
            }
        }
    }
    let mut vertical_pixels = 0usize;
    for x in 0..bw {
        let mut run = 0usize;
        for y in 0..=bh {
            if y < bh && ink[y * bw + x] {
                run += 1;
            } else {
                if run >= vertical_len {
                    vertical_pixels += run;
                }
                run = 0;
            }
        }
    }
    if rule_pixels > bw * 3 {
        return if vertical_pixels > bh { 3 } else { 6 };
    }
    let min_row_ink = (bw / 100).max(12);
    let active: Vec<usize> = (0..bh)
        .filter(|&y| ink[y * bw..(y + 1) * bw].iter().filter(|&&v| v).count() >= min_row_ink)
        .collect();
    if active.is_empty() {
        return 3;
    }
    let (c0, c1) = ((bw as f32 * 0.35) as usize, (bw as f32 * 0.65) as usize);
    let (mut longest, mut run) = (0usize, 0usize);
    for x in c0..c1 {
        let column_ink = active.iter().filter(|&&y| ink[y * bw + x]).count();
        if column_ink as f32 / active.len() as f32 <= 0.15 {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let gutter = ((bw as f32 * 0.025) as usize).max(12);
    if longest >= gutter {
        3
    } else {
        6
    }
}

pub(crate) fn shrink(source: &GrayImage, max_w: u32, max_h: u32) -> GrayImage {
    let (w, h) = source.dimensions();
    let scale = (max_w as f32 / w as f32).min(max_h as f32 / h as f32);
    if scale >= 1.0 {
        return source.clone();
    }
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::resize(source, nw, nh, image::imageops::FilterType::Triangle)
}

pub(crate) fn otsu(image: &GrayImage) -> u8 {
    let mut histogram = [0u64; 256];
    for pixel in image.pixels() {
        histogram[pixel[0] as usize] += 1;
    }
    let total: u64 = histogram.iter().sum();
    let sum: f64 = histogram.iter().enumerate().map(|(i, &c)| i as f64 * c as f64).sum();
    let (mut weight_b, mut sum_b, mut best, mut threshold) = (0u64, 0f64, -1f64, 127u8);
    for (t, &count) in histogram.iter().enumerate() {
        weight_b += count;
        if weight_b == 0 {
            continue;
        }
        let weight_f = total - weight_b;
        if weight_f == 0 {
            break;
        }
        sum_b += t as f64 * count as f64;
        let mean_b = sum_b / weight_b as f64;
        let mean_f = (sum - sum_b) / weight_f as f64;
        let between = weight_b as f64 * weight_f as f64 * (mean_b - mean_f).powi(2);
        if between > best {
            best = between;
            threshold = t as u8;
        }
    }
    threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(binary: PathBuf, tessdata: PathBuf) -> OcrConfig {
        OcrConfig { binary, tessdata, language: "tur".into(), timeout: Duration::from_secs(60), threads: 1 }
    }

    /// Bundled model + system Tesseract, when this machine has them.
    fn real_tesseract() -> Option<OcrConfig> {
        // Windows (and any machine): point at an installed Tesseract.
        if let (Some(binary), Some(tessdata)) = (std::env::var_os("ARSIV_TEST_TESSERACT"), std::env::var_os("ARSIV_TEST_TESSDATA")) {
            return Some(config(binary.into(), tessdata.into()));
        }
        let tessdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/tessdata");
        let binary = ["/usr/bin/tesseract", "/usr/local/bin/tesseract"].iter().map(PathBuf::from).find(|p| p.is_file())?;
        tessdata.join("tur.traineddata").is_file().then(|| config(binary, tessdata))
    }

    #[test]
    fn check_reports_missing_binary_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let error = config(dir.path().join("nope"), dir.path().into()).check(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("binary not found"), "{error:#}");
        if let Some(real) = real_tesseract() {
            let error = config(real.binary, dir.path().into()).check(dir.path()).unwrap_err();
            assert!(format!("{error:#}").contains("model not found"), "{error:#}");
        }
    }

    /// The failure class `--version` could not see: model file present but
    /// not loadable by the binary. The probe must turn that into not-ready.
    #[test]
    fn check_rejects_a_model_the_binary_cannot_load() {
        let Some(real) = real_tesseract() else {
            eprintln!("skipped: tesseract or bundled tessdata unavailable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let tessdata = dir.path().join("tessdata");
        std::fs::create_dir_all(&tessdata).unwrap();
        std::fs::write(tessdata.join("tur.traineddata"), b"not a model").unwrap();
        let error = config(real.binary, tessdata).check(&dir.path().join("scratch")).unwrap_err();
        assert!(format!("{error:#}").contains("could not read a test image"), "{error:#}");
    }

    #[test]
    fn check_reads_the_probe_through_recognize() {
        let Some(real) = real_tesseract() else {
            eprintln!("skipped: tesseract or bundled tessdata unavailable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let detail = real.check(&dir.path().join("scratch")).unwrap();
        assert!(detail.contains("tur model verified"), "{detail}");
        // The probe image is cleaned up.
        assert_eq!(std::fs::read_dir(dir.path().join("scratch")).unwrap().count(), 0);
    }

    /// Folder names outside the system code page (Arabic on a Turkish PC)
    /// used to fail with "cannot read input file …???…": both the image and
    /// the model folder now reach Tesseract without their path.
    #[test]
    fn reads_images_and_models_in_non_ascii_folders() {
        let Some(real) = real_tesseract() else {
            eprintln!("skipped: tesseract or bundled tessdata unavailable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let tessdata = dir.path().join("Zoria Arşiv مجلد").join("tessdata");
        std::fs::create_dir_all(&tessdata).unwrap();
        std::fs::copy(real.tessdata.join("tur.traineddata"), tessdata.join("tur.traineddata")).unwrap();
        let photos = dir.path().join("مجلد اختبار").join("صور");
        std::fs::create_dir_all(&photos).unwrap();
        let image = photos.join("IMG_ölçüm.png");
        std::fs::write(&image, PROBE_PNG).unwrap();
        let config = config(real.binary, tessdata);
        assert!(config.check(&dir.path().join("scratch")).is_ok());
        let output = config.recognize(&image).unwrap();
        assert!(output.text.contains(PROBE_TOKEN), "{:?}", output.text);
    }

    #[test]
    fn tsv_keeps_literal_quotes_and_line_breaks() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\t\"Ankara'da\n\
                   5\t1\t1\t1\t1\t2\t0\t0\t10\t10\t70\tşirket\n\
                   5\t1\t1\t1\t2\t1\t0\t0\t10\t10\t-1\t \n\
                   5\t1\t1\t1\t2\t2\t0\t0\t10\t10\t50\tİSTANBUL\n";
        let (text, confidence) = parse_tsv(tsv).unwrap();
        assert_eq!(text, "\"Ankara'da şirket\nİSTANBUL\n");
        assert!((confidence.unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn two_column_page_selects_psm3_and_single_column_psm6() {
        let mut two = GrayImage::from_pixel(800, 1000, image::Luma([255]));
        let mut one = two.clone();
        for line in 0..40 {
            let y = 200 + line * 15;
            for x in 80..360 {
                if x % 9 != 0 {
                    two.put_pixel(x, y, image::Luma([0]));
                    two.put_pixel(x + 360, y, image::Luma([0]));
                }
            }
            for x in 80..720 {
                if x % 9 != 0 {
                    one.put_pixel(x, y, image::Luma([0]));
                }
            }
        }
        assert_eq!(select_psm(&two), 3);
        assert_eq!(select_psm(&one), 6);
    }
}

#[cfg(all(test, unix))]
mod cancel_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A cancel kills the running Tesseract process within one poll instead
    /// of waiting for it (or for the 120 s timeout).
    #[test]
    fn cancelling_kills_the_running_tesseract() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let binary = dir.path().join("tesseract");
        std::fs::write(&binary, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pid_file.display())).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = OcrConfig {
            binary,
            tessdata: dir.path().to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(60),
            threads: 1,
        };
        let image = dir.path().join("sayfa.png");
        GrayImage::from_pixel(40, 40, image::Luma([255])).save(&image).unwrap();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let error = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !pid_file.exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                cancel.store(true, Ordering::SeqCst);
            });
            config.recognize_cancellable(&image, Some(6), Some(&cancel)).unwrap_err()
        });
        assert!(is_cancelled(&error), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(10), "waited {:?}", started.elapsed());
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists(), "tesseract {pid} still running");
        // Already cancelled: nothing is started at all.
        std::fs::remove_file(&pid_file).unwrap();
        let error = config.recognize_cancellable(&image, Some(6), Some(&cancel)).unwrap_err();
        assert!(is_cancelled(&error));
        assert!(!pid_file.exists());
    }
}
