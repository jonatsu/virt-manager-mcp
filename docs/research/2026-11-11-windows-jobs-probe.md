# Windows jobs probe (2026-11-11)

This probe settles, on a real Windows guest, the mechanisms behind the six design amendments in
[`live-interaction-run-3-implementation-plan.md`](../plans/live-interaction-run-3-implementation-plan.md): how a job
starts and survives a restart of the guest agent, how its output, exit code and liveness are read through the agent,
how it is stopped, and how its directory is protected.

## Setup

The guest is `t-win11` from `tests/testbed/win11/build.sh` at `3abbfb1`: Windows 11 Pro from the 26H2 business
editions ISO, PowerShell 5.1.26100.9444, and the QEMU guest agent from `virtio-win` 0.1.302, running as `SYSTEM`. Every
command ran through the guest agent with `virsh qemu-agent-command`, either as `guest-exec` of `powershell.exe
-EncodedCommand` or as `guest-file-open`, `guest-file-read` and `guest-file-close`. Probe files lived under
`C:\ProgramData\vmm-probe\`, which was removed afterwards.

## Findings

### Starting a job and restarting the agent

Three `ping -n 600 127.0.0.1` processes were started: one by a plain `guest-exec`, one by `Start-Process`, and one by
`Invoke-CimMethod Win32_Process -MethodName Create`. `Restart-Service QEMU-GA` then ran through `guest-exec`; its own
reply was lost, as expected, and the agent answered `guest-ping` again within seconds under a new service process ID.
All three pings were still running afterwards. The guest agent on Windows does not end its children when it stops.
`Win32_Process.Create` is kept as the start mechanism: the new process's parent is the WMI provider host rather than
the agent, so the job does not depend on that behaviour.

### Reading files that a process holds open

`guest-file-open` with mode `r` fails with "The process cannot access the file because it is being used by another
process." whenever another process holds the file open for writing, even when that process allows read sharing
(`FileShare.Read`). The agent evidently opens files without write sharing. Two consequences follow:

- A lock file held open by the wrapper shows liveness through file calls alone: the open fails while the wrapper runs,
  and succeeds once it has exited, been killed, or the guest has rebooted.
- A log that `cmd`'s redirect or a long-lived `FileStream` holds open cannot be read until the job ends. The wrapper
  therefore copies the job's output stream in chunks and opens the log only to append each chunk, retrying for up to
  5 s while a reader holds it. With that wrapper, reads 3 s and 7 s into a job printing a line every 2 s returned one
  and then three lines, and the final read returned all six.

A missing file reads "The system cannot find the file specified." and a missing directory "The system cannot find the
path specified."; on Linux both read "No such file or directory".

### Output encoding

The command file was written as UTF-8 with a byte-order mark and run with `powershell.exe -File`. The wrapper ran it
under `cmd.exe /d /c "chcp 65001 >nul & powershell.exe ... -File cmd.ps1 2>&1"`. The log held UTF-8 bytes for
PowerShell's own output (`päivää ünïcode €`), for a native program (`cmd /c echo native-äö`), and for text written to
stderr with `[Console]::Error.WriteLine` and `Write-Error`, which appears as PowerShell's usual error text, not CLIXML.
Lines end in CRLF. Without `chcp`, the console's output encoding is IBM437.

`exec` through `-EncodedCommand` wrote PowerShell's progress records ("Preparing modules for first use") to stderr as
CLIXML; setting `$ProgressPreference = 'SilentlyContinue'` removed them (`1fa4716`).

### Exit codes

The process exit code of `powershell.exe` for each script, run with `-File` (as jobs do) and with `-EncodedCommand`
(as `exec` and the `command` condition do):

| Script | `-File` | `-EncodedCommand` |
| -- | -- | -- |
| `exit 5` | 5 | 5 |
| `cmd /c exit 5` as the last command | 0 | 1 |
| a failing cmdlet as the last command | 0 | 1 |
| `throw "boom"` | 1 | 1 |
| `$false` as the last command | 0 | 0 |
| `Test-Path C:\nope` | 0 | 0 |
| `exit [int](-not (Test-Path C:\nope))` | 1 | 1 |
| `cmd /c exit 5; Write-Output ok` | 0 | 0 |

No other way of running a script file reproduced `-EncodedCommand`'s rule: `-Command "& 'f.ps1'"` and `". 'f.ps1'"`
lost `exit 5`; `-Command "& 'f.ps1'; exit $LASTEXITCODE"` carried a native program's code past later commands;
`Invoke-Expression` and a dot-sourced script block behaved like `-File`. A job's exit code on Windows therefore follows
`-File`: the code passed to `exit`, 1 after an uncaught error, and 0 otherwise, even after a failing native program.
A job ends with `exit $LASTEXITCODE` to pass on a native program's code. `$false` is never a failure, so a test needs
`exit [int](-not (...))`.

### Stopping

The wrapper recorded its process ID and its start time in ticks. A stop script read both, compared the start time of
the process with that ID, and on a match ran `taskkill /T /F /PID <id>`, which ended the wrapper, its `cmd`, the job's
`powershell` and the job's `ping`. Afterwards no exit file existed and the lock file opened, so the job reads "ended
without status". The probe's own process, compared against the recorded ticks, did not match, so a reused ID is not
killed.

### Directory protection

A directory created with owner `BUILTIN\Administrators`, inheritance removed, and full control for `SYSTEM` and
`Administrators` only passed this check: not a reparse point, owner `SYSTEM` or `Administrators`, access rules
protected from inheritance, and no access rule for any other security identifier. The check refused a plain
directory (it inherits rules), one with an added `Users` read entry, one whose owner was set to `Users`, a junction and
a symbolic link. A directory that `SYSTEM` creates is owned by `Administrators` by default.

### Cost of a read

Opening, reading and closing four job files through `virsh qemu-agent-command` took 0.12 s in all, and ten
`guest-ping` calls 0.35 s. A Windows job read needs only file calls, so the wait loop's 1 s probe interval has room.

### Agent readiness after setup

On the first boot after setup, `guest-ping` answered but the next `guest-exec` timed out after the agent's 10 s limit,
and the call after it failed the agent's synchronisation; later calls worked. On every later boot `exec` answered at
once.
