//! Tauri boundary. Commands serialize `arsiv-core` types; errors cross the IPC
//! line as `ApiError { code, message }` per docs/API.md.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arsiv_core::catalog::{FailureInfo, FolderInfo};
use arsiv_core::embed::EmbedConfig;
use arsiv_core::engine::{
    DocumentView, Engine, EngineError, EnginePaths, IndexProgress, OcrLanguageInfo, PageImage, SearchRequest,
    SearchResponse,
};
use arsiv_core::model::FileKind;
use arsiv_core::ocr::OcrConfig;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiError {
    code: String,
    message: String,
}

fn api_error(error: anyhow::Error) -> ApiError {
    let code = error
        .downcast_ref::<EngineError>()
        .map(EngineError::code)
        .unwrap_or("internal");
    ApiError { code: code.to_string(), message: format!("{error:#}") }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        api_error(error)
    }
}

struct AppState {
    engine: Arc<Engine>,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    ocr_ready: bool,
    ocr_detail: String,
    pdf_ready: bool,
    pdf_detail: String,
    /// EmbeddingGemma 2 usable → semantic side of the hybrid search is on.
    semantic_ready: bool,
    semantic_detail: String,
    index_dir: String,
    document_count: i64,
}

/// Runs blocking engine work (Tantivy, SQLite, Tesseract probe) on the
/// blocking pool so the IPC thread and the webview never wait on it.
async fn blocking<T, F>(state: &State<'_, AppState>, work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Engine) -> Result<T> + Send + 'static,
{
    run_blocking(state.engine.clone(), work).await
}

async fn run_blocking<T, F>(engine: Arc<Engine>, work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Engine) -> Result<T> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || work(&engine))
        .await
        .map_err(|e| ApiError { code: "internal".into(), message: format!("worker failed: {e}") })?
        .map_err(api_error)
}

