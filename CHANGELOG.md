# Changelog

Each release's section lists what changed for people who run the server and for the agents that use it.
[`CONTRIBUTING.md`](CONTRIBUTING.md) explains the version numbers.

## Unreleased

### Added

- Background jobs on Windows guests: `job_start`, `job_read`, `job_stop` and `job_list` run a PowerShell command as
  `SYSTEM`, started through WMI so it survives an agent restart, with UTF-8 output that can be read while the job
  runs. The job directory under `C:\ProgramData\virtmanager-mcp` must be owned by `SYSTEM` or `Administrators` and
  grant access to no one else, or `job_start` refuses. `job_stop` ends a Windows job's process tree at once, with or
  without `force`, and the job then reads "ended without status". While a reader holds a Windows job's log, up to
  64 MiB of its output waits in memory; beyond that, the log records how many bytes were dropped.
- `wait_for`'s `command`, `job_done` and `job_output` conditions work on Windows guests. The `command` condition runs
  its command with PowerShell, as `exec` does by default, and holds when it exits 0.
- `vm_addresses` lists a running VM's addresses per interface, without a guest agent where libvirt's DHCP server
  leased one. For each interface it prefers the lease, which the guest cannot forge, then the guest agent's answer,
  then the host's ARP table, and says which source answered. Read-only mode skips the guest agent.
- `copy_from_vm` copies a guest file into a pull directory on the host without passing it through the conversation.
  The file arrives with mode 644 and appears only once complete; missing parent directories are created with mode
  755, symlinks are never followed, an existing file is replaced only with `overwrite: true`, and a directory never
  is. `limits.max_pull_bytes` (default 1 GiB) caps one pull. Read-only mode hides it.
- The `host_pull_dirs` setting names host directories that pulls from a guest may write into. The server refuses to
  start when one overlaps a share directory, the shared project directory, an ISO directory or any libvirt storage
  pool, or sits on a filesystem without `renameat2` support. Each gets a private `.vmm-staging/` directory.
- `copy_dir_to_vm` pushes a host directory from a share into a Linux guest that has `tar`, such as a checkout. It
  follows `.gitignore` rules in the tree and in its parent directories inside the share, leaves `.git` out, and takes
  extra `exclude` patterns. A symlink goes along only when it stays inside the tree; other links, FIFOs, sockets and
  devices are skipped and named. An existing directory is replaced as a whole only with `replace: true`, never `/` or
  a directory directly below it, and a failed push leaves it in place. Read-only mode hides it.
- A `copy_to_vm` or `copy_from_vm` that outlasts `limits.max_wait_secs` goes on in the background and replies with
  a transfer ID instead of failing at the client's call timeout. `wait_for`'s `transfer_done` condition reports how
  it ended. `limits.max_transfers` (default 4) caps how many run at once.

### Fixed

- `exec` with PowerShell on Windows no longer fills stderr with CLIXML progress records such as "Preparing modules
  for first use".

## 0.2.0

The first release of this fork. It covers every change since upstream's `v1.0`, including upstream's VM management.

### Changed

- Tools state MCP annotations: `readOnlyHint`, `destructiveHint` and `idempotentHint`. Tools that send input to a
  guest, run commands, or replace state count as destructive, so a client's permission layer may now ask before
  `exec`, `console_write`, `send_keys`, `type_text`, `wait_for` and the management tools.
- `write_file` and `copy_to_vm` create files with mode 644, or the given `mode`, instead of the guest agent's 666.
  An existing file gets its mode before it is opened, so a failed `chmod` no longer empties it.
- `stop_vm` keeps the VM's console session, and `console_read` reports it closed. `console_open` with `force: true`
  replaces this server's own stale session.
- `console_read` with `wait_ms` waits for output it has not returned yet.

### Added

- VM management, off by default and enabled per operation under `[manage]`: `create_vm`, `delete_vm`,
  `configure_vm`, `attach_disk`, `detach_disk`, `set_cdrom`, `attach_nic`, `detach_nic`, `describe_vm`,
  `list_isos` and the snapshot tools.
- Disposable VMs from templates: `create_vm_from_template`, `reset_vm` and `list_templates`, with lifetimes that
  expire through a sweep in every server process or the `virtmanager-mcp sweep` command.
  `scripts/make-template.sh` builds a Debian 13 template.
- `wait_for`, which waits for console text, the guest agent, a command, a job, a settled screen or text on the
  screen, and can attach a screenshot of the match.
- Background jobs in Linux guests: `job_start`, `job_read`, `job_stop` and `job_list`.
- Paced `console_write`, console session generations, and a wait cursor.
- Region screenshots and the `mouse` tool.
- Screen text recognition with the `ocrs` models, set with `display.ocr_models`. `scripts/fetch-ocr-models.sh`
  fetches them.
- `max_vms` holds across every server process of one account, through a lock file.

### Fixed

- An unterminated OSC escape sequence no longer hides the console output after it.
- A slow `console_open` no longer stalls every wait.
- An expired VM whose disk cannot be deleted keeps its domain and its boot disk, so the next sweep finds it again.
- A refused `reset_vm` keeps the console open.
- `rmcp` 3.9.0 fixes RUSTSEC-2026-0461.
