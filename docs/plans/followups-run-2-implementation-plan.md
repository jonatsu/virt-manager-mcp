# Follow-ups run 2

Status: approved; runs unattended. This run lands roadmap follow-ups 24 and 15, probes follow-up 25, and records an
install from scratch for follow-up 26, all in [`fork-roadmap.md`](fork-roadmap.md). Follow-ups 16 and 27 wait on
outside releases, and 23 and the fix for 25 need a design decision, so they stay out.

## Run rules

- Answer a question from this file, the roadmap, `AGENTS.md` and the plans they name before parking it.
- Make reversible calls yourself, and record each under "Judgement calls" with its reason and how to undo it.
- Park what only the project owner can answer under "Parked questions", with options and a recommendation, then take
  the next item.
- Run every command standalone from the repository root: no `cd <dir> &&`, no `git -C`, nothing chained after a commit.
- Commit this file's update with each landed item, and keep working past a progress report.

## Preflight

- Follow `AGENTS.md`: its guardrails, its build gate, plus `VIRTMANAGER_REQUIRE_PWSH=1 cargo test --locked` and
  `cargo +1.94 test --locked`. Conventional Commits with explicit pathspecs, no attribution trailer, a `CHANGELOG.md`
  entry for each change users notice, and the roadmap item marked done in the commit that lands it.
- Touch only `trial-deb13`, and VMs and volumes named `t-*` or `tpl-*` in pool `agent-vms` on `qemu:///system`. Check
  `free -m` before starting a VM: `t-win11` needs 5 GiB available, and at most one large guest runs at a time.
- Run each changed Windows script on `t-win11` before committing it, and delete guest probe trees at the end of each
  probe.
- Dispatch every subagent with `model: sonnet`. Log any safeguard flag in `~/.config/claude/claude-error-logs/` as its
  `README.md` describes, then rerun the brief once.
- Nothing is pushed.

## Items

### 1. UTF-8 error text in the Windows scripts (follow-up 24)

**Result.** `push-probe.ps1`, `push-mkdir.ps1`, `push-inherit.ps1`, `push-swap.ps1`, `push-cleanup.ps1`,
`pull-size.ps1` and `pull-remove.ps1` write stderr through a UTF-8 `StreamWriter` with `AutoFlush`, as `pull-pack.ps1`
does, so a non-ASCII path in a message arrives intact.

**Verification.** The parse test passes. On `t-win11`, a push whose target parent is a file named
`C:\Windows\Temp\vmm-été` fails with a message that names the path intact. `win-transfer.json` and
`win-transfer-caps.json` pass. This is also unit 2 of the outcome 4 plan, so its execution-record row is filled too.

### 2. PowerShell errors as plain text (follow-up 15)

**Result.** A PowerShell error from `exec` on Windows reaches stderr as plain text, not a `#< CLIXML` block.

**Verification.** First probe on `t-win11` whether `-OutputFormat Text`, or redirecting the error stream inside the
command, gives plain text without changing exit codes or stdout. Then a unit test of the built command line, and a
`win-exec.json` call whose command fails, with `absent: "CLIXML"` and the error text expected.

### 3. Probe the push archive's access list (follow-up 25)

**Result.** A dated note in `docs/research/` records the archive's access list under `C:\Users\Public` on `t-win11`,
and whether a crafted hard-link or symbolic-link entry in an archive reaches outside the staging directory when
`tar.exe` unpacks it as `SYSTEM`. The roadmap item cites the note. The fix stays a parked question.

### 4. An install from scratch (follow-up 26)

**Result.** A dated note in `docs/research/` records an agent-style install of `cachyos-desktop-linux-260809.iso` from
`~/Downloads`, driven only through the server's tools: `create_vm` with the ISO, then the screen, mouse, keyboard,
console and wait tools, until the installed system boots and answers. The VM is `t-cachy`, at most 4 GiB of memory,
deleted at the end. The server needs an `iso_dirs` entry it can list and QEMU can read: the pool directory is
root-only, and the home directory is closed to `libvirt-qemu`. So the run copies the ISO to `/var/tmp/vmm-iso/`, which
only it uses, and names that directory in a scratch config with `manage.create` on; it deletes the copy at the end.
Gaps that block the install are fixed when the fix is small and needs no design decision;
otherwise they become roadmap follow-ups.

## Closing checks

After the last commit: the gate and `cargo +1.94 test --locked` exit 0, `tests/testbed/run-all.sh` exits 0 with
`t-win11` present, `git status --short` lists no tracked change, and a Sonnet `code-reviewer` pass over the run's
commits finds no blocking defect, its findings fixed or recorded.

## Execution record

| Item | Commits | Evidence | Review |
| -- | -- | -- | -- |
| 1. UTF-8 error text (24) | `fix(winsteps): write guest error text as UTF-8` | Parse test passes; under code page 437 on `t-win11`, a push below a file named `vmm-été-file` fails with `Cannot create "C:\Windows\Temp\vmm-été-file" because ...` intact; `run-all.sh win-transfer.json win-transfer-caps.json` passed all seven Windows cases | Self-reviewed; the closing review covers it |
| 2. PowerShell errors as text (15) | `fix(exec): report PowerShell errors on Windows as text` | Probe on `t-win11`: `-OutputFormat Text` works with `-Command` but not with `-EncodedCommand`; the decoded script block with a closing `$?` check matched today's exit code in all ten probe commands (1 1 0 0 3 1 0 0 0 1); `win-exec.json` passes with the new check, and a rerun passed all seven Windows cases after one intermittent `job_stop` failure, recorded as follow-up 28 | Self-reviewed; the closing review covers it |
| 3. Archive access probe (25) | `docs(research): probe a Windows push archive's access list and crafted entries` | [Probe note](../research/2026-11-23-windows-archive-probe.md): the archive inherits `INTERACTIVE:(M,DC)`; seven crafted archives wrote nothing outside the staging directory; symbolic links pointing outside stay in the tree. The fix is a parked question below | Self-reviewed |
| 4. Install from scratch (26) |  |  |  |
| Closing checks |  |  |  |

## Judgement calls

- `.gitattributes` pins `src/winsteps/*.ps1` to LF, as it pinned `src/jobs/*.ps1` (`9da8db5`). A global `*.ps1` CRLF
  rule turned the staged scripts into CRLF, which the editorconfig check refuses, although the server compiles them in
  with `include_str!`. Undo: revert that commit.

## Parked questions

- **How should a Windows push protect its archive (follow-up 25)?** An interactive user under `C:\Users\Public` can
  rewrite `.vmm-<id>.tar` before it is unpacked as `SYSTEM`. The probe shows the rewrite cannot write outside the
  staging directory, but it can change what arrives and plant symbolic links pointing anywhere. The options:

  - Write the archive inside a fresh directory that only `SYSTEM` and `Administrators` can write, made as
    `push-mkdir.ps1` makes the staging directory. It closes the gap, and costs one more guest step per push.
  - Accept the risk, and document that a push into a directory where untrusted users have write access delivers what
    they let through.

  Recommendation: the protected directory, because it reuses `push-mkdir.ps1`'s checked access list, and follow-up 23
  needs a similar decision about the staging directory's parent, so the two can share one design.