#[tauri::command]
async fn app_info(state: State<'_, AppState>) -> Result<AppInfo, ApiError> {
    let version = state.version;
    blocking(&state, move |engine| {
        let (ocr_ready, ocr_detail) = match engine.ocr_status() {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        let (pdf_ready, pdf_detail) = match engine.pdf_status() {
            Ok(()) => (true, "pdfium ready".into()),
            Err(detail) => (false, detail),
        };
        let (semantic_ready, semantic_detail) = match engine.semantic_status() {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        Ok(AppInfo {
            version,
            ocr_ready,
            ocr_detail,
            pdf_ready,
            pdf_detail,
            semantic_ready,
            semantic_detail,
            index_dir: engine.index_dir().to_string_lossy().into_owned(),
            document_count: engine.document_count()?,
        })
    })
    .await
}

#[tauri::command]
async fn list_folders(state: State<'_, AppState>) -> Result<Vec<FolderInfo>, ApiError> {
    blocking(&state, |engine| engine.folders()).await
}

/// File kinds from the UI; unknown names are rejected, not ignored.
fn parse_kinds(kinds: &[String]) -> Result<Vec<FileKind>, ApiError> {
    kinds
        .iter()
        .map(|kind| {
            FileKind::parse(kind)
                .ok_or_else(|| ApiError { code: "internal".into(), message: format!("unknown file kind {kind:?}") })
        })
        .collect()
}

#[tauri::command]
async fn add_folder(path: String, kinds: Option<Vec<String>>, state: State<'_, AppState>) -> Result<FolderInfo, ApiError> {
    // A `\\?\`-prefixed folder would become every file's catalog key and
    // reach Tesseract, which cannot open such paths (see `resource_dir`).
    let path = dunce::simplified(Path::new(&path)).to_string_lossy().into_owned();
    if !Path::new(&path).is_dir() {
        return Err(ApiError { code: "folder_missing".into(), message: path });
    }
    let kinds = kinds.as_deref().map(parse_kinds).transpose()?;
    blocking(&state, move |engine| engine.add_folder(&path, kinds.as_deref())).await
}

#[tauri::command]
async fn set_folder_kinds(id: i64, kinds: Vec<String>, state: State<'_, AppState>) -> Result<FolderInfo, ApiError> {
    let kinds = parse_kinds(&kinds)?;
    blocking(&state, move |engine| engine.set_folder_kinds(id, &kinds)).await
}

/// Returns at once; the run winds down in the background and reports
/// through `index://progress` (`cancelling`, then `cancelled`).
#[tauri::command]
async fn cancel_indexing(state: State<'_, AppState>) -> Result<bool, ApiError> {
    blocking(&state, |engine| Ok(engine.cancel_indexing())).await
}

#[tauri::command]
async fn remove_folder(id: i64, state: State<'_, AppState>) -> Result<(), ApiError> {
    blocking(&state, move |engine| engine.remove_folder(id)).await
}

#[tauri::command]
async fn reindex(folder_id: Option<i64>, state: State<'_, AppState>) -> Result<(), ApiError> {
    blocking(&state, move |engine| engine.reindex(folder_id)).await
}

/// "Şimdi tamamla": finish the pending semantic index (embedding backfill).
#[tauri::command]
async fn complete_embeddings(state: State<'_, AppState>) -> Result<(), ApiError> {
    blocking(&state, |engine| engine.complete_embeddings()).await
}

/// Document language for scanned pages (Settings page).
#[tauri::command]
async fn ocr_language(state: State<'_, AppState>) -> Result<OcrLanguageInfo, ApiError> {
    blocking(&state, |engine| engine.ocr_language()).await
}

/// Saves the document language; scans read with another one are read again
/// in the background.
#[tauri::command]
async fn set_ocr_language(language: String, state: State<'_, AppState>) -> Result<OcrLanguageInfo, ApiError> {
    blocking(&state, move |engine| engine.set_ocr_language(&language)).await
}

#[tauri::command]
fn index_status(state: State<'_, AppState>) -> IndexProgress {
    state.engine.progress()
}

#[tauri::command]
async fn list_failures(folder_id: Option<i64>, state: State<'_, AppState>) -> Result<Vec<FailureInfo>, ApiError> {
    blocking(&state, move |engine| engine.failures(folder_id)).await
}

/// "Tekrar dene": read unreadable / partly read files again now.
#[tauri::command]
async fn retry_failures(folder_id: Option<i64>, state: State<'_, AppState>) -> Result<usize, ApiError> {
    blocking(&state, move |engine| engine.retry_failures(folder_id)).await
}

#[tauri::command]
async fn search(request: SearchRequestInput, state: State<'_, AppState>) -> Result<SearchResponse, ApiError> {
    let request = SearchRequest {
        query: request.query,
        kinds: request.kinds.into_iter().filter_map(|k| FileKind::parse(&k)).collect(),
        modified_after: request.modified_after,
        modified_before: request.modified_before,
        limit: request.limit,
        offset: request.offset,
    };
    blocking(&state, move |engine| engine.search(&request)).await
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRequestInput {
    query: String,
    kinds: Vec<String>,
    modified_after: Option<i64>,
    modified_before: Option<i64>,
    limit: usize,
    offset: usize,
}

#[tauri::command]
async fn get_document(path: String, query: String, state: State<'_, AppState>) -> Result<DocumentView, ApiError> {
    blocking(&state, move |engine| engine.document(&path, &query)).await
}

#[tauri::command]
async fn get_page_image(path: String, page: usize, width: u32, state: State<'_, AppState>) -> Result<PageImage, ApiError> {
    blocking(&state, move |engine| engine.page_image(&path, page, width)).await
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    /// False when the build carries no updater public key: checks are off.
    enabled: bool,
    available: bool,
    version: Option<String>,
    notes: Option<String>,
}

/// `plugins.updater.pubkey` from tauri.conf.json, when set. The public key is
/// not secret; the private signing key never enters the repo (README).
fn updater_pubkey(config: &tauri::Config) -> Option<String> {
    config
        .plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("pubkey"))
        .and_then(|key| key.as_str())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
}

fn updater_error(error: impl std::fmt::Display) -> ApiError {
    ApiError { code: "update_failed".into(), message: error.to_string() }
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<UpdateInfo, ApiError> {
    if updater_pubkey(app.config()).is_none() {
        return Ok(UpdateInfo::default());
    }
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater().map_err(updater_error)?.check().await.map_err(updater_error)?;
    Ok(UpdateInfo {
        enabled: true,
        available: update.is_some(),
        version: update.as_ref().map(|u| u.version.clone()),
        notes: update.and_then(|u| u.body),
    })
}

/// Downloads, verifies (signature) and installs the pending update, then
/// restarts. On Windows the installer runs in passive mode.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), ApiError> {
    if updater_pubkey(app.config()).is_none() {
        return Err(updater_error("updates are disabled in this build"));
    }
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(updater_error)?
        .check()
        .await
        .map_err(updater_error)?
        .ok_or_else(|| updater_error("no update available"))?;
    update.download_and_install(|_, _| {}, || {}).await.map_err(updater_error)?;
    app.restart()
}

/// A file that was moved or deleted since it was indexed: say so
/// ("Dosya artık bulunamıyor") instead of "Beklenmeyen bir hata".
fn ensure_exists(path: &str) -> Result<(), ApiError> {
    if Path::new(path).exists() {
        Ok(())
    } else {
        Err(ApiError { code: "file_missing".into(), message: path.to_string() })
    }
}

#[tauri::command]
async fn open_path(path: String, app: AppHandle) -> Result<(), ApiError> {
    use tauri_plugin_opener::OpenerExt;
    ensure_exists(&path)?;
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| ApiError { code: "internal".into(), message: e.to_string() })
}

#[tauri::command]
async fn reveal_path(path: String, app: AppHandle) -> Result<(), ApiError> {
    use tauri_plugin_opener::OpenerExt;
    ensure_exists(&path)?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| ApiError { code: "internal".into(), message: e.to_string() })
}

