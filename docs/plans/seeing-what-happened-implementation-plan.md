# Seeing what happened: implementation plan

Status: approved, after two plan-critic reviews and a refresh after follow-ups 25, 29 and 30. Built from
[`seeing-what-happened-design.md`](seeing-what-happened-design.md) at `d41fa1c`, which owns the behaviour, and from
roadmap follow-ups 22, 24, 28 and 31 in [`fork-roadmap.md`](fork-roadmap.md) at `deaf2b3`. This plan owns order and
checks. Progress goes in the execution record at the end.

## Overview

The run builds roadmap outcome 4, so that an agent can find out why something failed in a guest, and folds in four
small follow-ups first.

- **Follow-ups first.** `exec` with `program` refuses arguments that are not ASCII on a Windows guest, with a message
  that suggests `command` (follow-up 22). The Windows transfer scripts write their error text as UTF-8, as
  `pull-pack.ps1` already does (follow-up 24). Both landed in follow-ups run 2. `job_stop` on Windows no longer
  reports a failure when the tree is already gone (follow-up 28), and `templates-reset-saved.json` waits for the login
  prompt instead of reading for a fixed time (follow-up 31). These two are units 17 and 18. They run first, before
  the probe: unit 17 needs a working `t-win11`, which the probe's forced stop error could leave unbootable, and unit
  18 changes a case that unit 7 changes again. They are numbered last so that the references among units 3 to 16 stay
  as they are. They also keep `run-all.sh` reliable, which the run's goal needs.
- **A probe before code.** The design leaves three facts to a probe: the libvirt event sequence of a guest panic under
  each crash action, whether Debian's pvpanic driver and Windows' Hyper-V crash registers raise the panic event, and
  which state a preserved crash leaves the VM in.
- **Foundations.** The config gains `manage.on_crash` and `limits.max_events`, and VMs the server creates gain a panic
  device, balloon statistics and `<on_crash>`. The event log, its kind mapping, attribution and cursors come next as
  plain Rust with no libvirt calls, so their rules are unit-tested before any callback feeds them.
- **The console from the first byte.** Console sessions move to the VM's UUID and gain a previous-run slot. One start
  operation starts a VM paused, opens its console, and resumes it, for every tool that starts a VM, and a snapshot
  revert does the same.
- **Events from libvirt.** Callbacks registered through `virt-sys` feed the log through a queue and a worker thread.
  One reconnect function, used by tool calls and by the worker alike, registers the callbacks again and records the
  gap.
- **Delivery.** The power tools record what they expect, `vm_events` lists the log, every reply that names a VM can
  carry a notice, `wait_for` waits for an event, and `vm_stats` reports resource use.

```mermaid
flowchart TD
  U1[1 exec: non-ASCII program arguments]
  U2[2 Windows scripts: UTF-8 errors]
  U3[3 Probe] --> U4[4 Config and devices]
  U3 --> U5[5 Event log core]
  U3 --> U6[6 Console sessions by UUID]
  U4 --> U7[7 Shared start operation]
  U5 --> U7
  U6 --> U7
  U7 --> U8[8 Snapshot revert with a capture]
  U5 --> U9[9 Event callbacks]
  U9 --> U10[10 Event worker and reconnect]
  U7 --> U11[11 Attribution and vm_events]
  U10 --> U11
  U11 --> U12[12 Notices in replies]
  U12 --> U13[13 wait_for event condition]
  U4 --> U14[14 vm_stats]
  U8 --> U15[15 Test-bed cases and docs]
  U13 --> U15
  U14 --> U15
  U1 --> U15
  U2 --> U15
  U17[17 job_stop: taskkill statuses] --> U3
  U17 --> U15
  U18[18 templates-reset-saved waits for the prompt] --> U3
  U18 --> U7
  U15 --> U16[16 Final gate and reviews]
```

## Design amendments this plan needs

Each lands in `seeing-what-happened-design.md` in the commit of the unit named.

1. **One server-wide notice cursor (units 5, 11 and 12).** MCP protocol `2026-07-28` removes sessions, and tells
   servers not to treat a stdio process or a connection as one either
   ([research](../research/2026-11-15-mcp-sessions.md)). So the design's per-session cursors and causes become
   server-wide:
   - each VM's log has one seen-cursor in the server process. A notice appears in the next reply, from any client,
     that names the VM, and the cursor then moves past the records it announced. `vm_events` without `kinds` and a
     `wait_for` event match move it too, over the records they return;
   - an expectation holds the VM, the tool and its kinds, without a session. A record a tool call caused carries that
     tool as its cause and raises no notice, whichever client made the call. Only `outside` and `expiry` records raise
     notices;
   - with several agents on one server, the first reply that names the VM carries the notice, and `vm_events` lists
     every record for the others. The design's "Event delivery" decision and "The notice in replies" section change
     to say so.
2. **Replacing a console session (unit 6).** A start that cannot open its console still moves the earlier run's
   session to the previous-run slot, so `console_read` never shows an old run as current while a new one runs. A
   `console_open` over a closed session of the same domain ID drops the old output, as `force` does. A moved session
   keeps the reason its stream ended; one moved while still live reads "replaced by a new run", and one ended by
   `console_close` reads "closed by console_close".
3. **One start at a time (unit 7).** While the start operation runs for a VM, a second start of it is refused with a
   message that the VM is starting, so nothing resumes the guest before its console opens.
4. **Snapshot reverts (unit 8).** A revert to a running snapshot gets a capture whether the VM was running or shut
   off before. A revert to a shut-off snapshot opens nothing.
5. **Missing memory figures (unit 14).** A third cause joins the design's two: the VM has no balloon device, as
   `t-win11` has `<memballoon model="none"/>`.
6. **Another process's sweep (unit 11).** VMs that the `sweep` command, run as its own process, destroys get the cause
   `outside`, because only the server process that destroys a VM can mark it. The README says so.
7. **A preserved Windows crash shows no stop screen (unit 3).** The probe found that the Hyper-V crash notification
   stops the guest before Windows paints a stop screen, so a screenshot shows the screen as it was at the crash. The
   design's crash-action reason, its preserved-crash section and its test-bed list say so; behaviour is unchanged.

## Preflight

These apply to every unit, which names only its additions.

- **Rules.** Follow `AGENTS.md`, including its guardrails, and its build gate. Load `rust-style`,
  `rust-error-handling` and `rust-testing` before editing Rust, and `bash-shell` before editing `run-all.sh`. Use `LSP`
  for named symbols where the Rust language server answers, and confirm references with `rg`.
- **Gate.** Every commit passes the build gate in `AGENTS.md`, plus `cargo +1.94 test --locked`, with
  `VIRTMANAGER_REQUIRE_PWSH=1` so the PowerShell parse test runs. The `mdformat` and `pretty-format-json` hooks may
  rewrite a file during a commit; re-add it and commit again.
- **PowerShell steps.** Each script is a constant `.ps1` file in `src/winsteps/`, its arguments arrive as quoted
  literals in `$argv`, and it maps each failure to an exit code listed in its header. Run each changed script on
  `t-win11` before committing it.
- **FFI.** Units 9 and 14 add the run's `unsafe` code. Every callback body runs inside `catch_unwind`, makes no
  libvirt call beyond getters on the pointer it receives, and never wraps a borrowed pointer in an owning `virt` type.
  Every `unsafe` block carries a `SAFETY` comment, and every libvirt allocation has one owner that frees it.
- **No new crates.** Times are kept as epoch seconds and formatted as UTC by a small tested function, because the run
  adds no crates.
