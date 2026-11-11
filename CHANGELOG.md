# Changelog

Each release's section lists what changed for people who run the server and for the agents that use it.
[`CONTRIBUTING.md`](CONTRIBUTING.md) explains the version numbers.

## Unreleased

### Added

- Background jobs on Windows guests: `job_start`, `job_read`, `job_stop` and `job_list` run a PowerShell command as
  `SYSTEM`, started through WMI so it survives an agent restart, with UTF-8 output that can be read while the job
  runs. The job directory under `C:\ProgramData\virtmanager-mcp` must be owned by `SYSTEM` or `Administrators` and
  grant access to no one else, or `job_start` refuses. `job_stop` ends a Windows job's process tree at once, with or
  without `force`, and the job then reads "ended without status".

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
