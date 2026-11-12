//! MCP tool definitions.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{self, Agent, ExecSpec, Shell, shell_spec};
use crate::config::Config;
use crate::console::{Consoles, Pacing};
use crate::guestpath::GuestPath;
use crate::hypervisor::{self, Hypervisor};
use crate::{
    addresses, ansi, dirpull, display, fsops, jobs, manage, mouse, ocrfetch, pull, push, transfer,
    wait,
};

/// Time a console write may take beyond its pauses, for a slow guest to take the bytes.
const WRITE_MARGIN: Duration = Duration::from_secs(10);

/// How long stop_vm waits for a console session to see its stream end once the VM is off; the reader polls every
/// 30 ms, so this is ample.
const CONSOLE_END_WAIT: Duration = Duration::from_secs(2);

/// Tools hidden in read-only mode.
const MUTATING_TOOLS: &[&str] = &[
    "guest_info",
    "exec",
    "write_file",
    "console_write",
    "screenshot",
    "send_keys",
    "type_text",
    "mouse",
    "start_vm",
    "stop_vm",
    "copy_to_vm",
    "copy_from_vm",
    "copy_dir_to_vm",
    "copy_dir_from_vm",
    "job_start",
    "job_stop",
];

/// Tools behind [manage] create / delete / reconfigure / snapshots.
const CREATE_TOOLS: &[&str] = &["create_vm"];
/// Tools behind [manage] create with at least one template.
const TEMPLATE_TOOLS: &[&str] = &["list_templates", "create_vm_from_template"];
/// Behind [manage] create and delete with at least one template, because a reset destroys disk state.
const RESET_TOOLS: &[&str] = &["reset_vm"];
const DELETE_TOOLS: &[&str] = &["delete_vm"];
const RECONFIGURE_TOOLS: &[&str] = &[
    "configure_vm",
    "attach_disk",
    "detach_disk",
    "set_cdrom",
    "attach_nic",
    "detach_nic",
];
const SNAPSHOT_TOOLS: &[&str] = &[
    "snapshot_create",
    "snapshot_list",
    "snapshot_revert",
    "snapshot_delete",
];

pub struct State {
    pub hv: Hypervisor,
    pub consoles: Consoles,
    /// The OCR recogniser, loaded from `display.ocr_models` by the first `screen_text` wait and shared after.
    pub ocr: Mutex<Option<Arc<display::Recognizer>>>,
    /// The model download that `VIRTMANAGER_OCR_DOWNLOAD=1` started, which the first load waits on.
    pub ocr_download: Option<Arc<ocrfetch::Download>>,
    /// Host destinations of the pulls running in this process.
    pub pulls: pull::Destinations,
    /// Copies running in, or finished by, this process.
    pub transfers: transfer::Transfers,
}

#[derive(Clone)]
pub struct VirtServer {
    state: Arc<State>,
    tool_router: ToolRouter<Self>,
}

