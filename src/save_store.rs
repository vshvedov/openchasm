//! Atomic same-directory storage for already encoded, bounded save bytes.
//!
//! The supplied directory must be trusted: name validation is not a defense
//! against concurrent hostile mutation of that directory or its ancestors.
use crate::save_format::MAX_SAVE_PAYLOAD;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Largest possible v1 outer envelope (48-byte header plus 32-byte checksum).
pub const MAX_SAVE_FILE_BYTES: usize = MAX_SAVE_PAYLOAD + 80;
const MAX_SLOT_LENGTH: usize = 64;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum StoreError {
    InvalidSlot,
    TooLarge,
    Io(io::Error),
}
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for StoreError {}
impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Replacement succeeded. Directory synchronization may be unavailable or
/// fail after replacement, so that state is not reported as an ordinary error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    DirectorySynced,
    DirectorySyncUnavailableOrFailed,
}

fn slot_path(directory: &Path, slot: &str) -> Result<PathBuf, StoreError> {
    if slot.is_empty()
        || slot.len() > MAX_SLOT_LENGTH
        || !slot
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(StoreError::InvalidSlot);
    }
    let upper = slot.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(StoreError::InvalidSlot);
    }
    Ok(directory.join(format!("{slot}.ocs")))
}

struct TempFile {
    path: PathBuf,
    armed: bool,
}
impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn create_temp(directory: &Path) -> Result<(File, TempFile), StoreError> {
    for _ in 0..128 {
        let index = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".openchasm-{}-{index}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, TempFile { path, armed: true })),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate save temporary file",
    )
    .into())
}

/// Write an already encoded blob to a named slot. Errors occur before a
/// successful rename and leave the prior slot intact under normal filesystem
/// rename semantics. Concurrent writers use distinct temporary files; the last
/// successful rename wins. No directory is created implicitly.
pub fn write(directory: &Path, slot: &str, bytes: &[u8]) -> Result<WriteOutcome, StoreError> {
    write_inner(directory, slot, bytes, || Ok(()))
}
fn write_inner(
    directory: &Path,
    slot: &str,
    bytes: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
) -> Result<WriteOutcome, StoreError> {
    let destination = slot_path(directory, slot)?;
    if bytes.len() > MAX_SAVE_FILE_BYTES {
        return Err(StoreError::TooLarge);
    }
    #[cfg(unix)]
    let directory_handle = File::open(directory)?;
    let (mut file, mut temporary) = create_temp(directory)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    before_rename()?;
    fs::rename(&temporary.path, destination)?;
    temporary.armed = false;
    #[cfg(unix)]
    {
        if directory_handle.sync_all().is_ok() {
            return Ok(WriteOutcome::DirectorySynced);
        }
    }
    Ok(WriteOutcome::DirectorySyncUnavailableOrFailed)
}

/// Read at most one complete envelope, with a streaming cap even if its file
/// grows during the read. The caller must decode and validate the bytes.
pub fn read(directory: &Path, slot: &str) -> Result<Vec<u8>, StoreError> {
    let path = slot_path(directory, slot)?;
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "save is not a regular file").into(),
        );
    }
    let mut result = Vec::new();
    file.take((MAX_SAVE_FILE_BYTES as u64) + 1)
        .read_to_end(&mut result)?;
    if result.len() > MAX_SAVE_FILE_BYTES {
        return Err(StoreError::TooLarge);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::save_format::{self, ContentDigest};
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let root = std::env::temp_dir();
            for _ in 0..128 {
                let index = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
                let path = root.join(format!(
                    "openchasm-save-test-{}-{index}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
            panic!("no test directory available")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn replaces_complete_encoded_file_and_detects_corruption_on_decode() {
        let dir = Directory::new();
        let digest = ContentDigest([4; 32]);
        let first = save_format::encode(digest, b"first").unwrap();
        let next = save_format::encode(digest, b"second").unwrap();
        write(&dir.0, "slot-1", &first).unwrap();
        assert_eq!(read(&dir.0, "slot-1").unwrap(), first);
        write(&dir.0, "slot-1", &next).unwrap();
        assert_eq!(read(&dir.0, "slot-1").unwrap(), next);
        fs::write(dir.0.join("slot-1.ocs"), b"broken").unwrap();
        assert!(save_format::decode(&read(&dir.0, "slot-1").unwrap()).is_err());
    }
    #[test]
    fn failed_replacement_preserves_old_file_and_cleans_temporary() {
        let dir = Directory::new();
        write(&dir.0, "slot", b"old").unwrap();
        assert!(write_inner(&dir.0, "slot", b"new", || Err(io::Error::other("injected"))).is_err());
        assert_eq!(read(&dir.0, "slot").unwrap(), b"old");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        assert!(matches!(
            write(&dir.0, "slot", &vec![0; MAX_SAVE_FILE_BYTES + 1]),
            Err(StoreError::TooLarge)
        ));
        assert_eq!(read(&dir.0, "slot").unwrap(), b"old");
    }
    #[test]
    fn invalid_names_and_oversized_reads_fail() {
        let dir = Directory::new();
        for name in [
            "", "../x", "a/b", "a\\b", ".", "CON", "lpt9", "x.ocs", "foo ",
        ] {
            assert!(
                matches!(write(&dir.0, name, b"x"), Err(StoreError::InvalidSlot)),
                "{name}"
            );
        }
        let file = File::create(dir.0.join("large.ocs")).unwrap();
        file.set_len((MAX_SAVE_FILE_BYTES + 1) as u64).unwrap();
        assert!(matches!(read(&dir.0, "large"), Err(StoreError::TooLarge)));
    }
}
