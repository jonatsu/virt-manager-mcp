# Fork roadmap

This fork, `jonatsu/virt-manager-mcp`, adapts
[`KubeRl/MCP-Server-For-VirtManager-AI`](https://github.com/KubeRl/MCP-Server-For-VirtManager-AI) so that coding
agents can create, snapshot and destroy throwaway test VMs on a libvirt host. It branched from upstream commit
`f47d3c3`. The evidence for every item below is in
[`docs/research/2026-11-09-vm-management-mcp-evaluation.md`](../research/2026-11-09-vm-management-mcp-evaluation.md),
and the manual test bed is in [`tests/testbed/`](../../tests/testbed/README.md). The fork goes its own way: it
does not merge upstream regularly, and it takes upstream changes case by case.

Work lands in the order below: the baseline first, because every later slice needs a build and test gate it can
trust, then the defects, then the outcomes in their listed order. Each item is done when its evidence line holds.

```mermaid
flowchart LR
  B[Baseline and defects: done] --> O1[1. Clean VM in seconds: done]
  O1 --> O2[2. Reliable interaction]
  O2 --> O3[3. Code and results in and out]
  O3 --> O4[4. Seeing what happened]
  O4 --> O5[5. Several VMs together]
  O5 --> O6[6. Operator trust and control]
  O1 -. builds on .-> O5
  B --> A[Tool annotations]
  B --> IT[Integration tests]
```

## Baseline

The fork audit on 2026-11-09 found the items below. None of them changes the server's behavior.

1. **Build and test gate: passing.** `cargo build --locked`, `cargo test --locked` (10 unit tests at the audit, none
   touching libvirt) and `cargo clippy --locked --all-targets` pass with no warnings on rustc 1.99.1.
2. **Done: reformatted once with default rustfmt.** Upstream did not run rustfmt with default settings. The fork
   diverges from upstream rather than tracking it, so it reformatted in a commit of its own and keeps default
   settings. `cargo fmt --check` passes.
3. **Done: project `AGENTS.md`, with `CLAUDE.md` as a symlink to it.** It states the build gate, the guardrails a
   change must not weaken, and the rule that testing never touches VMs outside the test bed. It also marks the two
   `example-global-*.md` files as templates for the server's users, not instructions for contributors. The
   `agents-context-docs` checker passes.
4. **Done: README drift fixed.** The README now names the real template files, `example-global-CLAUDE.md` and
   `example-global-AGENTS.md`, builds with `--locked`, and says that this fork publishes no release binaries.
   Upstream's own release is `v1.0`, while `Cargo.toml` says `0.1.0`. Judgement call: upstream's joke first line
   gave way to a paragraph under the title that names the fork, its upstream and this roadmap. To undo it, remove
   that paragraph and restore upstream's first line from commit `f47d3c3`.
5. **Done: hygiene files.** `.editorconfig`, `.gitattributes`, `.pre-commit-config.yaml` and the hook configs
   (`.markdownlint-cli2.jsonc`, `.mdformat.toml`, `.secretlintrc.json`, `.yamlfmt.yaml`, `.yamllint.yaml`) come from the
   `repo-management` baseline, adapted to Rust with report-only `cargo fmt --check` and `cargo clippy` hooks. Node
   hooks use the host's Node.js, because nodeenv in pre-commit 4.7.0 crashed while choosing a download. Betterleaks
   stays on its latest stable release, `v1.10.0`, rather than the `v2.0.0-rc.7` that `pre-commit autoupdate` picked.
   The research copies and the `example-global-*.md` templates are excluded from the Markdown formatters. A second
   `pre-commit run --all-files` pass changes nothing.
6. **Done: GitHub settings and CI.** Issues are enabled as the inbox for outside reports; accepted work moves into
   this roadmap. The description and topics name the fork's purpose and its upstream. CI runs the build gate on
   stable, the tests on the minimum supported Rust version (1.89, the highest among the locked dependencies), the
   pre-commit hooks, and `cargo deny check advisories` against the RustSec database.

## Defects

1. **Done: serial console reads appeared to stall.** The stream never stalled. `console_read` honoured `wait_ms`
   only when no bytes existed after `since`, so a reread from `since: 0` returned at once. The trial's reads
   therefore came back within milliseconds of each write, before the guest answered; a libvirt client debug log
   showed the whole trial sequence finishing in about 70 ms. With real pauses, a full login over the console worked
   unchanged, and the stream-event hypothesis was refuted. The fix makes a waiting read wait for output that no
   earlier read returned, then until the output pauses for 500 ms, whatever `since` is. `console-stall.json`, run
   unchanged against `trial-deb13`, now shows the `Password:` prompt, the motd and a root shell prompt. The evidence
   is in [`docs/research/2026-11-09-console-read-wait.md`](../research/2026-11-09-console-read-wait.md).
2. **Done: `console_read` reports a console closed by a power-off.** The trial's `closed: null` came from reads that
   finished before the power-off (agent-setup correction `f2e697c0`). A power-off from inside the guest already
   worked. A forced `stop_vm` deleted the session, so the next read said "no console open"; `stop_vm` now keeps the
   session, and its reader records the stream's end. In `tests/testbed/cases/console-closed-on-power-off.json`, both
   a guest power-off and a forced `stop_vm` make the next read return `closed: "console stream ended ..."`.
3. **Done: `write_file` no longer creates world-writable files.** The guest agent creates files with mode 666.
   On Linux guests, `write_from` in `src/agent.rs` now sets the mode right after opening a new file, before any data
   is written: 644 by default, or the new optional octal `mode` parameter, which refuses bits above 777. An existing
   file keeps its mode unless `mode` is given. `copy_to_vm` shares the path, so its new files get 644 too.
   `tests/testbed/cases/write-file-mode.json` shows 644, 600, a kept 640 after append and overwrite, and a refused
   `4755`.
4. **Dropped: `start_vm` on a running VM does wait for the guest agent.** The trial's guest had no agent installed,
   and the call was never timed (agent-setup correction `d346adb4`). In
   `tests/testbed/cases/start-vm-waits-for-agent.json`, the agent is stopped and restarts 20 s later; `start_vm` with
   `wait_for_agent_secs: 60` returns about 17 s after the call, reporting the agent responding. The wait loop is now
   `agent::wait_for`, with unit tests.

## Follow-ups

Reviews of earlier work found these. None blocks the server's use.

01. **Done: a stale console session could not be replaced.** After `stop_vm` keeps a session, a stream that never
    reports its end left `console_open` answering "already open". `force: true` now replaces this server's own
    session too, and a session whose reader has stopped counts as closed. `tests/testbed/cases/console-reopen.json`
    shows it.
02. **Done: the README says file writes need `guest-exec`.** `write_file` and `copy_to_vm` run `test`, `chmod` and
    `rm` through `guest-exec` on Linux guests, so a guest agent that blocks `guest-exec` but allows file operations
    fails on writes.
03. **An overwrite with `mode` truncates first.** `write_from` opens with `wb` before `chmod`, so a failed `chmod`
    leaves an existing file empty. Either set the mode after the first write or document it.
04. **No test covers `write_from`'s ordering.** Only the pure helpers are unit-tested; setting the mode before the
    data, and the clean-up after a failed `chmod`, rest on the test bed. A seam over the agent calls would allow it.
05. **`console_read` with `wait_ms: 0` right after `stop_vm`** can return `closed: null`, because the reader records
    the stream's end a moment later. A non-zero wait reports it.
06. **Convert the existing documents to the short, plain style.** `AGENTS.md` asks for short, plain designs. Bring this
    roadmap, the brainstorm draft, the test-bed README and the research notes into the same style; the research copies
    keep their copied text and change only their own notes.
07. **Done: an expiry no longer hides a failed volume deletion.** A volume that cannot be deleted keeps the domain, the
    expiry counts as failed so `virtmanager-mcp sweep` exits non-zero, and the next sweep retries it. Unit tests cover
    the decision; producing the failure needs root, so the test-bed README lists the manual check.
08. **Done: a refused `reset_vm` keeps the console open.** `reset_vm` closes the console only once `manage::reset` has
    passed every check, just before it forces the VM off. In `templates-reset-saved.json`, a console opened before a
    refused reset still reads output after it; in `templates-reset-uefi.json`, a reset that goes ahead closes it.
09. **Done: a test shows reset and expiry leaving a disk outside `disk_pool`.** `templates-outside-disk.json`, with the
    steps in the test-bed README, attaches a user-owned file to two VMs with `virsh`; the sweep's expiry and a reset
    both leave it in place, with its owner and checksum unchanged. A reset now reports such a disk as not deleted
    because it is no pool volume, rather than as one it could not delete.
10. **Done: a slow `console_open` no longer stalls every wait.** Each `wait_for` poll takes the console sessions lock
    to scan. `Consoles::open` and `Consoles::close` now hold that lock only to check and swap entries, and call
    libvirt and drop a replaced session, which joins its reader and aborts its stream, without it.
11. **An unterminated OSC sequence hides the output after it.** `ansi::strip` holds back everything after `ESC ]`
    until a BEL or `ESC \` arrives, so a stray `ESC ]` from line noise stops console waits with `strip_ansi` from
    matching, and hides the rest of a stripped `console_read`. Capping an OSC at a newline or a few KiB would bound
    it; a captured boot log would show whether stray sequences occur.
12. **Console conditions strip the same output once each.** Every console condition runs `ansi::strip` over its own
    span on each poll with new output, which costs little at the default 256 KiB scrollback but grows with
    `console_buffer_bytes` and the number of conditions. Strip once per poll and share the result.

## Outcomes

Planned features are grouped by what they let an agent do, in the order below. Each outcome needs an approved design
before it is built, and each feature from the earlier list sits under one of them.

1. **Done: a clean VM in seconds, not minutes.** `create_vm_from_template` gives an agent a disposable VM from an
   operator-prepared template, as a qcow2 overlay with a NoCloud seed, and its guest agent answered 11.9 s after the
   call on the test bed. `reset_vm` returns it to the template's state, and VMs with a lifetime expire through a
   sweep in every server process or the `virtmanager-mcp sweep` command. `scripts/make-template.sh` builds a Debian 13
   template. The VMs live on `qemu:///system`. Evidence:
   [`disposable-vms-implementation-plan.md`](disposable-vms-implementation-plan.md), whose execution record holds
   each unit's commits, test-bed results and reviews, built from
   [`disposable-vms-design.md`](disposable-vms-design.md) and
   [`2026-11-10-fatfs-seed-boot.md`](../research/2026-11-10-fatfs-seed-boot.md).
