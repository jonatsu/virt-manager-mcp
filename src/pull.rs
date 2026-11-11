//! Writing what a guest sends into a pull directory on the host.
//!
//! The guest controls the bytes, so every write follows the same rules. The destination is reached from a handle on
//! the pull directory one component at a time, never through a symlink. The copy is built in the pull directory's
//! staging directory under a random name, with a fixed mode, and only a complete copy moves into place, by an atomic
//! rename that never replaces something unexpected. A failed or abandoned pull leaves nothing behind.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use crate::pulldir::{LOCK_SUFFIX, STAGING};

/// Mode of every file a pull writes.
pub const FILE_MODE: u32 = 0o644;
/// Mode of every directory a pull creates.
pub const DIR_MODE: u32 = 0o755;

/// The host destinations of the pulls this process runs, so two of them never write the same place.
#[derive(Clone, Default)]
pub struct Destinations(Arc<Mutex<Vec<PathBuf>>>);

/// A claim on a destination, released when dropped.
pub struct Claim {
    set: Destinations,
    path: PathBuf,
}

impl Destinations {
    /// Claims `path`, refusing it while another running pull writes it, a directory above it, or a path inside it.
    pub fn claim(&self, path: &Path) -> Result<Claim> {
        let mut held = self
            .0
            .lock()
            .expect("no thread panics while holding this lock");
        if let Some(other) = held
            .iter()
            .find(|p| p.starts_with(path) || path.starts_with(p))
        {
            bail!(
                "another running pull writes {}, which overlaps {}; wait for it to end",
                other.display(),
                path.display()
            );
        }
        held.push(path.to_path_buf());
        Ok(Claim {
            set: self.clone(),
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut held = self
            .set
            .0
            .lock()
            .expect("no thread panics while holding this lock");
        held.retain(|p| p != &self.path);
    }
}

/// A destination inside a pull directory: the pull directory's handle, its parent directory's handle and its name.
pub struct Destination {
    pull_dir: OwnedFd,
    staging_path: PathBuf,
    parent: OwnedFd,
    name: OsString,
    path: PathBuf,
}

/// What exists at a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    Nothing,
    File,
    Directory,
    Other,
}

/// What a pull writes: a file or a directory tree. A pull replaces only an entry of the same kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

impl Kind {
    fn existing(self) -> Existing {
        match self {
            Kind::File => Existing::File,
            Kind::Directory => Existing::Directory,
        }
    }
}

impl Destination {
    /// Resolves `host_path` inside one of `pull_dirs`, creating missing parent directories with mode 755. Refuses a
    /// relative path, a `..` component, a path outside every pull directory, a path in or at the staging directory,
    /// a pull directory itself or a path holding one, and any symlink on the way. A path inside nested pull
    /// directories belongs to the innermost.
    pub fn open(pull_dirs: &[PathBuf], host_path: &str) -> Result<Self> {
        let path = Path::new(host_path);
        if !path.is_absolute() {
            bail!("{host_path}: host_path must be absolute");
        }
        if path.components().any(|c| c == Component::ParentDir) {
            bail!("{host_path}: host_path may not contain '..'");
        }
        // Pull directories may nest. A pull never replaces one of them, and a path inside a nested one stages in the
        // innermost, so an overwrite cannot swap out a pull directory with its staging directory and other pulls.
        if pull_dirs.iter().any(|d| d.starts_with(path)) {
            bail!("{host_path} is or holds a pull directory; name a path inside it");
        }
        let (dir, rest) = pull_dirs
            .iter()
            .filter_map(|d| path.strip_prefix(d).ok().map(|r| (d, r)))
            .max_by_key(|(d, _)| d.components().count())
            .ok_or_else(|| anyhow!("{host_path} is not inside host_pull_dirs"))?;
        let parts: Vec<&OsStr> = rest
            .components()
            .filter_map(|c| match c {
                Component::Normal(p) => Some(p),
                _ => None,
            })
            .collect();
        let Some((name, parents)) = parts.split_last() else {
            bail!("{host_path} is a pull directory itself; name a path inside it");
        };
        // Any component, not only the first: with one pull directory nested in another, the inner one's staging
        // directory lies deeper in the outer one.
        if parts.contains(&OsStr::new(STAGING)) {
            bail!("{host_path} is inside a staging directory {STAGING}");
        }
        let pull_dir = rustix::fs::open(
            dir,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("opening pull directory {}", dir.display()))?;
        let mut parent = rustix::io::dup(&pull_dir)?;
        let mut walked = dir.clone();
        for part in parents {
            walked.push(part);
            parent = open_or_create_dir(&parent, part)
                .with_context(|| format!("{}", walked.display()))?;
        }
        Ok(Self {
            pull_dir,
            staging_path: dir.join(STAGING),
            parent,
            name: name.to_os_string(),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What is at the destination now, without following a symlink.
    pub fn existing(&self) -> Result<Existing> {
        existing_at(&self.parent, &self.name)
    }

    /// Checks that a pull of a file may write here: refuses anything but a file, and a file without `overwrite`.
    pub fn check_file(&self, overwrite: bool) -> Result<()> {
        self.check(Kind::File, overwrite)
    }

    /// Checks that a pull of `kind` may write here: refuses an entry of another kind, and one of the same kind
    /// without `overwrite`.
    pub fn check(&self, kind: Kind, overwrite: bool) -> Result<()> {
        let path = self.path.display();
        let found = self.existing()?;
        if found == Existing::Nothing || (found == kind.existing() && overwrite) {
            return Ok(());
        }
        match (found, kind) {
            (Existing::File, Kind::File) | (Existing::Directory, Kind::Directory) => {
                bail!("{path} exists; pass overwrite: true to replace it")
            }
            (Existing::Directory, Kind::File) => {
                bail!("{path} is a directory; a file pull replaces only a file")
            }
            (Existing::File, Kind::Directory) => {
                bail!("{path} is a file; a directory pull replaces only a directory")
            }
            _ => bail!("{path} is a symlink or special file; a pull never replaces one"),
        }
    }

    /// Starts a staged file in this destination's pull directory.
    pub fn stage_file(&self) -> Result<Staged> {
        let mut staged = self.stage(Kind::File)?;
        staged.file = Some(File::from(create_excl(&staged.staging, &staged.id)?));
        Ok(staged)
    }

    /// Starts a staged directory, private until its commit, in this destination's pull directory.
    pub fn stage_dir(&self) -> Result<Staged> {
        let staged = self.stage(Kind::Directory)?;
        rustix::fs::mkdirat(&staged.staging, &staged.id, Mode::from(0o700))
            .with_context(|| format!("creating {STAGING}/{}", staged.id))?;
        Ok(staged)
    }

    /// A staged entry of `kind` whose lock is held, before its file or directory exists.
    fn stage(&self, kind: Kind) -> Result<Staged> {
        let staging = rustix::fs::openat(
            &self.pull_dir,
            STAGING,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("opening the staging directory")?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let lock_name = format!("{id}{LOCK_SUFFIX}");
        let lock = File::from(create_excl(&staging, &lock_name)?);
        lock.lock().context("locking the staged pull")?;
        // Returned before the entry exists, so a failed create still removes the lock file when it drops.
        Ok(Staged {
            staging,
            staging_path: self.staging_path.clone(),
            id,
            lock_name,
            _lock: lock,
            kind,
            file: None,
        })
    }
}

/// A file or directory being written in the staging directory. Dropped without `commit`, it removes itself and its
/// lock file.
pub struct Staged {
    staging: OwnedFd,
    staging_path: PathBuf,
    id: String,
    lock_name: String,
    _lock: File,
    kind: Kind,
    file: Option<File>,
}

impl Staged {
    pub fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("a staged file is open until commit")
    }

    /// A handle on a staged directory, to write the tree into.
    pub fn dir(&self) -> Result<OwnedFd> {
        rustix::fs::openat(
            &self.staging,
            &self.id,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("opening the staged directory")
    }

    /// Gives the entry its fixed mode and moves it to `dest`: a new destination never replaces one that appeared
    /// meanwhile, and with `overwrite` an existing entry of the same kind is swapped out in one step and then deleted.
    pub fn commit(self, dest: &Destination, overwrite: bool) -> Result<()> {
        self.commit_with(dest, overwrite, &|| {})
    }

    /// `commit`, with `before_swap` run between the type check and the swap, so tests can change the destination
    /// there.
    fn commit_with(
        mut self,
        dest: &Destination,
        overwrite: bool,
        before_swap: &dyn Fn(),
    ) -> Result<()> {
        match self.kind {
            Kind::File => {
                let file = self
                    .file
                    .take()
                    .expect("a staged file is open until commit");
                rustix::fs::fchmod(&file, Mode::from(FILE_MODE))
                    .context("setting the pulled file's mode")?;
                // On disk before the rename, so a host crash cannot leave a short file under the destination's name.
                file.sync_all().context("flushing the pulled file")?;
            }
            Kind::Directory => {
                let dir = self.dir()?;
                rustix::fs::fchmod(&dir, Mode::from(DIR_MODE))
                    .context("setting the pulled directory's mode")?;
                // Every file of the tree on disk before the rename, in one call for the whole filesystem.
                rustix::fs::syncfs(&dir).context("flushing the pulled tree")?;
            }
        }
        let expected = self.kind.existing();
        let existing = dest.existing()?;
        if existing == Existing::Nothing || !overwrite {
            return self.rename_new(dest, overwrite);
        }
        if existing != expected {
            dest.check(self.kind, overwrite)?;
        }
        before_swap();
        match rustix::fs::renameat_with(
            &self.staging,
            &self.id,
            &dest.parent,
            &dest.name,
            RenameFlags::EXCHANGE,
        ) {
            Ok(()) => {}
            // The old file went away meanwhile: place the new one as if it never existed.
            Err(Errno::NOENT) => return self.rename_new(dest, overwrite),
            Err(e) => return Err(e).context(format!("replacing {}", dest.path.display())),
        }
        // What came back must be of the kind that was checked; anything else moved in meanwhile, so put it back.
        if existing_at(&self.staging, OsStr::new(&self.id))? != expected {
            rustix::fs::renameat_with(
                &self.staging,
                &self.id,
                &dest.parent,
                &dest.name,
                RenameFlags::EXCHANGE,
            )
            .context("putting back what appeared at the destination")?;
            bail!(
                "{} changed while the pull ran; nothing was replaced",
                dest.path.display()
            );
        }
        Ok(())
    }

    fn rename_new(&self, dest: &Destination, overwrite: bool) -> Result<()> {
        match rustix::fs::renameat_with(
            &self.staging,
            &self.id,
            &dest.parent,
            &dest.name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => {
                // Name what appeared, as the check before the copy would have.
                dest.check(self.kind, overwrite)
                    .context("it appeared while the pull ran")?;
                bail!(
                    "{} appeared while the pull ran; nothing was replaced, so pull again",
                    dest.path.display()
                )
            }
            Err(e) => Err(e).context(format!("placing {}", dest.path.display())),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // After a commit, `<id>` is gone or holds the old entry that was swapped out; either way it goes. A staged
        // file is removed as a single file only, never recursively. A staged directory is removed recursively;
        // `remove_dir_all` never follows a symlink, and the staging directory is private to this user.
        match self.kind {
            Kind::File => {
                let _ = rustix::fs::unlinkat(&self.staging, &self.id, AtFlags::empty());
            }
            Kind::Directory => {
                let _ = std::fs::remove_dir_all(self.staging_path.join(&self.id));
            }
        }
        let _ = rustix::fs::unlinkat(&self.staging, &self.lock_name, AtFlags::empty());
    }
}

fn existing_at(dir: impl AsFd, name: &OsStr) -> Result<Existing> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile => Existing::File,
            FileType::Directory => Existing::Directory,
            _ => Existing::Other,
        }),
        Err(Errno::NOENT) => Ok(Existing::Nothing),
        Err(e) => Err(e).with_context(|| format!("checking {}", Path::new(name).display())),
    }
}

/// Opens the directory `name` below `parent` without following a symlink, creating it with mode 755 if missing.
pub fn open_or_create_dir(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd> {
    let created = match rustix::fs::mkdirat(parent, name, Mode::from(DIR_MODE)) {
        Ok(()) => true,
        Err(Errno::EXIST) => false,
        Err(e) => return Err(e).context("creating a directory"),
    };
    let fd = rustix::fs::openat(
        parent,
        name,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::LOOP | Errno::NOTDIR => {
            anyhow!("is a symlink or not a directory; a pull never follows one")
        }
        e => anyhow!(e),
    })?;
    if created {
        rustix::fs::fchmod(&fd, Mode::from(DIR_MODE))?;
    }
    Ok(fd)
}

pub fn create_excl(dir: &OwnedFd, name: &str) -> Result<OwnedFd> {
    rustix::fs::openat(
        dir,
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from(0o600),
    )
    .with_context(|| format!("creating {STAGING}/{name}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, symlink};

    use super::*;

    /// A pull directory with a staging directory, as start-up prepares it, removed when dropped.
    struct PullDir(PathBuf);

    impl PullDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "virtmanager-mcp-pull-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join(STAGING)).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn dirs(&self) -> Vec<PathBuf> {
            vec![self.0.clone()]
        }

        fn path(&self, rel: &str) -> String {
            self.0.join(rel).to_str().unwrap().to_string()
        }

        fn staging_is_empty(&self) -> bool {
            fs::read_dir(self.0.join(STAGING)).unwrap().count() == 0
        }
    }

    impl Drop for PullDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pull(dir: &PullDir, rel: &str, data: &[u8], overwrite: bool) -> Result<()> {
        let dest = Destination::open(&dir.dirs(), &dir.path(rel))?;
        dest.check_file(overwrite)?;
        let mut staged = dest.stage_file()?;
        staged.file().write_all(data)?;
        staged.commit(&dest, overwrite)
    }

    #[test]
    fn a_pulled_file_arrives_whole_with_mode_644_in_new_directories_with_mode_755() {
        let dir = PullDir::new("arrive");
        pull(&dir, "a/b/out.log", b"hello", false).unwrap();
        let file = dir.0.join("a/b/out.log");
        assert_eq!(fs::read(&file).unwrap(), b"hello");
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, FILE_MODE);
        for d in ["a", "a/b"] {
            assert_eq!(
                fs::metadata(dir.0.join(d)).unwrap().mode() & 0o7777,
                DIR_MODE,
                "{d}"
            );
        }
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn an_existing_file_is_replaced_only_with_overwrite_and_a_directory_never() {
        let dir = PullDir::new("overwrite");
        pull(&dir, "f", b"old", false).unwrap();
        let err = pull(&dir, "f", b"new", false).unwrap_err().to_string();
        assert!(err.contains("overwrite: true"), "{err}");
        assert_eq!(fs::read(dir.0.join("f")).unwrap(), b"old");
        pull(&dir, "f", b"new", true).unwrap();
        assert_eq!(fs::read(dir.0.join("f")).unwrap(), b"new");

        fs::create_dir(dir.0.join("d")).unwrap();
        fs::write(dir.0.join("d/keep"), "x").unwrap();
        let err = pull(&dir, "d", b"new", true).unwrap_err().to_string();
        assert!(err.contains("is a directory"), "{err}");
        assert!(dir.0.join("d/keep").exists());
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn symlinks_on_the_way_or_at_the_destination_are_refused() {
        let dir = PullDir::new("symlinks");
        let outside = PullDir::new("symlinks-outside");
        fs::write(outside.0.join("target"), "secret").unwrap();
        symlink(outside.0.join("target"), dir.0.join("link")).unwrap();
        symlink(&outside.0, dir.0.join("linkdir")).unwrap();

        let err = pull(&dir, "link", b"x", true).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
        let err = format!("{:#}", pull(&dir, "linkdir/new", b"x", true).unwrap_err());
        assert!(err.contains("symlink"), "{err}");
        assert_eq!(fs::read(outside.0.join("target")).unwrap(), b"secret");
        assert!(!outside.0.join("new").exists());
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn paths_outside_or_inside_the_staging_directory_are_refused() {
        let dir = PullDir::new("paths");
        let dirs = dir.dirs();
        for (path, why) in [
            ("relative/f".to_string(), "absolute"),
            (dir.path("a/../f"), ".."),
            ("/etc/passwd".to_string(), "not inside"),
            (dir.path(STAGING), "staging"),
            (dir.path(&format!("{STAGING}/x")), "staging"),
            (
                dir.0.to_str().unwrap().to_string(),
                "is or holds a pull directory",
            ),
        ] {
            let err = match Destination::open(&dirs, &path) {
                Ok(_) => panic!("{path} accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(why), "{path}: {err}");
        }
    }

    #[test]
    fn an_abandoned_pull_leaves_nothing_behind() {
        let dir = PullDir::new("abandon");
        let dest = Destination::open(&dir.dirs(), &dir.path("f")).unwrap();
        let mut staged = dest.stage_file().unwrap();
        staged.file().write_all(b"partial").unwrap();
        assert!(!dir.staging_is_empty());
        drop(staged);
        assert!(dir.staging_is_empty());
        assert!(!dir.0.join("f").exists());
    }

    #[test]
    fn a_directory_that_appears_before_the_swap_is_put_back() {
        let dir = PullDir::new("race");
        pull(&dir, "f", b"old", false).unwrap();
        let dest = Destination::open(&dir.dirs(), &dir.path("f")).unwrap();
        let mut staged = dest.stage_file().unwrap();
        staged.file().write_all(b"new").unwrap();
        let swap_in_a_directory = || {
            fs::remove_file(dir.0.join("f")).unwrap();
            fs::create_dir(dir.0.join("f")).unwrap();
            fs::write(dir.0.join("f/keep"), "x").unwrap();
        };
        let err = staged
            .commit_with(&dest, true, &swap_in_a_directory)
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed while the pull ran"), "{err}");
        assert_eq!(fs::read(dir.0.join("f/keep")).unwrap(), b"x");
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn a_file_that_appears_before_a_directory_swap_is_put_back() {
        let dir = PullDir::new("race-dir");
        fs::create_dir(dir.0.join("d")).unwrap();
        fs::write(dir.0.join("d/old"), "old").unwrap();
        let dest = Destination::open(&dir.dirs(), &dir.path("d")).unwrap();
        dest.check(Kind::Directory, true).unwrap();
        let staged = dest.stage_dir().unwrap();
        let tree = staged.dir().unwrap();
        rustix::fs::mkdirat(&tree, "new", Mode::from(0o755)).unwrap();
        let swap_in_a_file = || {
            fs::remove_dir_all(dir.0.join("d")).unwrap();
            fs::write(dir.0.join("d"), "theirs").unwrap();
        };
        let err = staged
            .commit_with(&dest, true, &swap_in_a_file)
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed while the pull ran"), "{err}");
        assert_eq!(fs::read(dir.0.join("d")).unwrap(), b"theirs");
        assert!(dir.staging_is_empty());

        // Without a race, the whole old tree goes.
        let dest = Destination::open(&dir.dirs(), &dir.path("e")).unwrap();
        fs::create_dir(dir.0.join("e")).unwrap();
        fs::write(dir.0.join("e/old"), "old").unwrap();
        let staged = dest.stage_dir().unwrap();
        rustix::fs::mkdirat(staged.dir().unwrap(), "new", Mode::from(0o755)).unwrap();
        staged.commit(&dest, true).unwrap();
        assert!(dir.0.join("e/new").is_dir() && !dir.0.join("e/old").exists());
        assert_eq!(
            fs::metadata(dir.0.join("e")).unwrap().mode() & 0o7777,
            DIR_MODE
        );
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn a_file_that_appears_meanwhile_is_not_replaced_without_overwrite() {
        let dir = PullDir::new("appear");
        let dest = Destination::open(&dir.dirs(), &dir.path("f")).unwrap();
        dest.check_file(false).unwrap();
        let mut staged = dest.stage_file().unwrap();
        staged.file().write_all(b"new").unwrap();
        fs::write(dir.0.join("f"), "theirs").unwrap();
        let err = format!("{:#}", staged.commit(&dest, false).unwrap_err());
        assert!(err.contains("appeared"), "{err}");
        assert!(err.contains("overwrite: true"), "{err}");
        assert_eq!(fs::read(dir.0.join("f")).unwrap(), b"theirs");
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn a_directory_that_appears_meanwhile_is_named_as_one() {
        let dir = PullDir::new("appear-dir");
        let dest = Destination::open(&dir.dirs(), &dir.path("f")).unwrap();
        let mut staged = dest.stage_file().unwrap();
        staged.file().write_all(b"new").unwrap();
        fs::create_dir(dir.0.join("f")).unwrap();
        let err = format!("{:#}", staged.commit(&dest, false).unwrap_err());
        assert!(err.contains("is a directory"), "{err}");
        assert!(!err.contains("overwrite: true"), "{err}");
        assert!(dir.0.join("f").is_dir());
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn a_file_that_vanishes_before_the_swap_is_placed_anew() {
        let dir = PullDir::new("vanish");
        pull(&dir, "f", b"old", false).unwrap();
        let dest = Destination::open(&dir.dirs(), &dir.path("f")).unwrap();
        let mut staged = dest.stage_file().unwrap();
        staged.file().write_all(b"new").unwrap();
        let remove = || fs::remove_file(dir.0.join("f")).unwrap();
        staged.commit_with(&dest, true, &remove).unwrap();
        assert_eq!(fs::read(dir.0.join("f")).unwrap(), b"new");
        assert!(dir.staging_is_empty());
    }

    #[test]
    fn a_nested_pull_directorys_staging_is_refused_too() {
        let outer = PullDir::new("nested");
        let inner = outer.0.join("q");
        fs::create_dir(&inner).unwrap();
        let dirs = vec![outer.0.clone(), inner];
        for rel in [format!("q/{STAGING}/x"), format!("a/{STAGING}/x")] {
            let err = match Destination::open(&dirs, &outer.path(&rel)) {
                Ok(_) => panic!("{rel} accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains("staging"), "{rel}: {err}");
        }
        assert!(!outer.0.join("a").exists());
    }

    #[test]
    fn a_nested_pull_directory_or_its_ancestor_is_never_a_destination() {
        let outer = PullDir::new("nested-dest");
        let inner = outer.0.join("a/q");
        fs::create_dir_all(&inner).unwrap();
        for dirs in [
            vec![outer.0.clone(), inner.clone()],
            vec![inner.clone(), outer.0.clone()],
        ] {
            for rel in ["a/q", "a"] {
                let err = match Destination::open(&dirs, &outer.path(rel)) {
                    Ok(_) => panic!("{rel} accepted with {dirs:?}"),
                    Err(e) => e.to_string(),
                };
                assert!(err.contains("pull directory"), "{rel}: {err}");
            }
        }
    }

    #[test]
    fn a_path_in_a_nested_pull_directory_stages_in_the_innermost_one() {
        let outer = PullDir::new("nested-inner");
        let inner = outer.0.join("q");
        fs::create_dir(&inner).unwrap();
        for dirs in [
            vec![outer.0.clone(), inner.clone()],
            vec![inner.clone(), outer.0.clone()],
        ] {
            let dest = Destination::open(&dirs, &outer.path("q/out")).unwrap();
            assert_eq!(dest.staging_path, inner.join(STAGING));
        }
    }

    #[test]
    fn running_pulls_may_not_overlap() {
        let set = Destinations::default();
        let a = set.claim(Path::new("/p/out")).unwrap();
        assert!(set.claim(Path::new("/p/out")).is_err());
        assert!(set.claim(Path::new("/p/out/sub")).is_err());
        assert!(set.claim(Path::new("/p")).is_err());
        let b = set.claim(Path::new("/p/other")).unwrap();
        drop(a);
        set.claim(Path::new("/p/out/sub")).unwrap();
        drop(b);
    }
}
