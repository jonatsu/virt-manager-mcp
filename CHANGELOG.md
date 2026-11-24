# Changelog

Each release's section lists what changed for people who run the server and for the agents that use it.
[`CONTRIBUTING.md`](CONTRIBUTING.md) explains the version numbers.

## Unreleased

### Added

- `create_vm` takes `display`: `"spice"` (the default, as before), `"vnc"` or `"both"`. A VNC display listens on
  127.0.0.1 only, on a port libvirt picks at each start, with no password. `describe_vm` reports the display types
  (`displays`) and, for a running VM, the VNC address and port (`vnc`), and `start_vm` names the port in its reply. A
  desktop client such as DeskVNC can then reach a VM during an OS install and after it. `screenshot`, `send_keys`,
  `type_text` and `mouse` work with a VNC-only VM.
- `set_display` changes the display of a shut-off VM that the server may manage to `"spice"`, `"vnc"` or `"both"`. It
  rewrites only the `<graphics>` elements of the persistent definition, and needs `manage.reconfigure`. It refuses a
  running VM, a VM under a seal, and a display the server did not write: another type than SPICE or VNC, a password, a
  fixed port, or a listener other than loopback TCP.
- The guest setup guide shows how to enable RDP inside a Windows guest through `exec` and in a GNOME guest with
  `grdctl`. libvirt on the host offers no RDP display, so RDP comes from the guest.

### Changed

- Every management call that changes a VM now refuses a VM that a seal is copying into a template, and names the
  template: `start_vm`, `stop_vm`, `configure_vm`, `attach_disk`, `detach_disk`, `set_cdrom`, `attach_nic`,
  `detach_nic`, `snapshot_create`, `snapshot_revert`, `snapshot_delete`, `reset_vm`, `delete_vm` and the expiry sweep.
  The server marks a VM in its owner metadata (`sealing`) for the length of a seal, and clears a mark that a crashed
  seal left. A process of another account, or with another `HOME`, honours a mark for 24 hours, because it cannot see
  the seal's lock. Nothing writes a mark yet, so no VM is refused until sealing arrives. A mark that the server cannot
  read refuses the VM until you remove the `sealing` attribute from its owner element with `virsh edit`.
- `configure_vm`, `attach_nic`, `detach_nic`, `snapshot_create`, `snapshot_delete` and `stop_vm` now take the
  management lock, as the other changing calls do. While another call holds it, they wait up to 30 seconds and then
  fail with "another virtmanager-mcp operation has held the management lock". `stop_vm` holds it only while it asks
  for the shutdown, not while it waits for the guest.

## 0.5.0

### Added

- `secure_boot` takes `"setup"` besides `"on"` and `"off"`, in `create_vm` and in template entries. Setup mode gives
  a UEFI VM Secure Boot firmware with no keys enrolled, so the guest can enrol its own, as Lanzaboote and `sbctl`
  need. `true` and `false` still mean `"on"` and `"off"`, and `"off"` and `"setup"` with `firmware: "bios"` are
  refused. `describe_vm` and `list_templates` keep the boolean `secure_boot`, which reads false in setup mode, and
  add `secure_boot_mode` with `"off"`, `"on"` or `"setup"`. For `describe_vm` it is `null` when the firmware does not
  say.
- `create_vm` takes `extra_isos`, up to four more ISOs from `iso_dirs`. Each becomes a read-only SATA CD-ROM after
  the install and driver drives, and the VM never boots from it, so an answer-file ISO can sit beside the Windows
  installer and virtio-win. q35's SATA controller has six ports, and `create_vm` refuses a VM that would need more.
- `set_cdrom` on a shut-off VM adds a CD-ROM drive at a free target of the form `sd<letter>` that you name, such as
  `sdc`. It refuses a target that a disk uses, a name that is not `sd<letter>`, a running VM, and a seventh SATA
  device. Without a `target` it behaves as before.
