use super::*;
use super::tests::{request, test_engine};
use crate::model::PageText;

fn setup() -> (tempfile::TempDir, PathBuf, Arc<Engine>, i64) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("documents");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("seed.txt"), "seedpersistedanchor").unwrap();
    let engine = test_engine(&dir.path().join("data"));
    let folder = engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    (dir, root, engine, folder)
}

fn pending_store(engine: &Engine, folder: i64, path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    let (size, modified, fraction) = file_times(&std::fs::metadata(path).unwrap()).unwrap();
    let record = FileRecord {
        path: path_key(path), folder_id: folder, kind: FileKind::Text,
        size, modified, modified_subsec: Some(fraction), page_count: 1,
        ocr_pages: 0, low_confidence_pages: 0, failed_pages: 0,
        content_hash: hash_candidate(path, size, &AtomicBool::new(false)).unwrap(),
        ocr_language: None,
    };
    engine.catalog.lock().store(&record,
        &[PageText::new(0, text.into(), TextSource::Native, None)], &[], None, false, now()).unwrap();
}

fn barrier(engine: &Engine) {
    let (tx, rx) = channel();
    engine.jobs.send(Job::Barrier(tx)).unwrap();
    rx.recv_timeout(Duration::from_secs(15)).expect("indexer did not finish its queued work");
}

#[test]
fn interrupted_store_recovers_a_nonempty_index_from_cached_pages() {
    let (dir, root, engine, folder) = setup();
    pending_store(&engine, folder, &root.join("pending.txt"), "recovereduniqueneedle");
    assert!(engine.catalog.lock().index_dirty().unwrap());
    assert_eq!(engine.search(&request("recovereduniqueneedle", 20, 0)).unwrap().total, 0);
    drop(engine);
    let recovered = test_engine(&dir.path().join("data"));
    assert_eq!(recovered.search(&request("recovereduniqueneedle", 20, 0)).unwrap().total, 1);
    assert_eq!(recovered.search(&request("seedpersistedanchor", 20, 0)).unwrap().total, 1);
    assert!(!recovered.catalog.lock().index_dirty().unwrap());
}

#[test]
fn removing_another_folder_cannot_acknowledge_uncommitted_pages() {
    let (dir, root, engine, folder) = setup();
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("other.txt"), "removeotherneedle").unwrap();
    let other_id = engine.index_folder_blocking(other.to_str().unwrap(), &|_| {}).unwrap();
    pending_store(&engine, folder, &root.join("pending.txt"), "remaininguniqueneedle");
    engine.remove_folder(other_id).unwrap();
    assert_eq!(engine.search(&request("remaininguniqueneedle", 20, 0)).unwrap().total, 1);
    assert_eq!(engine.search(&request("removeotherneedle", 20, 0)).unwrap().total, 0);
    assert!(!engine.catalog.lock().index_dirty().unwrap());
}

#[test]
fn equal_length_edits_with_distinct_subsecond_mtimes_replace_old_content() {
    let (_dir, root, engine, _) = setup();
    let path = root.join("fraction.txt");
    let seconds = now().saturating_sub(30) as u64;
    let write = |text: &str, fraction: u32| {
        std::fs::write(&path, text).unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_times(
            std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::new(seconds, fraction)),
        ).unwrap();
    };
    write("alphaneedle", 100_000_000);
    engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    write("betaxneedle", 800_000_000);
    engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    assert_eq!(engine.search(&request("alphaneedle", 20, 0)).unwrap().total, 0);
    assert_eq!(engine.search(&request("betaxneedle", 20, 0)).unwrap().total, 1);
    let stored = engine.catalog.lock().file(&path_key(&path)).unwrap().unwrap();
    assert_eq!(stored.modified, seconds as i64);
    assert_eq!(stored.modified_subsec, Some(800_000_000));
}

#[test]
fn legacy_fraction_adoption_preserves_cached_embedding_chunks() {
    let (_dir, root, engine, _) = setup();
    let path = path_key(&root.join("seed.txt"));
    let chunks = vec![(0, vec![0.4_f32, 0.6])];
    {
        let mut catalog = engine.catalog.lock();
        catalog.store_chunks(&path, &chunks).unwrap();
        catalog.db_for_tests().execute("UPDATE files SET modified_subsec=NULL WHERE path=?1", [&path]).unwrap();
    }
    engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    let catalog = engine.catalog.lock();
    assert!(catalog.file(&path).unwrap().unwrap().modified_subsec.is_some());
    assert_eq!(catalog.chunks(&path).unwrap(), chunks);
}

#[test]
fn directory_replacement_removes_its_former_descendants_immediately() {
    let (dir, root, engine, _) = setup();
    let child = root.join("reports");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(child.join("a.txt"), "descendantuniqueneedle").unwrap();
    engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    std::fs::rename(&child, dir.path().join("moved-reports")).unwrap();
    std::fs::write(&child, "replacement").unwrap();
    engine.detached_worker().update_paths(vec![child]).unwrap();
    assert_eq!(engine.search(&request("descendantuniqueneedle", 20, 0)).unwrap().total, 0);
    assert!(engine.catalog.lock().indexed_paths().unwrap().iter().all(|p| !p.contains("reports")));
}

