//! Pushing a host directory into a guest.
//!
//! Selection and reading are separate steps. The `ignore` crate selects paths by name, with `.gitignore` rules and
//! `exclude` patterns. Every selected path is then opened from a handle on the pushed tree one component at a time,
//! never through a symlink, so a directory swapped for a symlink after selection cannot lead outside the share. The
//! archive is a tar stream, built while it is sent. The guest unpacks it beside the target and swaps the new tree in,
//! so a failed push leaves the old tree in place.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{Match, WalkBuilder};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::Serialize;

use crate::agent::{Agent, ExecSpec};
use crate::transfer::CountingReader;

/// The setuid and setgid bits, which a pushed entry never carries.
const SETID_BITS: u32 = 0o6000;
/// How long one guest step other than unpacking may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long unpacking may take in the guest.
const UNPACK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How much of a guest step's stderr an error quotes.
const STDERR_MAX: usize = 4096;

/// What a push selects.
#[derive(Debug, Clone)]
pub struct Options {
    pub respect_gitignore: bool,
    pub include_git: bool,
    pub exclude: Vec<String>,
}

/// A host directory to push: the share directory that holds it and its own path, both canonical.
pub struct Tree {
    share: PathBuf,
    path: PathBuf,
}

impl Tree {
    /// Resolves `host_path`, which must be a directory inside one of `share_dirs`.
    pub fn resolve(share_dirs: &[PathBuf], host_path: &str) -> Result<Self> {
        let p = Path::new(host_path);
        if !p.is_absolute() {
            bail!("{host_path}: host_path must be absolute");
        }
        let real = p
            .canonicalize()
            .with_context(|| format!("cannot access {host_path}"))?;
        // The outermost share that holds it, so a repository's rules above a nested share still apply.
        let share = share_dirs
            .iter()
            .filter(|d| real.starts_with(d))
            .min_by_key(|d| d.as_os_str().len())
            .ok_or_else(|| anyhow!("{host_path} is not inside host_share_dirs"))?;
        if !real.is_dir() {
            bail!("{host_path} is not a directory; copy_to_vm copies a single file");
        }
        Ok(Self {
            share: share.clone(),
            path: real,
        })
    }

    /// Checks the `exclude` patterns, so a bad one is refused before anything happens in the guest.
    pub fn check(&self, options: &Options) -> Result<()> {
        overrides(&self.path, &options.exclude).map(drop)
    }

    /// A handle on the tree, reached from the share directory without following a symlink.
    fn open(&self) -> Result<OwnedFd> {
        let mut dir = open_dir_path(&self.share)?;
        for c in self.path.strip_prefix(&self.share)?.components() {
            let Component::Normal(name) = c else {
                bail!("{} is not a plain path", self.path.display());
            };
            dir = open_dir(&dir, name).with_context(|| {
                format!(
                    "{} changed after it was resolved; a push never follows a symlink",
                    self.path.display()
                )
            })?;
        }
        Ok(dir)
    }
}

/// The paths a push sends, relative to the tree, each directory before what it holds.
pub struct Selection {
    entries: Vec<PathBuf>,
    ignored: u64,
}

/// Selects what a push of `tree` sends.
pub fn select(tree: &Tree, options: &Options) -> Result<Selection> {
    let parents = if options.respect_gitignore {
        parent_rules(tree)?
    } else {
        Vec::new()
    };
    let selected = walk(tree, options, options.respect_gitignore, parents)?;
    let ignored = if options.respect_gitignore {
        let everything = walk(tree, options, false, Vec::new())?;
        non_dirs(&everything).saturating_sub(non_dirs(&selected))
    } else {
        0
    };
    Ok(Selection {
        entries: selected.into_iter().map(|(path, _)| path).collect(),
        ignored,
    })
}

fn non_dirs(entries: &[(PathBuf, bool)]) -> u64 {
    entries.iter().filter(|(_, is_dir)| !is_dir).count() as u64
}

