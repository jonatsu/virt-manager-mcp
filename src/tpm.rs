//! TPM state in the pool: `<vm>.tpm`, a file volume in `disk_pool` that the server makes with `swtpm_setup`, owns by the
//! host's swtpm user and deletes with the VM (`docs/plans/custom-templates-design.md`, "TPM state").
//!
//! libvirt starts swtpm as the swtpm user, which cannot create a file in the root-owned pool directory, and libvirt
//! does not change the owner of a pool file. The volume therefore exists, owned by that user and holding valid state,
//! before the VM's first start (`docs/research/2026-11-24-custom-templates-probes.md`, "TPM state in the pool").

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustix::process::{Pid, Signal, kill_process_group};
use virt::connect::Connect;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;

use crate::manage::{esc, upload};
use crate::volnames;

/// The first libvirt release whose `<tpm>` takes a file source and whose volumes take `templateFormat`, as
/// `Connect::get_lib_version` reads it: 10.10.0.
pub const MIN_LIBVIRT: u32 = 10_010_000;

/// The AppArmor file that holds the rule a start needs, which a refusal names.
const APPARMOR_LOCAL: &str = "/etc/apparmor.d/local/usr.bin.swtpm";

/// The user and group libvirt runs swtpm as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwtpmIds {
    pub uid: u32,
    pub gid: u32,
}

/// How long `swtpm_setup` may take. It makes keys and a few kilobytes of state, so a run that lasts this long hangs.
const SETUP_DEADLINE: Duration = Duration::from_secs(60);

/// The most state `generate_state` accepts. Fresh state is about 4 KiB, and a larger file is not what was asked for.
const MAX_STATE_BYTES: u64 = 1 << 20;

/// How a bounded child ended.
#[derive(Debug)]
pub enum Outcome {
    Exited(ExitStatus),
    TimedOut,
}

/// Reads the uid and gid of the swtpm user from the `getent passwd swtpm` output `line`.
fn parse_passwd(line: &str) -> Result<SwtpmIds> {
    let line = line.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        bail!(
            "the host has no user swtpm; install swtpm-tools, which creates it, to use tpm: true"
        );
    }
    let ids = || -> Option<SwtpmIds> {
        let mut fields = line.split(':');
        let (name, _password, uid, gid) = (
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        );
        (name == "swtpm").then_some(())?;
        Some(SwtpmIds {
            uid: uid.parse().ok()?,
            gid: gid.parse().ok()?,
        })
    };
    ids().ok_or_else(|| anyhow!("cannot read the uid and gid of the user swtpm from {line:?}"))
}

/// The swtpm user's uid and gid, from `getent passwd swtpm`, so that neither is hard-coded.
///
/// # Errors
///
/// Fails when `getent` cannot run, or the host has no user `swtpm`.
pub fn swtpm_ids() -> Result<SwtpmIds> {
    let out = Command::new("getent")
        .args(["passwd", "swtpm"])
        .stdin(Stdio::null())
        .output()
        .context("looking up the user swtpm with getent")?;
    // getent exits 2 for a name it does not find.
    if !out.status.success() && out.status.code() != Some(2) {
        bail!(
            "looking up the user swtpm with getent failed: {}",
            out.status
        );
    }
    parse_passwd(&String::from_utf8_lossy(&out.stdout))
}

/// Refuses a libvirt older than 10.10, which cannot give a `<tpm>` a file source.
///
/// # Errors
///
/// Fails when `version`, as `Connect::get_lib_version` reads it, is below 10.10.0.
pub fn check_libvirt(version: u32) -> Result<()> {
    if version < MIN_LIBVIRT {
        bail!(
            "tpm: true needs libvirt 10.10 or later, for a TPM state file in the pool; this host runs {}.{}.{}",
            version / 1_000_000,
            version / 1000 % 1000,
            version % 1000
        );
    }
    Ok(())
}

/// The `swtpm_setup` arguments that make fresh state in `state`, an empty file.
fn setup_args(state: &Path) -> Vec<OsString> {
    let mut url = OsString::from("file://");
    url.push(state);
    let mut args: Vec<OsString> = ["--tpm2", "--tpmstate"].map(OsString::from).to_vec();
    args.push(url);
    args.extend(
        [
            "--createek",
            "--lock-nvram",
            "--not-overwrite",
            "--pcr-banks",
            "sha256",
            "--config",
            "/etc/swtpm_setup.conf",
        ]
        .map(OsString::from),
    );
    args
}

