# Notes and limitations

- The server records each visible VM's starts, stops, crashes, reboots and similar events in memory, up to
  `limits.max_events` per VM, until it exits or the VM is deleted; `vm_events` lists them. When one of them happened
  outside any tool call, such as a guest panic, the next reply to a call that names the VM ends with a notice, once
  per server process whichever client makes the call. With several agents on one server, only the first sees the
  notice, and the others find the records with `vm_events`.
- `exec` is not interactive: output is returned when the process exits. On timeout the server tries
  to kill the process (`kill -KILL` / `taskkill /F /T`). On Linux, children of the shell may survive.
- When this server boots a VM, through `start_vm`, `create_vm_from_template` or `reset_vm`, it starts the VM paused,
  opens its console, then resumes it, so `console_read` from offset 0 holds the boot from its first byte. A Windows
  guest writes nothing to its serial console, so its boot shows only through the screen tools. A console opened on a
  VM the server did not boot holds output from the open on. While a boot runs, a second `start_vm` of the VM is
  refused. Only one client can hold a console: if virt-manager's serial console view is open when the server boots
  the VM, the reply says the console could not be captured, and `console_open` fails unless `force: true`, which
  disconnects the other client.
- `console_open` on a console this server holds open attaches to it and replies with its generation; `force: true`
  opens a new session and drops the old one's output. Every new session starts a new console generation, and every
  offset a console tool returns comes with its `generation`. A `console_read` or console wait from an offset above 0
  must pass that generation back, so an offset from an earlier session is refused instead of misread.
- Each VM keeps the console output of one earlier run. When a new run replaces a session, or `console_close`,
  `reset_vm` or `snapshot_revert` ends one, its output moves to the previous run, and `console_read` with
  `run: "previous"` reads it as closed with the reason it ended. The previous run has its own generation and offsets,
  and a second move drops the first. `console_read` also works on a stopped VM. After a guest panic, the panic text
  is in the current run while a preserved crash keeps the VM in place, and in the previous run once the VM has
  started again.
- The server's console output lives in memory and goes with `delete_vm`. With `host_pull_dirs` set, every session also
  writes to `console/<vm>/<UTC start>-g<generation>.log` in the first pull directory, which outlives the VM. Nothing
  deletes these logs, so clear old ones yourself; each stops at `limits.console_log_bytes`. Read-only mode writes none.
- `vm_stats` reports one snapshot from two samples about a second apart and keeps no history, so a spike that ended
  before the call leaves no trace. Its filesystem figures come from the guest agent and are the guest's own claim.
- `wait_for` returns within `limits.max_wait_secs` (default 50 s, below the 60 s at which some clients cut a call)
  plus 2 s. A wait that runs out returns resume points, so the next call still finds output that arrived in
  between. A console condition without `since` starts where the last `console_write` began, and a match moves that
  point past it. On a console the server captured from boot, that point is offset 0 until the first write, so a wait
  also sees a prompt printed before the call. A wait never changes what `console_read` returns.
- `console_write` can pause between bytes (`inter_char_delay_ms`) or between chunks (`chunk_size` with
  `chunk_delay_ms`), up to 1000 ms each, for firmware and bootloaders that drop input sent back to back. A write
  whose pauses would exceed `limits.max_wait_secs` is refused.
- Input sent before the guest program that should read it is waiting can go astray. Right after a command line, the
  shell may still hold the terminal in its own raw, no-echo mode, so input that arrives before the command starts
  is not echoed, and a Ctrl-D in it is read as data. Wait for the program's prompt with `wait_for`, or pause, before
  sending its input.
- Jobs keep their files in `/var/lib/virtmanager-mcp/jobs/<id>/` in the guest and run under `systemd-run`, or
  `setsid` without systemd, so they survive an agent restart and a server restart. The launcher refuses a job
  directory parent that is a symbolic link, or is not a root-owned directory with mode 700. Windows jobs are
  described under [Windows guest](guest-setup.md#windows-guest).
- `write_file` gives a new file on a Linux guest mode 644, or the octal `mode` it is given (up to
  `777`). Overwriting or appending keeps an existing file's mode unless `mode` is given. The guest
  agent itself would create the file world-writable, so the mode is set before any data is written.
  On Linux guests `write_file` and `copy_to_vm` therefore also need `guest-exec`, which runs `test`,
  `chmod` and `rm`; an agent that allows file operations but blocks `guest-exec` cannot write files.
- `stop_vm` without `force` sends a shutdown request and waits (60 s by default). A guest that
  ignores ACPI and has no guest agent will not shut down; use `force: true` to power it off.
- `type_text` assumes a US keyboard layout in the guest and supports printable ASCII only.
- Created VMs reference their disks by file path, like virt-manager. Volume-type disks
  (`<source pool=… volume=…/>`) failed to open under Ubuntu's AppArmor setup.
- Logs go to stderr; set verbosity with `RUST_LOG` (e.g. `RUST_LOG=debug`).
