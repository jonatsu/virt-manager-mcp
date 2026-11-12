//! Pulling a guest directory into a pull directory on the host.
//!
//! The guest packs the directory with its own `tar` into its temporary directory while the server watches the
//! archive's size. The archive then streams to the host, where the server unpacks it itself. The archive is untrusted:
//! the unpacker reads it in the `tar` crate's raw mode, merges long names itself, checks every entry, leaves links and
//! special files out, and builds the tree in the staging directory with fixed modes. Only a complete tree moves into
//! place, as `crate::pull` moves a single file.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use serde::Serialize;
use tar::EntryType;

use crate::agent::{Agent, ExecOutcome, ExecSpec, GuestOs};
use crate::pull::{self, Destination, FILE_MODE};
use crate::push::Skipped;
use crate::transfer::CountingWriter;

/// The largest long-name or PAX header the unpacker reads, so metadata cannot fill memory.
const META_MAX: u64 = 64 * 1024;
/// The longest path the unpacker accepts, in bytes: Linux's own `PATH_MAX`.
const PATH_MAX: usize = 4096;
/// The deepest path the unpacker accepts. Deleting a tree keeps a handle open per level, so this stays well under the
/// usual 1024-handle limit.
const DEPTH_MAX: usize = 256;
/// How many skipped entries a reply names; it counts the rest.
const SKIPPED_SHOWN: usize = 100;
/// How often the server checks the guest's archive while `tar` builds it.
pub const POLL: Duration = Duration::from_millis(500);
/// How long the guest's `tar` may run.
const PACK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long a short guest step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of a guest step's output an error quotes.
const OUTPUT_MAX: usize = 4096;
/// Where the guest's archive goes: the first of these that exists.
pub const TEMP_DIRS: [&str; 2] = ["/var/tmp", "/tmp"];
/// GNU tar's warning for a file that changed while it was read, which still leaves a usable archive.
const CHANGED: &str = "file changed as we read it";

/// The caps on one directory pull.
pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: u64,
}

/// What a directory pull wrote, and what it left out.
#[derive(Debug, Default, Serialize)]
pub struct Unpacked {
    pub files: u64,
    pub directories: u64,
    /// File bytes written.
    pub bytes: u64,
    /// Entries that `exclude` left out.
    pub excluded: u64,
    pub skipped: Vec<Skipped>,
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped_more: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Unpacked {
    fn skip(&mut self, path: &Path, reason: &'static str) {
        if self.skipped.len() < SKIPPED_SHOWN {
            self.skipped.push(Skipped {
                path: path.display().to_string(),
                reason,
            });
        } else {
            self.skipped_more += 1;
        }
    }
}

/// `exclude` as one gitignore matcher over the archive's relative paths.
pub fn excludes(patterns: &[String]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new("");
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .with_context(|| format!("exclude pattern {pattern:?}"))?;
    }
    Ok(builder.build()?)
}

/// Unpacks the untrusted archive `src` into the directory `root`. Any failure leaves whatever was written in `root`
/// for the caller to discard.
pub fn unpack(
    src: &mut dyn Read,
    root: &OwnedFd,
    limits: &Limits,
    exclude: &Gitignore,
) -> Result<Unpacked> {
    let mut archive = tar::Archive::new(src);
    let mut out = Unpacked::default();
    let mut tree = Tree {
        root,
        limits,
        keys: RandomState::new(),
        seen: HashSet::new(),
        dirs: HashSet::new(),
        entries: 0,
    };
    let mut long_name: Option<Vec<u8>> = None;
    for item in archive.entries().context("reading the archive")?.raw(true) {
        let mut entry = item.context("reading the archive")?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() {
            let data = metadata(&mut entry)?;
            let name = data.split(|b| *b == 0).next().unwrap_or_default();
            long_name = Some(name.to_vec());
            continue;
        }
        if kind.is_gnu_longlink() {
            // Only links carry one, and links are skipped.
            metadata(&mut entry)?;
            continue;
        }
        if kind.is_pax_global_extensions() {
            bail!("the archive has a PAX global header, which the unpacker refuses");
        }
        if kind.is_pax_local_extensions() {
            if let Some(path) = pax_path(&mut entry)? {
                long_name = Some(path);
            }
            continue;
        }
        if kind.is_gnu_sparse() {
            bail!("the archive has a GNU sparse entry, which the unpacker refuses");
        }
        tree.count()?;
        let raw = match long_name.take() {
            Some(name) => name,
            None => entry.header().path_bytes().into_owned(),
        };
        let rel = relative(&raw)?;
        if rel.as_os_str().is_empty() {
            if kind.is_dir() {
                continue;
            }
            bail!("an entry other than a directory names the archive's root");
        }
        if !tree.seen.insert(tree.key(&rel)) {
            bail!("{} appears twice in the archive", rel.display());
        }
        if exclude
            .matched_path_or_any_parents(&rel, kind.is_dir())
            .is_ignore()
        {
            out.excluded += 1;
            continue;
        }
        match kind {
            EntryType::Regular | EntryType::Continuous => {
                write_file(&mut entry, &mut tree, &rel, &mut out)
                    .with_context(|| format!("{}", rel.display()))?;
            }
            EntryType::Directory => {
                tree.dir(&rel)?;
                out.directories += 1;
            }
            EntryType::Symlink => out.skip(&rel, "symlink"),
            EntryType::Link => out.skip(&rel, "hard link"),
            EntryType::Char | EntryType::Block => out.skip(&rel, "device file"),
            EntryType::Fifo => out.skip(&rel, "FIFO"),
            other => bail!("{}: unsupported entry type {other:?}", rel.display()),
        }
    }
    if long_name.is_some() {
        bail!("the archive ends with a long name that names no entry");
    }
    Ok(out)
}