- **Strict arguments.** Every tool refuses fields it does not know (follow-up 30). New parameter structs carry
  `#[serde(deny_unknown_fields)]`, which `every_tool_with_parameters_refuses_unknown_ones` in `src/server.rs` checks,
  and a new `wait_for` condition with no fields is a struct variant, as `Agent {}` is, because serde ignores extra
  fields on a unit variant of a tagged enum.
- **Test bed.** Touch only `trial-deb13`, and VMs and volumes named `t-*` or `tpl-*` in pool `agent-vms` on
  `qemu:///system`. Throwaway VMs come from `create_vm_from_template` with names starting `t-`; `config-templates.toml`
  allows three of 1 GiB each. Check `free -m` before starting a VM: `t-win11` needs 5 GiB available. A crashed VM under
  `preserve` keeps its memory and counts against `max_vms`, so each case that crashes a VM stops it with `force: true`
  and deletes it in the same case; `run-all.sh`'s cleanup trap removes any `t-*` VM a failed case leaves. Delete guest
  trees and probe VMs at the end of each probe or case.
- **Build leftovers.** Delete extra Cargo target directories and worktrees made for the run, such as
  `~/.cache/vmm-*-target`, once the unit that made them lands.
- **Review.** Units 6, 7, 10, 12, 13 and 14 change concurrency, FFI, the wait engine or what an agent is told about
  who caused an event. After each, dispatch `code-reviewer` with `model: opus` on its commits and evidence, covering
  the units since the last review, and fix blocking findings before the next dependent unit. If safeguards stop an
  Opus review, run the same brief once on `model: sonnet`, and record both in the unit's row. The other units are
  self-reviewed, and the final review covers them.
- **Commits.** One logical change per commit, Conventional Commits, explicit pathspecs, and a `CHANGELOG.md` entry
  under `## Unreleased` for each change users notice. Mark roadmap items done in the commit that lands them. Nothing
  is pushed in the run.

## Invariants and their tests

The design asks for a test of each invariant. Each row names its owning unit.

| Invariant | Test | Unit |
| -- | -- | -- |
| The server opens a console automatically only for an allowed VM it starts, never with the force flag | Unit test of the start operation through a fake console opener that records each call's VM and flags | 7 |
| No tool changes the XML of an existing VM | Unit test: a reset's XML keeps `<on_crash>` and gains no panic device or stats period | 4 |
| No reply holds an event, console output or statistics of a hidden or deleted VM | Unit tests for `vm_events`, `vm_stats`, `wait_for` `event`, `console_read` with `run: "previous"` and the notice path, each with a hidden VM and a deleted one; a test-bed case with a VM outside `allowed_vms` | 11, 12, 13, 14, 15 |
| Retained diagnostics are bounded | Unit tests: the log drops its oldest records past `max_events`; a second move into the previous-run slot drops the first | 5, 6 |
| A notice never replaces a result or error, and announces each record once per server process (amendment 1) | Unit tests of the cursors and of the reply's blocks | 5, 12 |
| An event callback makes no libvirt call and cannot unwind into libvirt | Unit test: a callback body that panics returns normally through the wrapper; review of the callback code | 9 |
| `manage.on_crash` accepts only `preserve` and `destroy`, and `limits.max_events` is positive | Unit tests of both refusals | 4 |
| Read-only mode keeps `vm_events` and `vm_stats`, sends no guest-agent query for `vm_stats`, and hides every tool that starts or stops a VM | The tool-visibility test in `src/server.rs` extended to both tools; a `vm_stats` unit test with a fake agent that fails on any call | 11, 14 |
| `start_vm` never resumes a crashed VM | Unit tests of the start decision for `VIR_DOMAIN_CRASHED` and for paused with reason crashed; the test-bed refusal for whichever state the probe found | 7 |

## Units

### 1. `exec`: non-ASCII program arguments on Windows (built in follow-ups run 2)

Built in [follow-ups run 2](followups-run-2-implementation-plan.md) as `888fc3f`: on a Windows guest, `exec` refuses a
program path, arguments or `env` entries that are not ASCII, and suggests `command`. Follow-up 22. The goal counts it
as landed; its row below holds the evidence.

### 2. Windows scripts: UTF-8 errors (built in follow-ups run 2)

Built in [follow-ups run 2](followups-run-2-implementation-plan.md) as `2799546`: every script in `src/winsteps/` that
writes error text writes it as UTF-8. Follow-up 24. The goal counts it as landed; its row below holds the evidence.

### 3. Probe: panics, devices and event sequences

**Result.** A dated note, `docs/research/<date>-crash-and-events-probe.md`, records the facts later units depend on,
before any of them is written.

**Changes.** `virsh event --all --loop --timestamp` records events throughout. On a VM from
`create_vm_from_template` whose XML the probe edits with `virsh define` to add `<panic model='isa'/>`, a balloon stats
period and each `<on_crash>` value, the probe records:

- the event sequence and final state with its reason for `echo c > /proc/sysrq-trigger` (after
  `echo 1 > /proc/sys/kernel/sysrq`) under `preserve` and under `destroy`, and whether the vCPUs stay stopped under
  `preserve` (CPU time from `virsh domstats --cpu-total` twice, 10 s apart);
- what `exec` returns for the call that panics the guest, which the test-bed cases then expect;
- that the guest loads its pvpanic driver (`lsmod`, `dmesg`);
- the sequences for a start with `virsh start --paused` then `virsh resume`, a `snapshot-revert --paused` to a running
  snapshot of a BIOS VM, a guest `reboot`, a `poweroff` from the guest, `stop_vm` with and without `force`, and the
  guest agent stopping and starting;
- whether a console opened between the paused start and the resume holds the first firmware or kernel output, and
  whether a revert to a running snapshot gives the domain a new ID.

On `t-win11`: check `free -m`, shut it down, and clone its disk to `t-win11-pre-crash.qcow2` in `agent-vms` with
`virsh vol-clone`. Add `<panic model='hyperv'/>` to `tests/testbed/t-win11.xml`, redefine the VM from that file, and
record the event sequence and final state for a stop error forced as `SYSTEM` by ending a critical process such as
`wininit`, under `preserve`, with a screenshot of the stop screen. Stop it with `force: true`, check it boots, then
delete the clone.

**Verification.** The note holds each item above, and ends with a table of the observed sequences that unit 5's kind
mapping copies.

**Recovery.** If `t-win11` does not boot after the crash, restore its disk from the clone with `virsh vol-upload` or
by cloning back, and stop. If the probe shows that `preserve` lets a crashed guest run on, that Debian does not raise
the event with `isa`, or that Windows does not raise it with `hyperv`, revert `tests/testbed/t-win11.xml`, redefine
`t-win11` from the reverted file, and stop the run: each is a design decision.

### 4. Config and devices on created VMs (self-reviewed)

**Result.** `manage.on_crash` (`preserve` by default, or `destroy`) and `limits.max_events` (default 256, refused
when 0) are read and checked, and both create tools write the panic device, `<stats period='5'/>` in the balloon, and
`<on_crash>`.

**Changes.** `src/config.rs` gains both keys, with an enum for `on_crash` whose other values stop the server with a
message naming the two. `domain_xml` in `src/manage.rs` adds the panic device by guest OS (`isa` for Linux, `hyperv`
for Windows), the stats period and `<on_crash>`, beside the `<firmware>` features and pflash loader that `<os>` gains
for `secure_boot: false` (follow-up 29). `reset_vm` edits the existing XML, so it keeps what a VM has. The README's
configuration section and the CHANGELOG describe the keys.

**Verification.** Unit tests: an unknown `on_crash` and `max_events = 0` are refused; the XML of both create paths
holds the three elements, with the right panic model per OS, with `secure_boot` true and false; a reset's XML keeps
`<on_crash>` and lacks a panic device when the original lacked one. `firmware-secure-boot.json` still passes. A VM
made by `create_vm_from_template` shows the elements in `virsh dumpxml`.