/// Legal texts bundled with the app for the About page: third-party license
/// notices (generated by `scripts/gen-notices.py`), the path of their
/// plain-text copy so the user can open it in any editor, the app's own
/// terms of use in Turkish and English (the installer shows both), and its
/// Apache-2.0 license.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Notices {
    json: String,
    file: String,
    terms: String,
    terms_en: String,
    license: String,
}

fn read_text(path: &Path) -> Result<String, ApiError> {
    Ok(std::fs::read_to_string(path)
        .map_err(|e| ApiError { code: "file_missing".into(), message: format!("{}: {e}", path.display()) })?
        .trim_start_matches('\u{feff}')
        .to_owned())
}

#[tauri::command]
async fn third_party_notices(app: AppHandle) -> Result<Notices, ApiError> {
    let dir = resource_dir(&app, "licenses").ok_or_else(|| ApiError {
        code: "file_missing".into(),
        message: "licenses".into(),
    })?;
    tauri::async_runtime::spawn_blocking(move || {
        let file = dir.join("THIRD_PARTY_NOTICES.txt").to_string_lossy().into_owned();
        Ok(Notices {
            json: read_text(&dir.join("third-party.json"))?,
            file,
            terms: read_text(&dir.join("KULLANIM_KOSULLARI.txt"))?,
            terms_en: read_text(&dir.join("TERMS.txt"))?,
            license: read_text(&dir.join("LICENSE.txt"))?,
        })
    })
    .await
    .map_err(|e| ApiError { code: "internal".into(), message: e.to_string() })?
}

/// Resource directory candidates for bundled binaries, then PATH fallback.
fn resource_dir(app: &AppHandle, name: &str) -> Option<PathBuf> {
    if let Ok(dir) = app.path().resource_dir() {
        let candidate = dir.join(name);
        if candidate.is_dir() {
            // Strip Windows `\\?\` verbatim prefix: tesseract.exe is a C binary
            // and cannot open `\\?\`-prefixed tessdata paths.
            return Some(dunce::simplified(&candidate).to_path_buf());
        }
    }
    // Dev builds: `<repo>/resources/<name>` relative to the binary target dir.
    if let Ok(exe) = std::env::current_exe() {
        for up in [3usize, 4, 5] {
            let mut base = exe.clone();
            for _ in 0..up {
                if !base.pop() {
                    break;
                }
            }
            let candidate = base.join("resources").join(name);
            if candidate.is_dir() {
                return Some(dunce::simplified(&candidate).to_path_buf());
            }
        }
    }
    None
}

fn ocr_config(app: &AppHandle) -> OcrConfig {
    let binary_name = if cfg!(windows) { "tesseract.exe" } else { "tesseract" };
    let binary = resource_dir(app, "bin")
        .map(|dir| dir.join(binary_name))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(binary_name));
    let tessdata = resource_dir(app, "tessdata")
        .unwrap_or_else(|| PathBuf::from("/usr/share/tesseract-ocr/5/tessdata"));
    OcrConfig { binary, tessdata, language: "tur".into(), timeout: Duration::from_secs(120), threads: 1 }
}