// ---- parameter types ----------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct VmParam {
    /// Name of the VM (as shown in virt-manager).
    pub vm: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExecParams {
    pub vm: String,
    /// Shell command line to run. Use either `command` or `program`.
    pub command: Option<String>,
    /// Shell that interprets `command`.
    #[serde(default)]
    pub shell: Shell,
    /// Program to run directly, without a shell (argv style). Use either `command` or `program`.
    pub program: Option<String>,
    /// Arguments for `program`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Data written to the process's stdin.
    pub stdin: Option<String>,
    /// Extra environment variables as "NAME=value".
    #[serde(default)]
    pub env: Vec<String>,
    /// Kill the command after this many seconds (default from config).
    pub timeout_secs: Option<u64>,
    /// Max bytes returned for stdout and for stderr each (default from config).
    pub max_output_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReadEncoding {
    /// UTF-8 text if the content is valid UTF-8, otherwise base64.
    #[default]
    Auto,
    /// UTF-8 text; invalid sequences are replaced.
    Text,
    Base64,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadFileParams {
    pub vm: String,
    /// Absolute path inside the guest (e.g. /etc/os-release or C:\Windows\win.ini).
    pub path: String,
    /// Byte offset to start reading from.
    #[serde(default)]
    pub offset: u64,
    /// Max bytes to return (default from config).
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub encoding: ReadEncoding,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum WriteEncoding {
    #[default]
    Text,
    Base64,
}

#[derive(Deserialize, JsonSchema)]
pub struct WriteFileParams {
    pub vm: String,
    /// Absolute path inside the guest. Parent directory must exist.
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub encoding: WriteEncoding,
    /// Append instead of overwriting.
    #[serde(default)]
    pub append: bool,
    /// Permission bits as octal, from "0" to "777" (e.g. "600"). A new file gets 644 when omitted; an existing
    /// file keeps its mode unless this is given. Linux guests only.
    pub mode: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PathParams {
    pub vm: String,
    /// Absolute path inside the guest.
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CopyToVmParams {
    pub vm: String,
    /// Absolute path of the file on the host. Must be inside one of the directories listed in the server instructions.
    pub host_path: String,
    /// Absolute destination path inside the guest. Parent directory must exist; an existing file is overwritten.
    pub guest_path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct CopyFromVmParams {
    pub vm: String,
    /// Absolute path of the file inside the guest.
    pub guest_path: String,
    /// Absolute destination path on the host, inside one of the pull directories named in the server instructions.
    pub host_path: String,
    /// Replace an existing file at host_path (default false). A directory or symlink is never replaced.
    #[serde(default)]
    pub overwrite: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, JsonSchema)]
pub struct CopyDirFromVmParams {
    pub vm: String,
    /// Absolute path of a directory inside the guest.
    pub guest_path: String,
    /// Absolute path of the directory to create on the host, inside one of the pull directories named in the server
    /// instructions; missing parent directories are created.
    pub host_path: String,
    /// Replace an existing directory at host_path with the pulled tree as a whole (default false). A file, symlink
    /// or special file there is never replaced.
    #[serde(default)]
    pub overwrite: bool,
    /// Paths to leave out, as gitignore patterns relative to guest_path, such as "build/" or "*.o". The server
    /// applies them on the host, so excluded files still cross the channel and count towards the limits.
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CopyDirToVmParams {
    pub vm: String,
    /// Absolute path of a directory on the host, inside one of the shared directories named in the server
    /// instructions.
    pub host_path: String,
    /// Absolute path of the directory to create in the guest; missing parent directories are created.
    pub guest_path: String,
    /// Replace an existing directory at guest_path with the pushed tree as a whole (default false). Refused for / or a
    /// Windows drive root and the directories directly below it, and for anything but a directory.
    #[serde(default)]
    pub replace: bool,
    /// Leave out what .gitignore and .git/info/exclude rules ignore, in the tree and in its parent directories inside
    /// the share (default true).
    #[serde(default = "yes")]
    pub respect_gitignore: bool,
    /// Send .git directories too (default false).
    #[serde(default)]
    pub include_git: bool,
    /// More paths to leave out, as gitignore patterns relative to host_path, such as "target/" or "*.log".
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StartVmParams {
    pub vm: String,
    /// After starting, wait up to this many seconds for the guest agent to respond (default 0, max 300).
    #[serde(default)]
    pub wait_for_agent_secs: u64,
}

#[derive(Deserialize, JsonSchema)]
pub struct StopVmParams {
    pub vm: String,
    /// Power off immediately, like pulling the plug, instead of asking the guest to shut down.
    #[serde(default)]
    pub force: bool,
    /// How long to wait for a graceful shutdown to finish (default 60, max 300).
    pub wait_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ConsoleOpenParams {
    pub vm: String,
    /// Console device alias (e.g. "serial0", "console1"); omit for the first console.
    pub device: Option<String>,
    /// Take over the console even if another client (e.g. virt-manager) has it open. Also replaces this
    /// server's own session for the VM, discarding its buffered output.
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ConsoleReadParams {
    pub vm: String,
    /// Absolute offset to read from (use `next` from the previous read). Omit to get the most recent output.
    pub since: Option<u64>,
    /// The `generation` returned with `since`. Needed whenever `since` is above 0, so an offset from an earlier
    /// console session is refused rather than misread.
    pub generation: Option<u64>,
    /// Max bytes to return (default 65536).
    pub max_bytes: Option<usize>,
    /// Wait up to this many milliseconds (max 30000) for output that no earlier read returned, then until the
    /// output pauses for half a second. 0 returns at once. Waiting works with any `since`, including 0.
    #[serde(default)]
    pub wait_ms: u64,
    /// Remove ANSI escape sequences (colors, cursor movement). Default true.
    pub strip_ansi: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobStartParams {
    pub vm: String,
    /// Command to run in the background: a `/bin/sh` command as root on Linux, a PowerShell script as SYSTEM on
    /// Windows. It is stored in the guest as data and run as is. On Windows the exit code is the one passed to `exit`,
    /// 1 after an uncaught error, and otherwise 0, even after a failing native program: end with
    /// `exit $LASTEXITCODE` to pass a program's code on, and `exit [int](-not (...))` for a test, since `$false` is
    /// not a failure.
    pub command: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobReadParams {
    pub vm: String,
    /// The job ID that job_start returned.
    pub job: String,
    /// Byte offset in the job's output to read from (use `next` from the previous read). Default 0.
    #[serde(default)]
    pub since: u64,
    /// Max bytes to return (default: limits.max_output_bytes).
    pub max_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct JobStopParams {
    pub vm: String,
    /// The job ID that job_start returned.
    pub job: String,
    /// Send SIGKILL instead of SIGTERM. A killed job records no exit code and reads "ended without status". Windows
    /// has no SIGTERM, so a Windows job's process tree always ends at once, whatever this says.
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct WaitForParams {
    pub vm: String,
    /// Seconds to wait, at most limits.max_wait_secs (default 50), which is also the default. 0 checks console conditions once and is refused with agent, command, job and screen conditions.
    pub timeout_secs: Option<u64>,
    /// What to wait for, at most 16; the call returns on the first that holds.
    pub conditions: Vec<wait::Condition>,
    /// On a match, attach a PNG screenshot, downscaled to 1280 pixels wide, of the matched screen condition's display
    /// head, or of head 0 for other kinds; the result gives its size. Refused in read-only mode.
    pub screenshot: Option<bool>,
}

/// Whether a `wait_for` call attaches a screenshot to a match. Read-only mode refuses the attachment, which
/// `validate_conditions` cannot see.
fn attachment_wanted(config: &Config, screenshot: Option<bool>) -> Result<bool> {
    let wanted = screenshot.unwrap_or(false);
    if wanted && config.read_only {
        bail!("screenshot attachments are disabled in read-only mode");
    }
    Ok(wanted)
}

#[derive(Deserialize, JsonSchema)]
pub struct ConsoleWriteParams {
    pub vm: String,
    /// Text to send. Control characters are sent as-is (e.g. "\u0003" for Ctrl+C, "\u0004" for Ctrl+D).
    pub text: String,
    /// Append a carriage return (Enter). Default true.
    pub press_enter: Option<bool>,
    /// Pause this many milliseconds (at most 1000) between bytes, for receivers that drop input sent back to
    /// back, such as firmware and bootloaders. All pauses together may take at most `limits.max_wait_secs`.
    pub inter_char_delay_ms: Option<u64>,
    /// Send in chunks of this many bytes, pausing `chunk_delay_ms` between them. Use instead of
    /// `inter_char_delay_ms`.
    pub chunk_size: Option<usize>,
    /// Pause between chunks, in milliseconds (at most 1000).
    pub chunk_delay_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScreenshotParams {
    pub vm: String,
    /// Display head index (default 0).
    #[serde(default)]
    pub screen: u32,
    /// Downscale so the image is at most this many pixels wide (default 1280; 0 = original size).
    pub max_width: Option<u32>,
    /// Return only this part of the screen, cropped at full resolution before any downscaling. Its coordinates are
    /// pixels of a full-screen image from this tool without a region, with that image's size.
    pub region: Option<display::Region>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendKeysParams {
    pub vm: String,
    /// Key combos pressed one after another, e.g. ["ctrl+alt+delete"], ["enter"], ["win+r"], ["alt+f4"].
    /// Key names: a-z, 0-9, f1-f12, enter, esc, tab, space, backspace, delete, insert, home, end,
    /// pageup, pagedown, up, down, left, right, ctrl, alt, shift, win/meta/super, altgr, menu,
    /// capslock, printscreen, or a single punctuation character.
    pub keys: Vec<String>,
    /// How long each combo is held, in milliseconds (default 50).
    pub hold_ms: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TypeTextParams {
    pub vm: String,
    /// Text to type (printable ASCII; "\n" presses Enter, "\t" presses Tab). Uses a US keyboard layout.
    pub text: String,
    /// Delay between keystrokes in milliseconds (default 20).
    pub delay_ms: Option<u64>,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    Linux,
    /// Adds Hyper-V enlightenments and a local-time clock.
    Windows,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    #[default]
    Uefi,
    Bios,
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateVmParams {
    /// Name of the new VM: 1-64 characters of A-Z, a-z, 0-9, '.', '_', '-'.
    pub name: String,
    pub os: OsKind,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Size of the system disk (a new qcow2 volume in the disk pool).
    pub disk_gib: u64,
    /// Default uefi.
    #[serde(default)]
    pub firmware: Firmware,
    /// Add an emulated TPM 2.0 (needed by Windows 11).
    #[serde(default)]
    pub tpm: bool,
    /// Install ISO (absolute host path from list_isos), attached as a bootable CD-ROM.
    pub iso: Option<String>,
    /// Second ISO attached as another CD-ROM, e.g. virtio-win.iso so a Windows installer can load
    /// the virtio disk and network drivers.
    pub driver_iso: Option<String>,
    /// libvirt network to connect to (default: the first allowed network; none if no network is allowed).
    pub network: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateFromTemplateParams {
    /// Name of the new VM, also its hostname: lowercase letters, digits and hyphens, starting with a letter, not
    /// ending with a hyphen, at most 63 characters.
    pub name: String,
    /// Template name, from list_templates.
    pub template: String,
    /// Default 2, or the limit if lower.
    pub vcpus: Option<u32>,
    /// Default 1024, or the limit if lower.
    pub memory_mib: Option<u64>,
    /// Size of the system disk; never smaller than the template's. Default: the template's size.
    pub disk_gib: Option<u64>,
    /// libvirt network to connect to (default: the first allowed network).
    pub network: Option<String>,
    /// Delete the VM this many minutes after creation. Omit for no expiry. The limit is in the server instructions.
    pub lifetime_minutes: Option<u32>,
    /// Start the VM after creating it (default true).
    pub start: Option<bool>,
    /// After starting, wait up to this many seconds for the guest agent to respond (default 60, max 300).
    pub wait_for_agent_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ResetVmParams {
    pub vm: String,
    /// After starting, wait up to this many seconds for the guest agent to respond (default 60, max 300).
    pub wait_for_agent_secs: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ConfigureVmParams {
    pub vm: String,
    /// New number of vCPUs (takes effect at next boot).
    pub vcpus: Option<u32>,
    /// New memory size in MiB (takes effect at next boot).
    pub memory_mib: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AttachDiskParams {
    pub vm: String,
    /// Size of the new disk.
    pub size_gib: u64,
}

#[derive(Deserialize, JsonSchema)]
pub struct DetachDiskParams {
    pub vm: String,
    /// Disk target as shown by describe_vm, e.g. "vdb".
    pub target: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetCdromParams {
    pub vm: String,
    /// Absolute host path of an ISO (from list_isos). Omit to eject.
    pub iso: Option<String>,
    /// CD-ROM target as shown by describe_vm, e.g. "sda". Omit for the first CD-ROM drive.
    pub target: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AttachNicParams {
    pub vm: String,
    /// libvirt network name (one of the allowed networks).
    pub network: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct DetachNicParams {
    pub vm: String,
    /// MAC address of the interface, as shown by describe_vm.
    pub mac: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SnapshotParams {
    pub vm: String,
    /// Snapshot name.
    pub snapshot: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SnapshotCreateParams {
    pub vm: String,
    /// Snapshot name: 1-64 characters of A-Z, a-z, 0-9, '.', '_', '-'.
    pub snapshot: String,
    pub description: Option<String>,
}

/// The tools `config` hides: mutating tools in read-only mode, and tools whose feature is off.
pub fn hidden_tools(config: &Config) -> Vec<&'static str> {
    let mut hidden = Vec::new();
    if config.read_only {
        hidden.extend_from_slice(MUTATING_TOOLS);
    }
    // VMs created by this server can be powered on and off even when power_vms is empty.
    if config.power_vms.is_empty() && !config.manage.enabled() {
        hidden.extend_from_slice(&["start_vm", "stop_vm"]);
    }
    if config.host_share_dirs.is_empty() {
        hidden.extend_from_slice(&["copy_to_vm", "copy_dir_to_vm"]);
    }
    if config.host_pull_dirs.is_empty() {
        hidden.extend_from_slice(&["copy_from_vm", "copy_dir_from_vm"]);
    }
    let m = &config.manage;
    let off = |on: bool, tools: &'static [&'static str]| {
        if on && !config.read_only {
            &[][..]
        } else {
            tools
        }
    };
    for tools in [
        off(m.create, CREATE_TOOLS),
        off(m.create && !m.templates.is_empty(), TEMPLATE_TOOLS),
        off(m.create && m.delete && !m.templates.is_empty(), RESET_TOOLS),
        off(m.delete, DELETE_TOOLS),
        off(m.reconfigure, RECONFIGURE_TOOLS),
        off(m.snapshots, SNAPSHOT_TOOLS),
        off(m.enabled(), &["describe_vm"]),
        off(
            (m.create || m.reconfigure) && !m.iso_dirs.is_empty(),
            &["list_isos"],
        ),
    ] {
        hidden.extend_from_slice(tools);
    }
    hidden.sort_unstable();
    hidden.dedup();
    hidden
}

// ---- tools --------------------------------------------------------------------

#[tool_router]
impl VirtServer {
    pub fn new(state: Arc<State>) -> Self {
        let mut tool_router = Self::tool_router();
        for name in hidden_tools(&state.hv.config) {
            tool_router.remove_route(name);
        }
        Self { state, tool_router }
    }

    /// Runs blocking libvirt work off the async runtime.
    async fn blocking<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&State) -> Result<Vec<ContentBlock>> + Send + 'static,
    {
        let state = self.state.clone();
        match tokio::task::spawn_blocking(move || f(&state)).await {
            Ok(Ok(content)) => Ok(CallToolResult::success(content)),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{e:#}"
            ))])),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List VMs this server may access: power state, whether start_vm/stop_vm may be used on them, \
        whether the VM management tools may act on them (managed), and for running VMs whether the QEMU guest agent responds and the guest OS family."
    )]
    async fn list_vms(&self) -> Result<CallToolResult, ErrorData> {
        self.blocking(|s| {
            let timeout = s.hv.config.limits.agent_timeout_secs.min(3);
            let vms: Vec<Value> =
                s.hv.allowed()?
                    .iter()
                    .map(|d| {
                        let name = d.get_name().unwrap_or_default();
                        let agent = Agent::new(d, timeout);
                        let ok = d.is_active().unwrap_or(false) && agent.ping();
                        json!({
                            "name": name,
                            "state": hypervisor::state_name(d),
                            "power_control": s.hv.can_power(d) && !s.hv.config.read_only,
                            "managed": s.hv.can_manage(d) && !s.hv.config.read_only,
                            "guest_agent": ok,
                            "os": if ok { agent.os().ok() } else { None },
                            "console_open": s.consoles_has(&name),
                        })
                    })
                    .collect();
            let mut out = vec![json_block(&vms)?];
            if vms.is_empty() {
                out.push(ContentBlock::text(
                    "No accessible VMs. Check that the VM exists and is listed in allowed_vms.",
                ));
            }
            Ok(out)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List a running VM's network addresses, per interface by MAC address, without waiting. For \
        each interface the answer comes from libvirt's DHCP lease where one exists, else from the guest agent, else \
        from the host's ARP table, and `source` says which. A lease is handed out by libvirt itself; `agent` and \
        `arp` answers are what the guest claims, so a compromised guest can name an address that is not its own. \
        Loopback and IPv6 link-local addresses are left out. An empty list means no source knows an address yet; \
        `unavailable` names the sources that failed. Read-only mode skips the guest agent."
    )]
    async fn vm_addresses(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let config = &s.hv.config;
            let answers = addresses::query(
                &dom,
                config.limits.agent_timeout_secs,
                addresses::sources(config),
            );
            Ok(vec![ContentBlock::text(
                addresses::reply(&answers).to_string(),
            )])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Query the guest agent for OS details, hostname, network interfaces, filesystems, logged-in users and timezone."
    )]
    async fn guest_info(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let agent = s.agent(&dom);
            let mut info = serde_json::Map::new();
            for (key, cmd) in [
                ("os", "guest-get-osinfo"),
                ("hostname", "guest-get-host-name"),
                ("timezone", "guest-get-timezone"),
                ("users", "guest-get-users"),
                ("network", "guest-network-get-interfaces"),
                ("filesystems", "guest-get-fsinfo"),
            ] {
                let v = agent
                    .call(cmd, None)
                    .unwrap_or_else(|e| json!({ "error": format!("{e:#}") }));
                info.insert(key.into(), v);
            }
            Ok(vec![json_block(&info)?])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Run a command inside the guest via the QEMU guest agent and return exit code, stdout and stderr. \
        Non-interactive: the command must not wait for user input. Provide either `command` (run by a shell; default \
        /bin/sh on Linux, PowerShell on Windows) or `program` + `args` (run directly)."
    )]
    async fn exec(
        &self,
        Parameters(p): Parameters<ExecParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let agent = s.agent(&dom);
            let limits = &s.hv.config.limits;
            let mut spec = match (&p.command, &p.program) {
                (Some(cmd), None) => shell_spec(agent.os()?, p.shell, cmd)?,
                (None, Some(prog)) => ExecSpec::new(prog, p.args.clone()),
                _ => bail!("provide exactly one of `command` or `program`"),
            };
            spec.env = p.env;
            spec.stdin = p.stdin.map(String::into_bytes);
            let timeout = Duration::from_secs(p.timeout_secs.unwrap_or(limits.exec_timeout_secs));
            let out = agent.exec(
                &spec,
                timeout,
                p.max_output_bytes.unwrap_or(limits.max_output_bytes),
            )?;

            let mut text = serde_json::to_string(&out)?;
            if out.timed_out {
                text.push_str(&format!(
                    "\ncommand did not finish within {}s",
                    timeout.as_secs()
                ));
            }
            text.push_str("\n--- stdout ---\n");
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            text.push_str("\n--- stderr ---\n");
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(vec![ContentBlock::text(text)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Read a file from the guest via the QEMU guest agent. Returns text, or base64 for binary content."
    )]
    async fn read_file(
        &self,
        Parameters(p): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let max = p
                .max_bytes
                .unwrap_or(s.hv.config.limits.max_file_read_bytes);
            let (data, eof) = s.agent(&dom).read_file(&p.path, p.offset, max)?;
            let (encoding, content) = match (p.encoding, String::from_utf8(data)) {
                (ReadEncoding::Base64, Ok(t)) => ("base64", B64.encode(t)),
                (ReadEncoding::Base64, Err(e)) => ("base64", B64.encode(e.into_bytes())),
                (_, Ok(t)) => ("text", t),
                (ReadEncoding::Text, Err(e)) => {
                    ("text", String::from_utf8_lossy(e.as_bytes()).into_owned())
                }
                (ReadEncoding::Auto, Err(e)) => ("base64", B64.encode(e.into_bytes())),
            };
            let meta = json!({
                "path": p.path,
                "offset": p.offset,
                "encoding": encoding,
                "truncated": !eof,
            });
            Ok(vec![
                ContentBlock::text(meta.to_string()),
                ContentBlock::text(content),
            ])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Write (or append to) a file in the guest via the QEMU guest agent. On Linux guests a new \
        file gets mode 644 unless `mode` says otherwise; an existing file keeps its mode."
    )]
    async fn write_file(
        &self,
        Parameters(p): Parameters<WriteFileParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let data = match p.encoding {
                WriteEncoding::Text => p.content.into_bytes(),
                WriteEncoding::Base64 => B64.decode(p.content.trim())?,
            };
            let mode = p.mode.as_deref().map(agent::parse_mode).transpose()?;
            let n = s.agent(&dom).write_file(&p.path, &data, p.append, mode)?;
            Ok(vec![ContentBlock::text(format!(
                "wrote {n} bytes to {}",
                p.path
            ))])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Copy a file from the host into the guest via the QEMU guest agent. The file is streamed \
        directly by the server, so it can be large and never passes through the conversation. Only files inside the \
        shared host directories (named in the server instructions) can be copied. A copy still running after \
        limits.max_wait_secs continues in the background and replies with a `transfer` ID; wait_for's \
        `transfer_done` condition reports how it ended."
    )]
    async fn copy_to_vm(
        &self,
        Parameters(p): Parameters<CopyToVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let src = s.hv.config.share_file(&p.host_path)?;
            let file = std::fs::File::open(&src)
                .with_context(|| format!("cannot open {}", src.display()))?;
            let timeout = s.hv.config.limits.agent_timeout_secs;
            let guest = dom.clone();
            s.copy(&p.vm, &dom, move |bytes| {
                let reader = transfer::CountingReader {
                    inner: std::io::BufReader::new(file),
                    count: bytes,
                };
                let n =
                    Agent::new(&guest, timeout).write_from(&p.guest_path, reader, false, None)?;
                Ok(format!(
                    "copied {n} bytes from {} to {}",
                    src.display(),
                    p.guest_path
                ))
            })
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Copy a file from the guest to the host via the QEMU guest agent, without passing it through \
        the conversation. host_path must lie inside one of the pull directories named in the server instructions; \
        missing parent directories there are created. The file arrives with mode 644, never executable, and appears \
        only once it is complete. An existing file at host_path is replaced only with overwrite: true; a directory, \
        symlink or special file there is never replaced. Files over limits.max_pull_bytes are refused. A copy still \
        running after limits.max_wait_secs continues in the background and replies with a `transfer` ID; wait_for's \
        `transfer_done` condition reports how it ended."
    )]
    async fn copy_from_vm(
        &self,
        Parameters(p): Parameters<CopyFromVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let config = &s.hv.config;
            // The VM first, so a call naming a hidden or missing VM creates no directories on the host.
            let dom = s.hv.domain(&p.vm)?;
            let dest = pull::Destination::open(&config.host_pull_dirs, &p.host_path)?;
            dest.check_file(p.overwrite)?;
            let claim = s.pulls.claim(dest.path())?;
            let (timeout, max) = (
                config.limits.agent_timeout_secs,
                config.limits.max_pull_bytes,
            );
            let guest = dom.clone();
            s.copy(&p.vm, &dom, move |bytes| {
                let _claim = claim;
                let mut staged = dest.stage_file()?;
                let mut writer = transfer::CountingWriter {
                    inner: staged.file(),
                    count: bytes,
                };
                let n = Agent::new(&guest, timeout).read_to(&p.guest_path, &mut writer, max)?;
                staged.commit(&dest, p.overwrite)?;
                Ok(format!(
                    "copied {n} bytes from {} to {}",
                    p.guest_path,
                    dest.path().display()
                ))
            })
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Push a host directory, such as a checkout, into a Linux or Windows guest as a tar archive \
        through the QEMU guest agent; a Linux guest needs tar, and Windows uses its own tar.exe. host_path must be a \
        directory inside the shared host directories named in the server instructions. guest_path names the \
        directory to create (on Windows a drive path such as C:\\work\\app); an existing one is refused unless \
        replace: true, which swaps the whole new tree in, so files deleted on the host do not linger. A failed push \
        leaves the old tree in place. By default .gitignore rules apply and .git is left out; exclude adds gitignore \
        patterns. Symlinks are sent only when they stay inside the tree; other symlinks, FIFOs, sockets and devices \
        are skipped and named. On Windows every symlink is skipped, as is every name Windows cannot hold, such as \
        CON or a name differing from another only in case, and a file open in the old tree makes replace fail. \
        Files arrive owned by the guest agent's user, without setuid or setgid bits. The reply \
        counts files, bytes and the files .gitignore kept out. A push still running after limits.max_wait_secs \
        continues in the background and replies with a `transfer` ID; wait_for's `transfer_done` condition reports \
        how it ended. Temporary entries beside the target start with .vmm-."
    )]
    async fn copy_dir_to_vm(
        &self,
        Parameters(p): Parameters<CopyDirToVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let timeout = s.hv.config.limits.agent_timeout_secs;
            let os = Agent::new(&dom, timeout).os()?;
            let tree = push::Tree::resolve(&s.hv.config.host_share_dirs, &p.host_path)?;
            let options = push::Options {
                respect_gitignore: p.respect_gitignore,
                include_git: p.include_git,
                exclude: p.exclude,
            };
            tree.check(&options)?;
            let target = push::Target::parse(os, &p.guest_path, p.replace)?;
            let guest = dom.clone();
            let replace = p.replace;
            s.copy(&p.vm, &dom, move |bytes| {
                let agent = Agent::new(&guest, timeout);
                let on = push::OnGuest {
                    agent: &agent,
                    steps: push::Steps(os),
                };
                let summary = push::push(&on, &target, replace, bytes, |out| {
                    push::write_archive(&tree, &push::select(&tree, &options)?, os, out)
                })?;
                let mut reply = serde_json::to_value(&summary)?;
                reply["guest_path"] = json!(target.path);
                Ok(reply.to_string())
            })
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Pull a directory from a Linux or Windows guest, such as build output or test results, into a \
        pull directory on the host, without passing it through the conversation; a Linux guest needs tar, and \
        Windows uses its own tar.exe. The guest packs guest_path (on Windows a drive path such as C:\\work\\out) in \
        /var/tmp (or /tmp), or in %SystemRoot%\\Temp on Windows, and the server unpacks the archive itself. On \
        Windows tar.exe follows junctions, and any error it reports, such as a file another program holds open, \
        fails the pull. host_path names the \
        directory to create inside one of the pull directories named in the server instructions; an existing one is \
        replaced as a whole only with overwrite: true, and a file there never is. Files arrive with mode 644 and \
        directories with 755; symlinks, hard links, devices and FIFOs are skipped and named. exclude takes gitignore \
        patterns, applied on the host. limits.max_pull_bytes caps the archive and the files written, and \
        limits.max_pull_entries the entries, counting the directories their paths imply; an archive with absolute or \
        '..' paths, paths over 4096 bytes or 256 levels, repeated paths or sparse files fails the pull, which then \
        writes nothing. If tar reports that a file changed while it was read, the pull \
        succeeds with tar's warning. A pull still running after limits.max_wait_secs continues in the background and \
        replies with a `transfer` ID; wait_for's `transfer_done` condition reports how it ended."
    )]
    async fn copy_dir_from_vm(
        &self,
        Parameters(p): Parameters<CopyDirFromVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let config = &s.hv.config;
            // The VM first, so a call naming a hidden or missing VM creates no directories on the host.
            let dom = s.hv.domain(&p.vm)?;
            let timeout = config.limits.agent_timeout_secs;
            let os = Agent::new(&dom, timeout).os()?;
            let guest_path = GuestPath::parse(os, &p.guest_path)?.normalized();
            let exclude = dirpull::excludes(&p.exclude)?;
            let dest = pull::Destination::open(&config.host_pull_dirs, &p.host_path)?;
            dest.check(pull::Kind::Directory, p.overwrite)?;
            let claim = s.pulls.claim(dest.path())?;
            let limits = dirpull::Limits {
                max_bytes: config.limits.max_pull_bytes,
                max_entries: config.limits.max_pull_entries,
            };
            let guest = dom.clone();
            s.copy(&p.vm, &dom, move |bytes| {
                let _claim = claim;
                let agent = Agent::new(&guest, timeout);
                let on = dirpull::OnGuest {
                    agent: &agent,
                    steps: dirpull::Steps(os),
                };
                let request = dirpull::Request {
                    guest_path: &guest_path,
                    dest: &dest,
                    overwrite: p.overwrite,
                    limits: &limits,
                    exclude: &exclude,
                    temp_dirs: dirpull::TEMP_DIRS,
                    poll: dirpull::POLL,
                    os,
                    remove_for: match os {
                        agent::GuestOs::Linux => Duration::ZERO,
                        agent::GuestOs::Windows => dirpull::REMOVE_FOR,
                    },
                };
                let unpacked = dirpull::pull(&on, &request, bytes)?;
                let mut reply = serde_json::to_value(&unpacked)?;
                reply["host_path"] = json!(dest.path());
                Ok(reply.to_string())
            })
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Start a VM that is shut off (or resume it if paused, wake it if suspended). \
        Only VMs with power_control: true in list_vms can be started."
    )]
    async fn start_vm(
        &self,
        Parameters(p): Parameters<StartVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let mut msg = format!("VM '{}': {}", p.vm, s.hv.start(&p.vm)?);
            let wait = Duration::from_secs(p.wait_for_agent_secs.min(300));
            if !wait.is_zero() {
                let dom = s.hv.domain(&p.vm)?;
                let agent = Agent::new(&dom, 2);
                let ready = agent::wait_for(wait, Duration::from_secs(1), || agent.ping());
                msg.push_str(if ready {
                    "; guest agent is responding"
                } else {
                    "; guest agent not responding yet (the guest may still be booting)"
                });
            }
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Shut down a VM. By default asks the guest to shut down cleanly (ACPI power button or guest \
        agent) and waits for it; with force: true it is powered off immediately, which can lose unsaved data. \
        Only VMs with power_control: true in list_vms can be stopped."
    )]
    async fn stop_vm(
        &self,
        Parameters(p): Parameters<StopVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let wait = Duration::from_secs(p.wait_secs.unwrap_or(60).min(300));
            // Only the session open before the stop is marked, not one a concurrent start and open made meanwhile.
            let generation = s.consoles.generation(&p.vm);
            let off = s.hv.stop(&p.vm, p.force, wait)?;
            let msg = if off {
                // Keep any console session, so a later console_read reports the console closed instead of "no
                // console open"; wait for it to record the end first, so even a read with wait_ms 0 sees it.
                s.consoles.note_shut_off(&p.vm, generation, CONSOLE_END_WAIT);
                format!("VM '{}' is shut off", p.vm)
            } else {
                format!(
                    "VM '{}' is still running after {}s; the guest may be ignoring the shutdown request. \
                     Wait longer or call stop_vm again with force: true.",
                    p.vm,
                    wait.as_secs()
                )
            };
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List a directory in the guest (name, type, size, modification time, mode, owner)."
    )]
    async fn list_dir(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            Ok(vec![json_block(&fsops::list_dir(
                &s.agent(&dom),
                &p.path,
            )?)?])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Get metadata of a single file or directory in the guest."
    )]
    async fn stat_path(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            Ok(vec![json_block(&fsops::stat(&s.agent(&dom), &p.path)?)?])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Attach to the VM's serial/virtio text console (like `virsh console`). Output is buffered \
        from this point on; read it with console_read. Needs a <console>/<serial> device in the VM and a getty or \
        other program on it in the guest."
    )]
    async fn console_open(
        &self,
        Parameters(p): Parameters<ConsoleOpenParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let msg = s
                .consoles
                .open(&s.hv, &p.vm, &dom, p.device.as_deref(), p.force)?;
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Read buffered output from an open serial console. Returns `next` and `generation`; pass both \
        as `since` and `generation` on the following call to get only new output. Each console_open starts a new \
        generation, and an offset from an earlier one is refused. With `wait_ms`, waits for output no earlier read \
        returned, then until it pauses for half a second; a closed console returns at once."
    )]
    async fn console_read(
        &self,
        Parameters(p): Parameters<ConsoleReadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let wait = Duration::from_millis(p.wait_ms.min(30_000));
            let r = s.consoles.read(
                &p.vm,
                p.since,
                p.generation,
                p.max_bytes.unwrap_or(64 * 1024),
                wait,
            )?;
            let text = if p.strip_ansi.unwrap_or(true) {
                String::from_utf8_lossy(&ansi::strip_final(&r.data)).into_owned()
            } else {
                String::from_utf8_lossy(&r.data).into_owned()
            };
            let meta = json!({
                "next": r.next,
                "generation": r.generation,
                "dropped_bytes": r.dropped,
                "closed": r.closed,
            });
            Ok(vec![
                ContentBlock::text(meta.to_string()),
                ContentBlock::text(text),
            ])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Send text to an open serial console (by default followed by Enter). Returns the offset and \
        generation where the guest's answer starts; wait_for and console_read can start there. For a receiver that \
        drops input sent back to back, pace the write: inter_char_delay_ms pauses between bytes, or chunk_size with \
        chunk_delay_ms pauses between chunks. The pauses may total at most limits.max_wait_secs. One write runs at \
        a time per console; a second one is refused while the first is still sending. Send input only once the \
        program that should read it is waiting for it: right after a command line, the shell may still hold the \
        terminal in its own mode, so input that arrives before the command starts can be swallowed without echo, and \
        a Ctrl-D in it read as data. Wait for the program's prompt with wait_for, or pause, before sending its input."
    )]
    async fn console_write(
        &self,
        Parameters(p): Parameters<ConsoleWriteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            s.hv.domain(&p.vm)?;
            let mut data = p.text.into_bytes();
            if p.press_enter.unwrap_or(true) {
                data.push(b'\r');
            }
            let max_wait = Duration::from_secs(s.hv.config.limits.max_wait_secs);
            let pacing = Pacing::from_options(
                p.inter_char_delay_ms,
                p.chunk_size,
                p.chunk_delay_ms,
                data.len(),
                max_wait,
            )?;
            // The pauses fit in max_wait; the margin leaves room for a slow guest to take the bytes.
            let budget = max_wait + WRITE_MARGIN;
            let (offset, generation) = s.consoles.write(&p.vm, &data, pacing, budget)?;
            Ok(vec![ContentBlock::text(format!(
                "sent {} bytes; the answer starts at offset {offset} (generation {generation})",
                data.len()
            ))])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Detach from a VM's serial console."
    )]
    async fn console_close(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let msg = if s.consoles.close(&p.vm) {
                "console closed"
            } else {
                "no console was open"
            };
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Wait until the first of several conditions holds on a VM, and return which one. Kinds: \
        `console` (the open console session's output matches a regex; ANSI-stripped by default), `agent` (the guest \
        agent answers), `command` (a command run through the guest agent with exec's default shell, /bin/sh on Linux \
        and PowerShell on Windows, exits 0; not in read-only mode), `job_done` (a job from job_start has ended) and \
        `job_output` (a job's output from `since` matches a regex); job conditions work in read-only mode too. On \
        Windows a `command`'s exit code is the one passed to `exit`, 1 after an uncaught error or when the last \
        command is a failing native program or cmdlet, and 0 otherwise. $false is not a failure, so a bare \
        `Test-Path` holds at once; use `exit [int](-not (Test-Path ...))`. `screen_settled` holds once \
        the graphical display, captured every 500 ms, has stopped changing for `settle_secs`; `screen_text` holds \
        once OCR of the display, or of a region, matches a regex over the recognised lines joined by newlines, and \
        needs the server's display.ocr_models. Neither screen kind works in read-only mode; all screen conditions of \
        one wait read the same display head, and a failed capture, as on a VM without graphics, ends the call with \
        an error. Results: `matched` with the condition's index and the matched text (lowest index wins a tie; \
        console and job_output matches add `next`, console matches also `generation`, screen matches `screen`, and a \
        console match moves the session's wait cursor past it); `closed` when the VM stops or restarts or the \
        console session ends; `timed_out` with a resume point per console and job_output condition (pass it back as \
        `since`, with `generation` for a console, to keep matching output that arrived in between), each command's \
        running PID, and each screen_text condition's last OCR text, to loosen a pattern that missed. With \
        `screenshot: true`, a match attaches a screenshot and gives \
        its size as `screenshot`, or a `screenshot_note` when it could not be taken. A console condition without \
        `since` starts where the last console_write began, so write first, then wait. A wait never changes what \
        console_read returns. `transfer_done` (a copy that replied with a `transfer` ID has ended) must be the only \
        condition of its wait and takes no screenshot; it matches with the copy's own reply as `text`, or its \
        failure as `error`, and times out with the bytes moved so far. It answers even after the VM stopped."
    )]
    async fn wait_for(
        &self,
        Parameters(p): Parameters<WaitForParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let ct = ctx.ct.clone();
        let progress = ctx
            .meta
            .get_progress_token()
            .map(|token| (token, ctx.peer.clone()));
        let runtime = tokio::runtime::Handle::current();
        self.blocking(move |s| {
            let max = s.hv.config.limits.max_wait_secs;
            let timeout = Duration::from_secs(p.timeout_secs.unwrap_or(max).min(max));
            let notify = |waited: Duration| {
                if let Some((token, peer)) = &progress {
                    let mut param =
                        ProgressNotificationParam::new(token.clone(), waited.as_secs_f64());
                    param.total = Some(timeout.as_secs_f64());
                    let peer = peer.clone();
                    runtime.spawn(async move {
                        let _ = peer.notify_progress(param).await;
                    });
                }
            };
            // A lone transfer_done is answered from the transfer table before the VM is looked up, so a copy cut off
            // by a stopped, expired or deleted VM still reports how it ended.
            if let [wait::Condition::TransferDone { transfer }] = p.conditions.as_slice() {
                if p.screenshot == Some(true) {
                    bail!("transfer_done takes no screenshot attachment");
                }
                let result = transfer::transfer_done(
                    &s.transfers,
                    &p.vm,
                    transfer,
                    timeout,
                    &|| ct.is_cancelled(),
                    &notify,
                )?;
                return Ok(vec![ContentBlock::text(result.to_string())]);
            }
            let checks = wait::validate_conditions(&s.hv.config, timeout, &p.conditions)?;
            let attach = attachment_wanted(&s.hv.config, p.screenshot)?;
            let dom = s.hv.domain(&p.vm)?;
            let recogniser = if checks.iter().any(wait::Check::reads_text) {
                Some(s.recogniser()?)
            } else {
                None
            };
            let env = wait::LiveEnv {
                consoles: &s.consoles,
                vm: &p.vm,
                cancelled: Box::new(|| ct.is_cancelled()),
                progress: Box::new(notify),
                recogniser,
            };
            let started = std::time::Instant::now();
            let guest: Arc<dyn wait::Guest> = Arc::new(wait::LiveGuest(dom));
            let outcome = wait::run(&env, guest.clone(), checks, timeout)?;
            let mut result = outcome.to_json(started.elapsed());
            let attachment = if attach {
                wait::attach(&env, &guest, &outcome, started + timeout)
            } else {
                None
            };
            if let Some(attachment) = &attachment {
                attachment.describe(&mut result);
            }
            // The JSON goes first, so a client reading the first text block finds it.
            let mut content = vec![ContentBlock::text(result.to_string())];
            if let Some(wait::Attachment::Image { png, .. }) = attachment {
                content.push(ContentBlock::image(B64.encode(png), "image/png"));
            }
            Ok(content)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Start a long-running command in a Linux or Windows guest as a background job, and return its \
        job ID at once. The job runs detached from the guest agent (on Linux through systemd-run, or setsid without \
        systemd; on Windows through WMI), so it survives an agent restart and a server restart. Its stdout and stderr \
        go to one log; read it with job_read, stop it with job_stop, and find jobs again with job_list. A reset or \
        deletion of the VM removes its jobs. On Linux the command runs with /bin/sh as root; where the guest's env \
        lacks --default-signal (busybox, coreutils before 9), the job starts with SIGINT and SIGQUIT ignored, and \
        under systemd SIGPIPE keeps its default action, so pipelines such as `cmd | head` end quietly. On Windows \
        the command is a PowerShell script run as SYSTEM in session 0, without a desktop, and its output is UTF-8."
    )]
    async fn job_start(
        &self,
        Parameters(p): Parameters<JobStartParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let agent = s.agent(&dom);
            let os = agent.os()?;
            let (id, how) = jobs::start(&agent, os, &p.command)?;
            Ok(vec![json_block(
                &json!({ "job": id.hyphenated().to_string(), "started": how }),
            )?])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Read a job's output from a byte offset, with its state: starting, running, exited (with \
        exit_code), or \"ended without status\" when its process is gone without an exit code, as after a reboot or \
        a forced stop. Pass `next` back as `since` to read on."
    )]
    async fn job_read(
        &self,
        Parameters(p): Parameters<JobReadParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let id = jobs::parse_id(&p.job)?;
            let dom = s.hv.domain(&p.vm)?;
            let agent = s.agent(&dom);
            let os = agent.os()?;
            let max = p.max_bytes.unwrap_or(s.hv.config.limits.max_output_bytes);
            let r = jobs::read(&agent, os, &id, p.since, max)?
                .ok_or_else(|| anyhow!("no job {id} in this guest"))?;
            let meta = json!({
                "job": p.job,
                "state": r.state.name(),
                "exit_code": r.state.exit_code(),
                "next": r.next,
            });
            Ok(vec![
                ContentBlock::text(meta.to_string()),
                ContentBlock::text(String::from_utf8_lossy(&r.data).into_owned()),
            ])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Stop a running job. On Linux: SIGTERM to its process group, which still records the exit \
        code, or SIGKILL with force. On Windows: its process tree ends at once (taskkill /T /F), with or without \
        force, and the job reads \"ended without status\"; processes it detached from its tree keep running. Refused \
        when the job's process is no longer the one it started."
    )]
    async fn job_stop(
        &self,
        Parameters(p): Parameters<JobStopParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let id = jobs::parse_id(&p.job)?;
            let dom = s.hv.domain(&p.vm)?;
            let agent = s.agent(&dom);
            let os = agent.os()?;
            Ok(vec![ContentBlock::text(jobs::stop(
                &agent, os, &id, p.force,
            )?)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List the jobs in a guest with their state and start time (seconds since the epoch)."
    )]
    async fn job_list(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let agent = s.agent(&dom);
            let os = agent.os()?;
            Ok(vec![json_block(&jobs::list(&agent, os)?)?])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Take a screenshot of the VM's graphical display (PNG). The reply gives the image's size as \
            WxH. With `region`, it returns only that part of the screen, at full resolution: give the region in pixels \
            of a full-screen screenshot, with that image's size as image_width and image_height. A region image's \
            pixels are not click coordinates; the reply gives the region's origin and size in the full-screen image."
    )]
    async fn screenshot(
        &self,
        Parameters(p): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let region = p.region.map(|r| r.check()).transpose()?;
            let max_width = match p.max_width.unwrap_or(1280) {
                0 => None,
                w => Some(w),
            };
            let frame = display::capture(&dom, p.screen)?;
            let crop = region
                .map(|r| r.on_frame(frame.width(), frame.height()))
                .transpose()?;
            let (png, w, h) = display::encode_png(&frame, crop, max_width)?;
            let text = match region {
                None => format!("{w}x{h}"),
                Some(r) => format!(
                    "{w}x{h} image of the {r}. Its pixels are not click coordinates: use coordinates in \
                     the full-screen image."
                ),
            };
            Ok(vec![
                ContentBlock::image(B64.encode(png), "image/png"),
                ContentBlock::text(text),
            ])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Press key combinations on the VM's virtual keyboard, in order (e.g. ctrl+alt+delete, win+r, enter)."
    )]
    async fn send_keys(
        &self,
        Parameters(p): Parameters<SendKeysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let combos: Vec<Vec<u32>> = p
                .keys
                .iter()
                .map(|k| display::parse_combo(k))
                .collect::<Result<_>>()?;
            for combo in &combos {
                display::send_combo(&dom, combo, p.hold_ms.unwrap_or(50))?;
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(vec![ContentBlock::text(format!(
                "sent {} key combo(s)",
                combos.len()
            ))])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Type text on the VM's virtual keyboard into whatever has focus (graphical or text console). \
        Assumes the guest uses a US keyboard layout."
    )]
    async fn type_text(
        &self,
        Parameters(p): Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            display::type_text(
                &dom,
                &p.text,
                Duration::from_millis(p.delay_ms.unwrap_or(20)),
            )?;
            Ok(vec![ContentBlock::text(format!(
                "typed {} characters",
                p.text.chars().count()
            ))])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Move, click, double-click, drag or scroll the VM's mouse, through QEMU's input-send-event on \
            its USB tablet. Give x and y in pixels of a full-screen image from `screenshot` without a region or from a \
            wait's screenshot attachment, with that image's size as image_width and image_height; a region image's \
            pixels are not click coordinates. A drag also takes to_x and to_y, a scroll a direction. A VM without a \
            USB tablet is refused; VMs from create_vm and create_vm_from_template have one. libvirt marks a domain \
            that receives QEMU monitor commands as tainted (custom-monitor)."
    )]
    async fn mouse(
        &self,
        Parameters(p): Parameters<mouse::MouseParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let steps = mouse::plan(&p)?;
            mouse::require_tablet(&dom, &p.vm)?;
            mouse::send(&dom, &steps)?;
            Ok(vec![ContentBlock::text(format!(
                "sent the {} at ({}, {}) as {} input-send-event command(s)",
                p.action.name(),
                p.x,
                p.y,
                steps.len()
            ))])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        ),
        description = "Create a new VM (shut off; start it with start_vm). It gets a new qcow2 system disk, an empty \
        or ISO-loaded SATA CD-ROM it boots from after the disk, a virtio NIC, the QEMU guest agent channel, a serial \
        console and a SPICE display. The guest agent must still be installed inside the guest. Limits and allowed \
        networks are named in the server instructions."
    )]
    async fn create_vm(
        &self,
        Parameters(p): Parameters<CreateVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let msg = manage::create(
                &s.hv,
                manage::NewVm {
                    name: p.name,
                    windows: p.os == OsKind::Windows,
                    vcpus: p.vcpus,
                    memory_mib: p.memory_mib,
                    disk_gib: p.disk_gib,
                    uefi: p.firmware == Firmware::Uefi,
                    tpm: p.tpm,
                    iso: p.iso,
                    driver_iso: p.driver_iso,
                    network: p.network,
                },
            )?;
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List the templates create_vm_from_template can make VMs from, with how many VMs this server \
        made from each."
    )]
    async fn list_templates(&self) -> Result<CallToolResult, ErrorData> {
        self.blocking(|s| Ok(vec![json_block(&manage::list_templates(&s.hv)?)?]))
            .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        ),
        description = "Create a disposable VM from a template in seconds: a thin copy-on-write disk on the template, \
        the guest agent already installed, and the VM's name as its hostname. By default it starts the VM and waits \
        for the guest agent. With lifetime_minutes the server deletes the VM, disks included, when it expires. \
        delete_vm removes it earlier."
    )]
    async fn create_vm_from_template(
        &self,
        Parameters(p): Parameters<CreateFromTemplateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let name = p.name.clone();
            let mut msg = manage::create_from_template(
                &s.hv,
                manage::NewTemplateVm {
                    name: p.name,
                    template: p.template,
                    vcpus: p.vcpus,
                    memory_mib: p.memory_mib,
                    disk_gib: p.disk_gib,
                    network: p.network,
                    lifetime_minutes: p.lifetime_minutes,
                },
            )?;
            if !p.start.unwrap_or(true) {
                msg.push_str("; it is shut off, start it with start_vm");
                return Ok(vec![ContentBlock::text(msg)]);
            }
            // The VM exists from here on: a failure to start or to reach the agent is reported, not undone.
            match s.hv.start(&name) {
                Ok(_) => msg.push_str("; started"),
                Err(e) => {
                    msg.push_str(&format!(
                        "; the VM was created but could not be started ({e:#}); fix the cause and use start_vm, \
                         or delete_vm"
                    ));
                    return Ok(vec![ContentBlock::text(msg)]);
                }
            }
            msg.push_str(&agent_wait_report(s, &name, p.wait_for_agent_secs));
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Return a VM made by create_vm_from_template to its template's state: it is powered off, its \
        disks, snapshots and attached disks are replaced by fresh ones from the template, and it starts again with \
        the same name, MAC address, hardware and expiry. Everything written to its disks is lost. VMs made by \
        create_vm cannot be reset."
    )]
    async fn reset_vm(
        &self,
        Parameters(p): Parameters<ResetVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let mut msg = manage::reset(&s.hv, &p.vm, || {
                s.consoles.close(&p.vm);
            })?;
            if let Err(e) = manage::start_after_reset(&s.hv, &p.vm) {
                msg.push_str(&format!(
                    "; it could not be started ({e:#}); fix the cause, then use reset_vm again, which also \
                     resets a UEFI VM's firmware variables, or delete_vm"
                ));
                return Ok(vec![ContentBlock::text(msg)]);
            }
            msg.push_str("; started");
            msg.push_str(&agent_wait_report(s, &p.vm, p.wait_for_agent_secs));
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Delete a shut-off VM with managed: true in list_vms, including its snapshots, NVRAM and TPM \
        state. Its disks in the server's disk pool are deleted; other disks and ISOs are left in place. Cannot be undone."
    )]
    async fn delete_vm(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let msg = manage::delete(&s.hv, &p.vm)?;
            s.consoles.close(&p.vm);
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "Show the configuration of a VM with managed: true in list_vms: vCPUs, memory, firmware, TPM, \
        disks and CD-ROMs (with targets), and network interfaces (with MACs)."
    )]
    async fn describe_vm(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(vec![json_block(&manage::describe(&s.hv, &p.vm)?)?]))
            .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Change the vCPU count and/or memory of a managed VM. Changes the persistent configuration, \
        so a running VM picks them up after it is shut down and started again."
    )]
    async fn configure_vm(
        &self,
        Parameters(p): Parameters<ConfigureVmParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            Ok(vec![ContentBlock::text(manage::set_resources(
                &s.hv,
                &p.vm,
                p.vcpus,
                p.memory_mib,
            )?)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        ),
        description = "Create a new empty qcow2 disk in the server's disk pool and attach it to a managed VM as the \
        next free virtio disk (vdb, vdc, ...). Also works while the VM is running."
    )]
    async fn attach_disk(
        &self,
        Parameters(p): Parameters<AttachDiskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            Ok(vec![ContentBlock::text(manage::attach_disk(
                &s.hv, &p.vm, p.size_gib,
            )?)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Detach a disk from a shut-off managed VM and DELETE its volume if it is in the server's disk \
        pool (disks elsewhere are only detached). Use describe_vm to find the target."
    )]
    async fn detach_disk(
        &self,
        Parameters(p): Parameters<DetachDiskParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            Ok(vec![ContentBlock::text(manage::detach_disk(
                &s.hv, &p.vm, &p.target,
            )?)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Insert an ISO (from list_isos) into a managed VM's CD-ROM drive, or eject it by omitting iso. \
        Works while the VM is running. A shut-off VM without a CD-ROM drive gets one added."
    )]
    async fn set_cdrom(
        &self,
        Parameters(p): Parameters<SetCdromParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let msg = manage::set_cdrom(&s.hv, &p.vm, p.target.as_deref(), p.iso.as_deref())?;
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        ),
        description = "Add a virtio network interface on an allowed libvirt network to a managed VM (also live)."
    )]
    async fn attach_nic(
        &self,
        Parameters(p): Parameters<AttachNicParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            Ok(vec![ContentBlock::text(manage::attach_nic(
                &s.hv, &p.vm, &p.network,
            )?)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Remove a network interface, identified by MAC address (see describe_vm), from a managed VM."
    )]
    async fn detach_nic(
        &self,
        Parameters(p): Parameters<DetachNicParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            Ok(vec![ContentBlock::text(manage::detach_nic(
                &s.hv, &p.vm, &p.mac,
            )?)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List the ISO images on the host that create_vm and set_cdrom may use."
    )]
    async fn list_isos(&self) -> Result<CallToolResult, ErrorData> {
        self.blocking(|s| Ok(vec![json_block(&manage::list_isos(&s.hv.config.manage))?]))
            .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        ),
        description = "Create a snapshot of a managed VM. For a running VM it includes the memory state, so \
        reverting resumes exactly where it was."
    )]
    async fn snapshot_create(
        &self,
        Parameters(p): Parameters<SnapshotCreateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let msg = manage::snapshot_create(&s.hv, &p.vm, &p.snapshot, p.description.as_deref())?;
            Ok(vec![ContentBlock::text(msg)])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true
        ),
        description = "List the snapshots of a managed VM, oldest first."
    )]
    async fn snapshot_list(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(vec![json_block(&manage::snapshot_list(&s.hv, &p.vm)?)?]))
            .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        ),
        description = "Revert a managed VM to a snapshot. Changes made since the snapshot are lost."
    )]
    async fn snapshot_revert(
        &self,
        Parameters(p): Parameters<SnapshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let msg = manage::snapshot_revert(&s.hv, &p.vm, &p.snapshot)?;
            // A revert replaces the guest's state under the console, running or not, so the session ends here.
            let closed = if s.consoles.close(&p.vm) {
                "; its console session was closed, so open a new one"
            } else {
                ""
            };
            Ok(vec![ContentBlock::text(format!("{msg}{closed}"))])
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        ),
        description = "Delete a snapshot of a managed VM. The VM itself is not changed."
    )]
    async fn snapshot_delete(
        &self,
        Parameters(p): Parameters<SnapshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            Ok(vec![ContentBlock::text(manage::snapshot_delete(
                &s.hv,
                &p.vm,
                &p.snapshot,
            )?)])
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VirtServer {
    fn get_info(&self) -> ServerConfig {
        let config = &self.state.hv.config;
        let mut extra = String::new();
        if config.read_only {
            extra.push_str(
                " The server is in READ-ONLY mode: only listing VMs, reading files/directories, reading the serial \
                 console, reading and listing existing jobs, and wait_for without command conditions are available.",
            );
        } else {
            if config.power_vms.is_empty() && !config.manage.enabled() {
                extra.push_str(" This server cannot start or stop VMs.");
            } else {
                extra.push_str(" start_vm/stop_vm can power VMs on and off where list_vms shows power_control: true.");
            }
            if !config.host_share_dirs.is_empty() {
                let dirs: Vec<String> = config
                    .host_share_dirs
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect();
                extra.push_str(&format!(
                    " copy_to_vm and copy_dir_to_vm can copy host files and directories from these directories into a guest: {}.",
                    dirs.join(", ")
                ));
            }
            if !config.host_pull_dirs.is_empty() {
                let dirs: Vec<String> = config
                    .host_pull_dirs
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect();
                extra.push_str(&format!(
                    " copy_from_vm and copy_dir_from_vm can copy guest files and directories to the host, into these pull directories only: {}.",
                    dirs.join(", ")
                ));
            }
            extra.push_str(&manage_instructions(&config.manage));
        }
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Access to libvirt/virt-manager VMs (Linux and Windows guests). Start with list_vms. \
                 exec, file and info tools need the QEMU guest agent running in the guest. console_* tools use the \
                 serial console; screenshot/send_keys/type_text use the graphical display. To wait for a prompt, a \
                 boot, a command or a job, use wait_for instead of sleeping and rereading: it returns on the first \
                 of several conditions, so list failure signs such as \"Kernel panic\" next to the success you \
                 expect. For work longer than an exec timeout, use job_start, then job_read or \
                 wait_for with job_done or job_output; job_list finds jobs again.{extra}"
            ))
    }
}

