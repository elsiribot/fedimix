//! The recovery phrase and the data folder lock.

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use fedimint_bip39::{Bip39RootSecretStrategy, Language, Mnemonic};
use fedimint_client::RootSecret;
use fedimint_client::secret::RootSecretStrategy;

use crate::error::{FedimixError, Result};

const SEED_FILE: &str = "seed";
const SEED_TMP_FILE: &str = "seed.tmp";
const LOCK_FILE: &str = "lock";
const WORD_COUNT: usize = 12;

fn io_err(what: &str, path: &Path, e: std::io::Error) -> FedimixError {
    FedimixError::Internal(format!("Fedimix could not {what} {}: {e}.", path.display()))
}

/// Creates the data folder with mode 0700, or checks an existing one: a folder
/// open to group or others is refused, not repaired, because the database
/// holds balances and history.
pub(crate) fn open_data_dir(dir: &Path) -> Result<()> {
    match fs::metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(FedimixError::Internal(format!(
                    "The data folder {} is not a folder.",
                    dir.display()
                )));
            }
            if meta.permissions().mode() & 0o077 != 0 {
                return Err(FedimixError::Internal(format!(
                    "The data folder {0} can be opened by other users. Run `chmod 700 {0}` and start Fedimix again.",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| io_err("create the data folder", dir, e)),
        Err(e) => Err(io_err("read the data folder", dir, e)),
    }
}

/// The wallet's recovery phrase, kept in `<data_dir>/seed` with mode 0600.
pub struct Seed {
    mnemonic: Mnemonic,
}

// No derived Debug: the phrase must never reach a log.
impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seed(..)")
    }
}

impl Seed {
    /// Loads the phrase, or creates it on first run. A file readable by
    /// group or others is refused, not repaired.
    /// Needs the lock, so two processes cannot create a seed at once.
    pub fn load_or_create(dir: &Path, _lock: &DataDirLock) -> Result<Seed> {
        open_data_dir(dir)?;
        let path = dir.join(SEED_FILE);
        match File::open(&path) {
            Ok(file) => Self::load(file, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::create(dir, &path),
            Err(e) => Err(io_err("open the seed file", &path, e)),
        }
    }

    /// The recovery phrase, space separated.
    pub(crate) fn words(&self) -> String {
        self.mnemonic.to_string()
    }

    /// What every federation's client is built from. Each join mixes in its
    /// federation id, so this must not be derived any further.
    pub(crate) fn root_secret(&self) -> RootSecret {
        RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<WORD_COUNT>::to_root_secret(
            &self.mnemonic,
        ))
    }

    fn load(mut file: File, path: &Path) -> Result<Seed> {
        use std::io::Read;
        // Check the opened file, not the path, so it cannot change between.
        let meta = file
            .metadata()
            .map_err(|e| io_err("read the seed file", path, e))?;
        if !meta.is_file() {
            return Err(FedimixError::Internal(format!(
                "The seed at {} is not a regular file.",
                path.display()
            )));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(FedimixError::Internal(format!(
                "The seed file {0} can be read by other users. Run `chmod 600 {0}` and start Fedimix again.",
                path.display()
            )));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|e| io_err("read the seed file", path, e))?;
        let mnemonic = Mnemonic::parse_in_normalized(Language::English, text.trim())
            .map_err(|_| {
                FedimixError::Internal(format!(
                    "The seed file {} does not hold a valid recovery phrase, so Fedimix will not start. Restore the file from your backup.",
                    path.display()
                ))
            })?;
        Ok(Seed { mnemonic })
    }

    /// Writes through a temp file and a hard link, so a crash leaves either
    /// no seed or a complete one.
    fn create(dir: &Path, path: &Path) -> Result<Seed> {
        let mnemonic = Bip39RootSecretStrategy::<WORD_COUNT>::random(&mut rand::thread_rng());
        let tmp = dir.join(SEED_TMP_FILE);
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("remove the stale file", &tmp, e)),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io_err("create the seed file", &tmp, e))?;
        file.write_all(mnemonic.to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| io_err("write the seed file", &tmp, e))?;
        drop(file);
        // A hard link fails if the seed exists, so one is never replaced.
        let linked = fs::hard_link(&tmp, path);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing =
                    File::open(path).map_err(|e| io_err("open the seed file", path, e))?;
                return Self::load(existing, path);
            }
            Err(e) => return Err(io_err("save the seed file", path, e)),
        }
        // Persist the new directory entry.
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_err("sync the data folder", dir, e))?;
        Ok(Seed { mnemonic })
    }
}

