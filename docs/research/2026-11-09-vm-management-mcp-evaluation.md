> Copied from the `agent-setup` repository, `docs/evaluations/2026-11-09-vm-management-mcp.md` at commit `18c23040`.
> In the text, "this repository" and `TODO.md` refer to `agent-setup`. Trial assets live in [`tests/testbed/`](../../tests/testbed/).
>
> Correction: agent-setup commit `f2e697c0` withdrew the first two defects below. The console never stalled:
> `console_read` ignored `wait_ms` on rereads, see [`2026-11-09-console-read-wait.md`](2026-11-09-console-read-wait.md).
> Closed-console reporting works; see Defect 2 in the fork roadmap. A second correction, `d346adb4`, withdrew the
> fourth defect: the `start_vm` guest had no guest agent, and the call was never timed. Of the four, only the
> `write_file` mode was a real defect, and the fork has fixed it. This copy keeps the original text.

# VM Management for Agents: Survey and KubeRl MCP Trial

Date: 2026-11-09. Subjects:
[`KubeRl/MCP-Server-For-VirtManager-AI`](https://github.com/KubeRl/MCP-Server-For-VirtManager-AI) at
`f47d3c38d310daafcf910e49ee19fa4ed9288526` (GPL-2.0, Rust, binary `virtmanager-mcp`), built from source with
`cargo build --release --locked` on rustc 1.99.1, and [`Neanderthal/mcp-qemu-vm`](https://github.com/Neanderthal/mcp-qemu-vm)
at `bc2bc77e2b87f479012a7e619245f6e611ce9613` (MIT, Python), read but not run. Host: Ubuntu 26.04.1, libvirt 12.0.0,
QEMU 10.2.1, virt-install 5.1.0, a Wayland desktop, and a user in the `libvirt` group.

The user wants agents to create, snapshot and destroy throwaway test VMs on this host, which has QEMU, libvirt and
virt-manager. They named the two subjects, and a research lane surveyed the alternatives.

Verdict: **KubeRl adopted, through a fork.** Its design is the strongest found, and the trial confirmed it works. Four
defects need fixing first, the serial console above all. The fork is
[`jonatsu/virt-manager-mcp`](https://github.com/jonatsu/virt-manager-mcp), which tracks that work; this record holds
the evidence.

## Candidates

**KubeRl** talks to libvirt through its C API, using the Rust `virt` crate, not by running `virsh`. It reaches inside
the guest through the QEMU guest agent (command execution and files), the serial console, and the graphical display
(screenshots and key input). Its guardrails are all set by the operator in a config file and default to deny:

- No VM is visible until `allowed_vms` lists it.
- Create, delete, reconfigure and snapshots are enabled one at a time, and `read_only` hides every mutating tool.
- A VM the server creates carries an owner element in its libvirt metadata. Management tools act only on owned VMs
  and on VMs listed in `manage_vms`. No tool writes metadata, so an agent cannot claim another VM.
- `delete_vm` refuses a running VM, and it deletes only the disks that are volumes in the configured `disk_pool`.
  `attach_disk` only creates new volumes, and ISO paths must sit inside `iso_dirs`, so an owned VM only ever holds
  its own disks.
- `create_vm` refuses an existing name, and limits cap the VM count, vCPUs, memory and disk size.
- VM names must match a strict character set, generated XML is escaped, and `list_dir` and `stat_path` pass the
  path as data, not as shell code.

It supports only `qemu:///system` in its documentation; `uri` is a config key, but session mode was not tested.
Membership of the `libvirt` group is effectively root on the host, so the server is built from reviewed source
rather than run from the upstream release binary.

**Neanderthal** does not manage VMs at all. It connects over SSH to one VM that the user has already set up and drives
that guest's desktop with `xdotool`, `scrot` and text recognition. The guest must run an X11 session, which many
current distributions no longer ship. It disables SSH host-key checking by default (`known_hosts=None` in
`connect_ssh`) and commits about 68 KB of log files under `logs/`. It is a desktop computer-use tool, not a fit for
this need. KubeRl's screenshots and key input go through libvirt at the virtual display device instead, so they work
whatever display server the guest runs.

## Survey

A research lane searched the official MCP registry, GitHub and project trees. It did not search Glama, Smithery or
mcp.so, and four GitHub queries hit the rate limit. Every libvirt MCP server it found had 0 to 11 stars, so popularity
cannot separate them. Facts marked "read" were read from source, LICENSE files or the GitHub API on 2026-11-09.

| Candidate                                                                     | Kind            | Mechanism and guardrails                                                                                                                                                                                 | Licence (read)  |
| ----------------------------------------------------------------------------- | --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------- |
| [lima-vm/lima](https://github.com/lima-vm/lima)                               | CLI (`limactl`) | Runs QEMU itself, without libvirt, so it cannot reach virt-manager VMs. Create, start, stop, delete, shell, copy and snapshots. Its own MCP server has only in-guest file and shell tools, no lifecycle. | Apache-2.0      |
| [virt-lightning](https://github.com/virt-lightning/virt-lightning)            | CLI             | libvirt-python, cloud images, SSH. `down` deletes only domains tagged with its own metadata context (`api.py` lines 417 to 427). Default `qemu:///system`. No snapshots seen.                            | Apache-2.0      |
| [teemtee/testcloud](https://github.com/teemtee/testcloud)                     | CLI             | libvirt-python plus a few `shell=True` calls. System or session URI by flag. Removing a running instance needs `--force`. Disables SSH host-key checks.                                                  | GPL-2.0         |
| [akarasulu/nested-mcp-server](https://github.com/akarasulu/nested-mcp-server) | MCP             | Best guardrail design found: mutations off by default, per-domain allowlists, a required `mcp_test_` name prefix, a role-based tool allowlist, a QMP allowlist and an audit log.                         | No licence file |
| [kimlherme/qemu-lab-mcp](https://github.com/kimlherme/qemu-lab-mcp)           | MCP             | Hard-coded `qemu:///session`, one fixed VM, checksum-verified Ubuntu image, pinned SSH host keys. `virsh` called with an argument list.                                                                  | No licence file |
| [rzippert/libvirt-mcp](https://github.com/rzippert/libvirt-mcp)               | MCP             | libvirt-python, read-only connection for read tools. Destructive tools take a `confirm=True` argument that the agent fills in itself, which protects nothing.                                            | AGPL-3.0        |

The lane rejected these, on evidence read from source:

- **kcli's MCP server** listens on `0.0.0.0` over HTTP with no authentication seen, and its delete tools have no
  guardrails.
- **testenv-vm**'s create call destroys and undefines any existing domain with the same name (`domain.go` lines 35
  to 38).
- **boxes-mcp** scans both the system and session URIs by default.
- **0xmortuex/qemu-mcp** accepts arbitrary QEMU `extra_args`.
- **moonlightelite/kvm_mcp** serves HTTP with CORS `*`.
- **vagrant-libvirt** has had no push since 2024-08.

**arighi/virtme-ng**'s `mcp.py` is kernel-test tooling, out of scope. Its tool descriptions tell the agent "do NOT
wait for user confirmation", an embedded directive that alone would rule it out.

The survey's guardrail synthesis has three parts. First, put agent VMs where the user's VMs are structurally
unreachable, on `qemu:///session` or outside libvirt. Second, tag owned VMs in libvirt metadata and check the tag
before any change. Third, let the operator, not the agent, enable each class of operation. KubeRl does the second and
third, and its `allowed_vms` list stands in for the first.

## Trial

The server ran over stdio under a small MCP client that sent a fixed list of tool calls. No agent configuration
changed. The config allowed one VM, capped created VMs at one and vCPUs at two, used a dedicated directory pool
`agent-vms` at `/var/lib/libvirt/images/agent-vms`, and left reconfiguration off. The guest was Debian 13, installed
unattended with a Debian preseed file that adds `qemu-guest-agent` and a serial getty.

| Area              | Result                                                                                                                                                                                  |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Command execution | Works. Exit code, stdout, stderr and stdin round-trip. A 3-second timeout killed `sleep 30`. The program-and-arguments form passed `$(id)` and `;rm -rf /` through as literal text.     |
| Files             | `write_file` (including append), `read_file` with a byte limit, and `stat_path` work. `list_dir` on `/tmp/it's a dir; touch /tmp/pwned` failed cleanly, and no file was created.        |
| Guest information | `guest_info` returns OS, hostname, filesystems, network interfaces and timezone.                                                                                                        |
| Snapshots         | Create, list, revert and delete work on a running VM. A revert removed a file written after the snapshot, and the VM came back running.                                                 |
| Lifecycle         | `start_vm`, `stop_vm` with `force`, `create_vm` and `delete_vm` work. Delete refused a running VM, then removed a created UEFI VM with its disk volume and NVRAM.                       |
| Guardrails        | Refused: a VM outside `allowed_vms`, without revealing whether it exists; a name with a space and `;`; four vCPUs; an existing name; a second created VM; an ISO path of `/etc/shadow`. |
| Display           | `screenshot`, `send_keys` (including Ctrl+Alt+Delete) and `type_text` work.                                                                                                             |
| Serial console    | Broken; see the first defect.                                                                                                                                                           |

## Defects

1. **Serial console reads stall after the first burst.** After `console_write` sent `root`, the guest's echo
   arrived, and then no further output did, not even the password prompt. Other libvirt calls in between did not
   help. `virsh console` on the same guest echoed, prompted and logged in. The reader in `src/console.rs`
   (`read_loop`) polls `virStreamRecv` on a non-blocking stream every 30 ms. `virsh` drives its stream from
   `virStreamEventAddCallback` instead; whether that difference is the cause is unverified.
2. **`console_read` does not report a closed console** after the VM powers off; `closed` stayed `null`.
3. **`write_file` creates world-writable files.** A new `/tmp/hello.txt` had mode `666`.
4. **`start_vm` on a running VM ignores `wait_for_agent_secs`.** It returned "already running" at once while the guest
   agent was not yet up.

`type_text` returns as soon as the keys are sent, so text typed during boot or a slow login is lost. An agent has to
check the screen between steps. A wait tool for this was proposed for the fork, alongside MCP tool annotations,
template-based disposable VMs and a tested `qemu:///session` mode.

## Guest-image findings

Two cloud-image attempts never applied their cloud-init user data: Debian 13 `genericcloud` and `generic`, each
started with `virt-install --cloud-init`. The hostname stayed `localhost`, so neither the user account nor the guest
agent existed. The cause was not found, and `libguestfs-tools` is not installed to read a stopped guest's disk. The
Debian 13 `nocloud` image rejected an empty root password on the graphical console. A preseed network install
(`virt-install --location` with `--initrd-inject`) worked first time.

Two traps cost time. virt-install's first boot of a cloud-init import powers the VM off on reboot, so a reboot
during that boot looks like a crash. And `virt-install` fails inside this repository with
`No module named 'gi'`, because the repository's mise Python shadows the system one; `TODO.md` tracks that.
