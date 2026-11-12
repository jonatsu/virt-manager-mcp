//! Pushing a host directory into a guest.
//!
//! Selection and reading are separate steps. The `ignore` crate selects paths by name, with `.gitignore` rules and
//! `exclude` patterns. Every selected path is then opened from a handle on the pushed tree one component at a time,
//! never through a symlink, so a directory swapped for a symlink after selection cannot lead outside the share. The
//! archive is a tar stream, built while it is sent. The guest unpacks it beside the target and swaps the new tree in,
//! so a failed push leaves the old tree in place.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{Match, WalkBuilder};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::Serialize;

use crate::agent::{Agent, ExecSpec, GuestOs};
use crate::guestpath::GuestPath;
use crate::transfer::CountingReader;
use crate::winsteps;

/// The setuid and setgid bits, which a pushed entry never carries.
const SETID_BITS: u32 = 0o6000;
/// How long one guest step other than unpacking may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long unpacking, or deleting a replaced tree, may take in the guest.
const LONG_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How much of a guest step's stderr an error quotes.
const STDERR_MAX: usize = 4096;
/// The largest `.gitignore` or `.git/info/exclude` file a push reads.
const RULES_MAX: u64 = 1 << 20;

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
    let outer = if options.respect_gitignore {
        outer_rules(tree)?
    } else {
        Vec::new()
    };
    let selected = walk(tree, options, options.respect_gitignore, outer)?;
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

/// The tree's paths with whether each is a directory. With `gitignore`, the walk applies the `.gitignore` files in
/// the tree and the `outer` rules, and fails on an entry it cannot read. Without it, the walk only counts, so it
/// skips what it cannot read.
fn walk(
    tree: &Tree,
    options: &Options,
    gitignore: bool,
    outer: Vec<Gitignore>,
) -> Result<Vec<(PathBuf, bool)>> {
    let mut builder = WalkBuilder::new(&tree.path);
    // The standard filters also read the host user's global excludes and hidden-file rules, which differ between
    // machines, and ripgrep's `.ignore` files. The walk's own `.git/info/exclude` support follows a `.git` file to
    // any path it names, so `outer_rules` reads those files instead.
    builder
        .standard_filters(false)
        .git_ignore(gitignore)
        .require_git(false)
        .follow_links(false)
        .overrides(overrides(&tree.path, &options.exclude)?)
        .sort_by_file_name(|a, b| a.cmp(b));
    let include_git = options.include_git;
    let refused: Arc<Mutex<Option<anyhow::Error>>> = Arc::default();
    let refusal = refused.clone();
    builder.filter_entry(move |entry| {
        if !include_git && entry.file_name() == ".git" {
            return false;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if ignored_by(&outer, entry.path(), is_dir) {
            return false;
        }
        // The walk reads a directory's `.gitignore` once this returns true, so check it here.
        if gitignore
            && is_dir
            && let Err(e) = rules_file(&entry.path().join(".gitignore"))
        {
            let mut refusal = refusal.lock().expect("no thread panics holding this lock");
            refusal.get_or_insert(e);
            return false;
        }
        true
    });
    let mut out = Vec::new();
    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(e) if gitignore => return Err(e).context("selecting the files to push"),
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(&tree.path)?.to_path_buf();
        out.push((rel, entry.file_type().is_some_and(|t| t.is_dir())));
    }
    if let Some(e) = refused
        .lock()
        .expect("no thread panics holding this lock")
        .take()
    {
        return Err(e);
    }
    Ok(out)
}

/// Whether the rules file at `path` exists, refusing one that is not a regular file of at most `RULES_MAX` bytes.
/// Rules files are read by path, so a symlink to `/dev/zero`, a FIFO or a huge file there would exhaust memory or
/// block the push; git itself refuses a symlinked `.gitignore`.
fn rules_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() && m.len() <= RULES_MAX => Ok(true),
        Ok(_) => bail!(
            "{} is not a regular file of at most 1 MiB; a push reads only such rules files",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// Refuses a rules file above the share that is not, once symlinks are followed, a regular file of at most
/// `RULES_MAX` bytes. Outside the share the host's own files live, where a symlinked `~/.gitignore` is common.
fn followed_rules_file(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() > RULES_MAX => bail!(
            "{} is larger than 1 MiB; a push reads the .gitignore files above its share",
            path.display()
        ),
        Ok(m) if !m.is_file() => bail!(
            "{} is not a regular file; a push reads the .gitignore files above its share",
            path.display()
        ),
        // A missing or unreadable file is one the walk skips too.
        _ => Ok(()),
    }
}

