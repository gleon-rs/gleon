//! I/O utilities for gleon.

use std::path::Path;

/// Errors that can occur during I/O operations.
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    /// IO error during file or directory access.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Error deserializing JSON content.
    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),
}

/// Loads and deserializes JSON content from the file at `path`.
///
/// # Errors
/// Returns [`IoError::Io`] if the file cannot be opened, or [`IoError::JsonParse`] if its
/// content is not valid JSON matching `T`.
pub fn load_json<T: serde::de::DeserializeOwned, P: AsRef<Path>>(path: P) -> Result<T, IoError> {
    let path = path.as_ref();
    std::fs::File::open(path)
        .map_err(|e| {
            tracing::debug!("Failed to open JSON file at {:?}: {}", path, e);
            IoError::Io(e)
        })
        .and_then(|file| {
            let reader = std::io::BufReader::new(file);
            serde_json::from_reader(reader).map_err(|e| {
                tracing::error!("Failed to parse JSON file at {:?}: {}", path, e);
                IoError::JsonParse(e)
            })
        })
}

/// Loads the JSON (or uses Default if missing), applies the closure, and saves it atomically.
///
/// # Errors
/// Returns `E` if loading fails for a reason other than the file being missing, if the
/// closure `f` returns an error, or if the atomic save fails.
pub fn update_json_atomically<T, P, F, D, E>(path: P, default_fn: D, f: F) -> Result<(), E>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    P: AsRef<Path>,
    D: FnOnce() -> T,
    F: FnOnce(&mut T) -> Result<(), E>,
    E: From<IoError>,
{
    let path = path.as_ref();
    let mut value = match load_json(path) {
        Ok(val) => val,
        Err(IoError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => default_fn(),
        Err(e) => return Err(E::from(e)),
    };

    match f(&mut value) {
        Ok(()) => save_json_atomically(path, &value).map_err(E::from),
        Err(err) => Err(err),
    }
}

/// Writes to a temporary file created next to `path` via the closure `f`, then durably and
/// atomically persists it to `path` (see [`gleon_model::fs::write_atomically_with`]).
///
/// # Errors
/// Returns `E` if `path` is the root, the parent directory cannot be created, the temporary file
/// cannot be created or written, the closure `f` returns an error, or the final persist/fsync
/// step fails.
pub fn write_file_atomically<P, F, E>(path: P, f: F) -> Result<(), E>
where
    P: AsRef<Path>,
    F: FnOnce(&mut std::io::BufWriter<&std::fs::File>) -> Result<(), E>,
    E: From<IoError>,
{
    let path = path.as_ref();
    // The closure's own error is carried out of the `io::Result` of the shared writer.
    let mut closure_error = None;
    gleon_model::fs::write_atomically_with(path, gleon_model::fs::Durability::Durable, |writer| {
        f(writer).map_err(|e| {
            closure_error = Some(e);
            std::io::Error::other("the writer failed")
        })
    })
    .map_err(|e| {
        tracing::error!("Failed to save file atomically to {:?}: {}", path, e);
        closure_error
            .take()
            .unwrap_or_else(|| E::from(IoError::Io(e)))
    })
}

/// Atomically writes raw bytes to `path`.
///
/// # Errors
/// Returns [`IoError`] if the write or the atomic persist step fails.
pub fn save_file_atomically<P: AsRef<Path>>(path: P, content: &[u8]) -> Result<(), IoError> {
    write_file_atomically(path, |writer| {
        use std::io::Write;
        writer.write_all(content).map_err(IoError::Io)
    })
}

/// Serializes `value` to pretty-printed JSON and atomically writes it to `path`.
///
/// # Errors
/// Returns [`IoError::JsonParse`] if serialization fails, or [`IoError::Io`] if the atomic
/// write fails.
pub fn save_json_atomically<T: serde::Serialize + ?Sized, P: AsRef<Path>>(
    path: P,
    value: &T,
) -> Result<(), IoError> {
    write_file_atomically(path, |writer| {
        serde_json::to_writer_pretty(writer, value).map_err(IoError::JsonParse)
    })
}

/// Most threads working on small files (reading manifests and case reports, removing case
/// reports): beyond a few, file operations contend in the kernel. Loading 50k manifests (`tests/manifest_scale.rs`, M3 Max, 14 cores) takes:
/// - macOS on APFS: 1.03 s on one thread, 0.57-0.62 s on three, 0.59-0.70 s on four, 0.87 s on
///   six and 1.8 s on fourteen;
/// - Linux 7.0 on ext4 (Docker VM on the same machine): 158 ms on one thread, 91 ms on three,
///   78 ms on four, 70 ms on six, 69 ms on eight and 168 ms on fourteen.
///
/// Removing 25k case reports (`tests/cases_scale.rs`, macOS on APFS, parsing included) takes 2.55 s
/// on one thread, 1.71 s on three, 1.85 s on six and 2.6 s on ten.
///
/// Windows is not measured yet and keeps the cautious macOS value.
#[cfg(target_os = "linux")]
pub(crate) const FILE_THREADS: usize = 6;
/// See the Linux value.
#[cfg(not(target_os = "linux"))]
pub(crate) const FILE_THREADS: usize = 3;

/// Fewest files per thread: starting a thread costs tens of microseconds, reading or removing 64
/// small files about a millisecond.
pub(crate) const MIN_FILES_PER_THREAD: usize = 64;

/// `work` on every item of `files`, results in their order, on up to [`FILE_THREADS`] threads (on
/// this one for a few files).
pub(crate) fn map_files<F: Sync, T: Send>(files: &[F], work: impl Fn(&F) -> T + Sync) -> Vec<T> {
    let read_chunk = |chunk: &[F]| -> Vec<T> { chunk.iter().map(&work).collect() };
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get().min(FILE_THREADS))
        .min(files.len().div_ceil(MIN_FILES_PER_THREAD));
    if threads <= 1 {
        return read_chunk(files);
    }
    std::thread::scope(|scope| {
        // Collected: every worker must be spawned before the first is joined.
        let workers: Vec<_> = files
            .chunks(files.len().div_ceil(threads))
            .map(|chunk| scope.spawn(move || read_chunk(chunk)))
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use serde::Serialize;

    use super::*;

    /// A panic of one worker reaches the caller, like it would on one thread.
    #[test]
    #[cfg_attr(miri, ignore = "spawns threads over many items")]
    fn test_map_files_passes_on_a_panic_of_a_worker() {
        let files: Vec<usize> = (0..MIN_FILES_PER_THREAD * FILE_THREADS).collect();
        let last = files.len() - 1;
        let panic = std::panic::catch_unwind(|| {
            map_files(&files, |&file| assert!(file != last, "file {file}"))
        })
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<String>().map(String::as_str),
            Some(&*format!("file {last}"))
        );
        assert_eq!(map_files(&files, |&file| file * 2)[last], last * 2);
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_write_file_atomically_new_file_is_group_world_readable() {
        // A freshly created file must land with the usual 0644-style mode, not the 0600 that
        // `tempfile` defaults to: manifests written this way are committed to Git and read back
        // by other users/containers in CI.
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fresh.json");
        save_file_atomically(&path, b"{}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "expected 0644 for a new file, got {mode:o}");
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_write_file_atomically_preserves_existing_mode() {
        // When the target already exists its mode wins, so a deliberately locked-down file
        // stays locked down across rewrites.
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        save_file_atomically(&path, b"new").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing mode must be preserved, got {mode:o}");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[derive(Serialize)]
    struct Dummy {
        value: String,
    }

    #[test]
    fn test_write_file_atomically_returns_the_writer_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a.json");
        let result = write_file_atomically(&path, |_| {
            Err(IoError::Io(std::io::Error::other("serializer failed")))
        });
        assert!(matches!(result, Err(IoError::Io(ref e)) if e.to_string() == "serializer failed"));
        assert!(!path.exists());
    }

    #[test]
    fn test_save_json_atomically_root_path_fails() {
        let dummy = Dummy {
            value: "test".to_string(),
        };
        // Saving to "/" should fail because it has no parent directory
        let result = save_json_atomically(Path::new("/"), &dummy);
        assert!(matches!(
            result,
            Err(IoError::Io(ref err))
                if err.kind() == std::io::ErrorKind::InvalidInput
                    && err.to_string() == "Cannot resolve parent directory for root path"
        ));
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug, Default)]
    struct TestData {
        count: u32,
    }

    #[test]
    fn test_update_json_atomically_missing_file_uses_default() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("data.json");

        update_json_atomically::<TestData, _, _, _, IoError>(
            &file_path,
            TestData::default,
            |data: &mut TestData| {
                data.count += 5;
                Ok(())
            },
        )
        .unwrap();

        let loaded: TestData = load_json(&file_path).unwrap();
        assert_eq!(loaded, TestData { count: 5 });
    }

    #[test]
    fn test_update_json_atomically_corrupted_file_fails() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("corrupt.json");

        // Write invalid JSON content to simulate file corruption
        std::fs::write(&file_path, "{ invalid json ").unwrap();

        let result = update_json_atomically::<TestData, _, _, _, IoError>(
            &file_path,
            TestData::default,
            |data: &mut TestData| {
                data.count += 5;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), IoError::JsonParse(_)));

        // Verify the corrupted file content was NOT overwritten
        let raw_content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(raw_content, "{ invalid json ");
    }

    #[test]
    fn test_io_error_display() {
        let err1 = IoError::Io(std::io::Error::other("io test"));
        assert!(err1.to_string().contains("IO error"));

        let serde_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("{ invalid").unwrap_err();
        let err2 = IoError::JsonParse(serde_err);
        assert!(err2.to_string().contains("JSON parse error"));
    }
}