#[test]
fn failed_traversal_preserves_records_and_retry_clears_the_error() {
    let (_dir, root, engine, folder) = setup();
    let child = root.join("reports");
    let file = child.join("a.txt");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(&file, "traversalretainedneedle").unwrap();
    engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::remove_dir(&child).unwrap();
    // A traversal may fail after the caller has established that its root
    // exists. Its incomplete result must not establish document absence.
    let present = engine.detached_worker().collect_dir(folder, &child, &FileKind::DEFAULT, &mut Vec::new()).unwrap();
    assert!(present.contains(&path_key(&file)));
    assert_eq!(engine.search(&request("traversalretainedneedle", 20, 0)).unwrap().total, 1);
    assert_eq!(engine.failures(Some(folder)).unwrap().len(), 1);
    std::fs::create_dir(&child).unwrap();
    std::fs::write(&file, "traversalrestoredneedle fresh").unwrap();
    assert_eq!(engine.retry_failures(Some(folder)).unwrap(), 1);
    barrier(&engine);
    assert!(engine.failures(Some(folder)).unwrap().is_empty());
    assert_eq!(engine.search(&request("traversalrestoredneedle", 20, 0)).unwrap().total, 1);
}

#[test]
fn physical_folder_aliases_are_rejected() {
    let (dir, root, engine, _) = setup();
    let hop = dir.path().join("hop");
    std::fs::create_dir(&hop).unwrap();
    let alias = hop.join("..").join(root.file_name().unwrap());
    let error = engine.add_folder(alias.to_str().unwrap(), None).unwrap_err();
    assert_eq!(error.downcast_ref::<EngineError>().unwrap().code(), "folder_duplicate");
    assert_eq!(engine.folders().unwrap().len(), 1);
}

#[test]
fn hash_cancellation_stops_before_the_next_read() {
    struct Input<'a> { cancel: &'a AtomicBool, reads: usize }
    impl std::io::Read for Input<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            assert_eq!(self.reads, 1, "read continued after cancellation");
            buffer.fill(b'x');
            self.cancel.store(true, Ordering::SeqCst);
            Ok(buffer.len())
        }
    }
    let cancel = AtomicBool::new(false);
    let error = hash_reader(Input { cancel: &cancel, reads: 0 }, &cancel).unwrap_err();
    assert!(is_cancelled(&error));
}

#[test]
fn known_oversized_inputs_are_not_hashed_before_rejection() {
    let dir = tempfile::tempdir().unwrap();
    for (name, limit) in [("large.txt", MAX_TEXT_FILE_BYTES), ("large.log", MAX_TEXT_FILE_BYTES),
        ("large.png", MAX_IMAGE_FILE_BYTES), ("large.docx", MAX_OFFICE_FILE_BYTES), ("large.xlsx", MAX_OFFICE_FILE_BYTES)] {
        let path = dir.path().join(name);
        std::fs::File::create(&path).unwrap().set_len(limit + 1).unwrap();
        assert!(hash_candidate(&path, (limit + 1) as i64, &AtomicBool::new(false)).unwrap().is_none(), "{name}");
    }
}

#[cfg(windows)]
#[test]
fn case_only_rename_keeps_one_physical_file_identity() {
    let (_dir, root, engine, _) = setup();
    let before = root.join("seed.txt");
    let after = root.join("SEED.txt");
    std::fs::rename(&before, &after).unwrap();
    engine.detached_worker().update_paths(vec![before, after]).unwrap();
    assert_eq!(engine.search(&request("seedpersistedanchor", 20, 0)).unwrap().total, 1);
    assert_eq!(engine.document_count().unwrap(), 1);
}

#[cfg(windows)]
#[test]
fn watcher_and_full_scan_both_exclude_child_junctions() {
    let (dir, root, engine, _) = setup();
    let external = dir.path().join("external");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("outside.txt"), "outsideuniqueneedle").unwrap();
    let link = root.join("linked");
    let status = std::process::Command::new("cmd.exe").args(["/C", "mklink", "/J"]).arg(&link).arg(&external).status().unwrap();
    assert!(status.success());
    engine.detached_worker().update_paths(vec![link.clone(), link.join("outside.txt")]).unwrap();
    assert_eq!(engine.search(&request("outsideuniqueneedle", 20, 0)).unwrap().total, 0);
    engine.index_folder_blocking(root.to_str().unwrap(), &|_| {}).unwrap();
    assert_eq!(engine.search(&request("outsideuniqueneedle", 20, 0)).unwrap().total, 0);
    std::fs::remove_dir(&link).unwrap();
}
