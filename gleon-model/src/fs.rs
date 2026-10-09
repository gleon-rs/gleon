//! Atomic file writes shared by the gleon CLI and other integrations (through `gleon-ffi`).
//!
//! Every write goes to a temporary file next to its target and is renamed over it, so a reader
//! (another process, a concurrent test) never sees a partial file. How much durability a write
//! also needs depends on the data, see [`Durability`].

use std::{
    fs,
    io::{self, BufWriter, Write as _},
    path::{Path, PathBuf},
};

/// Whether a write must survive a power loss, not only be atomic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Committed data (goldens, manifests, blobs, the config): the file is flushed to disk before
    /// the rename and its directory entry afterwards. Costly (`F_FULLFSYNC` on macOS, around
    /// 5 ms per flush).
    Durable,
    /// Output the next run recreates (case reports, failure artifacts): atomic, but left to the OS
    /// cache. A power loss may lose the newest version, never corrupt it.
    Atomic,
}

/// Writes `bytes` to `path` atomically, creating missing parent directories.
///
/// See [`write_atomically_with`].
///
/// # Errors
/// Returns the I/O error of creating, writing or renaming the file.
pub fn write_atomically(path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
    write_atomically_with(path, durability, |writer| writer.write_all(bytes))
}

/// Removes the file at `path`; a file that is not there is fine.
///
/// # Errors
/// Returns any other I/O error of the removal.
pub fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Writes `bytes` to `path` atomically ([`write_atomically`]), or without bytes removes `path`
/// ([`remove_if_exists`]), so an image of an earlier outcome never passes for this one's.
///
/// # Errors
/// Returns the I/O error of the write or the removal.
pub fn write_or_remove(
    path: &Path,
    bytes: Option<&[u8]>,
    durability: Durability,
) -> io::Result<()> {
    bytes.map_or_else(
        || remove_if_exists(path),
        |bytes| write_atomically(path, bytes, durability),
    )
}

/// Writes the output of `write` to `path` atomically, creating missing parent directories.
///
/// A symbolic link at `path` is written through (its target is replaced, the link stays). On
/// Unix the file gets the permission bits of the file it replaces, or `0644` for a new one (a
/// temporary file alone would be `0600`, while these files are committed or read by other users).
/// A failed write removes the temporary file.
///
/// # Errors
/// Returns the I/O error of `write` or of creating, flushing or renaming the file;
/// [`io::ErrorKind::InvalidInput`] if `path` has no parent (the root) or no file name.
pub fn write_atomically_with(
    path: &Path,
    durability: Durability,
    write: impl FnOnce(&mut BufWriter<&fs::File>) -> io::Result<()>,
) -> io::Result<()> {
    let target = resolve_link(path);
    let parent = parent(&target)?;
    let file_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid file name"))?;
    fs::create_dir_all(parent)?;
    let temp = tempfile::Builder::new()
        .prefix(file_name)
        .suffix(".tmp")
        .tempfile_in(parent)?;
    {
        let mut writer = BufWriter::new(temp.as_file());
        write(&mut writer)?;
        writer.flush()?;
    }
    set_mode(&temp, &target)?;
    if durability == Durability::Durable {
        temp.as_file().sync_all()?;
    }
    temp.persist(&target).map_err(|e| e.error)?;
    if durability == Durability::Durable {
        sync_dir(parent);
    }
    Ok(())
}

/// Creates `path` with `bytes` unless it already exists; an existing file is never touched, and
/// one created concurrently by another process is not an error.
///
/// Meant for small scaffold files (`.gleon/.gitignore`): an exclusive create, without a temporary
/// file or a flush.
///
/// # Errors
/// Returns the I/O error of creating or writing the file.
pub fn create_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    create_new_with(path, |file| file.write_all(bytes))
}

/// [`create_new`] with the content from `write`. A failed write removes the file: a partial one
/// would never be completed, since the next call keeps it.
fn create_new_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => write(&mut file).inspect_err(|_| {
            let _ = fs::remove_file(path);
        }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Most links followed from one path (Linux's `SYMLOOP_MAX`); a longer chain is a cycle.
const MAX_LINKS: usize = 40;

/// The file a symbolic link at `path` points to, following chains, or `path` itself. The target
/// need not exist yet (a link to a golden that is written for the first time).
fn resolve_link(path: &Path) -> PathBuf {
    let mut target = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        let Ok(next) = fs::read_link(&target) else {
            break;
        };
        // A relative target is relative to the link's directory; `join` keeps an absolute one.
        target = target
            .parent()
            .map_or_else(|| next.clone(), |dir| dir.join(&next));
    }
    target
}

/// The directory of `path`; `.` for a bare file name.
fn parent(path: &Path) -> io::Result<&Path> {
    match path.parent() {
        Some(dir) if dir.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(dir) => Ok(dir),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot resolve parent directory for root path",
        )),
    }
}

