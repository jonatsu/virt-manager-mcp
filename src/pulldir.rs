//! Pull directories on the host: the host directories that pulls from a guest write into.
//!
//! Each pull directory holds a staging directory, `.vmm-staging/`, where a pull builds its copy before it moves into
//! place. A pull owns three entries there, all named after one random ID: `<id>.lock`, which it holds an `flock` on
//! while it runs; `<id>`, its copy; and `<id>.old`, an old destination it swapped out. Start-up removes the entries of
//! pulls that no process runs any more.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rustix::fs::{RenameFlags, renameat_with};

/// Name of the staging directory inside each pull directory.
pub const STAGING: &str = ".vmm-staging";

/// Suffix of a pull's lock file in the staging directory.
const LOCK_SUFFIX: &str = ".lock";

/// How old a staged entry must be before start-up may remove it. A pull creates its lock file and locks it in two
/// steps; the age keeps start-up from taking the lock in between.
const STALE_AFTER: Duration = Duration::from_secs(60);

/// Prepares every pull directory: creates its staging directory, checks that its filesystem supports the atomic
/// renames pulls rely on, and removes what dead pulls left behind.
pub fn prepare(pull_dirs: &[PathBuf]) -> Result<()> {
    for dir in pull_dirs {
        let context = || format!("host_pull_dirs: {}", dir.display());
        let staging = staging_dir(dir).with_context(context)?;
        check_renames(&staging, &|from, to, flags| {
            renameat_with(rustix::fs::CWD, from, rustix::fs::CWD, to, flags)
                .map_err(io::Error::from)
        })
        .with_context(context)?;
        let removed = remove_stale(&staging, STALE_AFTER).with_context(context)?;
        if removed > 0 {
            tracing::info!(
                "removed {removed} entries of ended pulls from {}",
                staging.display()
            );
        }
    }
    Ok(())
}

/// Creates `dir/.vmm-staging` with mode 700, or checks that the existing one is a real directory owned by this
/// account and makes it private again, and returns its path.
fn staging_dir(dir: &Path) -> Result<PathBuf> {
    let staging = dir.join(STAGING);
    match DirBuilder::new().mode(0o700).create(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", staging.display())),
    }
    let meta =
        fs::symlink_metadata(&staging).with_context(|| format!("reading {}", staging.display()))?;
    if !meta.is_dir() {
        bail!(
            "{} is not a directory; remove it so the server can create its staging directory there",
            staging.display()
        );
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        bail!(
            "{} belongs to another user; remove it, or choose a pull directory of this account",
            staging.display()
        );
    }
    if meta.mode() & 0o077 != 0 {
        // The path is a real directory of this account, checked above, so this changes nothing else.
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making {} private", staging.display()))?;
    }
    Ok(staging)
}

/// The rename call, as a parameter so tests can stand in for a filesystem without `renameat2` support.
type Rename<'a> = &'a dyn Fn(&Path, &Path, RenameFlags) -> io::Result<()>;

/// Checks that `rename` honours `RENAME_NOREPLACE` and `RENAME_EXCHANGE` in `staging`, using two scratch files there.
fn check_renames(staging: &Path, rename: Rename<'_>) -> Result<()> {
    let id = uuid::Uuid::new_v4();
    let a = staging.join(format!("probe-{id}-a"));
    let b = staging.join(format!("probe-{id}-b"));
    let result = (|| {
        create_new(&a)?;
        create_new(&b)?;
        match rename(&a, &b, RenameFlags::NOREPLACE) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(unsupported("RENAME_NOREPLACE", &e)),
            Ok(()) => bail!("RENAME_NOREPLACE replaced an existing file"),
        }
        rename(&a, &b, RenameFlags::EXCHANGE).map_err(|e| unsupported("RENAME_EXCHANGE", &e))
    })();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    result
}

fn unsupported(flag: &str, e: &io::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "the filesystem does not support renameat2 with {flag} ({e}), which pulls need to replace a destination \
         safely; choose a pull directory on a local filesystem such as ext4, xfs, btrfs or tmpfs"
    )
}

fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// Removes the entries of pulls that no process runs any more, and returns how many it removed. A pull is over when
/// nobody holds the lock on its lock file, or when it has no lock file. Entries younger than `min_age` stay, because
/// a pull may be between creating its lock file and locking it. Other server processes may finish pulls meanwhile,
/// and a leftover entry harms nothing, so an entry that vanishes is skipped and any other problem with one entry is
/// logged rather than stopping the server.
fn remove_stale(staging: &Path, min_age: Duration) -> Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;
    let entries =
        fs::read_dir(staging).with_context(|| format!("reading {}", staging.display()))?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        match remove_if_stale(staging, &entry, now, min_age) {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("leaving {} in place: {e}", path.display()),
        }
    }
    Ok(removed)
}

/// Removes one staging entry if it is old enough and its pull is over; returns whether it did.
fn remove_if_stale(
    staging: &Path,
    entry: &fs::DirEntry,
    now: SystemTime,
    min_age: Duration,
) -> io::Result<bool> {
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(false);
    };
    let meta = entry.metadata()?;
    let young = meta
        .modified()
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .is_none_or(|age| age < min_age);
    if young || is_running(staging, pull_id(name))? {
        return Ok(false);
    }
    if meta.is_dir() {
        fs::remove_dir_all(entry.path())?;
    } else {
        fs::remove_file(entry.path())?;
    }
    Ok(true)
}