- `create_vm` takes `hibernate`, which turns suspend to disk on and leaves suspend to memory off, so a guest can
  hibernate. A hibernated VM is shut off, and `start_vm` resumes it. `describe_vm` reports `hibernate`. libvirt's
  automatic firmware choice refuses suspend to disk, so a UEFI VM with `hibernate` is defined twice: first by
  automatic choice, then, with the same name, UUID and MAC address, with the firmware files libvirt chose written
  into its definition. A Linux guest with `secure_boot` `"on"` runs under kernel lockdown and refuses to hibernate;
  use `"off"` or `"setup"`. `describe_vm` still reports `secure_boot_mode` for such a VM.

### Changed

- A UEFI VM that `create_vm` or `create_vm_from_template` makes now keeps its firmware variables as `<name>.vars` in
  `disk_pool` (`manage.disk_pool`), not in libvirt's own `/var/lib/libvirt/qemu/nvram/`. libvirt creates the file at
  the VM's first start, so it appears in the pool after that start, owned by the QEMU user. `delete_vm` and expiry
  delete it with the VM, and `reset_vm` restores it from the firmware's template. UEFI VMs that exist keep their
  variables where they are.
- `create_vm` and `create_vm_from_template` refuse a UEFI VM whose `<name>.vars` already exists in the pool, a
  leftover of an earlier VM, because libvirt would reuse its variables. The error names the volume and the command
  that removes it, `virsh -c <uri> vol-delete --pool <disk_pool> <name>.vars`; nothing is created.
  The check rescans the pool first, and a rescan that fails now fails the create instead of passing on a stale listing.
- `create_vm` with `tpm: true` now keeps the VM's TPM 2.0 state as `<name>.tpm` in `disk_pool` (`manage.disk_pool`), not
  in libvirt's own `/var/lib/libvirt/swtpm/`. The server makes the state with `swtpm_setup` before it defines the VM,
  and makes the volume owned by the host's swtpm user with mode 0600, because libvirt starts swtpm as that user and
  does not change the owner of a pool file. `delete_vm` and expiry delete it with the VM. A VM that exists keeps its
  state where it is.
- `tpm: true` now needs the host to have `swtpm-tools` (`swtpm_setup`), the user `swtpm`, libvirt 10.10 or later, and
  a root-made AppArmor rule, because swtpm's stock profile allows writes only under `/var/lib/libvirt/swtpm/`.
  Add this line to `/etc/apparmor.d/local/usr.bin.swtpm`, with your pool's directory, and reload the profile:
  `owner /var/lib/libvirt/images/agent-vms/*.tpm rwk,`. Without the rule, `create_vm` still succeeds and `start_vm`
  is refused with a message that names the file and the line. `create_vm` also refuses a TPM while `<name>.tpm`
  already exists in the pool, and names the `virsh vol-delete` command that removes it.
- `delete_vm` and expiry now keep a VM's TPM state file or directory when its definition names a path outside
  `manage.disk_pool`, and say so in the reply, as they do for disks outside the pool. Before, libvirt deleted whatever
  path a `manage_vms` VM's `<tpm>` named. State in the pool, and libvirt's own state under `/var/lib/libvirt/swtpm/`,
  is still deleted with the VM.
- `create_vm` with `secure_boot` on, the default, now asks libvirt for firmware with Secure Boot and the enrolled keys,
  instead of leaving the choice to libvirt. VMs that exist keep their definition. A BIOS VM with `secure_boot` off is
  still refused, now with `secure_boot "off" and "setup" need firmware "uefi"`.
- `create_vm` and `create_vm_from_template` refuse the VM name `tpl`, which the server keeps for the names of
  template volumes. Name the VM anything else; `tpl2` and `my-tpl` are fine, and VMs already named `tpl` stay as they
  are.
- The first `start_vm`, `create_vm_from_template` or `reset_vm` start of a UEFI VM that the server made, with its
  variables in the pool, records the firmware code file libvirt chose and its SHA-256 in the VM's owner metadata
  (`firmware_code` and `firmware_sha256`). A VM that `virsh` or virt-manager started first, and a
  VM made before this change, carry no record. The record changes only the persistent definition.