2. **Reliable interaction with a live guest.** An agent knows when the guest is ready and does not lose input, on
   guests with or without a guest agent: installers, bootloaders, kernels, Windows and appliances. This covers the wait
   tool (console text, screen text, a settled screen, the guest agent, a command or a job), the mouse and region
   screenshots, paced console input, long-running jobs on Linux and Windows whose output streams back, and
   expect-style console sessions. `type_text` returns at once, and the trial lost input that way. Design:
   [`live-interaction-design.md`](live-interaction-design.md), accepted. Run 1 is done: `wait_for` with console,
   guest-agent, command, `job_done` and `job_output` conditions; console generations, a wait cursor and paced
   `console_write`; and background jobs on Linux guests (`job_start`, `job_read`, `job_stop`, `job_list`). A
   serial-only OpenWrt guest, `t-owrt`, joined the test bed. Evidence:
   [`live-interaction-run-1-implementation-plan.md`](live-interaction-run-1-implementation-plan.md), whose execution
   record holds each unit's commits, test-bed results and reviews. Run 2 brings OCR, screen conditions and the mouse,
   and run 3 brings Windows.
3. **Moving code and results in and out.** An agent pushes a checkout in and pulls artifacts or logs back without
   passing them through its context, and reaches services running in the guest. This covers copying from the VM,
   directory sync, the guest's IP address and port forwarding. Each new path out of the guest needs a guardrail.