/// What a failed start of `swtpm_setup` means for the person who runs the server.
fn setup_spawn_error(e: &io::Error) -> anyhow::Error {
    match e.kind() {
        io::ErrorKind::NotFound => {
            anyhow!(
                "swtpm_setup is not installed; tpm: true needs the swtpm-tools package on the host"
            )
        }
        _ => anyhow!("cannot run swtpm_setup: {e}"),
    }
}

/// The volume XML of `<vm>.tpm`: raw, owned by the swtpm user, mode 0600, `capacity` bytes.
fn volume_xml(name: &str, capacity: u64, ids: SwtpmIds) -> String {
    format!(
        "<volume><name>{}</name><capacity unit='bytes'>{capacity}</capacity><target><format type='raw'/>\
         <permissions><owner>{}</owner><group>{}</group><mode>0600</mode></permissions></target></volume>",
        esc(name),
        ids.uid,
        ids.gid
    )
}

/// The `<tpm>` device that keeps its state in the file `path`.
pub fn device_xml(path: &str) -> String {
    format!(
        "<tpm model='tpm-crb'><backend type='emulator' version='2.0'><source type='file' path='{}'/></backend></tpm>",
        esc(path)
    )
}

/// The refusal for a failed start, when the VM's inactive definition `xml` keeps its TPM state in a `.tpm` file and
/// libvirt's `message` shows that `swtpm_setup` failed.
pub fn start_refusal(xml: &str, message: &str) -> Option<String> {
    if !message.contains("swtpm_setup") {
        return None;
    }
    let doc = roxmltree::Document::parse(xml).ok()?;
    let path = doc
        .descendants()
        .filter(|n| n.has_tag_name("tpm"))
        .flat_map(|tpm| tpm.children().filter(|n| n.has_tag_name("backend")))
        .flat_map(|backend| backend.children().filter(|n| n.has_tag_name("source")))
        .filter(|source| source.attribute("type") == Some("file"))
        .filter_map(|source| source.attribute("path"))
        .find(|path| path.ends_with(".tpm"))?;
    let dir = Path::new(path).parent()?.display();
    Some(format!(
        "libvirt could not run swtpm_setup for the TPM state file {path}. The first thing to check is the host's \
         AppArmor rule: {APPARMOR_LOCAL} needs the line `owner {dir}/*.tpm rwk,`, and the swtpm profile must be \
         reloaded after it changes. A state volume that the swtpm user does not own fails the same way. libvirt said: \
         {message}"
    ))
}

/// `~/.cache/virtmanager-mcp`, where the scratch directories go.
fn scratch_parent(home: Option<OsString>) -> Result<PathBuf> {
    let home = home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            anyhow!(
                "cannot place the scratch directory of swtpm_setup: HOME is not an absolute path"
            )
        })?;
    Ok(home.join(".cache").join("virtmanager-mcp"))
}

/// Makes fresh TPM 2.0 state with `swtpm_setup`, run as the server's own user in a mode 0700 directory under
/// `~/.cache/virtmanager-mcp/`, which is removed afterwards. The state has no endorsement-key certificate.
///
/// # Errors
///
/// Fails when `swtpm_setup` is missing, fails or outlasts its deadline, or leaves no state or too much of it.
pub fn generate_state() -> Result<Vec<u8>> {
    let dir = ScratchDir::new_in(&scratch_parent(std::env::var_os("HOME"))?)?;
    let state = dir.path().join("state.tpm");
    // swtpm_setup fills an empty file, which it unlinks and makes again.
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&state)
        .with_context(|| format!("creating {}", state.display()))?;
    let log = dir.path().join("setup.log");
    let outcome = run_logged(
        Command::new("swtpm_setup").args(setup_args(&state)),
        &log,
        SETUP_DEADLINE,
    )
    .map_err(|e| setup_spawn_error(&e))?;
    match outcome {
        Outcome::Exited(status) if status.success() => {}
        Outcome::Exited(status) => bail!("swtpm_setup failed ({status}): {}", log_tail(&log)),
        Outcome::TimedOut => bail!(
            "swtpm_setup did not finish within {} s and was stopped: {}",
            SETUP_DEADLINE.as_secs(),
            log_tail(&log)
        ),
    }
    let size = fs::metadata(&state)
        .with_context(|| format!("reading the state swtpm_setup made in {}", state.display()))?
        .len();
    if size == 0 || size > MAX_STATE_BYTES {
        bail!("swtpm_setup left {size} bytes of state; expected a few kilobytes");
    }
    fs::read(&state).with_context(|| format!("reading {}", state.display()))
}