- `create_vm` and `create_vm_from_template` also write the `secure_boot` mode into a UEFI VM's owner metadata, and
  `describe_vm` reports it. Before, `describe_vm` guessed setup from on by the variables template's file name for a
  VM with its firmware pinned by path (one that hibernates), which a template with another name defeats. VMs made
  earlier keep the guess.

## 0.4.0

### Changed

- `copy_from_vm` and `copy_dir_from_vm` refuse a destination in or at `console/` in the first pull directory, which
  now holds the console logs, so no pull can replace or delete a log. Pull into another path.

### Added

- With `host_pull_dirs` set, every console session the server opens also writes its output to
  `console/<vm>/<UTC start>-g<generation>.log` in the first pull directory, so a boot log survives `delete_vm`. The
  replies of `console_open` and of a boot name the file. The server never overwrites or deletes a log, and a log stops
  at `limits.console_log_bytes`, 64 MiB by default. A log that cannot be written leaves the capture running, and the
  reply says why. Read-only mode writes no logs.
- `manage.max_total_memory_mib` caps the memory of all the VMs the server created that run at once. A start that
  would pass it is refused and names the running VMs, and a VM larger than the cap is refused at create and
  reconfigure. Unset, the default, keeps today's behaviour.
- A how-to guide, `docs/how-to/agent-test-vms.md`, sets up a server whose agents create, install and delete VMs of
  their own from installer ISOs, while every existing VM stays hidden from them.

### Fixed

- With `allowed_vms` empty and VM management on, the server no longer warns at startup that no VM will be
  accessible. It logs instead that only the VMs it creates will be.

## 0.3.0

### Changed

- The example global instructions, `example-global-CLAUDE.md` and `example-global-AGENTS.md`, tell an agent to find
  out what happened before it retries: read a notice's records with `vm_events`, the panic text with `console_read`,
  and the VM's resource use with `vm_stats`.
- Every tool that takes arguments refuses one it does not know, with an error that names it, instead of ignoring it.
  The tools' input schemas say so with `additionalProperties: false`. A client that sends extra fields must stop
  sending them.
- `console_open` on a console this server already holds open attaches to it and replies with its generation, instead
  of reporting it open. `force: true` still replaces the session.
- `start_vm`, `create_vm_from_template` and `reset_vm` boot a VM paused, open its serial console, then resume it, so
  `console_read` from offset 0 holds the boot from its first byte, and a later `console_open` attaches to that
  capture. The reply says when the console could not be captured, for example because virt-manager holds it; the VM
  starts anyway. A second `start_vm` of a VM while its boot runs is refused.
- `snapshot_revert` to a snapshot of a running VM reverts paused, opens the console, then resumes, so `console_read`
  from offset 0 holds the guest's output from the snapshot on. The session before the revert moves to the previous
  run, and a revert while the VM is starting is refused.

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
  then the host's ARP table, and says which source answered. Read-only mode skips the guest agent. It works on Linux
  and Windows guests.
- `copy_from_vm` copies a guest file into a pull directory on the host without passing it through the conversation.
  The file arrives with mode 644 and appears only once complete; missing parent directories are created with mode
  755, symlinks are never followed, an existing file is replaced only with `overwrite: true`, and a directory never
  is. `limits.max_pull_bytes` (default 1 GiB) caps one pull. It works on Linux and Windows guests. Read-only mode
  hides it.
- The `host_pull_dirs` setting names host directories that pulls from a guest may write into. The server refuses to
  start when one overlaps a share directory, the shared project directory, an ISO directory or any libvirt storage
  pool, or sits on a filesystem without `renameat2` support. Each gets a private `.vmm-staging/` directory. Pull
  directories may nest, but a pull never replaces one, or a directory that holds one.