/// Semantic pieces under `resources/`: `ort/` holds the ONNX Runtime
/// library, `models/` the EmbeddingGemma 2 ONNX export + tokenizer.
/// Missing directories keep `None` — search stays pure BM25.
fn embed_config(app: &AppHandle) -> Option<EmbedConfig> {
    let runtime = resource_dir(app, "ort")?;
    let models = resource_dir(app, "models")?;
    Some(EmbedConfig { runtime, models })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init());
    // Without a configured public key the updater is not even registered.
    if updater_pubkey(context.config()).is_some() {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }
    builder
        .setup(|app| {
            let handle = app.handle().clone();
            // OCR scratch renders live below it and go to Tesseract: no
            // `\\?\` prefix (see `resource_dir`).
            let data_dir = dunce::simplified(
                &handle.path().app_data_dir().map_err(|e| anyhow::anyhow!("app_data_dir: {e}"))?,
            )
            .join("index");
            let pdfium_dir = resource_dir(&handle, "pdfium").unwrap_or_else(|| PathBuf::from("."));
            let progress_handle = handle.clone();
            let changed_handle = handle.clone();
            let engine = Engine::open(
                EnginePaths {
                    data_dir,
                    pdfium_dir,
                    ocr: ocr_config(&handle),
                    ocr_jobs: 0,
                    // Lab: R@10 +0.006 (13 queries better, 1 worse), OCR
                    // confidence up (deskew).
                    deskew: true,
                    embed: embed_config(&handle),
                },
                Arc::new(move |p: &IndexProgress| {
                    let _ = progress_handle.emit("index://progress", p);
                }),
                Arc::new(move |count: i64| {
                    let _ = changed_handle.emit("index://changed", serde_json::json!({ "documentCount": count }));
                }),
            )?;
            app.manage(AppState { engine, version: env!("CARGO_PKG_VERSION") });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_folders,
            add_folder,
            set_folder_kinds,
            cancel_indexing,
            remove_folder,
            reindex,
            complete_embeddings,
            ocr_language,
            set_ocr_language,
            index_status,
            list_failures,
            retry_failures,
            search,
            get_document,
            get_page_image,
            check_update,
            install_update,
            open_path,
            reveal_path,
            third_party_notices,
        ])
        .run(context)
        .expect("zoria-arsiv crashed");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Command bodies run on the blocking pool, never on the IPC caller's thread.
    #[test]
    fn engine_work_runs_off_the_calling_thread() {
        let dir = std::env::temp_dir().join(format!("arsiv-blocking-{}", std::process::id()));
        let engine = Engine::open(
            EnginePaths {
                data_dir: dir.clone(),
                pdfium_dir: dir.join("no-pdfium"),
                ocr: OcrConfig {
                    binary: dir.join("no-tesseract"),
                    tessdata: dir.join("no-tessdata"),
                    language: "tur".into(),
                    timeout: Duration::from_secs(5),
                    threads: 1,
                },
                ocr_jobs: 1,
                deskew: false,
                embed: None,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap();
        let caller = std::thread::current().id();
        let (worker, count) = tauri::async_runtime::block_on(run_blocking(engine, |engine| {
            Ok((std::thread::current().id(), engine.document_count()?))
        }))
        .unwrap_or_else(|e| panic!("{}: {}", e.code, e.message));
        assert_ne!(worker, caller);
        assert_eq!(count, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn updates_stay_off_until_a_public_key_is_configured() {
        let config: tauri::Config = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        // The repo ships without a key: the updater must not be registered.
        assert_eq!(updater_pubkey(&config), None);
        let updater = &config.plugins.0["updater"];
        let endpoint = updater["endpoints"][0].as_str().unwrap();
        assert!(endpoint.starts_with("https://github.com/ZoriaSoft/local-search-ai/releases/"), "{endpoint}");
        assert!(endpoint.ends_with("/latest.json"), "{endpoint}");
        assert_ne!(config.bundle.create_updater_artifacts, tauri::utils::config::Updater::Bool(true));

        let mut keyed = config.clone();
        keyed.plugins.0.insert("updater".into(), serde_json::json!({ "pubkey": "  dW50cnVzdGVk  ", "endpoints": [] }));
        assert_eq!(updater_pubkey(&keyed).as_deref(), Some("dW50cnVzdGVk"));
    }

    /// The installer shows both terms texts; its file must be exactly the
    /// English and Turkish files (which the About page shows) joined.
    #[test]
    fn installer_terms_are_the_english_and_turkish_terms_joined() {
        let strip = |t: &'static str| t.trim_start_matches('\u{feff}').to_string();
        let en = strip(include_str!("../../resources/licenses/TERMS.txt"));
        let tr = strip(include_str!("../../resources/licenses/KULLANIM_KOSULLARI.txt"));
        let installer = strip(include_str!("../../resources/licenses/INSTALLER_TERMS.txt"));
        let separator = format!("\r\n{}\r\n\r\n", "-".repeat(72));
        assert_eq!(installer, format!("{en}{separator}{tr}"));
        assert!(en.contains("Apache License, Version 2.0") && tr.contains("Apache Lisansı 2.0"));
        let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["bundle"]["licenseFile"], "../resources/licenses/INSTALLER_TERMS.txt");
        assert_eq!(config["bundle"]["resources"]["../LICENSE"], "licenses/LICENSE.txt");
    }

    /// Release bundles must not ship the web inspector: `devtools` stays an
    /// opt-in crate feature, never a default feature of the tauri dependency.
    #[test]
    fn devtools_is_not_a_default_tauri_feature() {
        let manifest = include_str!("../Cargo.toml");
        let tauri_line = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("tauri = "))
            .expect("tauri dependency line");
        assert!(!tauri_line.contains("devtools"), "{tauri_line}");
        assert!(manifest.contains("devtools = [\"tauri/devtools\"]"));
    }
}