/// The last lines of `log`, for an error message.
fn log_tail(log: &Path) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(3).collect();
    let tail = lines.into_iter().rev().collect::<Vec<_>>().join(" / ");
    if tail.is_empty() {
        "no output".into()
    } else {
        tail
    }
}

/// Makes the volume `<vm>.tpm` in `pool` holding `state`, owned by the swtpm user `ids` with mode 0600 and a capacity
/// equal to the state's size.
///
/// # Errors
///
/// Fails when the volume exists, or libvirt cannot create or fill it; a volume it made is deleted again.
pub fn create_volume(
    conn: &Connect,
    pool: &StoragePool,
    vm: &str,
    state: &[u8],
    ids: SwtpmIds,
) -> Result<StorageVol> {
    let name = volnames::tpm_name(vm);
    if StorageVol::lookup_by_name(pool, &name).is_ok() {
        bail!("volume '{name}' already exists in the disk pool");
    }
    let xml = volume_xml(&name, state.len() as u64, ids);
    let vol = StorageVol::create_xml(pool, &xml, 0)
        .map_err(|e| anyhow!("cannot create volume '{name}': {}", e.message()))?;
    if let Err(e) = upload(conn, &vol, state) {
        let _ = vol.delete(0);
        return Err(e.context(format!("cannot upload volume '{name}'")));
    }
    Ok(vol)
}

/// A private directory for one run of `swtpm_setup`, removed when it drops.
struct ScratchDir(PathBuf);

impl ScratchDir {
    /// Makes a mode 0700 directory, and `parent` with it when missing.
    fn new_in(parent: &Path) -> Result<Self> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let dir = parent.join(format!("tpm-{}", uuid::Uuid::new_v4()));
        DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Nothing useful can be done when the removal fails; the state in it is a throwaway key set.
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `command` with its output in `log`, in its own process group, and waits up to `deadline`. A child that
/// outlasts it, with whatever it started, is killed and reaped.
fn run_logged(command: &mut Command, log: &Path, deadline: Duration) -> io::Result<Outcome> {
    let out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(log)?;
    let err = out.try_clone()?;
    // The child leads its own process group, so a timeout reaches what it started too.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()?;
    let end = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Outcome::Exited(status)),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => break,
            Err(e) => {
                stop(&mut child);
                return Err(e);
            }
        }
    }
    stop(&mut child);
    Ok(Outcome::TimedOut)
}