- `copy_dir_to_vm` pushes a host directory from a share into a Linux guest that has `tar`, or a Windows guest, such
  as a checkout. It follows `.gitignore` rules in the tree and in its parent directories inside the share, leaves
  `.git` out, and takes extra `exclude` patterns. A symlink goes along only when it stays inside the tree; other
  links, FIFOs, sockets and devices are skipped and named. An existing directory is replaced as a whole only with
  `replace: true`, never `/` or a directory directly below it, and a failed push leaves it in place. A Windows guest
  takes a drive path such as `C:\work\app`, never a UNC or drive-relative path, and never a drive root or, with
  `replace`, a directory directly below one. It unpacks with its own `tar.exe`: nothing arrives read-only, and a name
  outside the guest's code page fails the push.
  The push skips and names every symlink and every name Windows cannot hold, such as `CON`, `a?b` or a name that
  differs from another only in case. A `replace` fails, leaving the old tree, while a file in it is open. Read-only
  mode hides it.
- `copy_dir_from_vm` pulls a directory from a Linux guest that has `tar`, or a Windows guest, into a pull directory.
  A Linux guest packs it in `/var/tmp`. From either guest the server unpacks the archive itself: files arrive with
  mode 644 and directories with 755, links, devices and FIFOs are skipped and named, and an archive with absolute or
  `..` paths, paths over 4096 bytes or 256 levels, repeated paths or sparse files fails the pull, which then writes
  nothing.
  `exclude` takes gitignore patterns, applied on the host. An existing directory is replaced as a whole only with
  `overwrite: true`. `limits.max_pull_bytes` caps the archive and the files written, and the new
  `limits.max_pull_entries` (default 100,000) the entries, counting the directories their paths imply. A Windows
  guest packs with its own `tar.exe` in `%SystemRoot%\Temp`, which follows junctions; any non-zero exit of `tar.exe`
  fails the pull, and the archive can pass `limits.max_pull_bytes` by about a second of writing before it stops.
  Read-only mode hides it.
- A copy that outlasts `limits.max_wait_secs`, through any of the four copy tools, goes on in the background and
  replies with a transfer ID instead of failing at the client's call timeout. `wait_for`'s `transfer_done`
  condition reports how it ended. `limits.max_transfers` (default 4) caps how many run at once.
- `VIRTMANAGER_OCR_DOWNLOAD=1` in the server's environment downloads the OCR models with `curl` into
  `~/.local/share/virtmanager-mcp/ocr-models`, or under `$XDG_DATA_HOME`, while `display.ocr_models` is unset. The
  download runs in the background after startup and checks the same pinned sums as `scripts/fetch-ocr-models.sh`.
  Until it ends, `screen_text` replies that the models are still downloading, and after a failure it names the cause.
- `create_vm` takes `secure_boot`, true by default. False gives a UEFI VM firmware without Secure Boot, so an
  installer that is not signed boots; with `firmware: "bios"` it is refused. Template entries take the same
  `secure_boot` key. `list_templates` reports it, and `describe_vm` reports it from the VM's firmware, or `null`
  when the firmware does not say.
- VMs the server creates get a panic device (`isa` for Linux, `hyperv` for Windows), a balloon that reports guest
  memory every 5 s, and the crash action `manage.on_crash`: `preserve`, the default, keeps a crashed VM stopped in
  place until `stop_vm` with `force: true`, and `destroy` stops it. Any other value stops the server at start.
  `limits.max_events` (default 256) bounds the events kept for each VM. Existing VMs keep their XML.
- `vm_events` lists what happened to a VM since the server started: starts, stops, pauses, crashes, reboots,
  watchdog firings, the guest agent connecting and disconnecting, and gaps while the libvirt connection was lost.
  Each record names its cause: the tool that caused it, `expiry` for the server's own expiry sweep, or `outside`,
  which includes expiries by the `virtmanager-mcp sweep` command. It works on a VM in any state and in read-only
  mode, and takes `since` and `kinds`.