/// The tree's paths with whether each is a directory. `gitignore` applies `.gitignore` and `.git/info/exclude` files
/// in the tree and the `parents` rules of directories above it inside the share.
fn walk(
    tree: &Tree,
    options: &Options,
    gitignore: bool,
    parents: Vec<Gitignore>,
) -> Result<Vec<(PathBuf, bool)>> {
    let mut builder = WalkBuilder::new(&tree.path);
    // The standard filters also read the host user's global excludes and hidden-file rules, which differ between
    // machines, and ripgrep's `.ignore` files.
    builder
        .standard_filters(false)
        .git_ignore(gitignore)
        .git_exclude(gitignore)
        .require_git(false)
        .follow_links(false)
        .overrides(overrides(&tree.path, &options.exclude)?)
        .sort_by_file_name(|a, b| a.cmp(b));
    let include_git = options.include_git;
    builder.filter_entry(move |entry| {
        if !include_git && entry.file_name() == ".git" {
            return false;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        !ignored_by(&parents, entry.path(), is_dir)
    });
    let mut out = Vec::new();
    for item in builder.build() {
        let entry = item.context("selecting the files to push")?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(&tree.path)?.to_path_buf();
        out.push((rel, entry.file_type().is_some_and(|t| t.is_dir())));
    }
    Ok(out)
}

/// `exclude` as override globs, which the walk treats as ignore rules because each starts with `!`.
fn overrides(root: &Path, exclude: &[String]) -> Result<Override> {
    let mut builder = OverrideBuilder::new(root);
    for pattern in exclude {
        builder
            .add(&format!("!{pattern}"))
            .with_context(|| format!("exclude pattern {pattern:?}"))?;
    }
    Ok(builder.build()?)
}

/// The ignore rules of the share directory and each directory below it above the tree, outermost first.
fn parent_rules(tree: &Tree) -> Result<Vec<Gitignore>> {
    let mut rules = Vec::new();
    let mut dir = tree.share.clone();
    let below: Vec<_> = tree.path.strip_prefix(&tree.share)?.components().collect();
    for i in 0..below.len() {
        if i > 0 {
            dir.push(below[i - 1]);
        }
        let mut builder = GitignoreBuilder::new(&dir);
        for file in [dir.join(".gitignore"), dir.join(".git/info/exclude")] {
            if file.is_file()
                && let Some(e) = builder.add(&file)
            {
                return Err(e).with_context(|| format!("reading {}", file.display()));
            }
        }
        rules.push(builder.build()?);
    }
    Ok(rules)
}

/// Whether the parent rules ignore `path`: the deepest rule file that matches decides, as in git. A tree's own
/// `!pattern` cannot bring back what a parent's rule leaves out, because the walk never reaches it.
fn ignored_by(rules: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for rule in rules.iter().rev() {
        match rule.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// What a push sent, and what it left out.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    /// File bytes sent.
    pub bytes: u64,
    /// Files that `.gitignore` rules kept out.
    pub ignored: u64,
    pub skipped: Vec<Skipped>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// An entry the push left out, and why.
#[derive(Debug, Serialize, PartialEq)]
pub struct Skipped {
    pub path: String,
    pub reason: &'static str,
}

/// Writes the selected entries of `tree` to `out` as a tar archive. Every entry is owned by uid and gid 0 and has
/// its host mode without the setuid and setgid bits.
pub fn write_archive(tree: &Tree, selection: &Selection, out: &mut dyn Write) -> Result<Summary> {
    let root = tree.open()?;
    let mut builder = tar::Builder::new(out);
    let mut summary = Summary {
        ignored: selection.ignored,
        ..Default::default()
    };
    for rel in &selection.entries {
        add_entry(&mut builder, &root, rel, &mut summary)
            .with_context(|| format!("{}", rel.display()))?;
    }
    builder.into_inner().context("finishing the archive")?;
    Ok(summary)
}

fn add_entry(
    builder: &mut tar::Builder<&mut dyn Write>,
    root: &OwnedFd,
    rel: &Path,
    summary: &mut Summary,
) -> Result<()> {
    let mut skip = |reason| {
        summary.skipped.push(Skipped {
            path: rel.display().to_string(),
            reason,
        });
    };
    let Some(name) = rel.file_name() else {
        bail!("not a plain path");
    };
    let Some(parent) = open_parent(root, rel)? else {
        skip("vanished during the push");
        return Ok(());
    };
    let st = match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(Errno::NOENT) => {
            skip("vanished during the push");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    match FileType::from_raw_mode(st.st_mode) {
        FileType::RegularFile => {
            let fd = match rustix::fs::openat(
                &parent,
                name,
                OFlags::RDONLY
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::NOCTTY
                    | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => {
                    skip("vanished during the push");
                    return Ok(());
                }
                Err(Errno::LOOP) => bail!("became a symlink during the push"),
                Err(e) => return Err(e.into()),
            };
            let st = rustix::fs::fstat(&fd)?;
            if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
                bail!("stopped being a regular file during the push");
            }
            let size = u64::try_from(st.st_size)?;
            let mut header = header(tar::EntryType::Regular, &st);
            header.set_size(size);
            let data = Exact {
                inner: File::from(fd),
                left: size,
            };
            builder.append_data(&mut header, rel, data)?;
            summary.files += 1;
            summary.bytes += size;
        }
        FileType::Directory => {
            let mut header = header(tar::EntryType::Directory, &st);
            builder.append_data(&mut header, rel, io::empty())?;
            summary.directories += 1;
        }
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(&parent, name, Vec::new())?;
            let target = Path::new(OsStr::from_bytes(target.as_bytes()));
            if link_stays_inside(root, rel, target) {
                let mut header = header(tar::EntryType::Symlink, &st);
                builder.append_link(&mut header, rel, target)?;
                summary.symlinks += 1;
            } else {
                skip(
                    "symlink whose target is outside the pushed tree, missing, or reached through another symlink",
                );
            }
        }
        FileType::Fifo => skip("FIFO"),
        FileType::Socket => skip("socket"),
        FileType::CharacterDevice | FileType::BlockDevice => skip("device file"),
        FileType::Unknown => skip("unknown file type"),
    }
    Ok(())
}

fn header(kind: tar::EntryType, st: &rustix::fs::Stat) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(st.st_mode & 0o7777 & !SETID_BITS);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(u64::try_from(st.st_mtime).unwrap_or(0));
    header.set_size(0);
    header
}

/// Reads exactly `left` bytes, failing if the file ends sooner, so an archive entry always matches its header.
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let max = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the file shrank during the push",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// The directory holding `rel`, reached from `root` without following a symlink, or `None` when part of the way
/// vanished. A directory on the way that became a symlink or a file fails the push.
fn open_parent(root: &OwnedFd, rel: &Path) -> Result<Option<OwnedFd>> {
    let mut dir = rustix::io::dup(root)?;
    let Some(parent) = rel.parent() else {
        return Ok(Some(dir));
    };
    for c in parent.components() {
        let Component::Normal(name) = c else {
            bail!("not a plain path");
        };
        dir = match open_dir(&dir, name) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => bail!(
                "a directory on its way became a symlink or a file during the push; a push never follows one"
            ),
            Err(e) => return Err(e.into()),
        };
    }
    Ok(Some(dir))
}