/// `dir/.git/info/exclude` when `.git` and `info` are real directories and the file passes `rules_file`.
fn exclude_file(dir: &Path) -> Result<Option<PathBuf>> {
    for sub in [".git", ".git/info"] {
        match std::fs::symlink_metadata(dir.join(sub)) {
            Ok(m) if m.file_type().is_dir() => {}
            // A `.git` file belongs to a worktree whose exclude rules live elsewhere; they are not read.
            _ => return Ok(None),
        }
    }
    let path = dir.join(".git/info/exclude");
    Ok(rules_file(&path)?.then_some(path))
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

/// The rules the walk does not read itself, outermost first: the `.gitignore` files of the share directory and each
/// directory below it above the tree, and the `.git/info/exclude` files of those directories and the tree. Also
/// checks the tree's own `.gitignore`, which the walk reads before any filter sees it. An unparsable line is left
/// out, as git and the walk do.
fn outer_rules(tree: &Tree) -> Result<Vec<Gitignore>> {
    // The walk also reads, without applying, the `.gitignore` of every directory above the share.
    for dir in tree.share.ancestors().skip(1) {
        followed_rules_file(&dir.join(".gitignore"))?;
    }
    rules_file(&tree.path.join(".gitignore"))?;
    let mut dirs = vec![tree.share.clone()];
    for c in tree.path.strip_prefix(&tree.share)?.components() {
        let next = dirs.last().expect("the share is first").join(c);
        dirs.push(next);
    }
    let last = dirs.len() - 1;
    let mut rules = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        let mut builder = GitignoreBuilder::new(dir);
        let gitignore = dir.join(".gitignore");
        if i < last && rules_file(&gitignore)? {
            let _partial = builder.add(&gitignore);
        }
        if let Some(exclude) = exclude_file(dir)? {
            let _partial = builder.add(&exclude);
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

/// Writes the selected entries of `tree` to `out` as a tar archive for a guest running `os`. Every entry is owned by
/// uid and gid 0. On Linux it has its host mode without the setuid and setgid bits. On Windows every file has mode
/// 644 and every directory 755, and the archive leaves out symlinks and every entry Windows cannot hold under its
/// name, naming each in the summary; a directory left out takes its subtree with it.
pub fn write_archive(
    tree: &Tree,
    selection: &Selection,
    os: GuestOs,
    out: &mut dyn Write,
) -> Result<Summary> {
    let root = tree.open()?;
    let mut builder = tar::Builder::new(out);
    let mut summary = Summary {
        ignored: selection.ignored,
        ..Default::default()
    };
    let mut windows = (os == GuestOs::Windows).then(WindowsNames::default);
    for rel in &selection.entries {
        if let Some(names) = &mut windows {
            if names.inside_skipped(rel) {
                continue;
            }
            if let Some(reason) = names.problem(rel) {
                summary.skipped.push(Skipped {
                    path: rel.display().to_string(),
                    reason,
                });
                names.skip_subtree(rel);
                continue;
            }
        }
        let added = summary.files + summary.directories + summary.symlinks;
        add_entry(&mut builder, &root, rel, os, &mut summary)
            .with_context(|| format!("{}", rel.display()))?;
        if let Some(names) = &mut windows
            && summary.files + summary.directories + summary.symlinks > added
        {
            names.taken(rel);
        }
    }
    builder.into_inner().context("finishing the archive")?;
    Ok(summary)
}

/// What a push to Windows has seen: the names taken in each directory, compared without regard to case, and the
/// directories it left out.
#[derive(Default)]
struct WindowsNames {
    taken: HashMap<PathBuf, HashSet<String>>,
    skipped: Vec<PathBuf>,
}

impl WindowsNames {
    fn inside_skipped(&self, rel: &Path) -> bool {
        self.skipped.iter().any(|dir| rel.starts_with(dir))
    }

    /// Why Windows cannot hold `rel`'s name, if it cannot.
    fn problem(&self, rel: &Path) -> Option<&'static str> {
        let name = rel.file_name()?;
        let Some(name) = name.to_str() else {
            return Some("name is not valid UTF-8, which Windows cannot hold");
        };
        if let Some(reason) = windows_name_problem(name) {
            return Some(reason);
        }
        let parent = rel.parent().unwrap_or(Path::new(""));
        if self
            .taken
            .get(parent)
            .is_some_and(|names| names.contains(&fold_case(name)))
        {
            return Some(
                "name differs from an earlier one in its directory only in case, which Windows treats as one",
            );
        }
        None
    }

    fn skip_subtree(&mut self, rel: &Path) {
        self.skipped.push(rel.to_path_buf());
    }

    fn taken(&mut self, rel: &Path) {
        if let (Some(name), Some(parent)) = (rel.file_name().and_then(OsStr::to_str), rel.parent())
        {
            self.taken
                .entry(parent.to_path_buf())
                .or_default()
                .insert(fold_case(name));
        }
    }
}

/// Why Windows cannot hold a file named `name`, if it cannot: a reserved device name, also with an extension; a
/// control character or one of `< > : " | ? * \`; or a dot or a space at the end.
fn windows_name_problem(name: &str) -> Option<&'static str> {
    if name
        .chars()
        .any(|c| c.is_control() || "<>:\"|?*\\".contains(c))
    {
        return Some(
            r#"name holds a control character or one of < > : " | ? * \, which Windows cannot hold"#,
        );
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("name ends in a dot or a space, which Windows drops");
    }
    if is_short_alias(name) {
        return Some(
            "name has the shape of a Windows short name such as LONGFI~1.TXT, which a longer name in the push may \
             already hold",
        );
    }
    // Windows reads `CON.txt`, and `CON .txt`, as the device `CON`.
    let base = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = base.to_uppercase();
    let reserved = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|rest| {
            let mut chars = rest.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some('0'..='9' | '¹' | '²' | '³'), None)
            )
        })
    });
    reserved.then_some("name is a device name reserved by Windows")
}

/// Whether `name` has the shape of an 8.3 short name that Windows generates for a longer name, such as `LONGFI~1.TXT`:
/// up to six characters, `~` and digits, eight at most, then an optional extension of up to three characters.
/// Windows may already have given that alias to another file of the push, and `tar.exe` would then replace that file.
fn is_short_alias(name: &str) -> bool {
    let (base, extension) = match name.split_once('.') {
        Some((base, extension)) => (base, Some(extension)),
        None => (name, None),
    };
    if extension.is_some_and(|e| e.is_empty() || e.chars().count() > 3 || e.contains('.')) {
        return false;
    }
    let Some((stem, number)) = base.rsplit_once('~') else {
        return false;
    };
    (1..=6).contains(&stem.chars().count())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && base.chars().count() <= 8
}