/// The data of a long-name or PAX header, refused when larger than `META_MAX`.
fn metadata<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Vec<u8>> {
    let size = entry.size();
    if size > META_MAX {
        bail!(
            "the archive has a {size}-byte name or PAX header, over the unpacker's {META_MAX}-byte limit"
        );
    }
    let mut data = Vec::new();
    entry.read_to_end(&mut data)?;
    Ok(data)
}

/// The `path` record of a PAX header. A `size` record or a `GNU.sparse` key fails the pull: in raw mode the `tar`
/// crate reads past each entry by its ustar size and ignores both, so honouring them would misplace the stream or
/// corrupt the file.
fn pax_path<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Option<Vec<u8>>> {
    if entry.size() > META_MAX {
        bail!(
            "the archive has a {}-byte PAX header, over the unpacker's {META_MAX}-byte limit",
            entry.size()
        );
    }
    let mut path = None;
    let records = entry
        .pax_extensions()?
        .ok_or_else(|| anyhow!("a PAX header without records"))?;
    for record in records {
        let record = record.context("reading a PAX header")?;
        let key = record
            .key()
            .map_err(|_| anyhow!("a PAX record with a key that is not UTF-8"))?;
        if key == "size" {
            bail!("the archive has a PAX size record, which the unpacker refuses");
        }
        if key.starts_with("GNU.sparse.") {
            bail!("the archive has a PAX sparse entry, which the unpacker refuses");
        }
        if key == "path" {
            path = Some(record.value_bytes().to_vec());
        }
    }
    Ok(path)
}

/// The entry's path relative to the archive's root, refusing an absolute path, `..`, and a path longer than
/// `PATH_MAX` bytes or deeper than `DEPTH_MAX` levels.
fn relative(raw: &[u8]) -> Result<PathBuf> {
    if raw.len() > PATH_MAX {
        bail!(
            "the archive names a path of {} bytes, over the unpacker's {PATH_MAX}-byte limit",
            raw.len()
        );
    }
    let shown = String::from_utf8_lossy(raw);
    let mut rel = PathBuf::new();
    let mut depth = 0;
    for c in Path::new(OsStr::from_bytes(raw)).components() {
        match c {
            Component::Normal(name) => {
                depth += 1;
                if depth > DEPTH_MAX {
                    bail!("{shown}: the archive names a path deeper than {DEPTH_MAX} levels");
                }
                rel.push(name);
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("{shown}: the archive names a path with '..'"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("{shown}: the archive names an absolute path")
            }
        }
    }
    Ok(rel)
}

fn shared_path(rel: &Path) -> anyhow::Error {
    anyhow!(
        "{}: a file and a directory share this path in the archive",
        rel.display()
    )
}

/// What `open_or_create_dir` failing at `rel` means: a name that is not a directory, or a system error as it is.
fn dir_error(e: anyhow::Error, rel: &Path) -> anyhow::Error {
    if e.chain().any(|c| c.downcast_ref::<Errno>().is_some()) {
        e.context(format!("{}", rel.display()))
    } else {
        shared_path(rel)
    }
}

/// The tree an unpack builds, and what it has seen. Paths are kept as keyed 64-bit hashes, so memory stays bounded by
/// `max_pull_entries` whatever names the archive uses; the random key keeps an archive from aiming at a collision.
struct Tree<'a> {
    root: &'a OwnedFd,
    limits: &'a Limits,
    keys: RandomState,
    /// Every path the archive named.
    seen: HashSet<u64>,
    /// Every directory that exists in the tree, named by the archive or created on the way to an entry.
    dirs: HashSet<u64>,
    entries: u64,
}

impl Tree<'_> {
    fn key(&self, path: &Path) -> u64 {
        self.keys.hash_one(path)
    }

    /// Counts one more entry, archive entries and the directories made on their way alike, against the cap.
    fn count(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            bail!(
                "the archive holds more than {} entries, counting the directories its paths imply \
                 (limits.max_pull_entries)",
                self.limits.max_entries
            );
        }
        Ok(())
    }

    /// The directory that holds `rel`, creating missing ones with mode 755, each counted as an entry.
    fn parent_of<'p>(&mut self, rel: &'p Path) -> Result<(OwnedFd, &'p OsStr)> {
        let name = rel.file_name().expect("a relative path with a name");
        let mut dir = rustix::io::dup(self.root)?;
        let mut walked = PathBuf::new();
        for c in rel.parent().into_iter().flat_map(Path::components) {
            walked.push(c);
            let key = self.key(&walked);
            if !self.dirs.contains(&key) {
                self.count()?;
                self.dirs.insert(key);
            }
            dir =
                pull::open_or_create_dir(&dir, c.as_os_str()).map_err(|e| dir_error(e, &walked))?;
        }
        Ok((dir, name))
    }

    /// Creates the directory `rel`, which the archive names itself and so is counted already.
    fn dir(&mut self, rel: &Path) -> Result<()> {
        let (parent, name) = self.parent_of(rel)?;
        pull::open_or_create_dir(&parent, name).map_err(|e| dir_error(e, rel))?;
        let key = self.key(rel);
        self.dirs.insert(key);
        Ok(())
    }
}

