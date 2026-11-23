# Follow-ups run 2

Status: approved; built. This run lands roadmap follow-ups 24 and 15, probes follow-up 25, and records an
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
| 1. UTF-8 error text (24) | `fix(winsteps): write guest error text as UTF-8` | Parse test passes; under code page 437 on `t-win11`, a push below a file named `vmm-été-file` fails with `Cannot create "C:\Windows\Temp\vmm-été-file" because ...` intact; `run-all.sh win-transfer.json win-transfer-caps.json` passed all seven Windows cases | Self-reviewed; the Sonnet closing review approved it, nothing blocking |
| 2. PowerShell errors as text (15) | `fix(exec): report PowerShell errors on Windows as text`, then `fix(exec): keep stdin and backtick-ended commands working on Windows` and `fix(exec): pass $input only to a command that names it` from the review | Probe on `t-win11`: `-OutputFormat Text` works with `-Command` but not with `-EncodedCommand`; the decoded script block with a closing `$?` check matched today's exit code in all ten probe commands (1 1 0 0 3 1 0 0 0 1); `win-exec.json` passes with the new check, and a rerun passed all seven Windows cases after one intermittent `job_stop` failure, recorded as follow-up 28 | Self-reviewed; the Sonnet closing review and re-review approved it, nothing blocking |
| 3. Archive access probe (25) | `docs(research): probe a Windows push archive's access list and crafted entries` | [Probe note](../research/2026-11-23-windows-archive-probe.md): the archive inherits `INTERACTIVE:(M,DC)`; seven crafted archives wrote nothing outside the staging directory; symbolic links pointing outside stay in the tree. The fix is a parked question below | Self-reviewed |
| 4. Install from scratch (26) | `docs(research): record a CachyOS install from scratch` | [Run record](../research/2026-11-23-cachyos-install-from-scratch.md): installed with `firmware: "bios"` after UEFI Secure Boot refused the ISO; the installed system booted, took a login and ran `echo marker-$(hostname)-...` (`marker-t-cachy-7.3.5-2`); `t-cachy`, its disk and the ISO copy were deleted. The gaps became follow-ups 29 and 30, which need design decisions | Self-reviewed |
| Closing checks | none: the review's fixes are in item 2's row | Sonnet `code-reviewer` over `4d0a272..f8a4c76`: approved, nothing blocking. Its findings: the design still named `-EncodedCommand` (fixed); no automated test of the closing exit check (fixed: seven `win-exec.json` checks); a trailing backtick swallowed the check (fixed with a blank line). Checking stdin as it suggested found that `$input` was empty under the new form (fixed). Kept as notes: the command-length limit is unchanged at about 12,000 characters and untested for `exec`; only `push-probe.ps1`'s UTF-8 path has a guest test. The full test bed passed 49 of 49 before the fix and again after it. A scoped re-review of the fix approved it; its new point, that `$inputPath` also counted as `$input`, is fixed with a boundary-aware match (`fix(exec): pass $input only to a command that names it`), and a `$input` command with no stdin returned in 1.5 s on `t-win11`. After `fix(exec): pass $input only to a command that names it`, the gate passed (336 tests on both toolchains, with the PowerShell parse test), `run-all.sh` passed 49 of 49 with `t-win11`, and `git status --short` was clean. The run after this record's own commit failed `templates-reset-saved.json` once on a slow console answer; the case passed alone straight afterwards (roadmap follow-up 31). After the follow-up 31 commit the gate passed and `run-all.sh` passed 49 of 49 with `t-win11`; Sonnet reviews of the review fixes and of the two closing documentation commits found nothing blocking | Sonnet review, re-review and closing reviews |

## Judgement calls

- `.gitattributes` pins `src/winsteps/*.ps1` to LF, as it pinned `src/jobs/*.ps1` (`9da8db5`). A global `*.ps1` CRLF
  rule turned the staged scripts into CRLF, which the editorconfig check refuses, although the server compiles them in
  with `include_str!`. Undo: revert that commit.
- An agent's PowerShell command gets the outer `$input` only when its text names `$input` or `${input}`
  (`powershell_command` in `src/agent.rs`). Any use of the outer `$input` reads all of stdin, which leaves
  `[Console]::In` empty, so a command that uses both now sees stdin only through `$input`; under `-EncodedCommand` it
  saw it through whichever read first. Undo: always pass it, or never, in `powershell_command`.

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