/// `name` with each character in upper case where that is one character, as Windows compares names; `ß` stays.
fn fold_case(name: &str) -> String {
    name.chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

fn add_entry(
    builder: &mut tar::Builder<&mut dyn Write>,
    root: &OwnedFd,
    rel: &Path,
    os: GuestOs,
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
            let mut header = header(tar::EntryType::Regular, &st, os);
            header.set_size(size);
            let data = Exact {
                inner: File::from(fd),
                left: size,
            };
            append(builder, &mut header, rel, os, data)?;
            summary.files += 1;
            summary.bytes += size;
        }
        FileType::Directory => {
            let mut header = header(tar::EntryType::Directory, &st, os);
            append(builder, &mut header, rel, os, io::empty())?;
            summary.directories += 1;
        }
        FileType::Symlink if os == GuestOs::Windows => skip(
            "symlink, which a push to Windows leaves out: a Windows link needs a file or directory type the archive \
             cannot carry",
        ),
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(&parent, name, Vec::new())?;
            let target = Path::new(OsStr::from_bytes(target.as_bytes()));
            if link_stays_inside(root, rel, target) {
                let mut header = header(tar::EntryType::Symlink, &st, os);
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

/// Adds an entry at `rel`. `tar.exe` reads a name in a plain header in the guest's code page, so a Windows entry
/// whose path is not ASCII gets a PAX `path` record, which `tar.exe` reads as UTF-8, and an ASCII stand-in in its
/// own header.
fn append(
    builder: &mut tar::Builder<&mut dyn Write>,
    header: &mut tar::Header,
    rel: &Path,
    os: GuestOs,
    data: impl Read,
) -> Result<()> {
    let path = rel.as_os_str().as_bytes();
    if os == GuestOs::Linux || path.is_ascii() {
        builder.append_data(header, rel, data)?;
        return Ok(());
    }
    builder.append_pax_extensions([("path", path)])?;
    // Written into the header directly: `append_data` would add a GNU long-name entry for a long path.
    let stand_in: Vec<u8> = path
        .iter()
        .take(99)
        .map(|&b| if b.is_ascii() { b } else { b'_' })
        .collect();
    let name = &mut header.as_old_mut().name;
    name.fill(0);
    name[..stand_in.len()].copy_from_slice(&stand_in);
    header.set_cksum();
    builder.append(header, data)?;
    Ok(())
}

fn header(kind: tar::EntryType, st: &rustix::fs::Stat, os: GuestOs) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    // `tar.exe` marks a directory without its write bit read-only, so Windows gets fixed modes.
    let mode = match (os, kind) {
        (GuestOs::Linux, _) => st.st_mode & 0o7777 & !SETID_BITS,
        (GuestOs::Windows, tar::EntryType::Directory) => 0o755,
        (GuestOs::Windows, _) => 0o644,
    };
    header.set_mode(mode);
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
    separator: char,
    os: GuestOs,
    id: String,
}

impl Target {
    /// Parses `guest_path` under the rules of `os`, refusing a relative path, `..`, the root, and with `replace`, a
    /// directory directly below the root.
    pub fn parse(os: GuestOs, guest_path: &str, replace: bool) -> Result<Self> {
        let parsed = GuestPath::parse(os, guest_path)?;
        let depth = parsed.parts().len();
        if depth == 0 {
            bail!("{guest_path}: a push never replaces {}", parsed.root());
        }
        if replace && depth == 1 {
            bail!(
                "{guest_path}: replace refuses {} and the directories directly below it, as a guard against mistakes",
                parsed.root()
            );
        }
        Ok(Self {
            path: parsed.normalized(),
            parent: parsed.prefix(depth - 1),
            separator: parsed.separator(),
            os,
            id: uuid::Uuid::new_v4().simple().to_string(),
        })
    }

    /// A temporary name beside the target, starting with `.vmm-`.
    fn temp(&self, suffix: &str) -> String {
        let base = self.parent.trim_end_matches(self.separator);
        format!("{base}{}.vmm-{}.{suffix}", self.separator, self.id)
    }
}

/// One step a push runs in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Probe,
    /// Sending the archive, which `Guest::write` does; only the tests' guest names it, to fail it.
    #[cfg(test)]
    Write,
    /// Creating the directory to unpack into, a step of its own on Windows only, where `tar.exe` runs without a shell.
    MakeDir,
    Unpack,
    Swap,
    Cleanup,
}

/// How a guest step ended.
pub struct Ran {
    pub code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The step's stderr, else its stdout, else its exit status.
    fn describe(&self) -> String {
        match (self.stderr.is_empty(), self.stdout.is_empty(), self.code) {
            (false, _, _) => self.stderr.clone(),
            (true, false, _) => self.stdout.clone(),
            (true, true, Some(code)) => format!("exit status {code}"),
            (true, true, None) => "no exit status".into(),
        }
    }
}

/// What a push needs from a guest: running one of its steps with arguments, and writing a file.
pub trait Guest {
    fn run(&self, step: Step, args: &[&str]) -> Result<Ran>;
    fn write(&self, path: &str, data: &mut dyn Read) -> Result<u64>;
}

/// Builds a push's guest steps for the guest's OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Steps(pub GuestOs);

impl Steps {
    /// The command that runs `step` with `args`. Every script is a constant and every path a separate argument, so no
    /// path is ever parsed as shell code.
    pub fn spec(self, step: Step, args: &[&str]) -> Result<ExecSpec> {
        match self.0 {
            GuestOs::Linux => {
                let mut argv = vec![
                    "-c".to_string(),
                    linux_script(step)?.to_string(),
                    "sh".to_string(),
                ];
                argv.extend(args.iter().map(|a| (*a).to_string()));
                Ok(ExecSpec::new("/bin/sh", argv))
            }
            GuestOs::Windows => Ok(match step {
                Step::Probe => winsteps::script(winsteps::PUSH_PROBE, args),
                Step::MakeDir => winsteps::script(winsteps::PUSH_MKDIR, args),
                // `tar.exe` runs directly, without a shell, with the path that the probe printed, so stopping the step
                // after a timeout stops `tar.exe` itself.
                Step::Unpack => match args {
                    [archive, dir, tar] => {
                        ExecSpec::new(tar, ["-x", "-o", "-f", archive, "-C", dir])
                    }
                    _ => bail!("unpacking needs the archive, the directory and tar.exe's path"),
                },
                Step::Swap => winsteps::script(winsteps::PUSH_SWAP, args),
                Step::Cleanup => winsteps::script(winsteps::PUSH_CLEANUP, args),
                #[cfg(test)]
                Step::Write => bail!("writing the archive is no script step"),
            }),
        }
    }
}

/// The `sh` script of a Linux step.
fn linux_script(step: Step) -> Result<&'static str> {
    Ok(match step {
        Step::Probe => PROBE,
        Step::Unpack => UNPACK,
        Step::Swap => SWAP,
        Step::Cleanup => CLEANUP,
        Step::MakeDir => bail!("a Linux push creates its directory in the Unpack step"),
        #[cfg(test)]
        Step::Write => bail!("writing the archive is no script step"),
    })
}

