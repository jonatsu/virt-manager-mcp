# Guest panics, crash devices and libvirt event sequences (2026-11-23)

Probe for unit 3 of the [outcome 4 plan](../plans/seeing-what-happened-implementation-plan.md). libvirt 12.0.0 on
`qemu:///system`; `virsh event --all --loop --timestamp` ran throughout. The Linux guest was `t-pr`, made by
`create_vm_from_template` from `debian13` (BIOS, kernel `6.12.115+deb13.1-cloud-amd64`) and given, with `virt-xml`,
`<panic model='isa'/>`, `<stats period='5'/>` in its balloon, and each `<on_crash>` value. The Windows guest was
`t-win11` (Windows 11, UEFI) with `<panic model='hyperv'/>` and `<on_crash>preserve</on_crash>`.

## Linux guest

- **Driver.** The guest loads `pvpanic` and `pvpanic_mmio` itself.
- **Panic under `preserve`.** `echo c > /proc/sysrq-trigger` (after `echo 1 > /proc/sys/kernel/sysrq`) gives one
  event, `Crashed Panicked`, and the state `crashed (panicked)`. CPU time grew 24 ms over 10 s, so the vCPUs stay
  stopped. `exec` of the panicking command returns an error after its timeout: `guest agent 'guest-exec-status' failed:
  guest agent command timed out: guest agent didn't respond to command within '10' seconds` with `timeout_secs: 10`.
- **A crashed VM and the start paths.** `virsh resume` takes `crashed (panicked)` to `running (unpaused)`, so a resume
  lets a panicked guest run on. `virsh start` fails with "Domain is already active". Today's `start_vm` refuses: "VM
  't-pr' has crashed; stop it with force: true, then start it".
- **Force stop of a crashed VM.** `Shutdown Finished after host request`, then `Stopped Destroyed`.
- **Panic under `destroy`.** `Crashed Panicked`, then about 0.3 s later `Stopped Crashed`; the state is
  `shut off (crashed)`. `exec` of the panicking command returns `guest agent 'guest-exec-status' failed: Requested
  operation is not valid: domain is not running`.

## Windows guest

`wininit` is protected: `Stop-Process -Name wininit -Force` as `SYSTEM` fails with "Access is denied". The stop error
was raised instead by a PowerShell process that enabled `SeDebugPrivilege`, marked itself critical with
`RtlSetProcessIsCritical`, and exited (`CRITICAL_PROCESS_DIED`).

- One event, `Crashed Panicked`, and the state `crashed (panicked)`; CPU time grew 43 ms over 10 s.
- A screenshot shows the desktop as it was at the crash, not a stop screen: the Hyper-V crash notification stops the
  guest before Windows paints one. No bugcheck record (System event 1001) is written, because no dump is taken.
- `exec` of the crashing command returns `guest agent 'guest-exec-status' failed: Requested operation is not valid:
  domain is not running`.
- After `stop_vm` with `force: true`, the guest boots and its agent answers.

## Starts, reverts, reboots and stops

| Action | Events, in order | State after |
| -- | -- | -- |
| `start_vm` | agent `disconnected` (domain started), `Resumed Unpaused`, `Started Booted`, agent `connected` | running (booted) |
| `virsh start --paused` | none until the resume; then the same as a start | paused (user) |
| `snapshot-create-as` of a running VM | `Suspended Paused`, `Resumed Unpaused` | running |
| `snapshot-revert --paused` to a running snapshot, from running | `Shutdown Finished after host request`, `Stopped Snapshot`, agent `disconnected`, `Started Snapshot`, `Suspended Snapshot`, `Defined Snapshot` | paused (from snapshot), new domain ID |
| The same from shut off | agent `disconnected`, `Started Snapshot`, `Suspended Snapshot`, `Defined Snapshot` | paused (from snapshot), new domain ID |
| Guest `systemctl reboot` | agent `disconnected` (channel event), `reboot` twice, 6 ms apart, agent `connected` | running |
| Guest agent restart | agent `disconnected`, agent `connected` about 1 s later | running |
| `stop_vm` without `force` | agent `disconnected`, `Shutdown Finished after guest request`, `Stopped Shutdown` | shut off (shutdown) |
| Guest `systemctl poweroff` | the same as `stop_vm` without `force` | shut off (shutdown) |
| `stop_vm` with `force` | `Shutdown Finished after host request`, `Stopped Destroyed` | shut off (destroyed) |

A console opened between `virsh start --paused` and `virsh resume` holds the boot from its start: a read from offset
0 began with GRUB's `Booting 'Debian GNU/Linux'` and the kernel's `[    0.000000] Linux version` line.

## Observed sequences for the kind mapping

| libvirt event | Detail | Seen when |
| -- | -- | -- |
| lifecycle `Started` | `Booted` | start, resume after a paused start |
| lifecycle `Started` | `Snapshot` | revert to a running snapshot |
| lifecycle `Suspended` | `Paused`, `Snapshot` | snapshot create; paused revert |
| lifecycle `Resumed` | `Unpaused` | every start; resume |
| lifecycle `Shutdown` | `Finished after guest request` | graceful stop, guest poweroff |
| lifecycle `Shutdown` | `Finished after host request` | forced stop, revert from running |
| lifecycle `Stopped` | `Shutdown` | graceful stop, guest poweroff |
| lifecycle `Stopped` | `Destroyed` | forced stop |
| lifecycle `Stopped` | `Snapshot` | revert from running |
| lifecycle `Stopped` | `Crashed` | panic under `destroy`, after `Crashed Panicked` |
| lifecycle `Crashed` | `Panicked` | panic, Linux (`isa`) and Windows (`hyperv`) |
| lifecycle `Defined` | `Added`, `Updated`, `Snapshot` | define, XML edit, revert |
| `reboot` | none | guest reboot, sent twice |
| agent lifecycle | `connected` or `disconnected`, reason `channel event` or `domain started` | agent up or down, every start |