4. **Seeing what happened.** When something fails in the guest, an agent can find out why. This covers console output
   buffered from VM start, guest resource usage, and events such as a crash or reboot.
5. **Several VMs together.** An agent can test small networks, such as a client and a server. It builds on outcome 1,
   and it needs a network policy for each VM, which outcome 6 would otherwise bring.
6. **Operator trust and control.** An operator can let agents run with less supervision. This covers an audit log of
   mutating calls (following `akarasulu/nested-mcp-server`), `max_vms` enforced across processes (today only an
   in-process lock guards it, so several stdio instances can race past it), network policies for each VM, scoping per
   session, and tested `qemu:///session` support. Session scope is the survey's strongest guardrail: VMs there cannot
   reach the user's system VMs at all, and `uri` is already a config key but untested.

Two items stand outside the outcomes, because their result is already clear. Either can land at any time.

- **MCP tool annotations.** Set `readOnlyHint`, `destructiveHint` and `idempotentHint` on every tool, so that clients
  and permission layers can gate `delete_vm`, `snapshot_revert`, `detach_disk` and similar tools apart from reads.
  `src/` sets none today.
- **Integration tests against real libvirt.** Turn the test-bed call lists into automated tests that run only when
  the test bed is present, and skip otherwise.

Open questions for the designs:

- Outcome 5: should it bring its own network policy, or wait for outcome 6?

## Next

The baseline, the defects, outcome 1 and follow-ups 7 to 9 are done. Outcome 1 was re-verified in full at `aa44f14`,
and the plan's execution record holds its evidence. Follow-ups 7 to 9 landed in `63e28ab`, `31bb732`, `805a5d9` and
`3a64f86`. At `3a64f86`, the gate and `cargo +1.89 test --locked` passed (52 tests), and the reset, expiry and
outside-disk test-bed cases passed with no `t-*` VM or volume left and the template checksum unchanged. A separate
conformance review found no material mismatch. The manual root check for follow-up 7 has not been run yet.

Outcome 2 comes from the accepted [`live-interaction-design.md`](live-interaction-design.md), in three runs, each with
its own plan. Run 1 is done, per
[`live-interaction-run-1-implementation-plan.md`](live-interaction-run-1-implementation-plan.md). Next is the plan for
run 2: OCR with the screen-text and settled-screen conditions, the mouse, region screenshots, and the screenshot
attachment for `wait_for`. It starts with the probe that decides between `ocrs` and Tesseract.