### 5. Event log core (self-reviewed; unit 12's review covers it)

**Result.** `src/events.rs` holds the per-VM logs keyed by UUID, the process-wide `seq`, the kind mapping from the
probe's table, the merged crash record, the `max_events` bound with its dropped count, expectations with their
5 s window, one seen-cursor per VM (amendment 1), and the notice text. It makes no libvirt call.

**Changes.** A new module, with plain input records (UUID, name, event ID, event, detail, epoch seconds) and a clock
passed in, so tests drive it. Its API covers: append a record; record and expire an expectation; read after `since`
with optional kinds; take the notices for a VM; drop a VM. The cursor advances only over records returned, and a
read filtered by `kinds` advances nothing.

**Verification.** Unit tests for each design rule: the bound and dropped count; each row of the kind table; the
`stopped` with detail `crashed` merged into the preceding `crashed` record, and its own record when none precedes it;
an outside crash never consuming a `stop_vm` expectation; a window that closes 5 s after the tool returns; the
`expiry` cause; one notice per record; no notice for a record a tool caused; a filtered read advancing nothing;
two threads taking notices at once, each record going to exactly one of them; a dropped VM keeping nothing; the UTC
formatter.

### 6. Console sessions by UUID, with a previous run (reviewed, Opus)

**Result.** Console sessions are keyed by domain UUID, each VM has one previous-run slot, `console_open` attaches to
a live session, `console_close` moves a session to the previous run, `console_read` takes `run`, and every console
tool looks the VM up first.

**Changes.** In `src/console.rs`:

- sessions keyed by UUID, with the name kept for messages;
- opening split from libvirt's force flag, so the server can replace its own session without forcing, behind a small
  opener trait so unit 7's test can record the flags;
- a typed open result: opened with its generation, failed with libvirt's reason, or no console device;
- the previous-run slot and the replacement rules of amendment 2. The sessions lock is still held only to check and
  swap entries; joining the old reader and releasing its stream happen outside it, as today;
- dropping a VM's sessions and slot.

In `src/server.rs`, `console_read`, `console_close` and `list_vms` use `Hypervisor::lookup`, which works on a stopped
VM, and pass the UUID; `console_open` and `console_write` keep `Hypervisor::domain`, which requires a running VM.
`wait::LiveEnv` carries the UUID. `console_read` gains `run: "current" | "previous"`, and a previous run reads as
closed with its reason. `reset_vm`, `delete_vm` and `snapshot_revert` close the session, which now moves it to the
previous run, as intended: the output from before the reset or revert stays readable.

The cases whose reply this unit's own rules change are updated here: those that call `console_open` over a live
session of this server, at least `console-reopen.json`, `console-generation.json` and `console-stall.json`. Cases
that open the console of a VM the server started change only in unit 7, when that start opens a capture first.

**Verification.** Unit tests on the buffer and slot logic: a replacement by a new domain ID moves the old run; a
`force` or a non-force reopen over a closed session within the same ID drops it; close moves; a second move drops the
older slot content; the slot keeps its own generation and offsets; each moved session's reason. On the test bed, the
cases that call `console_open` pass, and on `trial-deb13`: `console_open` twice attaches; a stop, start and
`console_open` leave the earlier run readable with `run: "previous"`; `console_read` of a VM outside `allowed_vms`
answers "not found".

### 7. Shared start operation (reviewed, Opus)

**Result.** `start_vm`, `create_vm_from_template` and `reset_vm` start a VM through one operation that records an
expectation, starts the domain paused, opens the console in safe mode, and resumes. A console read from offset 0
holds the guest's first boot output. `start_vm` refuses a crashed VM in either crashed state, and a second start
during the operation is refused (amendment 3).

**Changes.** The operation lives on `State`, which owns the hypervisor, the consoles and the event log, with a per-UUID
"starting" guard held for its whole length. `Hypervisor::start` keeps the resume, wake and refusal paths and loses
the boot path; its state-to-action choice moves into a pure function, so both crashed states are unit-tested.
`manage::start_after_reset` gives its `RESET_NVRAM` flag to the operation. The session a start opens begins its wait
cursor at 0. A failed open still resumes and the reply says why; a failed resume leaves the VM paused and says so.
The tool descriptions for `start_vm`, `console_open` and `console_read` tell agents that a start captures the console
from its first byte, and that a Windows guest writes nothing there, so its boot shows through the screen tools.

A `console_open` after a start, reset or template create now attaches to the capture, so the cases that do that are
updated here: at least `templates-reset-saved`, `templates-reset-uefi`, `console-after-start`,
`console-closed-on-power-off`, `job-wait`, `wait-for`, `owrt-boot`, `owrt-pacing` and `owrt-wait`. Units 6 and 7
each run every case that calls `console_open`, and unit 8 every case that calls `snapshot_revert`, such as
`console-generation`, and each fixes any other case whose reply changed.

**Verification.** Unit tests: the start decision for each state; the reply on each open result; the fake opener
records one safe-mode open per start of an allowed VM and none with force; the guard refuses a concurrent start. On
the test bed: `start_vm` of `trial-deb13`, then `console_read` with `since: 0`, holds the kernel's first lines; the
same after `reset_vm` of a `t-*` VM; a wait for a login prompt without `since` matches a prompt printed before the
call; the refusal for whichever crashed state unit 3 found. The full `run-all.sh` passes.

### 8. Snapshot revert with a capture (self-reviewed; unit 10's review covers it)

**Result.** `snapshot_revert` to a running snapshot reverts with `VIR_DOMAIN_SNAPSHOT_REVERT_PAUSED`, opens the
console as the start operation does, and resumes, whether the VM was running or shut off (amendment 4).

**Changes.** `manage::snapshot_revert` reads the snapshot's `<state>` and chooses the flag; the server runs it inside
the start operation's guard and records the expectation.

**Verification.** On a BIOS `t-*` VM (running UEFI VMs cannot take a running snapshot): a revert to a running snapshot
from shut off and from running each leave a current run whose read from offset 0 holds output after the revert; a
revert to a shut-off snapshot opens nothing.

### 9. Event callbacks (reviewed with unit 10, Opus)

**Result.** The server registers its four event callbacks and a close callback, and each callback copies plain data
into a bounded queue without calling libvirt or unwinding.

**Changes.** `src/events/ffi.rs`: registration through `virConnectDomainEventRegisterAny` for lifecycle, reboot,
watchdog and agent lifecycle; callbacks inside `catch_unwind` that read the UUID and name with getters on the given
pointer and push into the queue; a free callback that alone releases the callback data; and
`virConnectRegisterCloseCallback`. `src/main.rs` registers when it starts serving, not in the `sweep` command.

**Verification.** Unit tests: the wrapper returns normally from a body that panics; a full queue sets the overflow
mark. On a `t-*` VM, a start and stop reach the queue (logged at debug level).

### 10. Event worker and reconnect (reviewed, Opus, with unit 9)

**Result.** A worker thread turns queued records into log entries for visible VMs only. One reconnect function, used
by `Hypervisor::with_conn` and by the worker after a close callback, opens the new connection, registers the
callbacks on it under the same write lock as the swap, appends a `gap` record to every log, and records `stopped` or
`started` with detail `during_gap` for each visible VM whose state differs from its last record.

**Changes.** The worker in `src/events/worker.rs` checks visibility through a UUID cache and otherwise
`Hypervisor::lookup`, records a `gap` when the queue overflowed, and drops a VM's diagnostics on `undefined`.
`src/hypervisor.rs` routes its lazy reconnect through the shared function, which takes a hook so the hypervisor does
not depend on the events module. Deregistering on the old connection is best effort.

