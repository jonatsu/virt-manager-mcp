use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Contents of the TOML configuration file. Every field is optional; CLI flags
/// override what is read from here.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// libvirt connection URI.
    pub uri: String,
    /// Names of the VMs the AI may access. `"*"` allows every VM.
    /// An empty list means no VM is accessible.
    pub allowed_vms: Vec<String>,
    /// VMs the AI may start and stop. They must also be in `allowed_vms`;
    /// `"*"` means every allowed VM. Empty by default.
    pub power_vms: Vec<String>,
    /// Host directories whose files `copy_to_vm` may copy into a guest.
    /// Empty by default, which disables the tool.
    pub host_share_dirs: Vec<PathBuf>,
    /// Also lets `copy_to_vm` read from the directory the server was started in.
    /// Claude Code starts stdio servers in the session's project directory.
    pub share_project_dir: bool,
    /// Host directories that pulls from a guest may write into. Empty by default, which hides the pull tools. A
    /// pull directory may not overlap a share directory, an ISO directory or a storage pool.
    pub host_pull_dirs: Vec<PathBuf>,
    /// Disables command execution, file writes, console input, keyboard input,
    /// screenshots and guest info queries.
    pub read_only: bool,
    pub limits: Limits,
    pub http: Http,
    pub manage: Manage,
    pub display: DisplayOptions,
}

/// Graphical display settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayOptions {
    /// Absolute directory holding the `ocrs` models that `scripts/fetch-ocr-models.sh` downloads. Unset turns
    /// screen text recognition off; set, the directory and both model files must exist.
    pub ocr_models: Option<PathBuf>,
}

/// File name of the `ocrs` text detection model in `display.ocr_models`.
pub const OCR_DETECTION_MODEL: &str = "text-detection.onnx";
/// File name of the `ocrs` text recognition model in `display.ocr_models`.
pub const OCR_RECOGNITION_MODEL: &str = "text-recognition.onnx";

/// VM management (create, delete, reconfigure, snapshots). Everything is off by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manage {
    /// Allows `create_vm`.
    pub create: bool,
    /// Allows `delete_vm`.
    pub delete: bool,
    /// Allows changing vCPUs, memory, disks, CD-ROM media and network interfaces.
    pub reconfigure: bool,
    /// Allows creating, listing, reverting and deleting snapshots.
    pub snapshots: bool,
    /// Existing VMs (also in `allowed_vms`) the enabled operations may act on, in
    /// addition to the VMs this server created. `"*"` means every allowed VM.
    pub manage_vms: Vec<String>,
    /// libvirt storage pool where new disk images are created. Required for `create`.
    pub disk_pool: String,
    /// Host directories install / driver ISOs may be used from.
    pub iso_dirs: Vec<PathBuf>,
    /// libvirt networks VMs may be connected to.
    pub networks: Vec<String>,
    /// Max number of VMs created by this server that may exist at once.
    pub max_vms: u32,
    pub max_vcpus: u32,
    pub max_memory_mib: u64,
    /// Max size of a single disk.
    pub max_disk_gib: u64,
    /// Longest lifetime an agent may give a VM made from a template.
    pub max_lifetime_minutes: u32,
    /// Templates VMs can be made from, by name. Empty hides the template tools.
    pub templates: BTreeMap<String, Template>,
}

/// A template volume that disposable VMs get a thin overlay on.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    /// qcow2 volume in `manage.disk_pool`. VMs made from it use it as their backing file.
    pub volume: String,
    #[serde(default)]
    pub os: GuestOs,
    #[serde(default)]
    pub firmware: Firmware,
    #[serde(default)]
    pub description: String,
    /// cloud-config file put into each new VM's seed. It must start with a `#cloud-config` line.
    pub user_data: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestOs {
    #[default]
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    #[default]
    Bios,
    Uefi,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Default wall-clock limit for `exec` (overridable per call).
    pub exec_timeout_secs: u64,
    /// Max bytes returned per stream (stdout / stderr) by `exec` (overridable per call).
    pub max_output_bytes: usize,
    /// Max bytes returned by `read_file` (overridable per call).
    pub max_file_read_bytes: usize,
    /// Timeout for a single guest agent request.
    pub agent_timeout_secs: i32,
    /// Size of the per-VM serial console scrollback buffer.
    pub console_buffer_bytes: usize,
    /// Longest a waiting call may block: `wait_for`, and a paced `console_write`. Several MCP clients cut tool
    /// calls off at 60 s, so the default stays below that.
    pub max_wait_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Http {
    /// Address to listen on in HTTP mode.
    pub bind: String,
    /// Bearer token required in HTTP mode. Can also come from the
    /// `VIRTMANAGER_MCP_TOKEN` environment variable.
    pub token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uri: "qemu:///system".into(),
            allowed_vms: Vec::new(),
            power_vms: Vec::new(),
            host_share_dirs: Vec::new(),
            share_project_dir: false,
            host_pull_dirs: Vec::new(),
            read_only: false,
            limits: Limits::default(),
            http: Http::default(),
            manage: Manage::default(),
            display: DisplayOptions::default(),
        }
    }
}

