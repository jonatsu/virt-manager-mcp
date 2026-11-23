# virtmanager-mcp

virtmanager-mcp is a server for the [Model Context Protocol](https://modelcontextprotocol.io) (MCP), which lets AI
agents call tools. It gives coding agents guarded access to virtual machines on a Linux host that runs QEMU/KVM
through libvirt, with Linux and Windows guests. Agents can create disposable test VMs from templates or installer
ISOs, snapshot, reset and delete them, and fully control them. They can also work in other VMs on the host that the
operator opts in. Inside a VM, agents run commands and long jobs, read and write files, and copy files and
directories to and from the host. They also read the serial console, take screenshots, type and click, and see
crashes and resource use. The operator's config decides which VMs and host directories an agent can reach, and
every default denies, so the operator's other VMs stay out of reach.

This repository is a GPL-2.0 fork of
[KubeRl/MCP-Server-For-VirtManager-AI](https://github.com/KubeRl/MCP-Server-For-VirtManager-AI) that diverges from
upstream and takes upstream changes case by case. The host is Linux, and the guests are Linux and Windows.

## Goals

### Purpose

virtmanager-mcp gives coding agents access to deploy and manage virtual machines on a QEMU/KVM host managed by libvirt.
It gives agents tools to create, manage and fully control disposable test VMs, and also lets them control any other VM
on that same host that the operator opts in, when asked to. It does both without putting the operator's other VMs at
risk.

Safety and usability both count. Guardrails default to deny, and each one stays proportionate to the risk it
covers. A guardrail that makes an ordinary agent task impractical is a defect, just as a gap in one is. When the
two truly conflict, safety wins, and the design that settles the conflict shows how safety still holds.

### Scope

In scope:

- a Linux host running QEMU/KVM through libvirt, with Linux and Windows guests;
- the outcomes in the [roadmap](docs/plans/fork-roadmap.md);
- upstream changes, taken case by case.

Out of scope:

- other hypervisors and other libvirt drivers, such as Xen or LXC, and cloud providers;
- a replacement for virt-manager's graphical interface;
- keeping up with upstream as an aim of its own;
- the roadmap's parked ideas, until an outcome takes them up.

The success signals, the method and the rules for changing these goals are on the [project goals](docs/goals.md) page.

## What agents can do

| Task | Tools | Needs |
| -- | -- | -- |
| Run commands, read and write files in a guest | `exec`, `read_file`, `write_file`, `list_dir`, `stat_path`, `guest_info`, `job_start` | the QEMU guest agent in the guest |
| Move files between host and guest | `copy_to_vm`, `copy_dir_to_vm`, `copy_from_vm`, `copy_dir_from_vm` | host directories in the config |
| Watch and drive a guest that has no agent | `console_open`, `console_read`, `console_write`, `screenshot`, `send_keys`, `type_text`, `mouse`, `wait_for` | a serial console or a graphics device |
| Inspect a VM | `list_vms`, `vm_addresses`, `vm_events`, `vm_stats` | none |
| Start and stop VMs | `start_vm`, `stop_vm` | the VM in `power_vms` |
| Create, snapshot and delete VMs | `create_vm`, `create_vm_from_template`, `reset_vm`, `snapshot_create`, `delete_vm`, `configure_vm` | `[manage]` enabled in the config |

The [tools reference](docs/tools.md) lists every tool, the mechanism behind it, and what read-only mode hides.

## Install

Install a release build, or build from source. Either way, the host needs the libvirt client library at run time.

### From a release

Each `v*` tag publishes an x86_64 Linux tarball and its SHA-256 checksum on the
[releases page](https://github.com/jonatsu/virt-manager-mcp/releases). The binary links dynamically against the libvirt
and glibc of GitHub's `ubuntu-latest` runner. Download both files, check the checksum, and unpack the tarball:

```sh
version=0.3.0
name=virtmanager-mcp-$version-x86_64-linux-gnu
base=https://github.com/jonatsu/virt-manager-mcp/releases/download/v$version
curl -fLO $base/$name.tar.gz -O $base/$name.tar.gz.sha256
sha256sum -c $name.tar.gz.sha256
tar -xzf $name.tar.gz
```

The unpacked directory holds the `virtmanager-mcp` binary, `config.example.toml`, `scripts/`, `LICENSE`, `README.md`
and `CHANGELOG.md`. Copy the binary to a directory on your `PATH`.

### From source

Install Rust 1.94 or later and the libvirt development files: `libvirt-dev` on Debian and Ubuntu, `libvirt-devel` on
Fedora, or `libvirt` on Arch. Then build the binary:

```sh
cargo build --release --locked   # binary: target/release/virtmanager-mcp
```

## Quick start

These steps take a VM you already have to an agent that can use it. They assume Claude Code and a local
`qemu:///system` libvirt.

1. Give your user access to libvirt, then log in again. Membership of the `libvirt` group is effectively root on the
   host.

   ```sh
   sudo usermod -aG libvirt $USER
   ```

2. Install and start the QEMU guest agent in the VM. [Guest setup](docs/guest-setup.md) shows how for Linux and Windows.

3. Write a config file that lists the VM. No VM is accessible until it is listed.

   ```sh
   mkdir -p ~/.config/virtmanager-mcp
   printf 'allowed_vms = ["my-vm"]\npower_vms = ["my-vm"]\n' > ~/.config/virtmanager-mcp/config.toml
   ```

4. Register the server with your MCP client. It talks to the client over stdio.

   ```sh
   claude mcp add virtmanager -- /path/to/virtmanager-mcp
   ```

5. Ask the agent to list your VMs. The server answers with its `list_vms` tool.

[MCP clients](docs/mcp-clients.md) covers other clients and the HTTP transport.

## Configuration

The server reads one TOML file. With `--config <file>`, that file must exist. Without it, the server reads
`$XDG_CONFIG_HOME/virtmanager-mcp/config.toml`, or `~/.config/virtmanager-mcp/config.toml` when `XDG_CONFIG_HOME` is
unset. If the default file is missing, the server logs a warning and runs with defaults, which make no VM accessible.
Every key is optional, and the server refuses a file with an unknown key.

[`config.example.toml`](config.example.toml) documents every key with its default. The main parts are:

- top-level keys: `uri`, `allowed_vms`, `power_vms`, `host_share_dirs`, `share_project_dir`, `host_pull_dirs` and
  `read_only`;
- `[manage]`: VM management, which is off by default, and `[manage.templates.<name>]` for disposable VMs;
- `[limits]`: timeouts and size caps, such as `exec_timeout_secs`, `max_output_bytes`, `max_wait_secs` and
  `max_pull_bytes`;
- `[display]`: `ocr_models`, the directory of text recognition models for the `screen_text` wait condition;
- `[http]`: `bind`, which defaults to `127.0.0.1:8765`, and `token`, which the HTTP transport requires.

The server reads these environment variables:

- `VIRTMANAGER_MCP_TOKEN`: the HTTP bearer token. It replaces `http.token` from the file.
- `VIRTMANAGER_OCR_DOWNLOAD=1`: downloads the OCR models in the background, unless `display.ocr_models` is set.
- `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME`: the locations of the config file and the downloaded OCR models.
  `HOME` also places the lock file `~/.cache/virtmanager-mcp/manage.lock`.
- `RUST_LOG`: the log verbosity, `info` by default. Logs go to stderr.

Command-line flags:

- `-c, --config <file>` and `-t, --transport <stdio|http>` choose the file and the transport. Stdio is the default.
- `--uri <uri>` and `--bind <address>` replace `uri` and `http.bind` from the file.
- `--allow <vm>`, `--allow-power <vm>` and `--share <dir>` add to `allowed_vms`, `power_vms` and `host_share_dirs`.
  Each repeats.
- `--share-project-dir` and `--read-only` switch their setting on. No flag switches either one off when the file
  turns it on.
- The `sweep` command deletes expired VMs made from templates once, then exits. It needs `manage.create` and a
  template in the config.
- `-V, --version` prints the version.

## Safety model

The config decides what an agent can reach, and every default denies.

- `allowed_vms` hides every VM it does not list, unless the list holds `"*"`.
- `power_vms` limits `start_vm` and `stop_vm` to the VMs it lists, which must also be in `allowed_vms`.
- VM management is off until the config enables each operation. Its tools act only on VMs the server created and on
  those in `manage_vms`.
- A VM carries the server's ownership marker only when a create tool made it. No tool adds the marker to an existing
  VM.
- Deletion removes only volumes in the configured `disk_pool`.
- Copies read only from `host_share_dirs` or the shared project directory, and write only into `host_pull_dirs`.
- `read_only` hides every tool that changes a VM or the host.

The HTTP transport sends its bearer token over plain `http://`, because the server has no TLS of its own. Keep the
default `127.0.0.1` bind, or put the server behind a TLS proxy. Requests without the matching token get `401`.

Only `qemu:///system` is tested.

## Documentation

- [Tools](docs/tools.md): every tool, how it reaches the guest, and what read-only mode hides.
- [Guest setup](docs/guest-setup.md): the guest agent and serial console for Linux and Windows guests.
- [File transfer](docs/file-transfer.md): host directories and the four copy tools, including Windows directories.
- [Screen and mouse](docs/screen-and-input.md): screenshots, keyboard and mouse input, and screen text recognition.
- [VM management](docs/vm-management.md): creating, reconfiguring, snapshotting and deleting VMs, and templates.
- [Agents' own test VMs](docs/how-to/agent-test-vms.md): let agents install and run VMs of their own while yours stay
  hidden.
- [MCP clients](docs/mcp-clients.md): client setup for stdio and HTTP, and instructions to give your agents.
- [Notes and limitations](docs/limitations.md): behaviour to know about, such as consoles, waits and jobs.

## Project

- [LICENSE](LICENSE): GPL-2.0.
- [CHANGELOG.md](CHANGELOG.md): what changed in each release.
- [CONTRIBUTING.md](CONTRIBUTING.md): how to send a change, the version rules and the release steps.
- [SECURITY.md](SECURITY.md): how to report a vulnerability.
- [docs/goals.md](docs/goals.md): the success signals, method and rules for changing the goals.
- [docs/plans/fork-roadmap.md](docs/plans/fork-roadmap.md): planned work.