fn write_file<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    tree: &mut Tree<'_>,
    rel: &Path,
    out: &mut Unpacked,
) -> Result<()> {
    let size = entry.size();
    let max = tree.limits.max_bytes;
    if out.bytes.saturating_add(size) > max {
        bail!("the archive's files pass the pull limit of {max} bytes (limits.max_pull_bytes)");
    }
    let (parent, name) = tree.parent_of(rel)?;
    let fd = match rustix::fs::openat(
        &parent,
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from(0o600),
    ) {
        Ok(fd) => fd,
        Err(Errno::EXIST) => return Err(shared_path(rel)),
        Err(e) => return Err(e.into()),
    };
    let mut file = File::from(fd);
    let copied = io::copy(entry, &mut file)?;
    if copied != size {
        bail!("the archive ends inside this file");
    }
    rustix::fs::fchmod(&file, Mode::from(FILE_MODE))?;
    out.files += 1;
    out.bytes += size;
    Ok(())
}

// ---- the guest side ----------------------------------------------------------------------------------------------

/// How a guest step ended.
pub struct Ran {
    pub code: Option<i64>,
    pub stdout: String,
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

/// One step a pull runs in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Prepare,
    Pack,
    Remove,
}

/// What a directory pull needs from a guest: running and starting its steps, and reading the archive.
pub trait Guest {
    fn run(&self, step: Step, args: &[&str]) -> Result<Ran>;
    fn start(&self, step: Step, args: &[&str]) -> Result<i64>;
    fn status(&self, pid: i64) -> Result<Option<Ran>>;
    /// The size of the file at `path`, or `None` when it does not exist yet.
    fn size(&self, path: &str) -> Result<Option<u64>>;
    fn kill(&self, pid: i64);
    fn read(&self, path: &str, out: &mut dyn Write, max_bytes: u64) -> Result<u64>;
}

/// Builds a pull's guest steps for the guest's OS.
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
                    linux_script(step).to_string(),
                    "sh".to_string(),
                ];
                argv.extend(args.iter().map(|a| (*a).to_string()));
                Ok(ExecSpec::new("/bin/sh", argv))
            }
            GuestOs::Windows => bail!("copy_dir_from_vm does not support Windows guests yet"),
        }
    }
}

/// The `sh` script of a Linux step.
fn linux_script(step: Step) -> &'static str {
    match step {
        Step::Prepare => PREPARE,
        Step::Pack => PACK,
        Step::Remove => REMOVE,
    }
}

fn ran(out: ExecOutcome) -> Ran {
    Ran {
        code: out.exit_code,
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    }
}

/// A guest reached through its agent, with its OS's steps.
pub struct OnGuest<'a> {
    pub agent: &'a Agent<'a>,
    pub steps: Steps,
}

impl Guest for OnGuest<'_> {
    fn run(&self, step: Step, args: &[&str]) -> Result<Ran> {
        let out = self
            .agent
            .exec(&self.steps.spec(step, args)?, STEP_TIMEOUT, OUTPUT_MAX)?;
        if out.timed_out {
            bail!(
                "the guest's {step:?} step did not finish within {} s",
                STEP_TIMEOUT.as_secs()
            );
        }
        Ok(ran(out))
    }

    fn start(&self, step: Step, args: &[&str]) -> Result<i64> {
        self.agent.start(&self.steps.spec(step, args)?)
    }

    fn status(&self, pid: i64) -> Result<Option<Ran>> {
        Ok(self.agent.status(pid, OUTPUT_MAX)?.map(ran))
    }

    fn size(&self, path: &str) -> Result<Option<u64>> {
        // On Linux the agent cannot tell a missing file from another failure, so any failure reads as a missing
        // archive; the size of the finished archive is required before anything is sent.
        Ok(self.agent.file_size(path).ok())
    }

    fn kill(&self, pid: i64) {
        let _ = self.agent.kill(pid);
    }

    fn read(&self, path: &str, out: &mut dyn Write, max_bytes: u64) -> Result<u64> {
        self.agent.read_to(path, &mut &mut *out, max_bytes)
    }
}

/// Exits 10 without `tar` and 11 when `$1` is not a directory; otherwise prints the first of `$2` and `$3` that is a
/// directory, or exits 12 when neither is.
const PREPARE: &str = r#"command -v tar >/dev/null 2>&1 || exit 10
[ -d "$1" ] || exit 11
for d in "$2" "$3"; do
  if [ -d "$d" ]; then printf '%s' "$d"; exit 0; fi
done
exit 12"#;

/// Packs directory `$1` into archive `$2`, readable by the agent's user only.
const PACK: &str = r#"umask 077; exec tar -c -f "$2" -C "$1" ."#;

/// Deletes the archive `$1`, the pull's own `.vmm-` name.
const REMOVE: &str = r#"rm -f -- "$1""#;

/// Where a directory pull goes, and what it takes.
pub struct Request<'a> {
    pub guest_path: &'a str,
    pub dest: &'a Destination,
    pub overwrite: bool,
    pub limits: &'a Limits,
    pub exclude: &'a Gitignore,
    pub temp_dirs: [&'a str; 2],
    pub poll: Duration,
}

