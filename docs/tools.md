# Tools and how they reach the guest

## How it talks to the guest

| Capability | Mechanism | Guest requirement |
| -- | -- | -- |
| `exec`, `read_file`, `write_file`, `copy_to_vm`, `copy_dir_to_vm`, `copy_from_vm`, `copy_dir_from_vm`, `list_dir`, `stat_path`, `guest_info`, `job_*` | QEMU guest agent over a virtio-serial channel (`virDomainQemuAgentCommand`) | qemu-guest-agent running; jobs work on Linux and Windows guests |
| `wait_for` | The console session, the guest agent and the graphical display, polled together | as for the condition's channel; `screen_text` needs the OCR models |
| `vm_stats` | libvirt domain statistics (`virDomainListGetStats`), and the guest agent's `guest-get-fsinfo` for filesystems | none; the guest's own memory figures need a balloon device with a stats period and a balloon driver in the guest, and filesystems need qemu-guest-agent |
| `console_open/read/write/close` | Serial / virtio console stream (`virDomainOpenConsole`, like `virsh console`) | `<console>`/`<serial>` device + getty on it |
| `screenshot`, `send_keys`, `type_text` | Graphical display (`virDomainScreenshot`, `virDomainSendKey`) | A graphics device (SPICE/VNC) |
| `mouse` | QEMU monitor `input-send-event` (`virDomainQemuMonitorCommand`) | A graphics device and a USB tablet |
| `start_vm`, `stop_vm` | libvirt (`virDomainCreateWithFlags` paused, `virDomainOpenConsole`, `virDomainResume`, `virDomainShutdown`, `virDomainDestroy`) | none (graceful stop needs ACPI or the guest agent) |
| VM management tools | libvirt domain, storage volume and snapshot APIs | none |

`list_dir` and `stat_path` run fixed helper commands through `guest-exec`
(`stat`/`find` on Linux, PowerShell on Windows), because qemu-ga has no directory listing.
The path is passed as data, never spliced into shell code.

## Tools

| Tool | Read-only mode |
| -- | -- |
| `list_vms`: allowed VMs, power state, agent status, OS family | ✅ |
| `read_file`, `list_dir`, `stat_path` | ✅ |
| `vm_addresses`: a running VM's addresses per interface, from libvirt's DHCP lease, else the guest agent, else ARP | ✅ (lease and ARP only) |
| `console_open`, `console_read`, `console_close` | ✅ |
| `vm_events`: what happened to a VM since the server started (starts, stops, crashes, reboots, watchdog, guest agent, connection gaps), with what caused each | ✅ |
| `vm_stats`: a running VM's CPU, memory, disk, network and filesystem use, from two samples a second apart | ✅ (no filesystems) |
| `wait_for`: the first of several conditions (console pattern, guest agent up, command exits 0, job ended, job output pattern, screen settled, screen text, background copy finished, VM event), optionally attaching a screenshot (see [Screen and mouse](screen-and-input.md)) | ✅ (no `command` or screen conditions, no screenshot attachment) |
| `job_read`, `job_list`: a background job's output and state | ✅ |
| `job_start`, `job_stop`: run a long command detached in a Linux or Windows guest, and stop it | ❌ |
| `exec`: shell command (`sh`/`bash`/`cmd`/`powershell`) or program + args, with timeout | ❌ |
| `write_file` (overwrite/append, text or base64) | ❌ |
| `copy_to_vm`: copy a host file into the guest (only with `host_share_dirs`) | ❌ |
| `copy_dir_to_vm`: push a host directory into a Linux or Windows guest as a `tar` archive, following `.gitignore`, replacing a directory only with `replace` (only with `host_share_dirs`) | ❌ |
| `copy_from_vm`: copy a guest file into a pull directory on the host, mode 644, replacing a file only with `overwrite` (only with `host_pull_dirs`) | ❌ |
| `copy_dir_from_vm`: pull a Linux or Windows guest directory into a pull directory as a `tar` archive the server unpacks itself, modes 644 and 755, without links or special files, replacing a directory only with `overwrite` (only with `host_pull_dirs`) | ❌ |
| `start_vm`, `stop_vm` (graceful or `force`) (only for VMs in `power_vms`) | ❌ |
| `console_write`, optionally paced per byte or per chunk | ❌ |
| `guest_info`: OS, hostname, NICs, filesystems, users, timezone | ❌ |
| `screenshot` (PNG, downscaled to 1280 px wide by default) of the whole screen or a `region` | ❌ |
| `send_keys` (`ctrl+alt+delete`, `win+r`, …), `type_text` (US layout) | ❌ |
| `mouse`: move, click, double-click, drag or scroll at a point of a full-screen screenshot | ❌ |
| `create_vm`, `delete_vm` (only with `manage.create` / `manage.delete`) | ❌ |
| `list_templates`, `create_vm_from_template` (with `manage.create` and a template), `reset_vm` (also `manage.delete`) | ❌ |
| `configure_vm`, `attach_disk`, `detach_disk`, `set_cdrom`, `attach_nic`, `detach_nic` (only with `manage.reconfigure`) | ❌ |
| `snapshot_create`, `snapshot_list`, `snapshot_revert`, `snapshot_delete` (only with `manage.snapshots`) | ❌ |
| `describe_vm` (with any `manage` operation), `list_isos` (with create/reconfigure and `iso_dirs`) | ❌ |

In read-only mode the ❌ tools are not advertised at all. `start_vm`/`stop_vm` are also hidden while
`power_vms` is empty (unless VM management is on), `copy_to_vm` and `copy_dir_to_vm` while no host directory is
shared (`host_share_dirs` / `share_project_dir`), and `copy_from_vm` and `copy_dir_from_vm` while `host_pull_dirs` is
empty.