**Verification.** Unit tests with a fake source: visibility; the overflow gap; `undefined`; a reconnect started by a
tool call and one started by the close callback both register again and record the gap and `during_gap`. On a `t-*`
VM: a guest panic produces one `crashed` record; a guest reboot a `rebooted`; the agent stopping and starting its
`agent_*` records. The live reconnect is an exception below.

### 11. Attribution and `vm_events` (self-reviewed; unit 12's review covers it)

**Result.** Power tools record what they expect, records carry their cause, and `vm_events` lists a VM's log.

**Changes.** `stop_vm`, `reset_vm`, `delete_vm` and `snapshot_revert` record expectations; the start operation
already does. `manage::expire` and `manage::reset` take a hook, as `reset`'s `before_power_off` already is, through
which the server records the expectation (cause `expiry` for the sweep) and drops or moves console sessions; the
`sweep` command's own process passes a hook that does nothing (amendment 6). `vm_events` takes `vm`, `since` and
`kinds`, answers for a visible VM in any state through `Hypervisor::lookup`, appears in read-only mode, and advances
the caller's cursor only without `kinds`. `stop_vm`'s description says that a stop that gives up waiting produces an
outside `stopped` later.

**Verification.** Unit tests: the tool-visibility test lists `vm_events` in read-only mode; a hidden and a deleted
VM's `vm_events` answer "not found". On a `t-*` VM: `stop_vm` records `stopped` with cause `stop_vm`; a guest's own
`reboot` records `rebooted` with cause `outside`; `vm_events` with `kinds` returns only those kinds.

### 12. Notices in replies (reviewed, Opus, covering units 5, 11 and 12)

**Result.** A reply to any tool call that names a VM gains a trailing text block for records with a notice that this
server has not announced and no tool caused, once (amendment 1), never replacing a result or error.

**Changes.** `VirtServer` implements `call_tool` itself instead of the one `#[tool_handler]` generates: it reads `vm`
from the request's arguments, calls `self.tool_router.call(ToolCallContext::new(self, request, context))`, and on a
`Complete` result appends the notice block. Finding the UUID runs in `spawn_blocking` through `Hypervisor::lookup`,
so a hidden VM gets no notice. No session is involved: the cursor lives in the event log. The crash notice under
`preserve` says the VM holds its memory until stopped with `force: true`.

**Verification.** Unit tests: the notice is a separate block after a JSON block; an error reply keeps its error and
gains the notice after it; two concurrent calls naming the VM carry the notice once between them; a hidden VM's
records produce none. On a `t-*` VM: after a forced panic and a wait for the `crashed` record (events arrive
asynchronously, so a case waits before asserting), the next reply that names the VM carries one notice, and the reply
after carries none; a `stop_vm` causes no notice.

### 13. `wait_for` event condition (reviewed, Opus)

**Result.** `wait_for` gains an `event` condition with `kinds` and `since`, evaluated before the stopped check; a
wait whose conditions are all events skips the running check and works on a stopped VM.

**Changes.** `src/wait.rs` gains the condition as a struct variant that refuses unknown fields, its check and an
`Env` method that reads the log; the zero-timeout
rule lets `event` conditions be checked once, as console ones are. `src/server.rs` uses `Hypervisor::lookup` instead
of `domain` when every condition is an event. A match returns the record and advances the VM's cursor over it.
The console condition still reads only the current run.

**Verification.** Unit tests in the wait engine with a fake log: an `event` condition with an unknown field is
refused; an event before the stopped check wins over `closed`; an all-event wait on a stopped VM matches `started`;
`since` from `vm_events` catches a record that landed between two calls; a hidden VM's records never match. On a
`t-*` VM: a wait for `crashed` together with a console condition returns the event when the guest panics.

### 14. `vm_stats` (reviewed, Opus)

**Result.** `vm_stats` reports CPU, memory, disk, network and filesystem use for a running VM from two samples about
a second apart, replies with the state for any other VM, and names why figures are missing.

**Changes.** `src/stats.rs` (new) samples through `virDomainListGetStats` with only this domain, read through
`virt-sys` and freed with `virDomainStatsRecordListFree` (the `virt` crate's `get_all_domain_stats` has no accessors
and leaks its records), computes the rates, and shapes the reply. `src/agent.rs` gains `guest-get-fsinfo`, skipped in
read-only mode and bounded by `limits.agent_timeout_secs`. The reply says the filesystem figures are the guest's own
claim. A missing-memory reason names one of three causes (amendment 5). `vm_stats` appears in read-only mode.

**Verification.** Unit tests: rates from fixed samples, including counters that reset; the reply shape; read-only
mode makes no agent call (a fake agent fails on any call); a hidden and a deleted VM answer "not found". On a `t-*`
VM: CPU use rises during `yes > /dev/null`; filesystem figures match `df`; a shut-off VM replies with its state; a VM
without an agent returns the rest and says why. On `t-win11`: the reply holds filesystems and names the missing
balloon device.

### 15. Test-bed cases and docs (self-reviewed; unit 16's reviews cover it)

**Result.** `run-all.sh` covers each item in the design's "Test bed" list, and the docs describe the outcome.

**Changes.** New cases, each with its own throwaway `t-*` VM made and deleted inside the case:

- `diag-start-capture.json`: a read from offset 0 after `start_vm`, after `reset_vm`, and after a revert to a running
  snapshot;
- `diag-crash-preserve.json` and `diag-crash-destroy.json`: a forced panic with its notice, `vm_events`, and the panic
  text in the current run under `preserve` and in the previous run after a stop and start under `destroy`;
  `run-all.sh` writes `config-crash-destroy.toml` from `config-templates.toml` with `on_crash = "destroy"`;
- `diag-attribution.json`: a guest's own reboot as `outside`, and a `stop_vm` with no notice;
- `diag-wait-event.json`: `wait_for` with an `event` condition, alone and with a console condition, on a VM that
  stops during the wait;
- `diag-stats.json`: `vm_stats` with and without a guest agent, and in read-only mode; the `t-win11` check joins
  `win-exec`'s unit;
- `diag-identity.json`: `console_open` attaching to a live capture, and a VM deleted and recreated under the same name
  that has no events or output from the first;