impl Default for Manage {
    fn default() -> Self {
        Self {
            create: false,
            delete: false,
            reconfigure: false,
            snapshots: false,
            manage_vms: Vec::new(),
            disk_pool: String::new(),
            iso_dirs: Vec::new(),
            networks: Vec::new(),
            max_vms: 2,
            max_vcpus: 2,
            max_memory_mib: 4096,
            max_disk_gib: 32,
            max_lifetime_minutes: 1440,
            templates: BTreeMap::new(),
        }
    }
}

impl Manage {
    /// Whether any management operation is enabled.
    pub fn enabled(&self) -> bool {
        self.create || self.delete || self.reconfigure || self.snapshots
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            exec_timeout_secs: 60,
            max_output_bytes: 1024 * 1024,
            max_file_read_bytes: 10 * 1024 * 1024,
            agent_timeout_secs: 10,
            console_buffer_bytes: 256 * 1024,
            max_wait_secs: 50,
        }
    }
}

impl Default for Http {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8765".into(),
            token: None,
        }
    }
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("virtmanager-mcp").join("config.toml"))
    }

    /// Loads `path`. A missing file is only an error when the path was given explicitly.
    pub fn load(path: &Path, explicit: bool) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                tracing::warn!("config file {} not found, using defaults", path.display());
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Checks limits, resolves `host_share_dirs` to canonical paths and, with
    /// `share_project_dir`, adds the working directory to them.
    pub fn validate(&mut self) -> Result<()> {
        if self.limits.agent_timeout_secs <= 0 {
            bail!("limits.agent_timeout_secs must be positive");
        }
        if self.limits.console_buffer_bytes == 0 {
            bail!("limits.console_buffer_bytes must be positive");
        }
        if self.limits.max_wait_secs == 0 {
            bail!("limits.max_wait_secs must be positive");
        }
        canonicalize_dirs(&mut self.host_share_dirs, "host_share_dirs")?;
        canonicalize_dirs(&mut self.manage.iso_dirs, "manage.iso_dirs")?;
        if let Some(dir) = &mut self.display.ocr_models {
            *dir = ocr_models_dir(dir)?;
        }
        let m = &self.manage;
        if m.create && m.disk_pool.is_empty() {
            bail!("manage.create needs manage.disk_pool");
        }
        if m.max_vcpus == 0 || m.max_memory_mib == 0 || m.max_disk_gib == 0 {
            bail!("manage.max_vcpus, max_memory_mib and max_disk_gib must be positive");
        }
        if m.max_lifetime_minutes == 0 {
            bail!("manage.max_lifetime_minutes must be positive");
        }
        for (name, t) in &m.templates {
            if !is_hostname(name) {
                bail!(
                    "manage.templates.{name}: a template name uses lowercase letters, digits and \
                     hyphens, starts with a letter and has at most 63 characters"
                );
            }
            if t.volume.is_empty() || t.volume.contains('/') || t.volume.starts_with('.') {
                bail!(
                    "manage.templates.{name}.volume must name a volume in manage.disk_pool, not a path"
                );
            }
        }
        if self.share_project_dir {
            let cwd = std::env::current_dir()?.canonicalize()?;
            let home = std::env::var_os("HOME").and_then(|h| PathBuf::from(h).canonicalize().ok());
            match project_dir(cwd, home.as_deref()) {
                Ok(dir) => self.host_share_dirs.push(dir),
                Err(e) => tracing::warn!("share_project_dir: {e:#}"),
            }
        }
        canonicalize_dirs(&mut self.host_pull_dirs, "host_pull_dirs")?;
        refuse_overlaps(
            &self.host_pull_dirs,
            &[
                (
                    "host_share_dirs (or the shared project directory)",
                    &self.host_share_dirs,
                ),
                ("manage.iso_dirs", &self.manage.iso_dirs),
            ],
        )?;
        Ok(())
    }

    pub fn is_allowed(&self, vm: &str) -> bool {
        self.allowed_vms.iter().any(|a| a == "*" || a == vm)
    }

    pub fn can_power(&self, vm: &str) -> bool {
        self.is_allowed(vm) && self.power_vms.iter().any(|a| a == "*" || a == vm)
    }

    /// Whether `vm` is listed in `manage.manage_vms` (VMs created by this server are
    /// recognised separately, from their libvirt metadata).
    pub fn in_manage_vms(&self, vm: &str) -> bool {
        self.is_allowed(vm) && self.manage.manage_vms.iter().any(|a| a == "*" || a == vm)
    }

    /// Resolves `path` (symlinks and `..` included) and checks that it is a regular
    /// file inside one of `host_share_dirs`.
    pub fn share_file(&self, path: &str) -> Result<PathBuf> {
        file_in(&self.host_share_dirs, path, "host_share_dirs")
    }

    /// Like `share_file`, for ISO images in `manage.iso_dirs`.
    pub fn iso_file(&self, path: &str) -> Result<PathBuf> {
        file_in(&self.manage.iso_dirs, path, "manage.iso_dirs")
    }
}