/// The pull ID an entry in the staging directory belongs to: its name up to the first dot.
fn pull_id(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// Whether some process holds the lock of pull `id` in `staging`. The lock file is opened without following a
/// symlink and without blocking, so a FIFO planted in its place cannot hang start-up.
fn is_running(staging: &Path, id: &str) -> io::Result<bool> {
    let path = staging.join(format!("{id}{LOCK_SUFFIX}"));
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "virtmanager-mcp-pulldir-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prepare_makes_a_private_staging_directory_on_this_filesystem() {
        let dir = test_dir("prepare");
        prepare(std::slice::from_ref(&dir)).unwrap();
        let meta = fs::symlink_metadata(dir.join(STAGING)).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o777, 0o700);
        assert_eq!(fs::read_dir(dir.join(STAGING)).unwrap().count(), 0);
        prepare(std::slice::from_ref(&dir)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlinked_staging_directory_is_refused() {
        let dir = test_dir("symlink");
        let elsewhere = dir.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let pull = dir.join("pull");
        fs::create_dir(&pull).unwrap();
        std::os::unix::fs::symlink(&elsewhere, pull.join(STAGING)).unwrap();
        let err = format!("{:#}", prepare(&[pull]).unwrap_err());
        assert!(err.contains("not a directory"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_filesystem_without_renameat2_flags_is_refused() {
        let dir = test_dir("renames");
        let einval = |_: &Path, _: &Path, _: RenameFlags| -> io::Result<()> {
            Err(io::Error::from_raw_os_error(
                rustix::io::Errno::INVAL.raw_os_error(),
            ))
        };
        let err = check_renames(&dir, &einval).unwrap_err().to_string();
        assert!(err.contains("RENAME_NOREPLACE"), "{err}");

        let no_exchange = |a: &Path, b: &Path, flags: RenameFlags| -> io::Result<()> {
            if flags.contains(RenameFlags::EXCHANGE) {
                Err(io::Error::from_raw_os_error(
                    rustix::io::Errno::INVAL.raw_os_error(),
                ))
            } else {
                renameat_with(rustix::fs::CWD, a, rustix::fs::CWD, b, flags)
                    .map_err(io::Error::from)
            }
        };
        let err = check_renames(&dir, &no_exchange).unwrap_err().to_string();
        assert!(err.contains("RENAME_EXCHANGE"), "{err}");

        let replaces = |a: &Path, b: &Path, _: RenameFlags| fs::rename(a, b);
        let err = check_renames(&dir, &replaces).unwrap_err().to_string();
        assert!(err.contains("replaced"), "{err}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "probe files remain");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cleanup_removes_only_entries_of_pulls_nobody_holds() {
        let dir = test_dir("cleanup");
        let held = "1111";
        let free = "2222";
        let lock = File::create(dir.join(format!("{held}{LOCK_SUFFIX}"))).unwrap();
        lock.lock().unwrap();
        fs::create_dir(dir.join(held)).unwrap();
        fs::write(dir.join(held).join("f"), "x").unwrap();
        File::create(dir.join(format!("{free}{LOCK_SUFFIX}"))).unwrap();
        fs::create_dir(dir.join(free)).unwrap();
        fs::write(dir.join(free).join("f"), "x").unwrap();
        fs::write(dir.join(format!("{free}.old")), "x").unwrap();
        fs::write(dir.join("3333"), "an entry without a lock file").unwrap();

        // Everything is younger than a minute, so nothing goes yet.
        assert_eq!(remove_stale(&dir, STALE_AFTER).unwrap(), 0);

        assert_eq!(remove_stale(&dir, Duration::ZERO).unwrap(), 4);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [held.to_string(), format!("{held}{LOCK_SUFFIX}")]);
        drop(lock);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A lock file that is a FIFO or a symlink is neither followed nor waited on; its entries stay, and start-up
    /// goes on.
    #[test]
    fn odd_lock_files_neither_hang_nor_stop_the_cleanup() {
        let dir = test_dir("odd");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            dir.join("4444.lock"),
            rustix::fs::Mode::from(0o600),
        )
        .unwrap();
        fs::write(dir.join("4444"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join("5555.lock")).unwrap();
        fs::write(dir.join("5555"), "x").unwrap();
        let removed = remove_stale(&dir, Duration::ZERO).unwrap();
        // The FIFO can be opened without blocking and locked, so its pull counts as over; the symlink cannot be
        // opened, so its pull's entries stay.
        assert!(!dir.join("4444").exists());
        assert!(dir.join("5555").exists());
        assert!(removed >= 2, "removed {removed}");
        assert!(fs::symlink_metadata("/etc/hostname").is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_staging_directory_others_can_read_is_made_private() {
        let dir = test_dir("private");
        let staging = dir.join(STAGING);
        fs::create_dir(&staging).unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o777)).unwrap();
        prepare(std::slice::from_ref(&dir)).unwrap();
        assert_eq!(
            fs::symlink_metadata(&staging).unwrap().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