/// Whether the relative symlink `target`, stored at `rel`, resolves inside the tree through real directories only.
/// A component that is a symlink itself, or missing, counts as outside, so `b -> .` with `a -> b/../x` cannot escape.
fn link_stays_inside(root: &OwnedFd, rel: &Path, target: &Path) -> bool {
    if target.as_os_str().is_empty() || target.is_absolute() {
        return false;
    }
    let Ok(start) = rustix::io::dup(root) else {
        return false;
    };
    // The tree's root stays at the bottom; popping it means leaving the tree.
    let mut stack = vec![start];
    for c in rel.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(name) = c else {
            return false;
        };
        match open_dir(stack.last().expect("the root stays"), name) {
            Ok(fd) => stack.push(fd),
            Err(_) => return false,
        }
    }
    let parts: Vec<Component> = target.components().collect();
    for (i, c) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.len() == 1 {
                    return false;
                }
                stack.pop();
            }
            Component::Normal(name) => {
                let dir = stack.last().expect("the root stays");
                let Ok(st) = rustix::fs::statat(dir, *name, AtFlags::SYMLINK_NOFOLLOW) else {
                    return false;
                };
                match FileType::from_raw_mode(st.st_mode) {
                    FileType::Symlink => return false,
                    FileType::Directory if !last => match open_dir(dir, name) {
                        Ok(fd) => stack.push(fd),
                        Err(_) => return false,
                    },
                    _ if !last => return false,
                    _ => {}
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn open_dir(parent: &OwnedFd, name: &OsStr) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

fn open_dir_path(path: &Path) -> Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening {}", path.display()))
}

// ---- the guest side ----------------------------------------------------------------------------------------------

/// A push target in the guest: its path, its parent, and the random part of the temporary names beside it.
pub struct Target {
    pub path: String,
    parent: String,
    id: String,
}

impl Target {
    /// Parses `guest_path`, refusing a relative path, `..`, and with `replace`, `/` or a directory directly below it.
    pub fn parse(guest_path: &str, replace: bool) -> Result<Self> {
        if !guest_path.starts_with('/') {
            bail!("{guest_path}: guest_path must be absolute");
        }
        let parts: Vec<&str> = guest_path
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.contains(&"..") {
            bail!("{guest_path}: guest_path may not contain '..'");
        }
        let Some((name, parents)) = parts.split_last() else {
            bail!("{guest_path}: a push never replaces /");
        };
        if replace && parents.is_empty() {
            bail!(
                "{guest_path}: replace refuses / and the directories directly below it, as a guard against mistakes"
            );
        }
        let parent = format!("/{}", parents.join("/"));
        let path = if parents.is_empty() {
            format!("/{name}")
        } else {
            format!("{parent}/{name}")
        };
        Ok(Self {
            path,
            parent,
            id: uuid::Uuid::new_v4().simple().to_string(),
        })
    }

    /// A temporary name beside the target, starting with `.vmm-`.
    fn temp(&self, suffix: &str) -> String {
        let base = self.parent.trim_end_matches('/');
        format!("{base}/.vmm-{}.{suffix}", self.id)
    }
}

/// One step a push runs in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Probe,
    /// Sending the archive, which `Guest::write` does; only the tests' guest names it, to fail it.
    #[cfg(test)]
    Write,
    Unpack,
    Swap,
    Cleanup,
}