/// Whether `name` is a hostname label: lowercase letters, digits and hyphens, starting with a letter, not ending
/// with a hyphen, at most 63 characters. Names of VMs made from templates follow this rule, because the name
/// becomes the guest's hostname.
pub fn is_hostname(name: &str) -> bool {
    name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn canonicalize_dirs(dirs: &mut [PathBuf], key: &str) -> Result<()> {
    for dir in dirs {
        let real = dir
            .canonicalize()
            .with_context(|| format!("{key}: cannot access {}", dir.display()))?;
        if !real.is_dir() {
            bail!("{key}: {} is not a directory", dir.display());
        }
        *dir = real;
    }
    Ok(())
}

/// Refuses a pull directory that equals, contains or lies inside any of `others`, each a named list of canonical
/// directories. A guest writes into pull directories, so it must not reach what a push reads, an install image, or a
/// VM's disk.
pub fn refuse_overlaps(pull_dirs: &[PathBuf], others: &[(&str, &[PathBuf])]) -> Result<()> {
    for pull in pull_dirs {
        for (key, dirs) in others {
            if let Some(other) = dirs
                .iter()
                .find(|d| pull.starts_with(d) || d.starts_with(pull))
            {
                bail!(
                    "host_pull_dirs: {} overlaps {} in {key}; a guest could write there",
                    pull.display(),
                    other.display()
                );
            }
        }
    }
    Ok(())
}

/// Checks that `dir` is an absolute directory holding both OCR models, and returns it resolved.
fn ocr_models_dir(dir: &Path) -> Result<PathBuf> {
    const KEY: &str = "display.ocr_models";
    if !dir.is_absolute() {
        bail!("{KEY}: {} must be an absolute path", dir.display());
    }
    let mut dirs = [dir.to_path_buf()];
    canonicalize_dirs(&mut dirs, KEY)?;
    let [real] = dirs;
    for model in [OCR_DETECTION_MODEL, OCR_RECOGNITION_MODEL] {
        if !real.join(model).is_file() {
            bail!(
                "{KEY}: {} has no {model}; scripts/fetch-ocr-models.sh downloads it",
                dir.display()
            );
        }
    }
    Ok(real)
}

fn file_in(dirs: &[PathBuf], path: &str, key: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        bail!("{path}: path must be absolute");
    }
    let real = p
        .canonicalize()
        .with_context(|| format!("cannot access {path}"))?;
    if !dirs.iter().any(|d| real.starts_with(d)) {
        bail!("{path} is not inside {key}");
    }
    if !real.is_file() {
        bail!("{path} is not a regular file");
    }
    Ok(real)
}