impl State {
    fn agent<'a>(&self, dom: &'a virt::domain::Domain) -> Agent<'a> {
        Agent::new(dom, self.hv.config.limits.agent_timeout_secs)
    }

    fn consoles_has(&self, vm: &str) -> bool {
        self.consoles.is_open(vm)
    }

    /// Runs a copy of `dom` as a transfer and waits for it up to `limits.max_wait_secs`. The reply is the copy's
    /// own once it finished in time, or the transfer's ID and progress while it still runs.
    fn copy(
        &self,
        vm: &str,
        dom: &virt::domain::Domain,
        work: impl FnOnce(&std::sync::atomic::AtomicU64) -> Result<String> + Send + 'static,
    ) -> Result<Vec<ContentBlock>> {
        let limits = &self.hv.config.limits;
        let owner = transfer::Owner {
            vm: vm.to_string(),
            uuid: dom.get_uuid_string().unwrap_or_default(),
        };
        let id = self.transfers.start(owner, limits.max_transfers, work)?;
        let wait = Duration::from_secs(limits.max_wait_secs);
        match self.transfers.wait(&id, vm, wait, &|| false, &|_| {})? {
            transfer::Status::Finished(Ok(reply)) => Ok(vec![ContentBlock::text(reply)]),
            transfer::Status::Finished(Err(error)) => Err(anyhow::anyhow!(error)),
            transfer::Status::Running { bytes } => Ok(vec![ContentBlock::text(
                transfer::running_reply(&id, bytes).to_string(),
            )]),
        }
    }

    /// The shared OCR recogniser, loaded on first use. A failed load is not kept, so the next call tries again once
    /// the models are fixed.
    fn recogniser(&self) -> Result<Arc<dyn wait::Recognise>> {
        let dir = self
            .hv
            .config
            .display
            .ocr_models
            .as_deref()
            .ok_or_else(|| anyhow!(wait::NO_OCR_MODELS))?;
        // The cache holds either nothing or a loaded recogniser, both valid, so a load that panicked leaves nothing to
        // repair and the next call tries again.
        let mut ocr = self.ocr.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(loaded) = &*ocr {
            return Ok(loaded.clone());
        }
        if let Some(download) = &self.ocr_download {
            download.ready()?;
        }
        let loaded = Arc::new(display::Recognizer::load(dir).with_context(|| {
            format!(
                "screen_text cannot load the OCR models in {}; scripts/fetch-ocr-models.sh downloads and checks them",
                dir.display()
            )
        })?);
        *ocr = Some(loaded.clone());
        Ok(loaded)
    }
}