- `wait_for` takes an `event` condition with `kinds` and `since`, which matches a record of the VM's event log and
  returns it as `record`, with its `seq` as `next`. It is checked before the check for a stopped VM, and after a stop
  or a closed console the wait reads the log for 1 s more, so a crash wins over the stop it causes. A wait on events
  alone works on a VM in any state.
- A reply to any tool call that names a VM ends with a notice when something the agent should know happened to it
  outside any tool call: a crash, a stop, a reboot, a watchdog firing, a pause for an I/O error, or a gap in the
  events. The notice is its own text block after the result or error, names `vm_events since` for the details, and
  appears once per server process, in the first such reply from any client. A crash under `preserve` adds that the VM
  holds its memory until `stop_vm` with `force: true`.
- `vm_stats` reports a running VM's resource use from two samples about a second apart: CPU use as a percentage of
  its vCPUs, memory (with the guest's own figures when the balloon reports them), per-disk and per-interface rates,
  and the guest's filesystems from the guest agent, marked as the guest's claim. Missing guest memory figures or
  filesystems come with the reason: no balloon device, no stats period, no balloon driver in the guest, or no answer
  from the agent. A VM in any other state gets
  its state. Read-only mode keeps it and skips the guest agent.
- Each VM keeps the console output of one earlier run. When a new run replaces a session, or `console_close`,
  `reset_vm` or `snapshot_revert` ends one, `console_read` with `run: "previous"` reads its output, closed, with the
  reason it ended and its own generation. `console_read` and `console_close` work on a stopped VM.

### Fixed

- `job_stop` on Windows no longer fails with "taskkill failed" when the job's root process exits during the stop. The
  stop ends whatever is left of the tree it listed, including a child `taskkill` would have left running, and says
  which status `taskkill` returned.
- `exec` with PowerShell on Windows no longer fills stderr with CLIXML progress records such as "Preparing modules
  for first use".
- `exec` refuses `args` together with `command`, and runs nothing. It used to run the command line and drop the
  arguments silently. `args` goes with `program`.
- Windows directory pushes and pulls report a guest error that names a non-ASCII path, such as
  `C:\Windows\Temp\vmm-été`, with the path intact. The guest scripts wrote such errors in the console code page, and
  the path arrived with replacement characters.
- `exec` with PowerShell on Windows, and `wait_for`'s `command` condition there, report a PowerShell error on stderr
  as the text a console shows. It arrived as a `#< CLIXML` block.
- `exec` on a Windows guest refuses a `program` path or argument that is not ASCII, and suggests `command`, which
  passes any text. It also refuses an `env` entry that is not ASCII, with either `program` or `command`. The guest
  agent failed such calls with only `failed to parse JSON: invalid utf-8 string`.
- `copy_dir_from_vm` pulls a Windows directory whose path is not ASCII, such as `C:\Windows\Temp\vmm-été`. It failed
  with `failed to parse JSON: invalid utf-8 string`. A pull of a tree holding a name outside the guest's ANSI code page
  still fails, now with a message naming that cause.
- A Windows push whose unpack or permission step ran out of time retries deleting its `.vmm-*` entries for up to
  10 s, while the stopped process may still hold files, before it names the entries it could not delete.

### Security

- Another guest user can no longer redirect a Windows push, or rewrite its archive before `tar.exe` reads it. Before,
  a user who could add entries to the target's parent, as any user can in `C:\Users\Public`, could plant a junction
  through which `tar.exe` wrote outside the target, and the parent's owner, such as the owner of `C:\Users\alice`,
  could swap entries between the push's steps. A Windows push now writes its archive and unpacks it in
  `%SystemRoot%\Temp`, in directories that only `SYSTEM` and `Administrators` can write. A helper it compiles in the
  guest then does every step in the target's parent on open handles, and the pushed tree gets the access list its
  parent passes on.
- A Windows push now refuses a target whose path passes through a junction, a symbolic link or a mount point, and a
  reserved device name such as `CON`. A target on another drive than `%SystemRoot%` is copied instead of moved. A
  guest whose PowerShell runs in Constrained Language Mode can no longer push directories.

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