/// Accepts `cwd` as a project directory unless sharing it would expose too much.
fn project_dir(cwd: PathBuf, home: Option<&Path>) -> Result<PathBuf> {
    if cwd.parent().is_none() || home.is_some_and(|h| h.starts_with(&cwd)) {
        bail!(
            "not sharing {}: it is the root or home directory, not a project",
            cwd.display()
        );
    }
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Hypervisor::lookup`, behind every per-VM tool including `mouse`, refuses a VM this check rejects.
    #[test]
    fn allowed_vms_admits_only_listed_vms_unless_it_holds_a_star() {
        assert!(!Config::default().is_allowed("trial-deb13"));
        let listed = Config {
            allowed_vms: vec!["trial-deb13".into()],
            ..Config::default()
        };
        assert!(listed.is_allowed("trial-deb13"));
        for other in ["trial-deb1", "trial-deb13 ", "*", ""] {
            assert!(!listed.is_allowed(other), "{other:?} is allowed");
        }
        let star = Config {
            allowed_vms: vec!["*".into()],
            ..Config::default()
        };
        assert!(star.is_allowed("anything"));
    }

    #[test]
    fn share_file_stays_inside_share_dirs() {
        let root =
            std::env::temp_dir().join(format!("virtmanager-mcp-test-{}", std::process::id()));
        let share = root.join("share");
        std::fs::create_dir_all(share.join("sub")).unwrap();
        std::fs::write(share.join("sub/ok.txt"), "x").unwrap();
        std::fs::write(root.join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("secret.txt"), share.join("link.txt")).unwrap();

        let mut config = Config {
            host_share_dirs: vec![share.clone()],
            ..Config::default()
        };
        config.validate().unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();

        assert!(config.share_file(&s(&share.join("sub/ok.txt"))).is_ok());
        assert!(
            config
                .share_file(&s(&share.join("sub/../../secret.txt")))
                .is_err()
        );
        assert!(config.share_file(&s(&share.join("link.txt"))).is_err());
        assert!(config.share_file(&s(&share.join("sub"))).is_err());
        assert!(config.share_file("sub/ok.txt").is_err());
        assert!(config.iso_file(&s(&share.join("sub/ok.txt"))).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A guest writes into pull directories, so each overlap with a directory a push reads, an ISO directory or a
    /// storage pool is refused, in either direction.
    #[test]
    fn pull_dirs_may_not_overlap_shares_isos_or_pools() {
        let root =
            std::env::temp_dir().join(format!("virtmanager-mcp-pull-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a/inner", "b", "c"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let (a, inner, b, c) = (
            root.join("a"),
            root.join("a/inner"),
            root.join("b"),
            root.join("c"),
        );
        let config = |pull: &Path, share: &Path, iso: &Path| Config {
            host_pull_dirs: vec![pull.to_path_buf()],
            host_share_dirs: vec![share.to_path_buf()],
            manage: Manage {
                iso_dirs: vec![iso.to_path_buf()],
                ..Manage::default()
            },
            ..Config::default()
        };

        config(&c, &a, &b).validate().unwrap();
        for (pull, share, iso, key) in [
            (&a, &a, &b, "host_share_dirs"),
            (&inner, &a, &b, "host_share_dirs"),
            (&a, &inner, &b, "host_share_dirs"),
            (&a, &b, &a, "manage.iso_dirs"),
            (&inner, &b, &a, "manage.iso_dirs"),
            (&a, &b, &inner, "manage.iso_dirs"),
        ] {
            let err = config(pull, share, iso).validate().unwrap_err().to_string();
            assert!(err.contains(key), "{} with {key}: {err}", pull.display());
        }

        // A pull directory reached through a symlink is compared by its real path.
        std::os::unix::fs::symlink(&a, root.join("link")).unwrap();
        let err = config(&root.join("link"), &a, &b)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("host_share_dirs"), "{err}");

        // Storage pool targets are checked at start, through the same function.
        let pool = [("storage pool agent-vms", std::slice::from_ref(&inner))];
        let err = refuse_overlaps(std::slice::from_ref(&a), &pool)
            .unwrap_err()
            .to_string();
        assert!(err.contains("storage pool agent-vms"), "{err}");
        refuse_overlaps(std::slice::from_ref(&c), &pool).unwrap();

        // A missing pull directory is refused before anything else.
        let err = config(&root.join("missing"), &a, &b)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("host_pull_dirs"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn parse(text: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn max_wait_defaults_below_client_cutoffs_and_refuses_zero() {
        assert_eq!(parse("").unwrap().limits.max_wait_secs, 50);
        let err = parse("[limits]\nmax_wait_secs = 0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_wait_secs"), "{err}");
    }

    #[test]
    fn templates_parse_with_defaults() {
        let config = parse(
            r#"
            [manage]
            create = true
            disk_pool = "agent-vms"
            [manage.templates.debian13]
            volume = "tpl-debian13.qcow2"
            [manage.templates.debian13-uefi]
            volume = "tpl-debian13.qcow2"
            os = "linux"
            firmware = "uefi"
            description = "Debian 13"
            user_data = "/etc/virtmanager-mcp/debian13.yaml"
            "#,
        )
        .unwrap();
        let m = &config.manage;
        assert_eq!(m.max_lifetime_minutes, 1440);
        let plain = &m.templates["debian13"];
        assert_eq!(
            (plain.os, plain.firmware, plain.user_data.as_ref()),
            (GuestOs::Linux, Firmware::Bios, None)
        );
        let uefi = &m.templates["debian13-uefi"];
        assert_eq!(uefi.firmware, Firmware::Uefi);
        assert_eq!(
            uefi.user_data.as_deref(),
            Some(Path::new("/etc/virtmanager-mcp/debian13.yaml"))
        );
    }

    #[test]
    fn example_config_parses() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert!(config.manage.templates.is_empty());
    }

    #[test]
    fn config_without_templates_has_none() {
        let config = parse("[manage]\ncreate = false\n").unwrap();
        assert!(config.manage.templates.is_empty());
    }

    #[test]
    fn bad_template_config_is_refused() {
        let table = |extra: &str| {
            format!(
                "[manage]\ndisk_pool = \"p\"\n[manage.templates.debian13]\nvolume = \"t.qcow2\"\n{extra}"
            )
        };
        for (text, why) in [
            (table("firmware = \"coreboot\""), "unknown firmware"),
            (table("os = \"bsd\""), "unknown os"),
            (table("colour = \"blue\""), "unknown key"),
            (
                "[manage.templates.Debian]\nvolume = \"t.qcow2\"".into(),
                "uppercase name",
            ),
            (
                "[manage.templates.debian_13]\nvolume = \"t.qcow2\"".into(),
                "underscore",
            ),
            (
                "[manage.templates.13debian]\nvolume = \"t.qcow2\"".into(),
                "leading digit",
            ),
            (
                "[manage.templates.debian13]\nvolume = \"../t.qcow2\"".into(),
                "path volume",
            ),
            (
                "[manage.templates.debian13]\nvolume = \"\"".into(),
                "empty volume",
            ),
            (
                "[manage.templates.debian13]\nos = \"linux\"".into(),
                "missing volume",
            ),
            ("[manage]\nmax_lifetime_minutes = 0".into(), "zero lifetime"),
        ] {
            assert!(parse(&text).is_err(), "{why} was accepted");
        }
    }

    #[test]
    fn hostname_rule() {
        for ok in ["t-vm1", "a", "debian13", &"a".repeat(63)] {
            assert!(is_hostname(ok), "{ok}");
        }
        for bad in [
            "",
            "T-vm",
            "1vm",
            "-vm",
            "vm-",
            "vm_1",
            "vm.1",
            "vm 1",
            &"a".repeat(64),
        ] {
            assert!(!is_hostname(bad), "{bad}");
        }
    }

    #[test]
    fn ocr_models_must_hold_both_models_when_set() {
        assert_eq!(parse("").unwrap().display.ocr_models, None);

        let root =
            std::env::temp_dir().join(format!("virtmanager-mcp-ocr-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config_for =
            |dir: &Path| format!("[display]\nocr_models = {:?}", dir.to_str().unwrap());
        let refusal = |dir: &Path| parse(&config_for(dir)).unwrap_err().to_string();

        let err = refusal(&root.join("missing"));
        assert!(err.contains("display.ocr_models"), "{err}");
        let err = refusal(Path::new("relative/models"));
        assert!(err.contains("absolute"), "{err}");
        std::fs::write(root.join(OCR_DETECTION_MODEL), "").unwrap();
        let err = refusal(&root);
        assert!(err.contains(OCR_RECOGNITION_MODEL), "{err}");
        let err = refusal(&root.join(OCR_DETECTION_MODEL));
        assert!(err.contains("not a directory"), "{err}");

        std::fs::write(root.join(OCR_RECOGNITION_MODEL), "").unwrap();
        let config = parse(&config_for(&root)).unwrap();
        assert_eq!(
            config.display.ocr_models,
            Some(root.canonicalize().unwrap())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn project_dir_refuses_root_and_home() {
        let home = Path::new("/home/u");
        assert!(project_dir("/home/u/code/app".into(), Some(home)).is_ok());
        assert!(project_dir("/home/u".into(), Some(home)).is_err());
        assert!(project_dir("/home".into(), Some(home)).is_err());
        assert!(project_dir("/".into(), Some(home)).is_err());
    }
}