- a hidden-VM check as a `run-all.sh` unit function: start a VM outside `allowed_vms` with `virsh`, as
  `unit_templates_expiry_command` runs host steps, then check that no reply mentions it, with a positive control (an
  allowed VM's start recorded in the same run), so silence does not just mean a dead worker.

The `exec` that panics a guest uses a short `timeout_secs` and expects what unit 3 recorded. Each case is added to
`UNITS` and, where cases depend on each other, `MEMBERS`. The README, the test-bed README, the CHANGELOG and both
`example-global-*.md` templates describe `vm_events`, `vm_stats`, notices, the previous run and the advice to read it
after a crash.

**Verification.** Each new case passes on its own, and `run-all.sh` passes as a whole.

### 16. Final gate and reviews

**Result.** The run is verified as a whole, the roadmap is closed, and the record is complete.

**Changes.** The roadmap marks outcome 4 done and rewrites its "Next" paragraph for outcome 5.

**Verification.**

- The gate and `cargo +1.94 test --locked` pass at the last commit.
- `run-all.sh` exits 0 with `t-win11` present.
- An Opus `code-reviewer` review of the whole run (Sonnet if safeguards stop it, as in the preflight), and a separate
  `conformance-review` against the design, the roadmap entries and this plan, find no material mismatch, with their
  findings fixed and rechecked.
- This row names the evidence for each of the design's "Checks".

### 17. `job_stop` on Windows: taskkill statuses (self-reviewed)

**Result.** `job_stop` on a Windows job whose process tree is gone by the time `taskkill` runs, or goes during it,
reports the job ended instead of "taskkill failed". Follow-up 28.

**Changes.** First, a probe on `t-win11`: a loop of at least 20 cycles that starts a job running
`ping.exe -n 300 127.0.0.1` and stops it, with the root ended at varying moments around the `taskkill` call so the
race happens. A loop inside the guest is the main form, because a round trip from the host is far longer than the
`taskkill` window. The probe records a histogram of `taskkill` exit statuses and outputs, and whether any descendant
of the tree survives a stop; its results go in a dated note in `docs/research/`. It deletes the jobs it made under
`C:\ProgramData\virtmanager-mcp\jobs\` when it ends. Then:

- `src/jobs/stop.ps1` lists the tree's descendants by parent PID, each with its start time, before `taskkill`. After
  a non-zero status, it rechecks the recorded process and those descendants by PID and start time, retrying for up
  to 2 s while one is still terminating. When all are gone, it exits with a new status, 7, "ended; taskkill exited N
  after the tree had gone", and it still exits 5 with the status while any of them runs. Its header lists the new
  status. Jobs scripts live in `src/jobs/` and take `$id`, not `$argv`; the parse test covers them.
- `jobs::stop` in `src/jobs.rs` maps exit 7 to the same success text as exit 0, followed by the script's line, so
  `win-jobs.json`'s `contains` strings still hold and the agent sees what happened.

The roadmap marks 28 done, and the CHANGELOG gains a "Fixed" entry.

**Verification.** The parse test passes. Unit tests with the fake Windows guest in `src/jobs.rs`: exit 7 gives the
success text with the script's line, and exit 5 still fails with it. On `t-win11`, the probe loop run again on the
fixed script ends every cycle without a failure and with no surviving descendant, and the record gives both
histograms. `win-jobs.json` passing five times in a row is a regression check only, because one stop per run rarely
meets the race.

### 18. `templates-reset-saved.json` waits for the login prompt (self-reviewed)

**Result.** The case's check that the console still reads output after a refused reset waits for the login prompt,
instead of a fixed 3 s `console_read` that a slow guest missed once. Follow-up 31.

**Changes.** In `tests/testbed/cases/templates-reset-saved.json`, after the `console_write` of an empty line, a
`wait_for` console condition for `login:` without `since` and with a 30 s timeout replaces the `console_read`.
Without `since`, the wait starts where the last `console_write` began, so only a prompt the guest printed after the
Enter matches, which is the case's claim that the console still answers. A `console_read` from offset 0 then checks
that the console is open and holds output. Unit 7 later changes the same case, on top of this. The roadmap marks 31
done; the change is to a test only, so the CHANGELOG has no entry.

**Verification.** The evidence is the structural one: the case waits up to 30 s for an answer instead of reading for
a fixed 3 s. That the Debian template's serial getty prints `login:` again after a bare Enter is unverified; the first
run confirms it, and the `templates-reset-uefi` unit of `run-all.sh`, which runs the case, then passes three times in
a row.

## Exceptions the run records

| Check | Why it cannot run here | Stand-in evidence |
| -- | -- | -- |
| A lost libvirt connection and the reconnect | Restarting `libvirtd` or dropping a client needs root | Unit 10's tests with a fake source, for a reconnect from a tool call and from the close callback |
| A notice shown once across two HTTP clients | `mcp_call.py` speaks stdio only | Unit 5's and 12's tests of two concurrent callers |
| A guest without pvpanic support | Debian 13 ships the driver | None: design risk 4 |
| The hidden and deleted VM refusals of `vm_events`, `vm_stats`, `wait_for` `event` and `console_read` as unit tests | Each comes from `Hypervisor::lookup`, which needs libvirt | The test-bed cases `console-refusals.json` and `diag-hidden.json` for a hidden VM and `diag-stats-after.json` for a deleted one; the notice path has its unit test |
| `vm_stats` filesystem figures matching `df`, checked by a case | A case cannot compare numbers across two replies | Unit 14's live check on `t-st`: `used_bytes` equal to `df`'s Used, `total_bytes` to Used plus Avail |

## Execution record

Run rules, re-read after any compaction or resume:

- Answer questions from this plan, the design, the roadmap and the files they name.
- Make reversible calls yourself, and record each under "Judgement calls" with its reason and how to undo it.
- Park what only the project owner can answer under "Parked questions", with options and a recommendation; continue.
- Run every command standalone from the repository root: no `cd … &&`, no `git -C`, nothing chained after a commit.
- Commit this record's update with each landed unit, and keep working past a progress report.

Order: 17, 18, 3, then 4 to 16 by the diagram. Units 1 and 2 landed in follow-ups run 2.

Judgement calls:

- Unit 17: after a failed `taskkill`, `stop.ps1` ends the listed processes still running itself, rather than only
  rechecking them, because the probe found a child that `taskkill` exit 128 leaves running. Undo: drop the
  `Stop-Process` line in the recheck loop, which turns those cases back into exit 5.
- Unit 3: `t-win11` got its panic device and `<on_crash>preserve</on_crash>` through `virt-xml` rather than by
  redefining it from `tests/testbed/t-win11.xml`, because the file holds no UUID and redefining would mean undefining
  the VM with its NVRAM and TPM state. The file carries the same two changes. Undo: remove both from the file and run
  `virt-xml t-win11 --remove-device --panic all` and `--edit --events on_crash=destroy`.
- Unit 5: a second `reboot` event within 1 s of the first makes no record, because the probe saw libvirt send each
  guest reboot twice, 6 ms apart, and a notice per event would announce one reboot twice. Undo: drop the
  `REBOOT_REPEAT_SECS` check in `Events::append`.
- Unit 5: amendment 1's design edits (one server-wide cursor; expectations without a session) land with this unit,
  which holds the cursor; units 11 and 12 build on them.
- Unit 3: `wininit` is protected even from `SYSTEM`, so the Windows stop error came from a process that marked itself
  critical with `RtlSetProcessIsCritical` and exited. The result is the same kind of stop error.
- Unit 6: `delete_vm` drops the VM's session and previous run rather than moving the session, because the design
  drops a deleted VM's diagnostics and nothing can read them afterwards. `reset_vm` and `snapshot_revert` record
  "closed by reset_vm" and "closed by snapshot_revert" as the reason. Undo: call `close` instead of `forget` in
  `delete_vm`.
- Unit 6: a console tool on a VM outside `allowed_vms` answers with the lookup's "not in allowed_vms" text, the same
  as every other tool, rather than "not found". Undo: map the lookup error in the console tools.
- Unit 7: the whole start, its state-to-action choice included, moved out of `Hypervisor::start` into `src/start.rs`
  on `State`, rather than leaving the resume and wake paths in the hypervisor, so that one guard and one expectation
  cover every path. The start decision reads libvirt's state and reason in one call, so a VM that stops between two
  reads cannot look running. `reset_vm` takes the "starting" claim before its power-off rather than only around its
  boot, so a start cannot boot the VM while its disks are replaced. Undo: move `State::start_vm`'s resume and wake
  arms back into a `Hypervisor` method, and claim inside `State::boot` only.
- Unit 8: the revert runs through `State::revert` in `src/start.rs`, which reuses the boot with a revert in place of
  the paused start, rather than through `manage::snapshot_revert` alone; `manage` keeps the snapshot lookup and the
  flag choice. Unit 10's review came before this unit was built, so the final review covers it. Undo: inline the
  revert in the `snapshot_revert` tool.
- Unit 11: the sweep records the `expiry` expectation around `manage::expire` itself, from the UUID it already
  holds, rather than through a hook passed into `expire`; `sweep_once` takes whether its process serves, and the
  `sweep` command passes false (amendment 6). `delete_vm` records no expectation, because it refuses a running VM
  and its only event, `undefined`, makes no record; it drops the VM's log at once instead of waiting for that
  event. Undo: give `expire` a hook, and add an expectation in `delete_vm`.
- Units 5, 12 and 13: each VM's log keeps the set of records a reply has shown instead of one seen-cursor, because
  the review of units 5, 11 and 12 and that of unit 13 each found that a `since` past an unannounced record, or a
  wait for another kind, moved the cursor over a crash no reply had shown, and its notice was lost. A record counts
  as shown once a notice, an unfiltered `vm_events` or a wait match returned it. Undo: restore the `seen` field.
- Units 12 and 13 land in one commit, because they share the fix to the log's record of what replies have shown and
  their `src/server.rs` changes interleave. Undo: revert that commit, which removes both.
- Unit 12: a reply looks the VM up for its notice only when a log under that name has a record to announce, so
  polling tools such as `console_read` make no extra libvirt call. Undo: drop the `has_pending_notice` check.
- Unit 13: after a VM check finds the VM stopped, or a console condition finds its session closed, a wait with event
  conditions reads the log for 1 s more before it ends as closed, because a crash's record reaches the log through
  the worker just after libvirt reports the stop. Console conditions are polled through that second too. Undo: drop
  `EVENT_GRACE` and return as before.
- Units 9 and 10: the worker and the reconnect hook live in `src/events/worker.rs` rather than `src/events/ffi.rs`, so
  the FFI file holds only the callbacks and their registration. Undo: move the file's contents into `ffi.rs`.
- Unit 10: the worker checks visibility by UUID and caches only a definite answer, and the reconnect leaves out of the
  `during_gap` comparison any VM whose events are still queued, so a stop just before the loss is recorded once. An
  `agent_disconnected` record says nothing about whether the VM runs, because the probe saw it follow a stop. These
  came from the review of units 9 and 10. Undo: revert the review-fix part of the units' commit.
- Unit 14: CPU use comes from the sum of `vcpu.<n>.time`, not `cpu.time`, and stops at 100 %, because `cpu.time` also
  counts QEMU's own threads and put a busy `t-win11` at 103.7 %. Undo: read `cpu.time` in `cpu_percent`.
- Unit 14: `vm_stats` reports figures only for a VM in libvirt's running state; a paused one gets its state, as the
  design's "on any other state it replies with the state" reads. A VM that stops between the samples gets its state
  too. Undo: accept `VIR_DOMAIN_PAUSED` in `stats::report`.
- Unit 14: the tool description says a filesystem's total is used plus available space, as `df`'s Used plus Avail,
  because that is what `guest-get-fsinfo` returns: on `t-st` its total for `/` was 904,593,408 + 1,885,691,904 bytes,
  below `df`'s size by the blocks reserved for root. Undo: drop the sentence.
- Unit 14: the refusal of a hidden or deleted VM by `vm_stats` has no unit test, because it comes from
  `Hypervisor::lookup`, which needs libvirt, as for `vm_events`. The test-bed cases of unit 15 show it instead:
  `diag-hidden.json` for a hidden VM, and `diag-stats-after.json` for a deleted one. Undo: none needed.
- Every review in the run up to unit 16 ran on Sonnet rather than Opus, because a standing instruction sends every
  subagent to Sonnet until Anthropic's safeguard false positives are fixed. The rows' "Sonnet code review" means that.
  The run's goal names an Opus code review, so unit 16 added one on Opus over the whole run, and safeguards did not
  stop it.
- Unit 16: the worker keeps the UUIDs of the batch it is handling, and a reconnect's comparison leaves those VMs to
  their own events, as it does queued ones. An event whose VM cannot be looked up because libvirt is unreachable stays
  with the worker, in flight, and is tried again with the next batch, so it lands in its own VM's log; only past the
  queue's 1024 events do the oldest give way to a gap. After one unanswered lookup the pass makes no more, and a VM's
  later events wait behind its waiting one, so each VM's events keep their order; a VM's `undefined` drops its waiting
  events, so no retry can cache a VM that comes back under its UUID as hidden. A VM that is not found counts as
  gone, and any other lookup error leaves the answer open, so a failed lookup never hides a VM for good. This is the
  Opus review's optional finding: a reconnect from inside the worker's lookup recorded a stop twice. Undo: revert the
  worker part of unit 16's last fix commit.
- Unit 16: the conformance review found that amendment 2 had not reached the design, and that the design still
  described the seen-cursor, notices once per MCP session, the expiry marking in `src/manage.rs` and four changed cases.
  The design now says what the code does, as its own rule for a built design asks; the behaviour behind each was already
  an amendment or a judgement call here. Undo: revert the design part of unit 16's fix commit.
- Unit 16: `reset_vm` moves its console session to the previous run after `manage::reset` returns, not from inside
  its power-off hook, because the hook runs under the connection lock and closing takes the console lock, which an
  open holds while it takes the connection lock. When the power-off ends the stream first, the moved run keeps the
  stream's own end reason instead of "closed by reset_vm". Undo: close inside the hook again, which brings the
  deadlock back.
- Unit 16: a `snapshot_revert` that libvirt refuses still leaves the console session in the previous run, as the
  final review's optional finding noted. The session closes before the revert so its end reason reads "closed by
  snapshot_revert"; closing after the revert would let the stream's end, which the revert causes, win the reason.
  The refusal is rare, because `snapshot_target` checks the snapshot first. Undo: close in `LibvirtRevert::start_paused`
  after the revert returns.
- Unit 16: a reconnect whose registration succeeds ends a registration that failed earlier, so the worker does not
  register a second set of callbacks, and the time without callbacks joins the reconnect's gap. This is the final
  review's required finding. Undo: none wanted; the old code doubled every event after such a failure.
- Unit 16: `stop_vm` checks power rights before it records an expectation, and `Events::finish` drops expectation
  windows that closed more than 60 s earlier, so refused calls claim no events and a quiet server's list stays small.
  The margin keeps a window for an event the worker reaches late. Undo: record before the check, and drop the pruning.
- Unit 16: the revert expectations include `shutdown`, which libvirt sends when a revert stops a running VM, so that
  record carries cause `snapshot_revert`; `diag-start-capture.json` checks it. Undo: drop `Kind::Shutdown` from both.
- Unit 15: `diag-stats` is three cases in one unit, because the read-only check needs its own server process: the
  first leaves `t-dst` running, `diag-stats-read-only` runs under `config-read-only.toml`, which `run-all.sh` writes,
  and `diag-stats-after` deletes the VM. Undo: fold them into one case without the read-only check.
- Unit 15: the hidden VM in `diag-hidden` is `trial-deb13`, which `config-templates.toml` leaves out, rather than a new
  VM, because it is the one VM outside the `t-*` names the run may start. The script shuts it off first and starts it
  with `virsh` during the case, then waits for its agent again. Undo: define a throwaway `t-*` VM outside
  `allowed_vms` instead.

Parked questions: none yet.

The run's goal:

- **Objective.** Units 1 to 18 are committed on `main`, each with its row here.
- **Success.** After the last change:
  - the gate and `cargo +1.94 test --locked` exit 0;
  - `tests/testbed/run-all.sh` exits 0, with `t-win11` present;
  - an Opus code review (Sonnet where safeguards stop Opus) and a separate conformance review find no material
    mismatch.
- **Boundaries.** Nothing is pushed. Only `trial-deb13`, `t-*` VMs and `t-*` or `tpl-*` volumes in `agent-vms` are
  touched.
- **Stop.** Stop and report when a step needs root, a design change beyond the amendments above (including unit 3's
  probe outcomes), another VM or pool, or when the host runs short of memory.

The command that arms the run:

```text
/goal Outcome 4 per docs/plans/seeing-what-happened-implementation-plan.md is built: units 1-18 on main, each with its record row. After the last change, AGENTS.md gate and `cargo +1.94 test --locked` exit 0, `tests/testbed/run-all.sh` exits 0 with t-win11 present, and an Opus code review (Sonnet if safeguards stop Opus) plus a separate conformance review against the design, roadmap and plan find no material mismatch. Nothing pushed; only trial-deb13 and t-*/tpl-* in agent-vms touched. Else stop: root, design change, other VM or pool, low memory
```

| Unit | Commits | Evidence | Review |
| -- | -- | -- | -- |
| 1. `exec`: non-ASCII program arguments on Windows | `fix(exec): refuse text the Windows guest agent cannot pass` | Probe on `t-win11`: a non-ASCII `env` entry fails like an argument, with `program` and with `command`, so `env` is refused in both modes. Three new unit tests pass; `win-exec.json` passes on `t-win11` with both refusals and `echo été` through `command` | Self-reviewed |
| 2. Windows scripts: UTF-8 errors | `fix(winsteps): write guest error text as UTF-8` | Parse test passes; under code page 437 on `t-win11`, a push below a file named `vmm-été-file` fails naming the path intact; the seven Windows cases pass. Built in [follow-ups run 2](followups-run-2-implementation-plan.md) | Self-reviewed |
| 3. Probe: panics, devices and event sequences | `docs(research): record the crash and event probe for outcome 4` | [Probe note](../research/2026-11-23-crash-and-events-probe.md). Debian loads pvpanic; both `isa` and `hyperv` raise `Crashed Panicked`; `preserve` leaves `crashed (panicked)` with the vCPUs stopped, and `destroy` adds `Stopped Crashed`; a console opened during a paused start holds the boot from GRUB on. `t-win11` booted after the crash and the clone was deleted. None of the stop outcomes held; design amendment 7 records the missing stop screen | Self-reviewed |
| 4. Config and devices on created VMs | `feat(manage): give created VMs a panic device, balloon stats and a crash action` | Unit tests: `on_crash` defaults to `preserve`, refuses `restart` and `coredump-destroy` naming both values, and `max_events = 0` is refused; the XML holds `<on_crash>`, the panic model per OS and `<stats period='5'/>` for both OSes, both `secure_boot` values and both actions; a reset of a VM without them keeps it that way. `virsh dumpxml` of a VM from `create_vm_from_template` shows all three; `firmware-secure-boot.json` passes | Self-reviewed |
| 5. Event log core | `feat(events): add the per-VM event log` | `src/events.rs`, 14 unit tests: every row of the probe's kind table; the crash stop merged into its crash and standing alone; the `max_events` bound and dropped count; an outside crash leaving a `stop_vm` expectation unused; an expectation claiming each kind once and lapsing 5 s after the tool returns; the `expiry` cause and its notice; one notice per record and none for a tool's record; a filtered read advancing nothing; a wait match advancing the cursor; a repeated reboot as one record; two threads taking 200 notices with each once; a gap in every log; a dropped or undefined VM keeping nothing; the UTC formatter | Unit 12's review |
| 6. Console sessions by UUID, with a previous run | `feat(console): key sessions by VM and keep the previous run` | 13 unit tests through a fake stream and opener: a new domain ID moves the old run; a force or a reopen over a closed session of the same run drops it; an open over a live session attaches, and one naming another device is refused; libvirt's force flag apart from replacing; close moves and keeps the stream's reason; a second move drops the older slot; the slot keeps its generation and offsets; a failed open still moves the earlier run; sessions keyed by UUID and forgotten with their VM. On the test bed the 12 cases that call `console_open` pass, and the new `console-refusals.json` shows the console tools refusing `trial-deb13` under a config that leaves it out. On `trial-deb13`: a second open attaches; after a forced stop and a start, `run: "previous"` reads the earlier output with its old generation | Sonnet code review: a committed refusal test was missing (added), a named device attached to a default-device session (now refused), and a close racing an open could put an older run in the slot (close and forget now take the open lock) |
| 7. Shared start operation | `feat(start): boot VMs paused with their console captured` | `src/start.rs`, 6 unit tests: the start decision for each state, with both crashed states refused and a VM shut off by the `destroy` action booted; the claim refusing a concurrent start and freeing the VM after; one safe-mode open per boot and none with force; the reply for an open with no device and a failed one, the VM resumed either way; a failed start opening nothing and a failed resume saying the VM stays paused; the boot's events carrying `start_vm` and no notice. On `trial-deb13`: `start_vm` replies "console captured from boot", a read from offset 0 holds GRUB's menu onward, a `wait_for` of `login:` without `since` matched a prompt printed before the call in 0 ms, and a second start says "already running". After `reset_vm` of `t-u` a read from offset 0 starts at the firmware's `BdsDxe` lines. The ten cases whose replies changed pass with updated expectations; the live crashed refusal is in unit 15's crash cases | Sonnet code review: a VM paused with reason crashed was resumed (now refused, with a test), amendment 3 was missing from the design (added, with the file list), the expectation closed before the resume, and a start could boot a VM during `reset_vm`'s disk swap (the claim now covers the reset); all fixed |
| 8. Snapshot revert with a capture | `feat(start): capture the console of a revert to a running snapshot` | Unit test: only a snapshot whose state is `running` makes the revert start the VM. On the BIOS `t-rv` from the `debian13` template, three runs in a row: a revert to a running snapshot from shut off and then from running each reply "console captured from the snapshot on", and a read from offset 0 holds the restored kernel's lines and a line the guest wrote after the revert; the earlier capture reads as the previous run, "closed by snapshot_revert"; a revert to a shut-off snapshot replies without a capture and leaves no console open. `console-generation.json` asserts the capture after a revert of the running `t-g` | Unit 10's review was before this unit; the final review covers it |
| 9. Event callbacks | `feat(events): record libvirt events through callbacks and a worker` | `src/events/ffi.rs`: unit tests show the wrapper returning from a body that panics, a full queue dropping new events and marking when, the close mark taken once with its time, and the free callback releasing the queue reference. On a throwaway `t-*` VM a start and a destroy reached the queue as `started booted` and `stopped destroyed` | Sonnet code review with unit 10 |
| 10. Event worker and reconnect | `feat(events): record libvirt events through callbacks and a worker` | `src/events/worker.rs`, 11 unit tests with a fake host and session: visibility and its cache, an answer that cannot be told left uncached, the overflow gap, `undefined` dropping the log and the console sessions, reconnect retries, a reconnect from a tool call and one from the close callback each registering again with the gap and `during_gap`, a stale close not dating the next gap, a failed registration retried with its gap, and a VM with queued events left out of `during_gap`. On `t-ev10` from the template: one `rebooted` for a reboot libvirt sent twice, the agent records around it and around an agent restart, one `crashed panicked` for a guest panic, and the forced stop; the undefine dropped the log. The live reconnect is an exception above | Sonnet code review: a stale close time could date a later gap, a failed lookup was cached as hidden, a failed registration was never retried, a panic in the hook could poison the connection lock, one unreadable VM stopped the `during_gap` pass, a registration error lost its message, a queued stop could be recorded twice, and `agent_disconnected` read as running; all fixed, with tests |
| 11. Attribution and `vm_events` | `feat(events): attribute power actions and add vm_events` | The hints test lists `vm_events` among the read-only tools, kept in read-only mode, at 45 tools. `console-refusals.json` shows `vm_events` refusing `trial-deb13` under a config that leaves it out. On `t-ev` from the template: the boot's `started`, `paused` and `resumed` carry `create_vm_from_template`; a reboot the guest started is `rebooted` with cause `outside`; `stop_vm` records `stopped shutdown` with cause `stop_vm`; `kinds` returns only those kinds; after `delete_vm` the VM's `vm_events` is refused | Unit 12's review |
| 12. Notices in replies | `feat(events): announce events in replies and wait for them` | Unit tests: the notice is its own block after a JSON block and comes once; an error keeps its error and gains the notice; a hidden VM gets none and keeps its records unannounced; two concurrent calls carry it once; a read or a wait match past an unannounced record leaves it to announce. On `t-nt` from the template: a panic during an `exec` gave that error reply one notice, "crashed (panicked) at …; it holds its memory until stop_vm with force: true; see vm_events since 5", the replies after it none; `start_vm` refused the crashed VM; the current run holds "Kernel panic - not syncing"; a forced `stop_vm` raised none | Sonnet code review of units 5, 11 and 12: a `since` past an unannounced record lost its notice (the log now keeps the records shown), and every reply paid a libvirt lookup (now only when a notice may be due); fixed with tests |
| 13. `wait_for` event condition | `feat(events): announce events in replies and wait for them` | 6 unit tests in the wait engine with a scripted log: an unknown field refused; an event before the stopped check wins over `closed`; an all-event wait on a stopped VM matches `started`; `since` catches a record that landed before the call, and without it only new records count; a zero timeout checks once; another VM's or kind's records never match. On `t-wc`: a wait for `crashed` with a console condition returned the event 4 s in, when the guest panicked; a zero-timeout wait with `since: 0` matched it; after `stop_vm` an event-only wait on the stopped VM matched `stopped` with cause `stop_vm` | Sonnet code review: a wait match moved the cursor over an unannounced crash (fixed with unit 12's), the CHANGELOG entry and the event resume point in the description were missing (added), and a console closing before the crash record could end the wait (the grace now covers it) |
| 14. `vm_stats` | `feat(stats): report a VM's resource use with vm_stats` | 13 unit tests in `src/stats.rs`: rates from fixed samples, the 100 % bound, counters that reset, devices matched by name, guest memory present and each of the three missing causes, read-only mode with an agent that fails the test on any call, a silent agent, a shut-off VM and one that stops between the samples; the hints test counts 46 tools and keeps `vm_stats` in read-only mode. On `t-st`: two busy vCPUs read 100.0 % and 99.6 %, one 50.1 %; the filesystem `/` had `used_bytes` equal to `df`'s Used and `total_bytes` equal to Used plus Avail; with the guest agent stopped the reply kept the rest and named the agent error; a shut-off VM got its state; a read-only server gave no filesystems and said why. On `t-win11`: three filesystems (FAT32 and NTFS) and "the VM has no memory balloon device" | Sonnet code review: approved; the description and CHANGELOG now say only missing memory figures and filesystems carry a reason, and a reset counter reads null |
| 15. Test-bed cases and docs | `test(testbed): cover events, captures, crashes and vm_stats` | `run-all.sh` with the eight new units passed all ten cases in 305 s: `diag-start-capture`, `diag-crash-preserve`, `diag-crash-destroy`, `diag-attribution`, `diag-wait-event`, `diag-stats` with `diag-stats-read-only` and `diag-stats-after`, `diag-identity` and `diag-hidden`. The `t-win11` `vm_stats` check runs in `win-exec.json` and passes in unit 16's full run. ShellCheck and shfmt pass on `run-all.sh` | Self-reviewed; unit 16's reviews cover it. The first run's failures were the cases' own expectations, each fixed: `start_vm` waits for the agent only with `wait_for_agent_secs`; the `debian13` template's console starts at `Booting 'Debian GNU/Linux'`, not at a GRUB menu; agent records carry cause `outside` and raise no notice; a new server process starts with an empty log; a recreated VM has no previous run at all |
| 16. Final gate and reviews | `docs(roadmap): mark outcome 4 done and point to outcome 5`; `fix(events): settle the final reviews of outcome 4`; `fix(events): leave a batch's events to themselves across a reconnect`; `fix(events): drop a VM's waiting events when it is undefined` | At `fix(events)`: the gate passed in each commit's hooks with `cargo deny check advisories` clean, `cargo +1.94 test --locked` with `VIRTMANAGER_REQUIRE_PWSH=1` passed 417 tests before the fixes and the suite 419 after, and `run-all.sh` passed 61 of 61 cases in 1448 s with `t-win11`, `win-exec`'s `vm_stats` check included. The design's checks: a read from offset 0 after `start_vm` holds `Booting 'Debian GNU/Linux'` (`diag-start-capture`); a forced panic gives one `crashed` record, one notice in the `exec` error and "Kernel panic - not syncing" in the console (`diag-crash-preserve`, `diag-crash-destroy`); each invariant has the unit test or case its table row names, with the hidden and deleted VM refusals in the test bed as the exceptions table records; CPU use reads above 90 % with two busy vCPUs (`diag-stats`), and the filesystem figures matched `df` by hand on `t-st` (unit 14) | Sonnet code review of the whole run: two required fixes (a doubled callback registration after a failed one, and a lock-order inversion in `reset_vm`) and one optional fix (expectations of refused calls), each fixed with a test; one optional finding declined as a judgement call; the scoped re-review found them addressed. Separate Sonnet conformance review: no implementation defect; it found amendment 2 missing from the design, stale design text, the `vm_events` description pointing at the wrong run, a revert's `shutdown` read as `outside`, and evidence gaps; all fixed or recorded, and its scoped recheck found C1 to C8 resolved. Opus code review of the whole run at `docs(plans): record outcome 4's final gate and reviews`: approved, with one optional finding, a reconnect inside the worker's batch that recorded a stop twice or lost an event without a gap, fixed in `fix(events): leave a batch's events to themselves across a reconnect` with tests, one of which fails without the fix. Its two scoped Opus re-reviews found that the first fix cached a failed lookup as hidden and put the gap in the wrong logs, then that retried events could land out of order; both were fixed in the same commit. The re-review of that commit approved, with one optional finding that the commit itself caused, a waiting event retried after its VM's undefine caching the UUID as hidden; it was fixed in `fix(events): drop a VM's waiting events when it is undefined`, with a test that fails without the fix. After that commit, `run-all.sh` passed 61 of 61 cases in 1448 s with `t-win11`. A second separate Sonnet conformance review of the whole run at that commit found no material mismatch and no design change beyond the amendments and judgement calls here; its minor findings (the design silent on the worker's waiting events, the overflow gap's logs, the worker's file in unit 10, the unit count and a long doc comment) were fixed in `docs(plans): settle the second conformance review of outcome 4`. With those fixes, `cargo +1.94 test --locked` passed 423 tests and `run-all.sh` passed 61 of 61 in 1452 s with `t-win11`, and the gate passed in that commit's hooks |
| 17. `job_stop` on Windows: taskkill statuses | `fix(jobs): end a Windows job's tree when taskkill fails on a racing root` | [Probe note](../research/2026-11-23-taskkill-race-probe.md). Before: 24 cycles gave exit 0 ×8, 128 ×14 (2 with a surviving child), 255 ×2. After, with the race injected: 30 cycles, exit 0 ×20 and 7 ×10, no survivors. Unit tests with the fake Windows guest for exits 7 and 5; parse test; `win-jobs.json` passed 5 times in a row on the rebuilt server, every stop with status 0 | Self-reviewed |
| 18. `templates-reset-saved.json` waits for the login prompt | `test(testbed): wait for the login prompt after a refused reset` | The getty on the Debian template prints `login:` again after a bare Enter, so the wait without `since` matched; the `templates-reset-uefi` unit passed 3 times in a row | Self-reviewed |