/// Commits the directory entry of a rename, best effort: Windows cannot sync directories, and
/// NFS/SMB/FUSE mounts (Docker volumes, WSL) reject it; the file itself is already durable.
fn sync_dir(dir: &Path) {
    if let Err(e) = fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        tracing::debug!("Directory fsync not supported for {dir:?} ({e}); the file is durable");
    }
}

#[cfg(all(unix, not(miri)))]
fn set_mode(temp: &tempfile::NamedTempFile, target: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    const NEW_FILE_MODE: u32 = 0o644;
    let mode = fs::metadata(target).map_or(NEW_FILE_MODE, |existing| {
        existing.permissions().mode() & 0o7777
    });
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(all(unix, not(miri))))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the Unix variant, which can fail"
)]
const fn set_mode(_: &tempfile::NamedTempFile, _: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, not(miri)))]
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
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn test_writes_creating_parents_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.bin");
        for durability in [Durability::Durable, Durability::Atomic] {
            write_atomically(&path, &[1, 2, 3], durability).unwrap();
            write_atomically(&path, &[4, 5], durability).unwrap();
            assert_eq!(fs::read(&path).unwrap(), [4, 5]);
            assert_eq!(entries(path.parent().unwrap()), ["c.bin"]);
        }
    }

    #[test]
    fn test_paths_without_a_parent() {
        assert_eq!(parent(Path::new("file.png")).unwrap(), Path::new("."));
        assert_eq!(parent(Path::new("dir/file.png")).unwrap(), Path::new("dir"));
        let error = write_atomically(Path::new("dir/.."), b"x", Durability::Atomic).unwrap_err();
        assert_eq!(error.to_string(), "Invalid file name");
        let error = write_atomically(Path::new("/"), b"x", Durability::Atomic).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "Cannot resolve parent directory for root path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_new_files_are_world_readable_and_replacements_keep_the_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("case.json");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        write_atomically(&path, b"{}", Durability::Atomic).unwrap();
        assert_eq!(mode(&path), 0o644);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomically(&path, b"{}", Durability::Durable).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn test_a_symbolic_link_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("shared/a.png");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"old").unwrap();
        let link = dir.path().join("a.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomically(&link, b"new", Durability::Durable).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn test_a_link_to_a_missing_file_creates_the_file_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("shared")).unwrap();
        let link = dir.path().join("a.png");
        std::os::unix::fs::symlink("shared/b.png", &link).unwrap();
        let chain = dir.path().join("c.png");
        std::os::unix::fs::symlink("a.png", &chain).unwrap();
        write_atomically(&chain, b"new", Durability::Atomic).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(&chain)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(dir.path().join("shared/b.png")).unwrap(), b"new");
    }

    #[test]
    fn test_a_target_that_cannot_be_replaced() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty directory cannot be replaced by a file on any OS.
        let target = dir.path().join("busy");
        fs::create_dir_all(target.join("inside")).unwrap();
        assert!(write_atomically(&target, b"x", Durability::Atomic).is_err());
        assert_eq!(
            entries(dir.path()),
            ["busy"],
            "the temporary file is removed"
        );
    }

    #[test]
    fn test_a_failing_writer_leaves_the_target_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(&path, b"old").unwrap();
        let result = write_atomically_with(&path, Durability::Atomic, |_| {
            Err(io::Error::other("serializer failed"))
        });
        assert_eq!(result.unwrap_err().to_string(), "serializer failed");
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(entries(dir.path()), ["a.json"]);
    }

    #[test]
    fn test_create_new_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".gitignore");
        create_new(&path, b"runs/\n").unwrap();
        create_new(&path, b"other\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"runs/\n");
    }

    #[test]
    fn test_create_new_removes_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".gitignore");
        let result = create_new_with(&path, |file| {
            file.write_all(b"ru")?;
            Err(io::Error::other("disk full"))
        });
        assert_eq!(result.unwrap_err().to_string(), "disk full");
        assert!(!path.exists());
        create_new(&path, b"runs/\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"runs/\n");
    }

    #[test]
    fn test_a_directory_sync_is_best_effort() {
        // Unsupported (Windows, network mounts) or failing: only logged, the file is durable.
        sync_dir(&tempfile::tempdir().unwrap().path().join("gone"));
    }

    #[test]
    fn test_io_errors_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, b"").unwrap();
        // A parent that is a file cannot be created.
        assert!(write_atomically(&blocker.join("x.png"), b"x", Durability::Atomic).is_err());
        assert!(create_new(&blocker.join("x.png"), b"x").is_err());
    }
}