/// Pulls the guest directory `req.guest_path` to `req.dest`. The guest's archive is deleted whatever happens.
pub fn pull(guest: &dyn Guest, req: &Request<'_>, bytes: &AtomicU64) -> Result<Unpacked> {
    let [first, second] = req.temp_dirs;
    let prep = guest.run(Step::Prepare, &[req.guest_path, first, second])?;
    match prep.code {
        Some(0) => {}
        Some(10) => bail!("the guest has no tar, which copy_dir_from_vm needs"),
        Some(11) => bail!("{} is not a directory in the guest", req.guest_path),
        Some(12) => bail!("the guest has neither {first} nor {second} for the archive"),
        _ => bail!("cannot prepare the pull in the guest: {}", prep.describe()),
    }
    let archive = format!(
        "{}/.vmm-{}.tar",
        prep.stdout.trim_end_matches('/'),
        uuid::Uuid::new_v4().simple()
    );
    let result = pack_and_unpack(guest, req, &archive, bytes);
    let _ = guest.run(Step::Remove, &[&archive]);
    result
}

fn pack_and_unpack(
    guest: &dyn Guest,
    req: &Request<'_>,
    archive: &str,
    bytes: &AtomicU64,
) -> Result<Unpacked> {
    let max = req.limits.max_bytes;
    let pid = guest.start(Step::Pack, &[req.guest_path, archive])?;
    let started = Instant::now();
    let packed = loop {
        // Every way out of this loop but `tar` ending stops `tar`, so it never writes on, uncapped, after the pull.
        match guest.status(pid) {
            Ok(Some(ran)) => break ran,
            Ok(None) => {}
            Err(e) => {
                guest.kill(pid);
                return Err(e.context("asking the guest whether tar finished"));
            }
        }
        // The archive may not exist yet in the first moments, which reads as empty.
        let size = match guest.size(archive) {
            Ok(size) => size.unwrap_or(0),
            Err(e) => {
                guest.kill(pid);
                return Err(e.context("measuring the archive in the guest while tar built it"));
            }
        };
        if size > max {
            guest.kill(pid);
            bail!(
                "the archive of {} passed the pull limit of {max} bytes (limits.max_pull_bytes) while tar built \
                 it; tar was stopped and the archive deleted",
                req.guest_path
            );
        }
        if started.elapsed() > PACK_TIMEOUT {
            guest.kill(pid);
            bail!(
                "tar did not finish within {} s; it was stopped",
                PACK_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(req.poll);
    };
    let warning = match packed.code {
        Some(0) => None,
        Some(1) if packed.stderr.contains(CHANGED) => Some(packed.stderr.clone()),
        _ => bail!("tar failed in the guest: {}", packed.describe()),
    };
    // A fast `tar` can finish between two polls, so the finished archive is measured too, before any of it is sent.
    let size = guest
        .size(archive)
        .context("measuring the archive in the guest")?
        .ok_or_else(|| anyhow!("cannot measure the archive {archive} in the guest"))?;
    if size > max {
        bail!(
            "the archive of {} is {size} bytes, over the pull limit of {max} bytes (limits.max_pull_bytes); it was \
             deleted",
            req.guest_path
        );
    }
    let staged = req.dest.stage_dir()?;
    let root = staged.dir()?;
    let (reader, writer) = io::pipe()?;
    let (read, unpacked) = std::thread::scope(|s| {
        let unpacker = s.spawn(move || {
            let mut reader = reader;
            let unpacked = unpack(&mut reader, &root, req.limits, req.exclude)?;
            // The archive's padding after its end, so the sender never meets a closed pipe.
            io::copy(&mut reader, &mut io::sink())?;
            Ok(unpacked)
        });
        let mut counted = CountingWriter {
            inner: writer,
            count: bytes,
        };
        let read = guest.read(archive, &mut counted, max);
        // Without a writer, an unpacker still reading meets the end at once instead of blocking.
        drop(counted);
        let unpacked = unpacker
            .join()
            .map_err(|_| anyhow!("unpacking panicked"))
            .and_then(|r: Result<Unpacked>| r);
        (read, unpacked)
    });
    let mut unpacked = match (read, unpacked) {
        (Ok(_), Ok(unpacked)) => unpacked,
        (Ok(_), Err(e)) => return Err(e),
        // A stream cut short can still end at an entry boundary, so a read failure fails the pull even when the
        // unpacker saw a clean end.
        (Err(e), Ok(_)) => return Err(e.context("reading the archive from the guest")),
        (Err(read), Err(unpacking)) if broken_pipe(&read) => return Err(unpacking),
        (Err(read), Err(_)) => return Err(read.context("reading the archive from the guest")),
    };
    staged.commit(req.dest, req.overwrite)?;
    unpacked.warning = warning;
    Ok(unpacked)
}

/// Whether `e` comes from writing to an unpacker that already stopped.
fn broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::process::{Child, Command, Stdio};

    use super::*;
    use crate::pulldir::STAGING;

    /// A temporary directory, removed when dropped.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "virtmanager-mcp-dirpull-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        /// A pull directory with its staging directory.
        fn pull_dir(name: &str) -> Self {
            let dir = Self::new(name);
            fs::create_dir(dir.0.join(STAGING)).unwrap();
            dir
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn staging_is_empty(&self) -> bool {
            fs::read_dir(self.0.join(STAGING)).unwrap().count() == 0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const LIMITS: Limits = Limits {
        max_bytes: 1 << 20,
        max_entries: 1000,
    };

    /// Unpacks `archive` into `out` inside the pull directory `pull`, as a pull does, and commits it.
    fn unpack_to(
        pull: &Temp,
        archive: &[u8],
        limits: &Limits,
        exclude: &[String],
    ) -> Result<Unpacked> {
        let dest = Destination::open(
            std::slice::from_ref(&pull.0),
            pull.0.join("out").to_str().unwrap(),
        )?;
        dest.check(pull::Kind::Directory, false)?;
        let staged = dest.stage_dir()?;
        let unpacked = unpack(
            &mut &archive[..],
            &staged.dir()?,
            limits,
            &excludes(exclude)?,
        )?;
        staged.commit(&dest, false)?;
        Ok(unpacked)
    }

    /// One raw entry: a header with `name` (at most 100 bytes) and `kind`, then `data` padded to a block.
    fn raw(kind: u8, name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(EntryType::new(kind));
        header.set_size(data.len() as u64);
        header.set_mode(0o4755);
        header.set_cksum();
        let mut out = header.as_bytes().to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn pax(records: &[(&str, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        for (key, value) in records {
            let body_len = key.len() + value.len() + 3;
            let mut len = body_len + 1;
            while format!("{len}").len() + body_len != len {
                len += 1;
            }
            data.extend_from_slice(format!("{len} {key}=").as_bytes());
            data.extend_from_slice(value);
            data.push(b'\n');
        }
        data
    }

    fn archive(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut out = entries.concat();
        out.extend_from_slice(&[0; 1024]);
        out
    }

    #[test]
    fn a_tree_arrives_with_fixed_modes_and_without_links_or_special_files() {
        let pull = Temp::pull_dir("arrive");
        let long = format!("d/{}", "n".repeat(150));
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &str, kind: EntryType, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o6777);
            if kind == EntryType::Symlink || kind == EntryType::Link {
                builder
                    .append_link(&mut header, path, "/etc/passwd")
                    .unwrap();
            } else {
                builder.append_data(&mut header, path, data).unwrap();
            }
        };
        add("./", EntryType::Directory, b"");
        add("./d", EntryType::Directory, b"");
        add("./d/f", EntryType::Regular, b"hello");
        add(&long, EntryType::Regular, b"long");
        add("./implicit/g", EntryType::Regular, b"g");
        add("./link", EntryType::Symlink, b"");
        add("./hard", EntryType::Link, b"");
        add("./fifo", EntryType::Fifo, b"");
        add("./dev", EntryType::Char, b"");
        let data = builder.into_inner().unwrap();

        let unpacked = unpack_to(&pull, &data, &LIMITS, &[]).unwrap();
        assert_eq!(
            (unpacked.files, unpacked.directories, unpacked.bytes),
            (3, 1, 10)
        );
        let skipped: Vec<_> = unpacked
            .skipped
            .iter()
            .map(|s| format!("{} {}", s.path, s.reason))
            .collect();
        assert_eq!(
            skipped,
            [
                "link symlink",
                "hard hard link",
                "fifo FIFO",
                "dev device file"
            ]
        );
        let out = pull.0.join("out");
        assert_eq!(fs::read(out.join("d/f")).unwrap(), b"hello");
        assert_eq!(fs::read(out.join(&long)).unwrap(), b"long");
        for (rel, mode) in [
            ("", 0o755),
            ("d", 0o755),
            ("implicit", 0o755),
            ("d/f", 0o644),
            ("implicit/g", 0o644),
        ] {
            let mode_found = fs::symlink_metadata(out.join(rel)).unwrap().mode() & 0o7777;
            assert_eq!(mode_found, mode, "{rel}");
        }
        assert_eq!(
            fs::read_dir(&out).unwrap().count(),
            2,
            "only d and implicit"
        );
        assert!(pull.staging_is_empty());
    }

    #[test]
    fn a_hostile_archive_fails_the_pull_and_writes_nothing() {
        let file = |name: &[u8]| raw(b'0', name, b"x");
        let dir = |name: &[u8]| raw(b'5', name, b"");
        let cases: Vec<(&str, Vec<Vec<u8>>, &str)> = vec![
            (
                "absolute",
                vec![file(b"ok"), file(b"/etc/evil")],
                "absolute",
            ),
            ("dotdot", vec![file(b"a/../../evil")], "'..'"),
            ("twice", vec![file(b"a"), file(b"./a")], "twice"),
            ("file-then-dir", vec![file(b"a"), dir(b"a/")], "twice"),
            (
                "file-then-child",
                vec![file(b"a"), file(b"a/b")],
                "share this path",
            ),
            (
                "dir-then-file",
                vec![file(b"a/b"), file(b"a")],
                "share this path",
            ),
            ("gnu-sparse", vec![sparse_tib()], "GNU sparse"),
            (
                "too-deep",
                vec![
                    raw(b'L', b"././@LongLink", &"d/".repeat(300).into_bytes()),
                    file(b"x"),
                ],
                "deeper than 256 levels",
            ),
            (
                "too-long",
                vec![
                    raw(b'L', b"././@LongLink", &"n".repeat(5000).into_bytes()),
                    file(b"x"),
                ],
                "4096-byte limit",
            ),
            (
                "pax-sparse",
                vec![
                    raw(b'x', b"h", &pax(&[("GNU.sparse.size", b"1099511627776")])),
                    file(b"big"),
                ],
                "PAX sparse",
            ),
            (
                "pax-size",
                vec![raw(b'x', b"h", &pax(&[("size", b"1")])), file(b"f")],
                "PAX size",
            ),
            (
                "pax-global",
                vec![raw(b'g', b"h", &pax(&[("comment", b"x")])), file(b"f")],
                "global",
            ),
            (
                "long-name",
                vec![raw(b'L', b"././@LongLink", &vec![b'a'; 70_000])],
                "byte name",
            ),
            (
                "dangling-name",
                vec![raw(b'L', b"././@LongLink", b"name\0")],
                "names no entry",
            ),
            ("root-file", vec![file(b"./")], "archive's root"),
            ("volume", vec![raw(b'V', b"label", b"")], "unsupported"),
        ];
        for (name, entries, why) in cases {
            let pull = Temp::pull_dir(name);
            let err = format!(
                "{:#}",
                unpack_to(&pull, &archive(&entries), &LIMITS, &[]).unwrap_err()
            );
            assert!(err.contains(why), "{name}: {err}");
            assert!(!pull.0.join("out").exists(), "{name}");
            assert!(pull.staging_is_empty(), "{name}");
        }
    }

    #[test]
    fn a_pax_path_record_names_the_next_entry() {
        let pull = Temp::pull_dir("pax-path");
        let data = archive(&[
            raw(
                b'x',
                b"h",
                &pax(&[("path", b"named/by-pax"), ("mtime", b"1")]),
            ),
            raw(b'0', b"ignored-name", b"pax"),
        ]);
        unpack_to(&pull, &data, &LIMITS, &[]).unwrap();
        assert_eq!(fs::read(pull.0.join("out/named/by-pax")).unwrap(), b"pax");
        assert!(!pull.0.join("out/ignored-name").exists());
    }

    #[test]
    fn the_caps_fail_the_pull_before_writing_past_them() {
        let pull = Temp::pull_dir("entries");
        let entries: Vec<_> = (0..4)
            .map(|i| raw(b'0', format!("f{i}").as_bytes(), b""))
            .collect();
        let tight = Limits {
            max_bytes: 1 << 20,
            max_entries: 3,
        };
        let err = unpack_to(&pull, &archive(&entries), &tight, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_pull_entries"), "{err}");
        assert!(pull.staging_is_empty());

        let pull = Temp::pull_dir("bytes");
        let data = archive(&[raw(b'0', b"a", b"123456"), raw(b'0', b"b", b"123456")]);
        let small = Limits {
            max_bytes: 10,
            max_entries: 1000,
        };
        let err = format!("{:#}", unpack_to(&pull, &data, &small, &[]).unwrap_err());
        assert!(err.contains("max_pull_bytes"), "{err}");
        assert!(!pull.0.join("out").exists());
        assert!(pull.staging_is_empty());

        // One entry, but three directories on its way: four against a cap of three.
        let pull = Temp::pull_dir("implied");
        let data = archive(&[raw(b'0', b"a/b/c/f", b"")]);
        let err = format!("{:#}", unpack_to(&pull, &data, &tight, &[]).unwrap_err());
        assert!(err.contains("directories its paths imply"), "{err}");
        assert!(pull.staging_is_empty());
    }

    /// A GNU sparse entry that declares a 1 TiB file and carries no data.
    fn sparse_tib() -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.set_path("big").unwrap();
        header.set_entry_type(EntryType::GNUSparse);
        header.set_size(0);
        header.as_gnu_mut().unwrap().set_real_size(1 << 40);
        header.set_cksum();
        header.as_bytes().to_vec()
    }

    #[test]
    fn a_deep_tree_that_then_fails_still_leaves_staging_empty() {
        let pull = Temp::pull_dir("deep");
        let deep = format!("{}f", "d/".repeat(250));
        let data = archive(&[
            raw(b'L', b"././@LongLink", deep.as_bytes()),
            raw(b'0', b"x", b"deep"),
            raw(b'0', b"../escape", b""),
        ]);
        let err = unpack_to(&pull, &data, &LIMITS, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("'..'"), "{err}");
        assert!(!pull.0.join("out").exists());
        assert!(pull.staging_is_empty());
    }

    #[test]
    fn exclude_patterns_match_as_in_gitignore() {
        let pull = Temp::pull_dir("exclude");
        let data = archive(&[
            raw(b'5', b"./build/", b""),
            raw(b'0', b"./build/out.o", b"x"),
            raw(b'0', b"./src/build/keep.rs", b"x"),
            raw(b'0', b"./src/main.rs", b"x"),
            raw(b'0', b"./run.log", b"x"),
            raw(b'0', b"./src/deep.log", b"x"),
        ]);
        let unpacked =
            unpack_to(&pull, &data, &LIMITS, &["/build/".into(), "*.log".into()]).unwrap();
        assert_eq!(unpacked.excluded, 4);
        let out = pull.0.join("out");
        assert!(out.join("src/main.rs").exists());
        assert!(
            out.join("src/build/keep.rs").exists(),
            "a rooted pattern matches at the top only"
        );
        assert!(!out.join("build").exists());
        assert!(!out.join("src/deep.log").exists());
        assert!(excludes(&["a{b".into()]).is_err());
    }

    /// A guest that runs each script with the host's `sh` and `tar`, with `PATH` replaced when `path_env` is set. With
    /// `cut_read`, reading the archive fails after that many bytes; with `fail_status`, every status query fails.
    struct LocalGuest {
        path_env: Option<String>,
        cut_read: Option<u64>,
        fail_status: bool,
        children: RefCell<Vec<Child>>,
    }

    impl LocalGuest {
        fn new(path_env: Option<String>) -> Self {
            Self {
                path_env,
                cut_read: None,
                fail_status: false,
                children: RefCell::new(Vec::new()),
            }
        }

        fn all_ended(&self) -> bool {
            self.children
                .borrow_mut()
                .iter_mut()
                .all(|c| c.try_wait().unwrap().is_some())
        }

        fn command(&self, script: &str, args: &[&str]) -> Command {
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c").arg(script).arg("sh").args(args);
            if let Some(path) = &self.path_env {
                cmd.env("PATH", path);
            }
            cmd
        }
    }

    impl Guest for LocalGuest {
        fn run(&self, step: Step, args: &[&str]) -> Result<Ran> {
            let out = self.command(linux_script(step), args).output()?;
            Ok(Ran {
                code: out.status.code().map(i64::from),
                stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }

        fn start(&self, step: Step, args: &[&str]) -> Result<i64> {
            let child = self
                .command(linux_script(step), args)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            let pid = i64::from(child.id());
            self.children.borrow_mut().push(child);
            Ok(pid)
        }

        fn status(&self, pid: i64) -> Result<Option<Ran>> {
            if self.fail_status {
                bail!("scripted status failure");
            }
            let mut children = self.children.borrow_mut();
            let child = children
                .iter_mut()
                .find(|c| i64::from(c.id()) == pid)
                .unwrap();
            let Some(status) = child.try_wait()? else {
                return Ok(None);
            };
            let mut stderr = String::new();
            child.stderr.take().unwrap().read_to_string(&mut stderr)?;
            Ok(Some(Ran {
                code: status.code().map(i64::from),
                stdout: String::new(),
                stderr: stderr.trim().to_string(),
            }))
        }

        fn size(&self, path: &str) -> Result<Option<u64>> {
            match fs::metadata(path) {
                Ok(meta) => Ok(Some(meta.len())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        }

        fn kill(&self, pid: i64) {
            let mut children = self.children.borrow_mut();
            if let Some(child) = children.iter_mut().find(|c| i64::from(c.id()) == pid) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        fn read(&self, path: &str, out: &mut dyn Write, max_bytes: u64) -> Result<u64> {
            let mut file = File::open(path)?;
            if let Some(cut) = self.cut_read {
                io::copy(&mut (&mut file).take(cut), out)?;
                bail!("scripted read failure after {cut} bytes");
            }
            let copied = io::copy(&mut (&mut file).take(max_bytes + 1), out)?;
            if copied > max_bytes {
                bail!("{path} is larger than the pull limit (limits.max_pull_bytes)");
            }
            Ok(copied)
        }
    }

    /// A guest tree to pull, a guest temporary directory, and a host pull directory.
    struct Bed {
        guest: Temp,
        tmp: Temp,
        pull: Temp,
    }

    impl Bed {
        fn new(name: &str) -> Self {
            let guest = Temp::new(&format!("{name}-guest"));
            guest.write("tree/a.txt", "alpha");
            guest.write("tree/sub/b.txt", "beta");
            symlink("/etc/passwd", guest.0.join("tree/sub/link")).unwrap();
            Self {
                guest,
                tmp: Temp::new(&format!("{name}-tmp")),
                pull: Temp::pull_dir(&format!("{name}-pull")),
            }
        }

        fn pull(&self, guest: &LocalGuest, overwrite: bool, limits: &Limits) -> Result<Unpacked> {
            let dest = Destination::open(
                std::slice::from_ref(&self.pull.0),
                self.pull.0.join("out").to_str().unwrap(),
            )?;
            dest.check(pull::Kind::Directory, overwrite)?;
            let missing = self.tmp.0.join("missing");
            let tree = self.guest.0.join("tree");
            let exclude = excludes(&[])?;
            let req = Request {
                guest_path: tree.to_str().unwrap(),
                dest: &dest,
                overwrite,
                limits,
                exclude: &exclude,
                temp_dirs: [missing.to_str().unwrap(), self.tmp.0.to_str().unwrap()],
                poll: Duration::from_millis(20),
            };
            pull(guest, &req, &AtomicU64::new(0))
        }

        fn tmp_is_empty(&self) -> bool {
            fs::read_dir(&self.tmp.0).unwrap().count() == 0
        }
    }

    /// A `PATH` whose `tar` is the shell script `body`, which may call the real `tar` as `$REAL`.
    fn fake_tar(dir: &Path, body: &str) -> String {
        let real = Command::new("/bin/sh")
            .args(["-c", "command -v tar"])
            .output()
            .unwrap();
        let real = String::from_utf8(real.stdout).unwrap();
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("tar"),
            format!("#!/bin/sh\nREAL={}\n{body}\n", real.trim()),
        )
        .unwrap();
        fs::set_permissions(bin.join("tar"), fs::Permissions::from_mode(0o755)).unwrap();
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
    }

    #[test]
    fn a_guest_tree_arrives_and_the_guest_archive_is_deleted() {
        let bed = Bed::new("whole");
        let unpacked = bed.pull(&LocalGuest::new(None), false, &LIMITS).unwrap();
        assert_eq!((unpacked.files, unpacked.directories), (2, 1));
        assert_eq!(unpacked.skipped[0].path, "sub/link");
        assert_eq!(fs::read(bed.pull.0.join("out/sub/b.txt")).unwrap(), b"beta");
        assert!(
            bed.tmp_is_empty(),
            "the second temporary directory got the archive and lost it"
        );
        assert!(bed.pull.staging_is_empty());

        let err = bed
            .pull(&LocalGuest::new(None), false, &LIMITS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pass overwrite: true"), "{err}");
        fs::write(bed.guest.0.join("tree/a.txt"), "changed").unwrap();
        fs::remove_file(bed.guest.0.join("tree/sub/b.txt")).unwrap();
        bed.pull(&LocalGuest::new(None), true, &LIMITS).unwrap();
        assert_eq!(fs::read(bed.pull.0.join("out/a.txt")).unwrap(), b"changed");
        assert!(
            !bed.pull.0.join("out/sub/b.txt").exists(),
            "overwrite replaces the whole tree"
        );
        assert!(bed.pull.staging_is_empty());
    }

    #[test]
    fn a_file_at_the_destination_or_a_guest_path_that_is_no_directory_is_refused() {
        let bed = Bed::new("refuse");
        bed.pull.write("out", "mine");
        let err = bed
            .pull(&LocalGuest::new(None), true, &LIMITS)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is a file; a directory pull replaces only a directory"),
            "{err}"
        );
        assert_eq!(fs::read(bed.pull.0.join("out")).unwrap(), b"mine");

        let bed = Bed::new("not-dir");
        fs::remove_dir_all(bed.guest.0.join("tree")).unwrap();
        bed.guest.write("tree", "a file");
        let err = bed
            .pull(&LocalGuest::new(None), false, &LIMITS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a directory in the guest"), "{err}");

        let err = bed
            .pull(
                &LocalGuest::new(Some("/nonexistent".into())),
                false,
                &LIMITS,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no tar"), "{err}");
    }

    #[test]
    fn tar_exit_status_1_for_a_changed_file_warns_and_anything_else_fails() {
        let bed = Bed::new("changed");
        let tools = Temp::new("changed-tools");
        let changed = fake_tar(
            &tools.0,
            "\"$REAL\" \"$@\" || exit 2\necho 'tar: ./a.txt: file changed as we read it' >&2\nexit 1",
        );
        let unpacked = bed
            .pull(&LocalGuest::new(Some(changed)), false, &LIMITS)
            .unwrap();
        assert!(
            unpacked
                .warning
                .unwrap()
                .contains("file changed as we read it")
        );
        assert!(bed.pull.0.join("out/a.txt").exists());

        for (name, body) in [
            (
                "other-1",
                "\"$REAL\" \"$@\"\necho 'tar: ./x: Cannot open: Permission denied' >&2\nexit 1",
            ),
            ("status-2", "\"$REAL\" \"$@\"\nexit 2"),
        ] {
            let bed = Bed::new(name);
            let tools = Temp::new(&format!("{name}-tools"));
            let guest = LocalGuest::new(Some(fake_tar(&tools.0, body)));
            let err = bed.pull(&guest, false, &LIMITS).unwrap_err().to_string();
            assert!(err.contains("tar failed in the guest"), "{name}: {err}");
            assert!(!bed.pull.0.join("out").exists(), "{name}");
            assert!(bed.tmp_is_empty(), "{name}");
            assert!(bed.pull.staging_is_empty(), "{name}");
        }
    }

    #[test]
    fn a_growing_archive_is_stopped_once_it_passes_the_cap() {
        let bed = Bed::new("growing");
        let tools = Temp::new("growing-tools");
        // `tar -c -f ARCHIVE -C DIR .` puts the archive in $3; this one never stops growing.
        let endless = fake_tar(
            &tools.0,
            "while :; do head -c 65536 /dev/zero >> \"$3\"; done",
        );
        let small = Limits {
            max_bytes: 256 * 1024,
            max_entries: 1000,
        };
        let started = Instant::now();
        let err = bed
            .pull(&LocalGuest::new(Some(endless)), false, &small)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("max_pull_bytes") && err.contains("tar was stopped"),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(bed.tmp_is_empty());
        assert!(!bed.pull.0.join("out").exists());
        assert!(bed.pull.staging_is_empty());
    }

    #[test]
    fn an_archive_that_finished_over_the_cap_is_never_sent() {
        let bed = Bed::new("finished-big");
        bed.guest.write("tree/big", &"x".repeat(300 * 1024));
        let small = Limits {
            max_bytes: 256 * 1024,
            max_entries: 1000,
        };
        let err = bed
            .pull(&LocalGuest::new(None), false, &small)
            .unwrap_err()
            .to_string();
        assert!(err.contains("over the pull limit"), "{err}");
        assert!(bed.tmp_is_empty());
        assert!(bed.pull.staging_is_empty());
    }

    #[test]
    fn a_read_cut_at_an_entry_boundary_fails_the_pull() {
        let bed = Bed::new("cut");
        let guest = LocalGuest {
            // GNU tar's first entry is the root directory, a header alone: the unpacker sees a clean end here.
            cut_read: Some(512),
            ..LocalGuest::new(None)
        };
        let err = format!("{:#}", bed.pull(&guest, false, &LIMITS).unwrap_err());
        assert!(err.contains("reading the archive from the guest"), "{err}");
        assert!(!bed.pull.0.join("out").exists());
        assert!(bed.pull.staging_is_empty());
        assert!(bed.tmp_is_empty());
    }

    #[test]
    fn a_failed_status_query_stops_tar() {
        let bed = Bed::new("status");
        let tools = Temp::new("status-tools");
        let guest = LocalGuest {
            fail_status: true,
            ..LocalGuest::new(Some(fake_tar(&tools.0, "exec sleep 60")))
        };
        let err = format!("{:#}", bed.pull(&guest, false, &LIMITS).unwrap_err());
        assert!(err.contains("whether tar finished"), "{err}");
        assert!(guest.all_ended(), "tar still runs");
        assert!(bed.tmp_is_empty());
    }
}
