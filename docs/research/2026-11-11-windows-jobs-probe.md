# Windows jobs probe (2026-11-11)

This probe settles, on a real Windows guest, the mechanisms behind the six design amendments in
[`live-interaction-run-3-implementation-plan.md`](../plans/live-interaction-run-3-implementation-plan.md). They cover
how a job starts and survives a restart of the guest agent, and how its output, exit code and liveness are read
through the agent. They also cover how a job is stopped and how its directory is protected.

## Setup

The guest is `t-win11` from `tests/testbed/win11/build.sh` at `3abbfb1`. It runs Windows 11 Pro from the 26H2
business editions ISO, PowerShell 5.1.26100.9444, and the QEMU guest agent from `virtio-win` 0.1.302 as `SYSTEM`.
Every command ran through the guest agent with `virsh qemu-agent-command`. Each was either a `guest-exec` of
`powershell.exe -EncodedCommand`, or `guest-file-open`, `guest-file-read` and `guest-file-close`. Probe files lived
under `C:\ProgramData\vmm-probe\`, which was removed afterwards.

## Findings

### Starting a job and restarting the agent

Three `ping -n 600 127.0.0.1` processes were started: one by a plain `guest-exec`, one by `Start-Process`, and one by
`Invoke-CimMethod Win32_Process -MethodName Create`. `Restart-Service QEMU-GA` then ran through `guest-exec`, and its
own reply was lost, as expected. The agent answered `guest-ping` again within seconds under a new service process ID.
All three pings were still running afterwards. The guest agent on Windows does not end its children when it stops.

`Win32_Process.Create` is kept as the start mechanism. The new process's parent is the WMI provider host rather than
the agent, so the job does not depend on that behaviour.

### Reading files that a process holds open

`guest-file-open` with mode `r` fails whenever another process holds the file open for writing. It fails even when
that process allows read sharing (`FileShare.Read`). The error reads "The process cannot access the file because it
is being used by another process." The agent evidently opens files without write sharing. Two consequences follow:

- A lock file held open by the wrapper shows liveness through file calls alone. The open fails while the wrapper
  runs. It succeeds once the wrapper has exited, been killed, or the guest has rebooted.
- A log that `cmd`'s redirect or a long-lived `FileStream` holds open cannot be read until the job ends. The wrapper
  therefore copies the job's output stream in chunks and opens the log only to append each chunk. While a reader
  holds the log, the wrapper retries for up to 5 s. With that wrapper, reads 3 s and 7 s into a job printing a line
  every 2 s returned one and then three lines. The final read returned all six.

A missing file reads "The system cannot find the file specified." and a missing directory "The system cannot find the
path specified."; on Linux both read "No such file or directory".

### Output encoding

The command file was written as UTF-8 with a byte-order mark and run with `powershell.exe -File`. The wrapper ran it
under `cmd.exe /d /c "chcp 65001 >nul & powershell.exe ... -File cmd.ps1 2>&1"`. The log held UTF-8 bytes for
PowerShell's own output (`päivää ünïcode €`) and for a native program (`cmd /c echo native-äö`). It also held UTF-8
for text written to stderr with `[Console]::Error.WriteLine` and `Write-Error`. That text appears as PowerShell's
usual error text, not CLIXML. Lines end in CRLF. Without `chcp`, the console's output encoding is IBM437.

`exec` through `-EncodedCommand` wrote PowerShell's progress records ("Preparing modules for first use") to stderr as
CLIXML. Setting `$ProgressPreference = 'SilentlyContinue'` removed them (`1fa4716`).

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

No other way of running a script file reproduced `-EncodedCommand`'s rule:

- `-Command "& 'f.ps1'"` and `". 'f.ps1'"` lost `exit 5`.
- `-Command "& 'f.ps1'; exit $LASTEXITCODE"` carried a native program's code past later commands.
- `Invoke-Expression` and a dot-sourced script block behaved like `-File`.

A job's exit code on Windows therefore follows `-File`. It is the code passed to `exit`, 1 after an uncaught error,
and 0 otherwise, even after a failing native program. A job ends with `exit $LASTEXITCODE` to pass on a native
program's code. `$false` is never a failure, so a test needs `exit [int](-not (...))`.

### Stopping

The wrapper recorded its process ID and its start time in ticks. A stop script read both and compared the start time
of the process with that ID. On a match it ran `taskkill /T /F /PID <id>`. That ended the wrapper, its `cmd`, the
job's `powershell` and the job's `ping`. Afterwards no exit file existed and the lock file opened, so the job reads
"ended without status". The probe's own process, compared against the recorded ticks, did not match, so a reused ID is
not killed.

### Directory protection

The check requires four things. The directory is not a reparse point, and its owner is `SYSTEM` or `Administrators`.
Its access rules are protected from inheritance, and no access rule names any other security identifier. A directory
created with owner `BUILTIN\Administrators`, inheritance removed, and full control for only `SYSTEM` and
`Administrators` passed it. The check refused a plain directory, because it inherits rules. It also refused one with
an added `Users` read entry, one whose owner was set to `Users`, a junction and a symbolic link. A directory that
`SYSTEM` creates is owned by `Administrators` by default.

### Cost of a read

Opening, reading and closing four job files through `virsh qemu-agent-command` took 0.12 s in all, and ten
`guest-ping` calls 0.35 s. A Windows job read needs only file calls, so the wait loop's 1 s probe interval has room.

### Agent readiness after setup

On the first boot after setup, `guest-ping` answered, but the next `guest-exec` timed out after the agent's 10 s
limit. The call after it failed the agent's synchronisation, and later calls worked. On every later boot `exec`
answered at once.

### Restarting the agent repeatedly

Building the test-bed cases restarted the `QEMU-GA` service about 22 times. The restarts ran through `exec`, through a
process started with `Win32_Process.Create`, and inside a job. Four restarts left the agent unreachable. libvirt
reported the `org.qemu.guest_agent.0` channel `disconnected`. Every call failed with "QEMU guest agent is not
connected" until the VM was powered off and on.

A job logged the service every second. For one of the four, it showed that the restart had started a new
`qemu-ga.exe` process and that the service stayed `Running` with it. The guest's Application log showed no agent call
after the restart. The new agent therefore ran but never reopened the virtio-serial port.

Six restarts in a row inside a job, and six more while the host sent `guest-ping` every 0.3 s, all came back within
about 15 s. The trigger is therefore unknown. The ACPI reboot that should have recovered the guest did nothing,
because Windows ignores the ACPI button while a user is logged on. `virsh shutdown --mode agent` and a power cycle
work. The jobs that ran through the restarts that came back kept running.

Restarting `QEMU-GA` through `exec` also loses `exec`'s reply, because the agent that would answer stops.
