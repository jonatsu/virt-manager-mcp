//! MCP tool definitions.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
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

use crate::agent::{self, Agent, ExecSpec, GuestOs, powershell};
use crate::console::{Consoles, Pacing};
use crate::hypervisor::{self, Hypervisor};
use crate::{ansi, display, fsops, manage, wait};

/// Time a console write may take beyond its pauses, for a slow guest to take the bytes.
const WRITE_MARGIN: Duration = Duration::from_secs(10);

/// Tools hidden in read-only mode.
const MUTATING_TOOLS: &[&str] = &[
    "guest_info",
    "exec",
    "write_file",
    "console_write",
    "screenshot",
    "send_keys",
    "type_text",
    "start_vm",
    "stop_vm",
    "copy_to_vm",
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

#[derive(Deserialize, JsonSchema, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    /// /bin/sh on Linux, PowerShell on Windows.
    #[default]
    Default,
    Sh,
    Bash,
    Cmd,
    Powershell,
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
pub struct WaitForParams {
    pub vm: String,
    /// Seconds to wait, at most limits.max_wait_secs (default 50), which is also the default. 0 checks once.
    pub timeout_secs: Option<u64>,
    /// What to wait for, at most 16; the call returns on the first that holds.
    pub conditions: Vec<wait::Condition>,
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

// ---- tools --------------------------------------------------------------------

#[tool_router]
impl VirtServer {
    pub fn new(state: Arc<State>) -> Self {
        let mut tool_router = Self::tool_router();
        let config = &state.hv.config;
        if config.read_only {
            for name in MUTATING_TOOLS {
                tool_router.remove_route(name);
            }
        }
        // VMs created by this server can be powered on and off even when power_vms is empty.
        if config.power_vms.is_empty() && !config.manage.enabled() {
            tool_router.remove_route("start_vm");
            tool_router.remove_route("stop_vm");
        }
        if config.host_share_dirs.is_empty() {
            tool_router.remove_route("copy_to_vm");
        }
        let m = &config.manage;
        let off = |on: bool, tools: &'static [&'static str]| {
            if on && !config.read_only {
                &[][..]
            } else {
                tools
            }
        };
        for name in [
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
        ]
        .concat()
        {
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
        description = "Copy a file from the host into the guest via the QEMU guest agent. The file is streamed \
        directly by the server, so it can be large and never passes through the conversation. Only files inside the \
        shared host directories (named in the server instructions) can be copied."
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
            let n = s.agent(&dom).write_from(
                &p.guest_path,
                std::io::BufReader::new(file),
                false,
                None,
            )?;
            Ok(vec![ContentBlock::text(format!(
                "copied {n} bytes from {} to {}",
                src.display(),
                p.guest_path
            ))])
        })
        .await
    }

    #[tool(
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
            let off = s.hv.stop(&p.vm, p.force, wait)?;
            let msg = if off {
                // Keep any console session: its reader sees the stream end and records why, so a
                // later console_read reports the console closed instead of "no console open".
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

    #[tool(description = "Get metadata of a single file or directory in the guest.")]
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
        description = "Send text to an open serial console (by default followed by Enter). Returns the offset and \
        generation where the guest's answer starts; wait_for and console_read can start there. For a receiver that \
        drops input sent back to back, pace the write: inter_char_delay_ms pauses between bytes, or chunk_size with \
        chunk_delay_ms pauses between chunks. The pauses may total at most limits.max_wait_secs. One write runs at \
        a time per console; a second one is refused while the first is still sending."
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

    #[tool(description = "Detach from a VM's serial console.")]
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
        description = "Wait until the first of several conditions holds on a VM, and return which one. Kinds: \
        `console` (the open console session's output matches a regex; ANSI-stripped by default), `agent` (the guest \
        agent answers) and `command` (a shell command run through the guest agent exits 0; Linux guests, not in \
        read-only mode). Results: `matched` with the condition's index and the matched text (lowest index wins a \
        tie; console matches add `next` and `generation`, and move the session's wait cursor past the match); \
        `closed` when the VM stops or restarts or the console session ends; `timed_out` with a resume point per \
        console condition (pass it back as `since` and `generation` to keep matching output that arrived in \
        between) and each command's running PID. A console condition without `since` starts where the last \
        console_write began, so write first, then wait. A wait never changes what console_read returns."
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
            let checks = wait::validate_conditions(&s.hv.config, &p.conditions)?;
            let dom = s.hv.domain(&p.vm)?;
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
            let env = wait::LiveEnv {
                consoles: &s.consoles,
                vm: &p.vm,
                dom: dom.clone(),
                cancelled: Box::new(|| ct.is_cancelled()),
                progress: Box::new(notify),
            };
            let started = std::time::Instant::now();
            let outcome = wait::run(&env, Arc::new(wait::LiveGuest(dom)), checks, timeout)?;
            Ok(vec![ContentBlock::text(
                outcome.to_json(started.elapsed()).to_string(),
            )])
        })
        .await
    }

    #[tool(description = "Take a screenshot of the VM's graphical display (PNG).")]
    async fn screenshot(
        &self,
        Parameters(p): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| {
            let dom = s.hv.domain(&p.vm)?;
            let max_width = match p.max_width.unwrap_or(1280) {
                0 => None,
                w => Some(w),
            };
            let (png, w, h) = display::screenshot(&s.hv, &dom, p.screen, max_width)?;
            Ok(vec![
                ContentBlock::image(B64.encode(png), "image/png"),
                ContentBlock::text(format!("{w}x{h}")),
            ])
        })
        .await
    }

    #[tool(
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
        description = "List the templates create_vm_from_template can make VMs from, with how many VMs this server \
        made from each."
    )]
    async fn list_templates(&self) -> Result<CallToolResult, ErrorData> {
        self.blocking(|s| Ok(vec![json_block(&manage::list_templates(&s.hv)?)?]))
            .await
    }

    #[tool(
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

    #[tool(description = "List the ISO images on the host that create_vm and set_cdrom may use.")]
    async fn list_isos(&self) -> Result<CallToolResult, ErrorData> {
        self.blocking(|s| Ok(vec![json_block(&manage::list_isos(&s.hv.config.manage))?]))
            .await
    }

    #[tool(
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

    #[tool(description = "List the snapshots of a managed VM, oldest first.")]
    async fn snapshot_list(
        &self,
        Parameters(p): Parameters<VmParam>,
    ) -> Result<CallToolResult, ErrorData> {
        self.blocking(move |s| Ok(vec![json_block(&manage::snapshot_list(&s.hv, &p.vm)?)?]))
            .await
    }

    #[tool(
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

    #[tool(description = "Delete a snapshot of a managed VM. The VM itself is not changed.")]
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
                " The server is in READ-ONLY mode: only listing VMs, reading files/directories and reading the serial console are available.",
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
                    " copy_to_vm can copy host files from these directories into a guest: {}.",
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
                 serial console; screenshot/send_keys/type_text use the graphical display.{extra}"
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

fn shell_spec(os: GuestOs, shell: Shell, cmd: &str) -> Result<ExecSpec> {
    Ok(match (os, shell) {
        (GuestOs::Linux, Shell::Default | Shell::Sh) => ExecSpec::new("/bin/sh", ["-c", cmd]),
        (GuestOs::Linux, Shell::Bash) => ExecSpec::new("bash", ["-c", cmd]),
        (GuestOs::Windows, Shell::Default | Shell::Powershell) => powershell(&format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8\n{cmd}"
        )),
        (GuestOs::Windows, Shell::Cmd) => ExecSpec::new(
            "cmd.exe",
            ["/d", "/s", "/c", &format!("chcp 65001 >nul & {cmd}")],
        ),
        (GuestOs::Linux, Shell::Cmd | Shell::Powershell) => {
            bail!("this shell is not available on a Linux guest")
        }
        (GuestOs::Windows, Shell::Sh | Shell::Bash) => {
            bail!("this shell is not available on a Windows guest")
        }
    })
}

fn json_block<T: serde::Serialize>(v: &T) -> Result<ContentBlock> {
    Ok(ContentBlock::text(serde_json::to_string_pretty(v)?))
}
