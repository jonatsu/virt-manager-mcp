# `taskkill /T /F` when a job's tree is already going (2026-11-23)

Probe for roadmap follow-up 28: `job_stop` on `t-win11` once failed with `taskkill failed with 255` although the job
ended. Both loops ran inside the guest as `SYSTEM`, through `exec`, because a round trip from the host is far longer
than the race.

## Before the fix

A loop of 24 cycles started `powershell.exe -Command "ping.exe -n 300 127.0.0.1"`, waited for its `ping.exe` child,
and ran `taskkill.exe /T /F /PID <root>` while ending the root at different moments: not at all, 100 ms before, or
0 to 40 ms after `taskkill` started. Each cycle then checked whether the child still ran.

| Exit status | Cycles | Cycles with a surviving child | Output |
| -- | -- | -- | -- |
| 0 | 8 | 0 | `SUCCESS: The process with PID … has been terminated.` for each process |
| 128 | 14 | 2 | `ERROR: The process "<root>" not found.` |
| 255 | 2 | 0 | `SUCCESS` for the child, then `ERROR: The process with PID <root> … could not be terminated. Reason: There is no running instance of the task.` |

So 255 means `taskkill` ended part of the tree and found the root gone mid-walk, and 128 means the root was gone
before it started. With 128 the root's child can keep running: `taskkill` finds no root to walk from.

## After the fix

`src/jobs/stop.ps1` now lists the tree with each process's start time before `taskkill`, and after a non-zero status
ends what is left of that list itself, exiting 7 when nothing is left. A loop of 30 cycles ran a copy of that script
against a fresh job directory and root, with the race injected just before its `taskkill` line: none, a synchronous
`Stop-Process` of the root, or a `Stop-Process` 0 to 59 ms later from another process.

| Injected race | Cycles | Script exit | Surviving processes |
| -- | -- | -- | -- |
| None | 10 | 0, "ended" | 0 |
| Root ended before `taskkill` | 10 | 7, "ended; taskkill exited 128 after the tree had gone" | 0 |
| Root ended 0 to 59 ms into `taskkill` | 10 | 0, "ended" | 0 |

The delayed kills all landed after `taskkill` had finished, so exit 255 did not recur; the script handles it on the
same path as 128. `win-jobs.json` then passed five times in a row with the rebuilt server, each stop ending with
status 0.