/// A guest reached through its agent, with its OS's steps.
pub struct OnGuest<'a> {
    pub agent: &'a Agent<'a>,
    pub steps: Steps,
}

impl Guest for OnGuest<'_> {
    fn run(&self, step: Step, args: &[&str]) -> Result<Ran> {
        let timeout = match step {
            Step::Unpack | Step::Cleanup => LONG_TIMEOUT,
            _ => STEP_TIMEOUT,
        };
        let out = self
            .agent
            .exec(&self.steps.spec(step, args)?, timeout, STDERR_MAX)?;
        if out.timed_out {
            bail!(
                "the guest's {step:?} step did not finish within {} s",
                timeout.as_secs()
            );
        }
        Ok(Ran {
            code: out.exit_code,
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    fn write(&self, path: &str, data: &mut dyn Read) -> Result<u64> {
        // Windows has no file modes; the archive's directory decides who can read it.
        let mode = (self.steps.0 == GuestOs::Linux).then_some(0o600);
        self.agent.write_from(path, data, false, mode)
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

/// Unpacks archive `$1` into the new directory `$2`, without restoring the archive's owners. `tar` replaces the
/// shell, so stopping the step after a timeout stops `tar` too.
const UNPACK: &str = r#"mkdir -m 755 -- "$2" && exec tar -x -o -f "$1" -C "$2""#;

/// Moves the unpacked tree `$2` to the target `$1`. An existing target, allowed only when `$4` is 1, moves aside to
/// `$3` first; when the new tree cannot move in, the old one moves back. `mv` moves a tree into a directory that
/// appeared at the target meanwhile, so the script checks for that afterwards and moves it back out.
const SWAP: &str = r#"t=$1 d=$2 o=$3 n=${2##*/}
if [ -e "$t" ] || [ -L "$t" ]; then
  [ "$4" = 1 ] || exit 11
  if [ -L "$t" ] || [ ! -d "$t" ]; then exit 12; fi
  mv -- "$t" "$o" || exit 14
  if ! mv -- "$d" "$t"; then
    mv -- "$o" "$t" || exit 18
    exit 15
  fi
  if [ -e "$t/$n" ]; then mv -- "$t/$n" "$d"; exit 19; fi
else
  mv -- "$d" "$t" || exit 16
  if [ -e "$t/$n" ]; then mv -- "$t/$n" "$d"; exit 11; fi
fi
exit 0"#;

/// The Windows swap's exit status for a move the guest refused because a file is in use or access was denied, after
/// which nothing has changed.
const SWAP_BUSY: i64 = 20;

/// How long a push waits before it runs a refused Windows swap once more.
const SWAP_RETRY: Duration = Duration::from_secs(1);

/// Deletes the push's own `.vmm-` entries named in its arguments. `rm` replaces the shell, so stopping the step
/// after a timeout stops `rm` too.
const CLEANUP: &str = r#"exec rm -rf -- "$@""#;

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
    let probe = guest.run(Step::Probe, &[&target.path, &target.parent, flag])?;
    match probe.code {
        Some(0) => {}
        Some(10) if target.os == GuestOs::Windows => bail!(
            "the guest has no System32\\tar.exe, which Windows 10 1803 and later ship and copy_dir_to_vm needs"
        ),
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
    // The Windows probe prints the path of `tar.exe`; the Linux steps find `tar` on the guest's `PATH`.
    let tar = probe.stdout.lines().last().unwrap_or_default();
    let result = send_and_swap(
        guest,
        target,
        flag,
        bytes,
        archive,
        [&packed, &unpacked, &old, tar],
    );
    // The old tree goes only once the new one is in place; after a failed swap it may be all that is left of it.
    let mut leftovers = vec![packed.as_str(), unpacked.as_str()];
    if result.is_ok() {
        leftovers.push(&old);
    }
    let cleanup = guest.run(Step::Cleanup, &leftovers);
    let mut summary = result?;
    let failure = match cleanup {
        Ok(ran) if ran.code == Some(0) => None,
        Ok(ran) => Some(ran.describe()),
        Err(e) => Some(format!("{e:#}")),
    };
    if let Some(failure) = failure {
        summary.note = Some(format!(
            "the new tree is in place, but deleting {} in the guest failed: {failure}",
            leftovers.join(", ")
        ));
    }
    Ok(summary)
}

fn send_and_swap(
    guest: &dyn Guest,
    target: &Target,
    flag: &str,
    bytes: &AtomicU64,
    archive: impl FnOnce(&mut dyn Write) -> Result<Summary> + Send,
    [packed, unpacked, old, tar]: [&str; 4],
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
    let ran = match target.os {
        GuestOs::Linux => guest.run(Step::Unpack, &[packed, unpacked])?,
        GuestOs::Windows => {
            let made = guest.run(Step::MakeDir, &[unpacked])?;
            if made.code != Some(0) {
                bail!(
                    "creating {unpacked} in the guest failed: {}",
                    made.describe()
                );
            }
            guest.run(Step::Unpack, &[packed, unpacked, tar])?
        }
    };
    if ran.code != Some(0) {
        bail!(
            "unpacking the archive in the guest failed: {}",
            ran.describe()
        );
    }
    let swap = [target.path.as_str(), unpacked, old, flag];
    let mut ran = guest.run(Step::Swap, &swap)?;
    if ran.code == Some(SWAP_BUSY) {
        // A virus scanner may still hold a file it is checking in the freshly unpacked tree.
        std::thread::sleep(SWAP_RETRY);
        ran = guest.run(Step::Swap, &swap)?;
    }
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
        Some(19) => bail!(
            "{} appeared again in the guest while the old tree was moved aside; nothing was replaced, and the old \
             tree is at {old}",
            target.path
        ),
        Some(SWAP_BUSY) => bail!(
            "the guest refused to move the tree into {}, usually because a file in it is open; the old tree is \
             unchanged: {}",
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
    use std::cell::RefCell;
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
        archived_for(tree, GuestOs::Linux)
    }

    /// The archive of `tree` for a guest running `os`, as `archived` gives it.
    fn archived_for(tree: &Tree, os: GuestOs) -> (Vec<(String, Option<String>, u32)>, Summary) {
        let mut out = Vec::new();
        let summary =
            write_archive(tree, &select(tree, &options()).unwrap(), os, &mut out).unwrap();
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
        // Read as text, `a` names t/x; through `b` it really names the share's x, outside the tree. Both exist, so
        // only the check of every hop can refuse it.
        share.write("t/x", "inside");
        share.write("x", "outside");
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
            write_archive(&tree, &selection, GuestOs::Linux, &mut out).unwrap_err()
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
    fn a_windows_archive_has_fixed_modes_and_no_symlinks() {
        let share = Temp::new("win-modes");
        share.write("t/run", "#!/bin/sh\n");
        share.write("t/ro/x", "x");
        let t = share.0.join("t");
        fs::set_permissions(t.join("run"), fs::Permissions::from_mode(0o6750)).unwrap();
        symlink("run", t.join("link")).unwrap();
        fs::set_permissions(t.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
        let tree = tree(&share.0, "t");

        let (entries, summary) = archived_for(&tree, GuestOs::Windows);
        assert_eq!(
            entries,
            [
                ("ro".to_string(), None, 0o755),
                ("ro/x".to_string(), None, 0o644),
                ("run".to_string(), None, 0o644),
            ]
        );
        let skipped: Vec<_> = summary.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(skipped, ["link"]);
        assert!(summary.skipped[0].reason.contains("symlink"));
        // The Linux archive of the same tree keeps the link and the host modes.
        let (entries, _) = archived(&tree);
        assert!(entries.contains(&("link".to_string(), Some("run".to_string()), 0o777)));
        assert!(entries.contains(&("ro".to_string(), None, 0o555)));
        // The share is deleted afterwards, which needs the write bit back.
        fs::set_permissions(t.join("ro"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_windows_archive_skips_names_windows_cannot_hold() {
        let share = Temp::new("win-names");
        let kept = [
            "CONSOLE",
            "COM10",
            "a.b.c",
            "straße",
            "STRASSE",
            "README",
            "ok/inner",
            "longfilename.txt",
            "toolong~1.txt",
            "ab~1x.txt",
            "a~1.json",
            "~1",
        ];
        let refused = [
            "CON",
            "con.txt",
            "CONIN$",
            "conout$.log",
            "longfi~1.txt",
            "AB~12",
            "Lpt1",
            "COM¹",
            "nul .txt",
            "a?b",
            "a\\b",
            "tail.",
            "tail ",
            "new\nline",
            "Readme",
            "aux/inside",
        ];
        for name in kept.iter().chain(&refused) {
            share.write(&format!("t/{name}"), "x");
        }
        let t = share.0.join("t");
        fs::write(t.join(OsStr::from_bytes(b"bad\xff")), "x").unwrap();
        let tree = tree(&share.0, "t");

        let (entries, summary) = archived_for(&tree, GuestOs::Windows);
        let mut names: Vec<_> = entries
            .iter()
            .map(|e| e.0.trim_end_matches('/').to_string())
            .collect();
        names.sort();
        let mut want: Vec<_> = kept.iter().map(|s| (*s).to_string()).collect();
        want.push("ok".into());
        want.sort();
        assert_eq!(names, want);
        let mut skipped: Vec<_> = summary.skipped.iter().map(|s| s.path.clone()).collect();
        skipped.sort();
        let mut want: Vec<_> = refused
            .iter()
            .map(|s| s.trim_end_matches("/inside").to_string())
            .chain([String::from_utf8_lossy(b"bad\xff").into_owned()])
            .collect();
        want.sort();
        assert_eq!(
            skipped, want,
            "a skipped directory is named once, without its files"
        );
        let reason = |path: &str| {
            summary
                .skipped
                .iter()
                .find(|s| s.path == path)
                .unwrap()
                .reason
        };
        assert!(reason("Readme").contains("only in case"));
        assert!(reason("CON").contains("reserved"));
        assert!(reason("a\\b").contains("control character or one of"));
        assert!(reason("tail ").contains("dot or a space"));
    }

    #[test]
    fn a_windows_archive_names_non_ascii_paths_in_pax_records() {
        let share = Temp::new("win-pax");
        let long = format!("{}-日本語.txt", "x".repeat(120));
        share.write("t/été/ü.txt", "u");
        share.write(&format!("t/{long}"), "l");
        share.write("t/plain.txt", "p");
        let tree = tree(&share.0, "t");

        let (entries, _) = archived_for(&tree, GuestOs::Windows);
        let names: Vec<_> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, ["plain.txt", long.as_str(), "été", "été/ü.txt"]);

        let mut out = Vec::new();
        let selection = select(&tree, &options()).unwrap();
        write_archive(&tree, &selection, GuestOs::Windows, &mut out).unwrap();
        let mut archive = tar::Archive::new(&out[..]);
        let raw: Vec<(tar::EntryType, Vec<u8>)> = archive
            .entries()
            .unwrap()
            .raw(true)
            .map(|e| {
                let e = e.unwrap();
                (e.header().entry_type(), e.path_bytes().into_owned())
            })
            .collect();
        // Every plain header holds ASCII, each non-ASCII path comes from the PAX record before it, and no GNU long-name
        // entry names a long path a second time.
        assert!(raw.iter().all(|(_, name)| name.is_ascii()), "{raw:?}");
        let kinds: Vec<_> = raw.iter().map(|(kind, _)| *kind).collect();
        use tar::EntryType::{Directory, Regular, XHeader};
        assert_eq!(
            kinds,
            [
                Regular, XHeader, Regular, XHeader, Directory, XHeader, Regular
            ]
        );
        // The Linux archive keeps plain UTF-8 names.
        let mut out = Vec::new();
        write_archive(&tree, &selection, GuestOs::Linux, &mut out).unwrap();
        let mut archive = tar::Archive::new(&out[..]);
        assert!(
            archive
                .entries()
                .unwrap()
                .raw(true)
                .all(|e| e.unwrap().header().entry_type() != tar::EntryType::XHeader)
        );
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
            let err = match Target::parse(GuestOs::Linux, path, replace) {
                Ok(_) => panic!("{path} accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(why), "{path}: {err}");
        }
        let t = Target::parse(GuestOs::Linux, "/srv//app/./src/", true).unwrap();
        assert_eq!(
            (t.path.as_str(), t.parent.as_str()),
            ("/srv/app/src", "/srv/app")
        );
        assert!(t.temp("tar").starts_with("/srv/app/.vmm-"));
        let t = Target::parse(GuestOs::Linux, "/new", false).unwrap();
        assert!(t.temp("d").starts_with("/.vmm-"), "{}", t.temp("d"));
    }

    #[test]
    fn windows_targets_keep_the_drive_root_and_its_children_safe() {
        for (path, replace, why) in [
            (r"C:\", false, r"never replaces C:\"),
            ("c:/", true, r"never replaces C:\"),
            (r"C:\Windows", true, "directly below it"),
            (r"c:\windows\", true, "directly below it"),
            (r"C:\Windows.", true, "dot or a space"),
            ("C:\\Windows ", true, "dot or a space"),
            (r"\\server\share\app", false, "UNC"),
            ("C:work", false, "relative to the drive"),
        ] {
            let err = match Target::parse(GuestOs::Windows, path, replace) {
                Ok(_) => panic!("{path} accepted"),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(why), "{path}: {err}");
        }
        let t = Target::parse(GuestOs::Windows, "c:/work//app/", true).unwrap();
        assert_eq!(
            (t.path.as_str(), t.parent.as_str()),
            (r"C:\work\app", r"C:\work")
        );
        assert!(
            t.temp("tar").starts_with(r"C:\work\.vmm-"),
            "{}",
            t.temp("tar")
        );
        // Without replace, a directory directly below the root is a valid new target.
        let t = Target::parse(GuestOs::Windows, r"C:\work", false).unwrap();
        assert_eq!(t.parent, r"C:\");
        assert!(t.temp("d").starts_with(r"C:\.vmm-"), "{}", t.temp("d"));
        assert!(!t.temp("d").starts_with(r"C:\\"), "{}", t.temp("d"));
    }

    #[test]
    fn each_step_runs_its_own_script_for_the_guest_os() {
        let spec = Steps(GuestOs::Linux)
            .spec(Step::Cleanup, &["/a/.vmm-x.tar"])
            .unwrap();
        assert_eq!(spec.path, "/bin/sh");
        assert_eq!(spec.args.last().map(String::as_str), Some("/a/.vmm-x.tar"));
        assert!(Steps(GuestOs::Linux).spec(Step::MakeDir, &["/a"]).is_err());

        let windows = Steps(GuestOs::Windows);
        let args = [r"C:\it's\x", r"C:\y"];
        for (step, script) in [
            (Step::Probe, winsteps::PUSH_PROBE),
            (Step::MakeDir, winsteps::PUSH_MKDIR),
            (Step::Swap, winsteps::PUSH_SWAP),
            (Step::Cleanup, winsteps::PUSH_CLEANUP),
        ] {
            let spec = windows.spec(step, &args).unwrap();
            assert_eq!(spec.path, "powershell.exe", "{step:?}");
            assert_eq!(spec.args, winsteps::script(script, &args).args, "{step:?}");
        }
        assert!(windows.spec(Step::Unpack, &[r"C:\a.tar", r"C:\d"]).is_err());
        let spec = windows
            .spec(Step::Unpack, &[r"C:\a.tar", r"C:\d", WIN_TAR])
            .unwrap();
        assert_eq!(spec.path, WIN_TAR);
        assert_eq!(spec.args, ["-x", "-o", "-f", r"C:\a.tar", "-C", r"C:\d"]);
    }

    const WIN_TAR: &str = r"C:\Windows\System32\tar.exe";

    /// A Windows guest that records each step with its arguments and answers with the exit codes it is given: `probe`,
    /// `make_dir` and `unpack` once each, and `swaps` one per swap run, 0 once they run out.
    struct WindowsGuest {
        calls: RefCell<Vec<String>>,
        probe: i64,
        make_dir: i64,
        unpack: i64,
        swaps: RefCell<Vec<i64>>,
    }

    impl WindowsGuest {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                probe: 0,
                make_dir: 0,
                unpack: 0,
                swaps: RefCell::new(Vec::new()),
            }
        }

        /// The recorded calls, with the random part of each temporary name replaced by `ID`.
        fn calls(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|c| {
                    let mut out = String::new();
                    let mut rest = c.as_str();
                    while let Some(at) = rest.find(".vmm-") {
                        out.push_str(&rest[..at + 5]);
                        rest = &rest[at + 5 + 32..];
                        out.push_str("ID");
                    }
                    out.push_str(rest);
                    out
                })
                .collect()
        }

        fn push(&self, replace: bool) -> Result<Summary> {
            let target = Target::parse(GuestOs::Windows, r"C:\work\app", replace)?;
            push(self, &target, replace, &AtomicU64::new(0), |out| {
                out.write_all(b"archive")?;
                Ok(Summary::default())
            })
        }
    }

    impl Guest for WindowsGuest {
        fn run(&self, step: Step, args: &[&str]) -> Result<Ran> {
            // The step must exist for Windows with these arguments.
            Steps(GuestOs::Windows).spec(step, args)?;
            self.calls
                .borrow_mut()
                .push(format!("{step:?} {}", args.join(" | ")));
            let (code, stdout, stderr) = match step {
                Step::Probe => (self.probe, WIN_TAR, ""),
                Step::MakeDir => (self.make_dir, "", "cannot create"),
                Step::Unpack => (
                    self.unpack,
                    "",
                    "tar.exe: Archive entry has empty or unreadable filename",
                ),
                Step::Swap => {
                    let mut swaps = self.swaps.borrow_mut();
                    let code = if swaps.is_empty() { 0 } else { swaps.remove(0) };
                    (code, "", "The process cannot access the file")
                }
                _ => (0, "", ""),
            };
            Ok(Ran {
                code: Some(code),
                stdout: stdout.into(),
                stderr: if code == 0 {
                    String::new()
                } else {
                    stderr.into()
                },
            })
        }

        fn write(&self, path: &str, data: &mut dyn Read) -> Result<u64> {
            self.calls.borrow_mut().push(format!("write {path}"));
            Ok(io::copy(data, &mut io::sink())?)
        }
    }

    #[test]
    fn a_windows_push_unpacks_with_tar_exe_and_swaps_the_tree_in() {
        let guest = WindowsGuest::new();
        guest.push(true).unwrap();
        assert_eq!(
            guest.calls(),
            [
                r"Probe C:\work\app | C:\work | 1",
                r"write C:\work\.vmm-ID.tar",
                r"MakeDir C:\work\.vmm-ID.d",
                r"Unpack C:\work\.vmm-ID.tar | C:\work\.vmm-ID.d | C:\Windows\System32\tar.exe",
                r"Swap C:\work\app | C:\work\.vmm-ID.d | C:\work\.vmm-ID.old | 1",
                r"Cleanup C:\work\.vmm-ID.tar | C:\work\.vmm-ID.d | C:\work\.vmm-ID.old",
            ]
        );
    }

    #[test]
    fn each_windows_failure_has_its_message_and_keeps_the_old_tree() {
        let cases = [
            (
                WindowsGuest {
                    probe: 10,
                    ..WindowsGuest::new()
                },
                "no System32\\tar.exe",
            ),
            (
                WindowsGuest {
                    probe: 11,
                    ..WindowsGuest::new()
                },
                "pass replace: true",
            ),
            (
                WindowsGuest {
                    probe: 12,
                    ..WindowsGuest::new()
                },
                "not a directory",
            ),
            (
                WindowsGuest {
                    probe: 13,
                    ..WindowsGuest::new()
                },
                "parent directories",
            ),
            (
                WindowsGuest {
                    make_dir: 1,
                    ..WindowsGuest::new()
                },
                "cannot create",
            ),
            (
                WindowsGuest {
                    unpack: 1,
                    ..WindowsGuest::new()
                },
                "unreadable filename",
            ),
            (
                WindowsGuest {
                    swaps: RefCell::new(vec![11]),
                    ..WindowsGuest::new()
                },
                "appeared",
            ),
            (
                WindowsGuest {
                    swaps: RefCell::new(vec![14]),
                    ..WindowsGuest::new()
                },
                "old tree is unchanged",
            ),
            (
                WindowsGuest {
                    swaps: RefCell::new(vec![18]),
                    ..WindowsGuest::new()
                },
                "the old tree is at",
            ),
        ];
        for (guest, why) in cases {
            let err = guest.push(true).unwrap_err().to_string();
            assert!(err.contains(why), "{why}: {err}");
            let calls = guest.calls();
            if guest.probe != 0 {
                // A refused probe sends nothing and leaves nothing to clean up.
                assert_eq!(calls.len(), 1, "{calls:?}");
                continue;
            }
            // Every later failure deletes the archive and the unpacked tree, never the old tree.
            assert_eq!(
                calls.last().unwrap(),
                r"Cleanup C:\work\.vmm-ID.tar | C:\work\.vmm-ID.d",
                "{why}"
            );
        }
    }

    #[test]
    fn a_swap_refused_for_an_open_file_runs_once_more_then_fails() {
        let guest = WindowsGuest {
            swaps: RefCell::new(vec![SWAP_BUSY]),
            ..WindowsGuest::new()
        };
        guest.push(true).unwrap();
        let swaps = |g: &WindowsGuest| g.calls().iter().filter(|c| c.starts_with("Swap")).count();
        assert_eq!(swaps(&guest), 2);

        let guest = WindowsGuest {
            swaps: RefCell::new(vec![SWAP_BUSY, SWAP_BUSY]),
            ..WindowsGuest::new()
        };
        let err = guest.push(true).unwrap_err().to_string();
        assert!(
            err.contains("usually because a file in it is open")
                && err.contains("old tree is unchanged")
                && err.contains("cannot access the file"),
            "{err}"
        );
        assert_eq!(swaps(&guest), 2);
        assert_eq!(
            guest.calls().last().unwrap(),
            r"Cleanup C:\work\.vmm-ID.tar | C:\work\.vmm-ID.d"
        );
    }

    /// A guest that runs each step's script with the host's `sh` and `tar`, failing the step named in `fail`, with
    /// `PATH` replaced when `path_env` is set, and creating the directory `appear` just before the swap.
    struct LocalGuest {
        fail: Option<Step>,
        path_env: Option<String>,
        appear: Option<PathBuf>,
    }

    impl LocalGuest {
        fn new() -> Self {
            Self {
                fail: None,
                path_env: None,
                appear: None,
            }
        }
    }

    impl Guest for LocalGuest {
        fn run(&self, step: Step, args: &[&str]) -> Result<Ran> {
            if self.fail == Some(step) {
                bail!("scripted failure at {step:?}");
            }
            if step == Step::Swap
                && let Some(dir) = &self.appear
            {
                fs::create_dir_all(dir)?;
            }
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(linux_script(step)?).arg("sh").args(args);
            if let Some(path) = &self.path_env {
                cmd.env("PATH", path);
            }
            let out = cmd.output()?;
            Ok(Ran {
                code: out.status.code().map(i64::from),
                stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
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
            let target = Target::parse(
                GuestOs::Linux,
                self.guest.0.join(to).to_str().unwrap(),
                replace,
            )?;
            let bytes = AtomicU64::new(0);
            push(guest, &target, replace, &bytes, |out| {
                write_archive(&tree, &select(&tree, &options())?, GuestOs::Linux, out)
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
            let target = Target::parse(
                GuestOs::Linux,
                bed.guest.0.join(to).to_str().unwrap(),
                replace,
            )
            .unwrap();
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
                ..LocalGuest::new()
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
        let target = Target::parse(
            GuestOs::Linux,
            bed.guest.0.join("app").to_str().unwrap(),
            true,
        )
        .unwrap();
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
        let target = Target::parse(
            GuestOs::Linux,
            bed.guest.0.join("app").to_str().unwrap(),
            true,
        )
        .unwrap();
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
                appear: None,
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
            appear: None,
        };
        let err = bed.push("app", true, &guest).unwrap_err().to_string();
        assert!(err.contains("the old tree is at"), "{err}");
        let entries = bed.guest_entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].starts_with(".vmm-") && entries[0].ends_with(".old"));
        let kept = bed.guest.0.join(&entries[0]).join("old.txt");
        assert_eq!(fs::read_to_string(kept).unwrap(), "old");
    }

    /// A `PATH` whose `name` is the shell script `body`, which may run the real tool as `$REAL`.
    fn fake_tool(dir: &Path, name: &str, body: &str) -> String {
        let real = Command::new("/bin/sh")
            .args(["-c", &format!("command -v {name}")])
            .output()
            .unwrap();
        let real = String::from_utf8(real.stdout).unwrap();
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join(name),
            format!("#!/bin/sh\nREAL={}\n{body}\n", real.trim()),
        )
        .unwrap();
        fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
    }

    #[test]
    fn a_rules_file_that_is_not_a_small_regular_file_refuses_the_push() {
        let outside = Temp::new("rules-outside");
        outside.write("ignore", "*.log\n");
        for (name, rel) in [
            ("root", ".gitignore"),
            ("sub", "sub/.gitignore"),
            ("parent", "../.gitignore"),
            ("exclude", ".git/info/exclude"),
        ] {
            let share = Temp::new(&format!("rules-{name}"));
            share.write("t/sub/f", "x");
            let path = share.0.join("t").join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink(outside.0.join("ignore"), &path).unwrap();
            let tree = tree(&share.0, "t");
            let err = format!("{:#}", select(&tree, &options()).err().unwrap());
            assert!(err.contains("not a regular file"), "{name}: {err}");
            let plain = Options {
                respect_gitignore: false,
                ..options()
            };
            assert!(select(&tree, &plain).is_ok(), "{name}");
        }
        let share = Temp::new("rules-big");
        share.write("t/f", "x");
        fs::write(share.0.join("t/.gitignore"), vec![b'#'; 2 << 20]).unwrap();
        let err = format!(
            "{:#}",
            select(&tree(&share.0, "t"), &options()).err().unwrap()
        );
        assert!(err.contains("at most 1 MiB"), "{err}");
    }

    #[test]
    fn a_gitignore_above_the_share_may_be_a_symlink_but_not_a_huge_file() {
        let base = Temp::new("rules-above");
        base.write("share/t/f", "x");
        base.write("real-ignore", "*.log\n");
        symlink(base.0.join("real-ignore"), base.0.join(".gitignore")).unwrap();
        let share = base.0.join("share");
        assert!(select(&tree(&share, "t"), &options()).is_ok());

        fs::write(base.0.join("real-ignore"), vec![b'#'; 2 << 20]).unwrap();
        let err = format!(
            "{:#}",
            select(&tree(&share, "t"), &options()).err().unwrap()
        );
        assert!(err.contains("larger than 1 MiB"), "{err}");
    }

    #[test]
    fn an_unreadable_directory_that_gitignore_leaves_out_does_not_fail_the_push() {
        let share = Temp::new("unreadable");
        share.write("t/.gitignore", "data/\n");
        share.write("t/data/locked/secret", "x");
        share.write("t/keep", "x");
        let locked = share.0.join("t/data/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let selected = select(&tree(&share.0, "t"), &options());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let names: Vec<_> = selected
            .unwrap()
            .entries
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(names, [".gitignore", "keep"]);
    }

    #[test]
    fn a_target_that_appears_before_the_swap_gets_nothing_moved_into_it() {
        let bed = Bed::new("appear");
        let guest = LocalGuest {
            appear: Some(bed.guest.0.join("new")),
            ..LocalGuest::new()
        };
        let err = bed.push("new", false, &guest).unwrap_err().to_string();
        assert!(err.contains("appeared"), "{err}");
        assert_eq!(fs::read_dir(bed.guest.0.join("new")).unwrap().count(), 0);
        assert_eq!(bed.guest_entries(), ["app", "new"]);
    }

    #[test]
    fn a_target_that_reappears_during_a_replace_keeps_the_old_tree_aside() {
        let bed = Bed::new("reappear");
        let tools = Temp::new("reappear-tools");
        // The first `mv -- t o` moves the old tree aside; then something creates `t` again.
        let count = tools.0.join("count");
        let body = format!(
            "n=$(($(cat {c} 2>/dev/null || echo 0) + 1)); echo $n > {c}\n\"$REAL\" \"$@\" || exit\n\
             if [ $n = 1 ]; then mkdir \"$2\"; fi",
            c = count.display()
        );
        let guest = LocalGuest {
            path_env: Some(fake_tool(&tools.0, "mv", &body)),
            ..LocalGuest::new()
        };
        let err = bed.push("app", true, &guest).unwrap_err().to_string();
        assert!(
            err.contains("appeared again") && err.contains("old tree is at"),
            "{err}"
        );
        let entries = bed.guest_entries();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(fs::read_dir(bed.guest.0.join("app")).unwrap().count(), 0);
        let old = bed.guest.0.join(&entries[0]).join("old.txt");
        assert_eq!(fs::read_to_string(old).unwrap(), "old");
    }

    #[test]
    fn a_replace_whose_old_tree_cannot_be_deleted_still_succeeds_with_a_note() {
        let bed = Bed::new("rm-fails");
        let tools = Temp::new("rm-fails-tools");
        let guest = LocalGuest {
            path_env: Some(fake_tool(
                &tools.0,
                "rm",
                "case \"$*\" in *.old*) echo 'rm: busy' >&2; exit 1;; esac\nexec \"$REAL\" \"$@\"",
            )),
            ..LocalGuest::new()
        };
        let summary = bed.push("app", true, &guest).unwrap();
        let note = summary.note.unwrap();
        assert!(
            note.contains("new tree is in place") && note.contains("rm: busy"),
            "{note}"
        );
        assert!(bed.guest.0.join("app/main.rs").exists());
        assert!(!bed.guest.0.join("app/old.txt").exists());
    }
}