/// How a guest step ended.
pub struct Ran {
    pub code: Option<i64>,
    pub stderr: String,
}

impl Ran {
    fn describe(&self) -> String {
        match (self.stderr.is_empty(), self.code) {
            (false, _) => self.stderr.clone(),
            (true, Some(code)) => format!("exit status {code}"),
            (true, None) => "no exit status".into(),
        }
    }
}

/// What a push needs from a guest: running a fixed `sh` script with arguments, and writing a file.
pub trait Guest {
    fn run(&self, step: Step, script: &str, args: &[&str]) -> Result<Ran>;
    fn write(&self, path: &str, data: &mut dyn Read) -> Result<u64>;
}

impl Guest for Agent<'_> {
    fn run(&self, step: Step, script: &str, args: &[&str]) -> Result<Ran> {
        // The script is a constant and every path is a separate argument, so no path is ever parsed as shell code.
        let mut argv = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
        argv.extend(args.iter().map(|a| (*a).to_string()));
        let timeout = if step == Step::Unpack {
            UNPACK_TIMEOUT
        } else {
            STEP_TIMEOUT
        };
        let out = self.exec(&ExecSpec::new("/bin/sh", argv), timeout, STDERR_MAX)?;
        if out.timed_out {
            bail!(
                "the guest's {step:?} step did not finish within {} s",
                timeout.as_secs()
            );
        }
        Ok(Ran {
            code: out.exit_code,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    fn write(&self, path: &str, data: &mut dyn Read) -> Result<u64> {
        self.write_from(path, data, false, Some(0o600))
    }
}

/// Exits 10 without `tar`, 11 when the target exists and `$3` is not 1, and 12 when it exists as anything but a
/// directory. Otherwise it creates the target's missing parents.
const PROBE: &str = r#"command -v tar >/dev/null 2>&1 || exit 10
if [ -L "$1" ]; then exit 12; fi
if [ -e "$1" ]; then
  [ -d "$1" ] || exit 12
  [ "$3" = 1 ] || exit 11
else
  mkdir -p -- "$2" || exit 13
fi"#;

/// Unpacks archive `$1` into the new directory `$2`, without restoring the archive's owners.
const UNPACK: &str = r#"mkdir -m 755 -- "$2" && tar -x -o -f "$1" -C "$2""#;

/// Moves the unpacked tree `$2` to the target `$1`. An existing target, allowed only when `$4` is 1, moves aside to
/// `$3` first and is deleted once the new tree is in; when the new tree cannot move in, the old one moves back.
const SWAP: &str = r#"t=$1 d=$2 o=$3
if [ -e "$t" ] || [ -L "$t" ]; then
  [ "$4" = 1 ] || exit 11
  if [ -L "$t" ] || [ ! -d "$t" ]; then exit 12; fi
  mv -- "$t" "$o" || exit 14
  if ! mv -- "$d" "$t"; then
    mv -- "$o" "$t" || exit 18
    exit 15
  fi
  rm -rf -- "$o"
else
  mv -- "$d" "$t" || exit 16
fi"#;

/// Deletes the push's temporary archive `$1` and unpacked tree `$2`; both are the push's own `.vmm-` names.
const CLEANUP: &str = r#"rm -rf -- "$1" "$2""#;

/// Pushes the archive that `archive` writes to `target`. Nothing is sent when the target exists without `replace`,
/// or exists as anything but a directory. A failure after that deletes the temporary entries and leaves the old tree.
pub fn push(
    guest: &dyn Guest,
    target: &Target,
    replace: bool,
    bytes: &AtomicU64,
    archive: impl FnOnce(&mut dyn Write) -> Result<Summary> + Send,
) -> Result<Summary> {
    let flag = if replace { "1" } else { "0" };
    let probe = guest.run(Step::Probe, PROBE, &[&target.path, &target.parent, flag])?;
    match probe.code {
        Some(0) => {}
        Some(10) => bail!("the guest has no tar, which copy_dir_to_vm needs"),
        Some(11) => bail!(
            "{} exists in the guest; pass replace: true to replace it",
            target.path
        ),
        Some(12) => bail!(
            "{} exists in the guest and is not a directory; replace replaces only a directory",
            target.path
        ),
        _ => bail!(
            "cannot create the parent directories of {} in the guest: {}",
            target.path,
            probe.describe()
        ),
    }
    let (packed, unpacked, old) = (target.temp("tar"), target.temp("d"), target.temp("old"));
    let result = send_and_swap(
        guest,
        target,
        flag,
        bytes,
        archive,
        [&packed, &unpacked, &old],
    );
    let cleanup = guest.run(Step::Cleanup, CLEANUP, &[&packed, &unpacked]);
    let mut summary = result?;
    match cleanup {
        Ok(ran) if ran.code == Some(0) => {}
        Ok(ran) => {
            summary.note = Some(format!(
                "could not delete {packed} and {unpacked} in the guest: {}",
                ran.describe()
            ))
        }
        Err(e) => {
            summary.note = Some(format!(
                "could not delete {packed} and {unpacked} in the guest: {e:#}"
            ))
        }
    }
    Ok(summary)
}

fn send_and_swap(
    guest: &dyn Guest,
    target: &Target,
    flag: &str,
    bytes: &AtomicU64,
    archive: impl FnOnce(&mut dyn Write) -> Result<Summary> + Send,
    [packed, unpacked, old]: [&str; 3],
) -> Result<Summary> {
    let (reader, writer) = io::pipe()?;
    let summary = std::thread::scope(|s| -> Result<Summary> {
        let builder = s.spawn(move || {
            let mut writer = writer;
            archive(&mut writer)
        });
        let mut counted = CountingReader {
            inner: reader,
            count: bytes,
        };
        let sent = guest.write(packed, &mut counted);
        // Without a reader, a builder still writing fails at once instead of blocking.
        drop(counted);
        let built = builder
            .join()
            .map_err(|_| anyhow!("building the archive panicked"))?;
        // A failed send makes the builder fail too, so its own error explains less.
        sent.context("sending the archive to the guest")?;
        built.context("building the archive")
    })?;
    let ran = guest.run(Step::Unpack, UNPACK, &[packed, unpacked])?;
    if ran.code != Some(0) {
        bail!(
            "unpacking the archive in the guest failed: {}",
            ran.describe()
        );
    }
    let ran = guest.run(Step::Swap, SWAP, &[&target.path, unpacked, old, flag])?;
    match ran.code {
        Some(0) => Ok(summary),
        Some(11) => bail!(
            "{} appeared in the guest during the push; nothing was replaced",
            target.path
        ),
        Some(12) => bail!(
            "{} became something other than a directory during the push; nothing was replaced",
            target.path
        ),
        Some(18) => bail!(
            "could not move the new tree into {}, nor the old tree back; the old tree is at {old}: {}",
            target.path,
            ran.describe()
        ),
        _ => bail!(
            "could not move the new tree into {}; the old tree is unchanged: {}",
            target.path,
            ran.describe()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    use super::*;

    /// A temporary directory, removed when dropped.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "virtmanager-mcp-push-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> Options {
        Options {
            respect_gitignore: true,
            include_git: false,
            exclude: Vec::new(),
        }
    }

    fn tree(share: &Path, rel: &str) -> Tree {
        Tree::resolve(&[share.to_path_buf()], share.join(rel).to_str().unwrap()).unwrap()
    }

    fn selected(tree: &Tree, options: &Options) -> (Vec<String>, u64) {
        let s = select(tree, options).unwrap();
        let names = s.entries.iter().map(|p| p.display().to_string()).collect();
        (names, s.ignored)
    }

    /// The archive of `tree`, as (path, link target, mode) per entry, with the summary.
    fn archived(tree: &Tree) -> (Vec<(String, Option<String>, u32)>, Summary) {
        let mut out = Vec::new();
        let summary = write_archive(tree, &select(tree, &options()).unwrap(), &mut out).unwrap();
        let mut archive = tar::Archive::new(&out[..]);
        let entries = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let link = e.link_name().unwrap().map(|l| l.display().to_string());
                assert_eq!(
                    (e.header().uid().unwrap(), e.header().gid().unwrap()),
                    (0, 0)
                );
                (
                    e.path().unwrap().display().to_string(),
                    link,
                    e.header().mode().unwrap(),
                )
            })
            .collect();
        (entries, summary)
    }

    #[test]
    fn gitignore_rules_apply_in_the_tree_and_above_it_inside_the_share_only() {
        let base = Temp::new("ignore");
        // Above the share: must not apply.
        base.write(".gitignore", "a.rs\n");
        base.write("share/.gitignore", "*.log\n");
        base.write("share/proj/.gitignore", "target/\n");
        for f in ["a.rs", "x.log", "target/out", "sub/b.rs", ".git/config"] {
            base.write(&format!("share/proj/{f}"), "x");
        }
        let share = base.0.join("share");
        let tree = tree(&share, "proj");

        let (names, ignored) = selected(&tree, &options());
        assert_eq!(names, [".gitignore", "a.rs", "sub", "sub/b.rs"]);
        assert_eq!(ignored, 2, "x.log and target/out");

        let (names, ignored) = selected(
            &tree,
            &Options {
                respect_gitignore: false,
                include_git: true,
                exclude: vec!["*.rs".into(), "target/".into()],
            },
        );
        assert_eq!(names, [".git", ".git/config", ".gitignore", "sub", "x.log"]);
        assert_eq!(ignored, 0);
    }

    #[test]
    fn a_bad_exclude_pattern_is_refused() {
        let share = Temp::new("bad-exclude");
        let tree = tree(&share.0, "");
        let bad = Options {
            exclude: vec!["a[".into()],
            ..options()
        };
        let err = tree.check(&bad).unwrap_err().to_string();
        assert!(err.contains("exclude pattern"), "{err}");
    }

    #[test]
    fn a_tree_outside_the_shares_or_not_a_directory_is_refused() {
        let share = Temp::new("resolve");
        share.write("f", "x");
        let outside = Temp::new("resolve-outside");
        symlink(&outside.0, share.0.join("out")).unwrap();
        let dirs = [share.0.clone()];
        for (path, why) in [
            ("relative".to_string(), "absolute"),
            (outside.0.display().to_string(), "not inside"),
            (share.0.join("out").display().to_string(), "not inside"),
            (share.0.join("f").display().to_string(), "not a directory"),
        ] {
            let err = match Tree::resolve(&dirs, &path) {
                Ok(_) => panic!("{path} accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(why), "{path}: {err}");
        }
    }

    #[test]
    fn only_symlinks_that_stay_inside_through_real_directories_are_sent() {
        let share = Temp::new("links");
        share.write("t/f", "x");
        share.write("t/sub/g", "y");
        share.write("outside", "secret");
        let t = share.0.join("t");
        symlink("f", t.join("in")).unwrap();
        symlink("../f", t.join("sub/up")).unwrap();
        symlink("sub/g", t.join("down")).unwrap();
        symlink("../outside", t.join("out")).unwrap();
        symlink("/etc/hostname", t.join("abs")).unwrap();
        symlink(".", t.join("b")).unwrap();
        symlink("b/../x", t.join("a")).unwrap();
        symlink("missing", t.join("dangling")).unwrap();
        let tree = tree(&share.0, "t");

        let (entries, summary) = archived(&tree);
        let links: Vec<_> = entries
            .iter()
            .filter_map(|(p, l, _)| l.as_ref().map(|l| format!("{p} -> {l}")))
            .collect();
        // `b -> .` names the tree's own root, so it stays inside; `a` passes through it and does not.
        assert_eq!(
            links,
            ["b -> .", "down -> sub/g", "in -> f", "sub/up -> ../f"]
        );
        let skipped: Vec<_> = summary.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(skipped, ["a", "abs", "dangling", "out"]);
        assert_eq!(summary.symlinks, 4);
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_after_selection_is_not_followed() {
        let share = Temp::new("swap-dir");
        share.write("t/d/f", "mine");
        let outside = Temp::new("swap-dir-outside");
        outside.write("f", "secret");
        let tree = tree(&share.0, "t");
        let selection = select(&tree, &options()).unwrap();
        fs::rename(share.0.join("t/d"), share.0.join("t/d.real")).unwrap();
        symlink(&outside.0, share.0.join("t/d")).unwrap();

        let mut out = Vec::new();
        let err = format!(
            "{:#}",
            write_archive(&tree, &selection, &mut out).unwrap_err()
        );
        assert!(err.contains("symlink"), "{err}");
        assert!(!String::from_utf8_lossy(&out).contains("secret"));
    }

    #[test]
    fn a_fifo_is_skipped_and_setuid_and_setgid_bits_are_dropped() {
        let share = Temp::new("special");
        share.write("t/run", "#!/bin/sh\n");
        let t = share.0.join("t");
        fs::set_permissions(t.join("run"), fs::Permissions::from_mode(0o6755)).unwrap();
        let dir = open_dir_path(&t).unwrap();
        rustix::fs::mkfifoat(&dir, "pipe", Mode::from(0o644)).unwrap();
        let tree = tree(&share.0, "t");

        let (entries, summary) = archived(&tree);
        assert_eq!(entries, [("run".to_string(), None, 0o755)]);
        assert_eq!(
            summary.skipped,
            [Skipped {
                path: "pipe".into(),
                reason: "FIFO"
            }]
        );
        assert_eq!((summary.files, summary.bytes), (1, 10));
    }

    #[test]
    fn guest_paths_are_checked_before_anything_runs() {
        for (path, replace, why) in [
            ("relative", false, "absolute"),
            ("/a/../b", false, ".."),
            ("/", false, "never replaces /"),
            ("//", true, "never replaces /"),
            ("/usr", true, "directly below it"),
            ("/usr/", true, "directly below it"),
        ] {
            let err = match Target::parse(path, replace) {
                Ok(_) => panic!("{path} accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(why), "{path}: {err}");
        }
        let t = Target::parse("/srv//app/./src/", true).unwrap();
        assert_eq!(
            (t.path.as_str(), t.parent.as_str()),
            ("/srv/app/src", "/srv/app")
        );
        assert!(t.temp("tar").starts_with("/srv/app/.vmm-"));
        let t = Target::parse("/new", false).unwrap();
        assert!(t.temp("d").starts_with("/.vmm-"), "{}", t.temp("d"));
    }

    /// A guest that runs each step's script with the host's `sh` and `tar`, failing the step named in `fail`.
    struct LocalGuest {
        fail: Option<Step>,
        path_env: Option<String>,
    }

    impl LocalGuest {
        fn new() -> Self {
            Self {
                fail: None,
                path_env: None,
            }
        }
    }

    impl Guest for LocalGuest {
        fn run(&self, step: Step, script: &str, args: &[&str]) -> Result<Ran> {
            if self.fail == Some(step) {
                bail!("scripted failure at {step:?}");
            }
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(script).arg("sh").args(args);
            if let Some(path) = &self.path_env {
                cmd.env("PATH", path);
            }
            let out = cmd.output()?;
            Ok(Ran {
                code: out.status.code().map(i64::from),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }

        fn write(&self, path: &str, data: &mut dyn Read) -> Result<u64> {
            let mut file = File::create(path)?;
            if self.fail == Some(Step::Write) {
                // Part of the archive arrives before the failure.
                let mut byte = [0u8; 1];
                data.read_exact(&mut byte)?;
                file.write_all(&byte)?;
                bail!("scripted failure while writing");
            }
            Ok(io::copy(data, &mut file)?)
        }
    }

    /// A share with a tree `src` to push, and a guest directory `guest` holding an old `app` tree.
    struct Bed {
        share: Temp,
        guest: Temp,
    }

    impl Bed {
        fn new(name: &str) -> Self {
            let share = Temp::new(&format!("{name}-share"));
            share.write("src/main.rs", "fn main() {}\n");
            share.write("src/lib/util.rs", "pub fn f() {}\n");
            let guest = Temp::new(&format!("{name}-guest"));
            guest.write("app/old.txt", "old");
            Self { share, guest }
        }

        fn push(&self, to: &str, replace: bool, guest: &LocalGuest) -> Result<Summary> {
            let tree = tree(&self.share.0, "src");
            let target = Target::parse(self.guest.0.join(to).to_str().unwrap(), replace)?;
            let bytes = AtomicU64::new(0);
            push(guest, &target, replace, &bytes, |out| {
                write_archive(&tree, &select(&tree, &options())?, out)
            })
        }

        fn guest_entries(&self) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(&self.guest.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn old_tree_is_intact(&self) -> bool {
            fs::read_to_string(self.guest.0.join("app/old.txt")).is_ok_and(|t| t == "old")
                && !self.guest.0.join("app/main.rs").exists()
        }
    }

    #[test]
    fn a_push_creates_a_new_tree_and_replace_swaps_a_whole_one_in() {
        let bed = Bed::new("swap");
        let summary = bed.push("deep/new", false, &LocalGuest::new()).unwrap();
        assert_eq!((summary.files, summary.directories), (2, 1));
        let new = bed.guest.0.join("deep/new");
        assert_eq!(
            fs::read_to_string(new.join("lib/util.rs")).unwrap(),
            "pub fn f() {}\n"
        );

        bed.push("app", true, &LocalGuest::new()).unwrap();
        assert!(bed.guest.0.join("app/main.rs").exists());
        assert!(!bed.guest.0.join("app/old.txt").exists());
        assert_eq!(bed.guest_entries(), ["app", "deep"]);
    }

    #[test]
    fn an_existing_target_without_replace_or_a_file_target_is_refused_before_sending() {
        let bed = Bed::new("refuse");
        bed.guest.write("file", "x");
        let built = std::sync::atomic::AtomicBool::new(false);
        for (to, replace, why) in [
            ("app", false, "pass replace: true"),
            ("file", true, "not a directory"),
            ("file", false, "not a directory"),
        ] {
            let target = Target::parse(bed.guest.0.join(to).to_str().unwrap(), replace).unwrap();
            let bytes = AtomicU64::new(0);
            let err = push(&LocalGuest::new(), &target, replace, &bytes, |_| {
                built.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(Summary::default())
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains(why), "{to}: {err}");
        }
        assert!(
            !built.load(std::sync::atomic::Ordering::Relaxed),
            "an archive was built for a refused push"
        );
        assert!(bed.old_tree_is_intact());
        assert_eq!(bed.guest_entries(), ["app", "file"]);
    }

    #[test]
    fn a_failure_at_any_guest_step_leaves_the_old_tree_and_no_temporary_entries() {
        for step in [Step::Write, Step::Unpack, Step::Swap] {
            let bed = Bed::new(&format!("fail-{step:?}"));
            let guest = LocalGuest {
                fail: Some(step),
                path_env: None,
            };
            let err = format!("{:#}", bed.push("app", true, &guest).unwrap_err());
            assert!(err.contains("scripted failure"), "{step:?}: {err}");
            assert!(bed.old_tree_is_intact(), "{step:?}");
            assert_eq!(bed.guest_entries(), ["app"], "{step:?}");
        }
    }

    #[test]
    fn a_broken_archive_fails_at_unpacking_and_cleans_up() {
        let bed = Bed::new("broken");
        let target = Target::parse(bed.guest.0.join("app").to_str().unwrap(), true).unwrap();
        let bytes = AtomicU64::new(0);
        let err = push(&LocalGuest::new(), &target, true, &bytes, |out| {
            out.write_all(&[b'x'; 1024])?;
            Ok(Summary::default())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("unpacking"), "{err}");
        assert_eq!(bytes.load(std::sync::atomic::Ordering::Relaxed), 1024);
        assert!(bed.old_tree_is_intact());
        assert_eq!(bed.guest_entries(), ["app"]);
    }

    #[test]
    fn a_builder_failure_fails_the_push_even_though_the_send_saw_a_clean_end() {
        let bed = Bed::new("builder");
        let target = Target::parse(bed.guest.0.join("app").to_str().unwrap(), true).unwrap();
        let bytes = AtomicU64::new(0);
        let err = format!(
            "{:#}",
            push(&LocalGuest::new(), &target, true, &bytes, |_| bail!(
                "the file shrank"
            ))
            .unwrap_err()
        );
        assert!(err.contains("building the archive"), "{err}");
        assert!(bed.old_tree_is_intact());
        assert_eq!(bed.guest_entries(), ["app"]);
    }

    /// A `PATH` whose `mv` fails on the calls numbered in `fail_at`, counting calls in `dir`.
    fn failing_mv(dir: &Path, fail_at: &[u32]) -> String {
        let real = Command::new("/bin/sh")
            .args(["-c", "command -v mv"])
            .output()
            .unwrap();
        let real = String::from_utf8(real.stdout).unwrap();
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let count = dir.join("count");
        let fails: Vec<String> = fail_at
            .iter()
            .map(|n| format!("[ $n = {n} ] && exit 1"))
            .collect();
        fs::write(
            bin.join("mv"),
            format!(
                "#!/bin/sh\nn=$(($(cat {c} 2>/dev/null || echo 0) + 1)); echo $n > {c}\n{}\nexec {} \"$@\"\n",
                fails.join("\n"),
                real.trim(),
                c = count.display()
            ),
        )
        .unwrap();
        fs::set_permissions(bin.join("mv"), fs::Permissions::from_mode(0o755)).unwrap();
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
    }

    #[test]
    fn a_failed_move_in_the_swap_puts_the_old_tree_back() {
        for (fail_at, why) in [
            (&[1][..], "old tree is unchanged"),
            (&[2][..], "old tree is unchanged"),
        ] {
            let bed = Bed::new(&format!("mv-{}", fail_at[0]));
            let tools = Temp::new(&format!("mv-tools-{}", fail_at[0]));
            let guest = LocalGuest {
                fail: None,
                path_env: Some(failing_mv(&tools.0, fail_at)),
            };
            let err = bed.push("app", true, &guest).unwrap_err().to_string();
            assert!(err.contains(why), "{fail_at:?}: {err}");
            assert!(bed.old_tree_is_intact(), "{fail_at:?}");
            assert_eq!(bed.guest_entries(), ["app"], "{fail_at:?}");
        }
    }

    #[test]
    fn a_swap_that_cannot_put_the_old_tree_back_says_where_it_is() {
        let bed = Bed::new("mv-stuck");
        let tools = Temp::new("mv-stuck-tools");
        let guest = LocalGuest {
            fail: None,
            path_env: Some(failing_mv(&tools.0, &[2, 3])),
        };
        let err = bed.push("app", true, &guest).unwrap_err().to_string();
        assert!(err.contains("the old tree is at"), "{err}");
        let entries = bed.guest_entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].starts_with(".vmm-") && entries[0].ends_with(".old"));
        let kept = bed.guest.0.join(&entries[0]).join("old.txt");
        assert_eq!(fs::read_to_string(kept).unwrap(), "old");
    }
}