/// Kills the child's process group and reaps the child.
fn stop(child: &mut std::process::Child) {
    // The group exists while the child, a zombie until the wait below, is in it.
    let _ = kill_process_group(Pid::from_child(child), Signal::KILL);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str =
        "swtpm:x:114:117:Account used for swtpm:/var/lib/swtpm:/usr/sbin/nologin\n";

    #[test]
    fn the_swtpm_ids_come_from_the_passwd_line() {
        assert_eq!(
            parse_passwd(PASSWD).unwrap(),
            SwtpmIds { uid: 114, gid: 117 }
        );
        // The ids differ between hosts, and nothing in the parse assumes these.
        assert_eq!(
            parse_passwd("swtpm:x:998:996::/:/bin/false").unwrap(),
            SwtpmIds { uid: 998, gid: 996 }
        );
    }

    #[test]
    fn a_missing_swtpm_user_is_refused_naming_the_user_and_the_package() {
        let err = parse_passwd("").unwrap_err().to_string();
        assert!(err.contains("no user swtpm"), "{err}");
        assert!(err.contains("swtpm-tools"), "{err}");
    }

    #[test]
    fn a_malformed_passwd_line_is_refused() {
        for line in [
            "swtpm:x:abc:117:x:/:/bin/false",
            "swtpm:x:114:-1:x:/:/bin/false",
            "swtpm:x:114",
            "swtpm::::",
            "other:x:114:117:x:/:/bin/false",
        ] {
            let err = parse_passwd(line).unwrap_err().to_string();
            assert!(err.contains("swtpm"), "{line}: {err}");
        }
    }

    #[test]
    fn libvirt_before_10_10_is_refused_and_the_floor_passes() {
        let err = check_libvirt(10_009_999).unwrap_err().to_string();
        assert!(err.contains("10.10"), "{err}");
        assert!(err.contains("10.9.999"), "{err}");
        check_libvirt(MIN_LIBVIRT).unwrap();
        check_libvirt(12_000_000).unwrap();
        assert_eq!(MIN_LIBVIRT, 10_010_000);
    }

    #[test]
    fn swtpm_setup_makes_tpm2_state_with_a_fresh_endorsement_key_that_it_does_not_overwrite() {
        let args: Vec<String> = setup_args(Path::new("/h/.cache/virtmanager-mcp/tpm-x/state.tpm"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args.join(" "),
            "--tpm2 --tpmstate file:///h/.cache/virtmanager-mcp/tpm-x/state.tpm --createek --lock-nvram \
             --not-overwrite --pcr-banks sha256 --config /etc/swtpm_setup.conf"
        );
    }

    #[test]
    fn a_missing_swtpm_setup_names_the_package_that_has_it() {
        let err = setup_spawn_error(&io::Error::from(io::ErrorKind::NotFound)).to_string();
        assert!(err.contains("swtpm_setup"), "{err}");
        assert!(err.contains("swtpm-tools"), "{err}");
        let err = setup_spawn_error(&io::Error::from(io::ErrorKind::PermissionDenied)).to_string();
        assert!(err.contains("swtpm_setup"), "{err}");
        assert!(!err.contains("not installed"), "{err}");
    }

    fn parse(xml: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse(xml).unwrap()
    }

    #[test]
    fn the_volume_is_raw_owned_by_the_swtpm_user_and_private_to_it() {
        let xml = volume_xml("t-v.tpm", 4288, SwtpmIds { uid: 114, gid: 117 });
        let doc = parse(&xml);
        let text = |tag: &str| {
            doc.descendants()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(text("name"), "t-v.tpm");
        assert_eq!(text("capacity"), "4288");
        assert_eq!(text("owner"), "114");
        assert_eq!(text("group"), "117");
        assert_eq!(text("mode"), "0600");
        let capacity = doc
            .descendants()
            .find(|n| n.has_tag_name("capacity"))
            .unwrap();
        assert_eq!(capacity.attribute("unit"), Some("bytes"));
        let format = doc
            .descendants()
            .find(|n| n.has_tag_name("format"))
            .unwrap();
        assert_eq!(format.attribute("type"), Some("raw"));
    }

    #[test]
    fn the_device_names_the_state_file_as_a_file_source() {
        let xml = device_xml("/var/lib/libvirt/images/agent-vms/t-v.tpm");
        let doc = parse(&xml);
        let tpm = doc.root_element();
        assert_eq!(tpm.tag_name().name(), "tpm");
        assert_eq!(tpm.attribute("model"), Some("tpm-crb"));
        let backend = tpm.children().find(|n| n.has_tag_name("backend")).unwrap();
        assert_eq!(backend.attribute("type"), Some("emulator"));
        assert_eq!(backend.attribute("version"), Some("2.0"));
        let source = backend
            .children()
            .find(|n| n.has_tag_name("source"))
            .unwrap();
        assert_eq!(source.attribute("type"), Some("file"));
        assert_eq!(
            source.attribute("path"),
            Some("/var/lib/libvirt/images/agent-vms/t-v.tpm")
        );
        // A path with markup stays one attribute.
        let odd = device_xml("/p/a'b\"<c>&.tpm");
        assert_eq!(
            parse(&odd)
                .descendants()
                .find(|n| n.has_tag_name("source"))
                .unwrap()
                .attribute("path"),
            Some("/p/a'b\"<c>&.tpm")
        );
    }

    /// What libvirt 12.0.0 said when `swtpm_setup` could not run for a file-form state volume.
    const SETUP_FAILURE: &str = "internal error: Could not run '/usr/bin/swtpm_setup'. exitstatus: 1; Check error \
                                 log '/var/log/swtpm/libvirt/qemu/t-v-swtpm.log' for details.";

    fn definition(tpm: &str) -> String {
        format!("<domain type='kvm'><name>t-v</name><devices>{tpm}</devices></domain>")
    }

    const FILE_TPM: &str = "<tpm model='tpm-crb'><backend type='emulator' version='2.0'>\
                            <source type='file' path='/var/lib/libvirt/images/agent-vms/t-v.tpm'/></backend></tpm>";

    #[test]
    fn a_failed_swtpm_setup_names_the_apparmor_rule_for_the_pool_directory() {
        let text = start_refusal(&definition(FILE_TPM), SETUP_FAILURE).unwrap();
        assert!(text.contains(APPARMOR_LOCAL), "{text}");
        assert!(
            text.contains("owner /var/lib/libvirt/images/agent-vms/*.tpm rwk,"),
            "{text}"
        );
        assert!(text.contains("t-v.tpm"), "{text}");
        assert!(text.contains("first thing to check"), "{text}");
    }

    #[test]
    fn other_failed_starts_keep_libvirts_own_message() {
        let xml = definition(FILE_TPM);
        // Not a swtpm_setup failure.
        assert_eq!(
            start_refusal(
                &xml,
                "internal error: process exited while connecting to monitor"
            ),
            None
        );
        // No TPM, or libvirt's own state location, or a source that is not a `.tpm` file.
        assert_eq!(start_refusal(&definition(""), SETUP_FAILURE), None);
        assert_eq!(
            start_refusal(
                &definition("<tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>"),
                SETUP_FAILURE
            ),
            None
        );
        assert_eq!(
            start_refusal(
                &definition(
                    "<tpm model='tpm-crb'><backend type='emulator' version='2.0'>\
                     <source type='dir' path='/p/t-v.tpmdir'/></backend></tpm>"
                ),
                SETUP_FAILURE
            ),
            None
        );
        assert_eq!(start_refusal("not xml", SETUP_FAILURE), None);
    }

    #[test]
    fn the_scratch_directories_live_in_the_cache_of_the_home_directory() {
        assert_eq!(
            scratch_parent(Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u/.cache/virtmanager-mcp")
        );
        assert!(scratch_parent(None).is_err());
        assert!(scratch_parent(Some("relative".into())).is_err());
    }

    #[test]
    fn a_scratch_directory_is_private_and_goes_when_it_drops() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("vmm-tpm-scratch-{}", std::process::id()));
        let parent = base.join("cache");
        let kept;
        {
            let dir = ScratchDir::new_in(&parent).unwrap();
            kept = dir.path().to_path_buf();
            assert_eq!(kept.parent(), Some(parent.as_path()));
            assert_eq!(
                std::fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
                0o700
            );
            // Two runs never share a directory.
            assert_ne!(ScratchDir::new_in(&parent).unwrap().path(), kept);
            std::fs::write(kept.join("state.tpm"), b"x").unwrap();
        }
        assert!(!kept.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    fn log_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vmm-tpm-{name}-{}.log", std::process::id()))
    }

    #[test]
    fn a_bounded_run_reports_the_exit_status_and_keeps_both_streams_in_the_log() {
        let log = log_path("exit");
        let out = run_logged(
            &mut sh("echo out; echo err >&2; exit 3"),
            &log,
            Duration::from_secs(10),
        )
        .unwrap();
        let Outcome::Exited(status) = out else {
            panic!("{out:?}")
        };
        assert_eq!(status.code(), Some(3));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("out") && text.contains("err"), "{text}");
        std::fs::remove_file(&log).unwrap();
    }

    #[test]
    fn a_bounded_run_that_outlasts_its_deadline_is_killed_with_what_it_started() {
        let log = log_path("timeout");
        let pidfile = log.with_extension("pid");
        let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let started = std::time::Instant::now();
        let out = run_logged(&mut sh(&script), &log, Duration::from_millis(500)).unwrap();
        assert!(matches!(out, Outcome::TimedOut), "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        // The grandchild is gone, or a zombie its new parent has yet to reap.
        let gone = || match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit(") ")
                .next()
                .is_some_and(|rest| rest.starts_with('Z')),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !gone() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone(), "the grandchild {pid} still runs");
        let _ = std::fs::remove_file(&log);
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn a_program_that_does_not_exist_is_a_spawn_error() {
        let log = log_path("missing");
        let err = run_logged(
            &mut Command::new("/nonexistent/swtpm_setup"),
            &log,
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_file(&log);
    }
}