/// Waits for the guest agent of a VM that create or reset just started, up to `wait_secs` (default 60, max 300).
/// Returns text to append to the result: the VM already exists, so a failure here is reported, never an error.
fn agent_wait_report(s: &State, vm: &str, wait_secs: Option<u64>) -> String {
    let wait = Duration::from_secs(wait_secs.unwrap_or(60).min(300));
    if wait.is_zero() {
        return String::new();
    }
    let dom = match s.hv.domain(vm) {
        Ok(dom) => dom,
        Err(e) => return format!("; the VM is not running now ({e:#})"),
    };
    let agent = Agent::new(&dom, 2);
    if agent::wait_for(wait, Duration::from_secs(1), || agent.ping()) {
        "; guest agent is responding".into()
    } else {
        "; guest agent not responding yet (the guest may still be booting)".into()
    }
}

fn manage_instructions(m: &crate::config::Manage) -> String {
    if !m.enabled() {
        return " This server cannot create, delete or reconfigure VMs.".into();
    }
    let mut ops = Vec::new();
    for (on, op) in [
        (m.create, "create"),
        (m.delete, "delete"),
        (m.reconfigure, "reconfigure"),
        (m.snapshots, "snapshot"),
    ] {
        if on {
            ops.push(op);
        }
    }
    let mut out = format!(
        " VM management: this server may {} VMs. Management tools act on VMs with managed: true in list_vms (VMs \
         this server created, which are also accessible and power-controllable, plus VMs the user listed). \
         Limits per VM: {} vCPUs, {} MiB memory, {} GiB per disk; at most {} VMs created by this server at once.",
        ops.join(", "),
        m.max_vcpus,
        m.max_memory_mib,
        m.max_disk_gib,
        m.max_vms,
    );
    if m.create || m.reconfigure {
        if m.networks.is_empty() {
            out.push_str(" No libvirt network is allowed, so VMs cannot get network interfaces.");
        } else {
            out.push_str(&format!(" Allowed networks: {}.", m.networks.join(", ")));
        }
        if m.iso_dirs.is_empty() {
            out.push_str(" No ISO directory is configured.");
        } else {
            out.push_str(" Use list_isos to find install/driver ISOs.");
        }
    }
    if m.create && !m.templates.is_empty() {
        out.push_str(&format!(
            " For a fresh VM in seconds, use list_templates and create_vm_from_template instead of create_vm; such a \
             VM may be given a lifetime of at most {} minutes, after which the server deletes it.",
            m.max_lifetime_minutes
        ));
        if m.delete {
            out.push_str(" reset_vm returns such a VM to its template's state.");
        }
    }
    out
}