/// An exclusive `flock` on `<data_dir>/lock`, released on drop.
#[derive(Debug)]
pub struct DataDirLock {
    _file: File,
}

impl DataDirLock {
    pub fn acquire(dir: &Path) -> Result<DataDirLock> {
        open_data_dir(dir)?;
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| io_err("open the lock file", &path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(DataDirLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(FedimixError::Internal(
                "Fedimix is already running with this data folder.".into(),
            )),
            Err(TryLockError::Error(e)) => Err(io_err("lock", &path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Temp folders follow the umask; the data folder check wants 0700.
    fn private_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn seed_file_is_0600_and_stable() {
        let dir = private_tempdir();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        let first = Seed::load_or_create(dir.path(), &lock).unwrap();
        let mode = std::fs::metadata(dir.path().join("seed"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(first.words().split(' ').count(), 12);
        let second = Seed::load_or_create(dir.path(), &lock).unwrap();
        assert_eq!(first.words(), second.words());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 2, "only seed and lock: {leftovers:?}");
    }

    #[test]
    fn seed_refuses_group_readable_file() {
        let dir = private_tempdir();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        let words = Seed::load_or_create(dir.path(), &lock).unwrap().words();
        let path = dir.path().join("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = Seed::load_or_create(dir.path(), &lock).err().unwrap();
        match err {
            crate::FedimixError::Internal(msg) => {
                assert!(msg.contains("chmod 600"), "{msg}");
                assert!(!msg.contains(&words), "the error must not leak the seed");
            }
            other => panic!("unexpected {other:?}"),
        }
        // Not silently fixed.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn seed_refuses_invalid_words() {
        let dir = private_tempdir();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        let path = dir.path().join("seed");
        std::fs::write(&path, "not a real recovery phrase").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = Seed::load_or_create(dir.path(), &lock).err().unwrap();
        match err {
            crate::FedimixError::Internal(msg) => {
                assert!(
                    msg.contains("does not hold a valid recovery phrase"),
                    "{msg}"
                )
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stale_temp_file_does_not_load() {
        let dir = private_tempdir();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        std::fs::write(dir.path().join("seed.tmp"), "partial").unwrap();
        let seed = Seed::load_or_create(dir.path(), &lock).unwrap();
        assert_eq!(seed.words().split(' ').count(), 12);
    }

    #[test]
    fn second_lock_is_refused() {
        let dir = private_tempdir();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        assert_eq!(
            DataDirLock::acquire(dir.path()).err(),
            Some(crate::FedimixError::Internal(
                "Fedimix is already running with this data folder.".into()
            ))
        );
        drop(first);
        DataDirLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn existing_seed_is_never_replaced() {
        let dir = private_tempdir();
        let lock = DataDirLock::acquire(dir.path()).unwrap();
        let winner = Seed::load_or_create(dir.path(), &lock).unwrap();
        let path = dir.path().join("seed");
        // A second creator that lost the race must adopt the existing seed.
        let adopted = Seed::create(dir.path(), &path).unwrap();
        assert_eq!(adopted.words(), winner.words());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), winner.words());
        assert!(!dir.path().join("seed.tmp").exists());
    }

    #[test]
    fn data_dir_is_created_private() {
        let base = private_tempdir();
        let dir = base.path().join("a").join("data");
        open_data_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn open_data_dir_refuses_group_access() {
        let dir = private_tempdir();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        match open_data_dir(dir.path()).err().unwrap() {
            crate::FedimixError::Internal(msg) => {
                assert!(msg.contains("chmod 700"), "{msg}")
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(DataDirLock::acquire(dir.path()).is_err());
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750, "not silently fixed");
    }
}