fn json_block<T: serde::Serialize>(v: &T) -> Result<ContentBlock> {
    Ok(ContentBlock::text(serde_json::to_string_pretty(v)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_states_its_hints() {
        let tools = VirtServer::tool_router().list_all();
        assert_eq!(tools.len(), 44);
        let read_only_mode = hidden_tools(
            &toml::from_str(
                r#"
                read_only = true
                power_vms = ["*"]
                host_share_dirs = ["/srv"]
                host_pull_dirs = ["/pull"]
                [manage]
                create = true
                delete = true
                reconfigure = true
                snapshots = true
                [manage.templates.debian]
                volume = "tpl.qcow2"
                "#,
            )
            .unwrap(),
        );
        for tool in &tools {
            let name = tool.name.as_ref();
            let a = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no annotations"));
            let (Some(read_only), Some(destructive), Some(_)) =
                (a.read_only_hint, a.destructive_hint, a.idempotent_hint)
            else {
                panic!("{name} leaves a hint unset: {a:?}");
            };
            assert!(
                !(read_only && destructive),
                "{name} is read-only and destructive"
            );
            // Read-only mode also hides some reads, such as describe_vm, with their feature; it must hide every
            // tool that changes something, apart from attaching to and detaching from the console, and wait_for,
            // whose command condition read-only mode refuses.
            if !read_only && !["console_open", "console_close", "wait_for"].contains(&name) {
                assert!(
                    read_only_mode.contains(&name),
                    "{name} changes something but read_only mode shows it"
                );
            }
        }
        // A read-only hint lets a client run the tool without asking, so the list is fixed here: adding a tool to
        // it is a deliberate change, made after checking that no argument lets the tool change anything.
        let mut read_only: Vec<&str> = tools
            .iter()
            .filter(|t| t.annotations.as_ref().unwrap().read_only_hint == Some(true))
            .map(|t| t.name.as_ref())
            .collect();
        read_only.sort_unstable();
        assert_eq!(
            read_only,
            [
                "console_read",
                "describe_vm",
                "guest_info",
                "job_list",
                "job_read",
                "list_dir",
                "list_isos",
                "list_templates",
                "list_vms",
                "read_file",
                "screenshot",
                "snapshot_list",
                "stat_path",
                "vm_addresses",
            ]
        );
        // Input to the guest can run anything, such as `rm -rf /`; so can wait_for's command condition.
        for name in [
            "exec",
            "job_start",
            "console_write",
            "send_keys",
            "type_text",
            "mouse",
            "wait_for",
        ] {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            let a = tool.annotations.as_ref().unwrap();
            assert_eq!(a.destructive_hint, Some(true), "{name}");
        }
    }

    #[test]
    fn read_only_hides_job_start_and_stop_but_not_reads_or_waits() {
        let read_only = Config {
            read_only: true,
            ..Config::default()
        };
        let hidden = hidden_tools(&read_only);
        for tool in ["job_start", "job_stop", "exec", "console_write", "mouse"] {
            assert!(hidden.contains(&tool), "{tool} stays visible");
        }
        for tool in ["job_read", "job_list", "wait_for", "console_read"] {
            assert!(!hidden.contains(&tool), "{tool} is hidden");
        }
        let writable = hidden_tools(&Config::default());
        for tool in [
            "job_start",
            "job_stop",
            "job_read",
            "job_list",
            "wait_for",
            "mouse",
        ] {
            assert!(!writable.contains(&tool), "{tool} is hidden");
        }
    }

    #[test]
    fn read_only_mode_refuses_the_screenshot_attachment() {
        let read_only = Config {
            read_only: true,
            ..Config::default()
        };
        let err = attachment_wanted(&read_only, Some(true)).unwrap_err();
        assert!(
            err.to_string().contains("disabled in read-only mode"),
            "{err}"
        );
        assert!(!attachment_wanted(&read_only, Some(false)).unwrap());
        assert!(!attachment_wanted(&read_only, None).unwrap());
        assert!(attachment_wanted(&Config::default(), Some(true)).unwrap());
        assert!(!attachment_wanted(&Config::default(), None).unwrap());
    }

    #[test]
    fn wait_for_advertises_the_screen_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(WaitForParams)).unwrap();
        assert_eq!(
            schema["properties"]["screenshot"]["type"],
            json!(["boolean", "null"])
        );
        let text = schema.to_string();
        for field in [
            "screen_settled",
            "max_changed_pixels",
            "settle_secs",
            "region",
            "screen_text",
            "pattern",
        ] {
            assert!(text.contains(field), "{field} missing from {text}");
        }
    }

    #[test]
    fn mouse_requires_coordinates_with_their_image_size() {
        let schema = serde_json::to_value(schemars::schema_for!(mouse::MouseParams)).unwrap();
        let required = schema["required"].as_array().unwrap();
        for field in ["vm", "action", "x", "y", "image_width", "image_height"] {
            assert!(required.contains(&json!(field)), "{field} is optional");
        }
        assert_eq!(
            schema["$defs"]["Action"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["const"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["move", "click", "double_click", "drag", "scroll"]
        );
    }

    #[test]
    fn read_only_hides_management_tools_even_when_enabled() {
        let mut config: Config = toml::from_str(
            r#"
            [manage]
            create = true
            delete = true
            reconfigure = true
            snapshots = true
            [manage.templates.debian]
            volume = "tpl.qcow2"
            "#,
        )
        .unwrap();
        let managed = [
            "create_vm",
            "create_vm_from_template",
            "reset_vm",
            "delete_vm",
            "configure_vm",
            "snapshot_create",
            "snapshot_revert",
        ];
        let writable = hidden_tools(&config);
        for tool in managed {
            assert!(!writable.contains(&tool), "{tool} is hidden");
        }
        config.read_only = true;
        let hidden = hidden_tools(&config);
        for tool in managed {
            assert!(hidden.contains(&tool), "{tool} stays visible");
        }
    }

    #[test]
    fn copy_from_vm_shows_only_with_pull_directories() {
        assert!(hidden_tools(&Config::default()).contains(&"copy_from_vm"));
        let pulls = Config {
            host_pull_dirs: vec!["/pull".into()],
            ..Config::default()
        };
        assert!(!hidden_tools(&pulls).contains(&"copy_from_vm"));
    }
}
