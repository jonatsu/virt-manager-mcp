# Test bed

This directory holds the manual test bed for `virtmanager-mcp`. A small Python client drives the server over stdio
through scripted tool calls, and the scripts target one Debian 13 guest on `qemu:///system`. The same setup ran the
2026-11-09 trial recorded in
[`docs/research/2026-11-09-vm-management-mcp-evaluation.md`](../../docs/research/2026-11-09-vm-management-mcp-evaluation.md).

| File | Purpose |
| -- | -- |
| `mcp_call.py` | Standard-library MCP stdio client. It runs a JSON list of tool calls and prints each reply. |
| `config.toml` | Server config that allows only `trial-deb13`, caps created VMs at one, and uses `agent-vms`. |
| `config-templates.toml` | Server config for VMs made from templates: no `trial-deb13`, at most three 1 GiB VMs, four templates. |
| `preseed.cfg` | Unattended Debian 13 install with `qemu-guest-agent` and a serial getty on `ttyS0`. |
| `cases/*.json` | Call lists from the trial, one per area. |

## Cases

| Case | What it exercises |
| -- | -- |
| `smoke.json` | Tool listing, `guest_info`, `exec` with a timeout, file round-trip, and refusal of a VM outside `allowed_vms`. |
| `guest-agent-and-snapshots.json` | `exec` exit codes, stderr, stdin and timeout; literal argument passing; files; snapshot create, revert and delete. |
| `lifecycle-guardrails.json` | `create_vm` refusals (bad name, vCPU cap, existing name, `max_vms`, ISO outside `iso_dirs`), then start, stop and delete. |
| `console-stall.json` | Logs in as root on the serial console, rereading from offset 0 with `wait_ms`. |
| `console-after-start.json` | Opens the console during boot and reads one byte at a time. |
| `write-file-mode.json` | `write_file` modes: 644 by default, an explicit `600`, a kept `640` on append and overwrite, a refused `4755`. |
| `start-vm-waits-for-agent.json` | Stops the guest agent, which restarts 20 s later; `start_vm` waits and reports it responding. |
| `console-reopen.json` | A second `console_open` reports the session open; with `force: true` it replaces the session, which then works. |
| `console-closed-on-power-off.json` | Powers the VM off under an open console, once from inside the guest and once with a forced `stop_vm`; each next read reports the console closed. The case starts the VM again. |

The template cases below run with `config-templates.toml`. They need the template volume `tpl-debian13.qcow2`, which
`scripts/make-template.sh` builds ([`docs/templates.md`](../../docs/templates.md)).

| Case | What it exercises |
| -- | -- |
| `templates-create.json` | `list_templates` hiding a missing volume; three VMs from templates, one UEFI, timed to a responding agent; hostname, size and lifetime refusals and `max_vms`; deletion of overlays, seeds and attached disks. |
| `templates-reset.json` | Resets a VM with a marker file, a snapshot and an attached disk; refuses the unowned `t-listed`, a `create_vm` VM and `trial-deb13`; leaves `t-s` running on the `scratch` template. |
| `templates-reset-missing.json` | Run after `virsh vol-delete --pool agent-vms tpl-scratch.qcow2`: the reset of `t-s` fails with its disks in place, then the case deletes its VMs. |
| `templates-reset-uefi.json` | Creates `t-u` from `debian13-uefi` and resets it; the reset closes the console opened before it, and the VM boots under UEFI with its marker file gone. |
| `templates-reset-saved.json` | Run after `virsh managedsave t-u`: the reset discards the saved state and boots fresh; a `create_vm` VM named `t-u-g4` holding the next disk name makes the next reset refuse with nothing changed, and the console opened before it still reads output; then both VMs are deleted. |
| `templates-expiry.json` | `t-exp` with a 1-minute lifetime and an attached disk, and `t-keep` without one; after 130 s the server's sweep has deleted `t-exp` and its volumes, and `t-keep` and `t-listed` remain. |
| `templates-expiry-command.json` | Creates `t-exp2` with a 1-minute lifetime and an attached disk, and exits. Wait 70 s, then run `virtmanager-mcp --config tests/testbed/config-templates.toml sweep`: it deletes `t-exp2` and its volumes and exits 0. |
| `templates-outside-disk-setup.json`, `templates-outside-disk.json` | A reset and an expiry leave a disk file outside every pool in place; run with the steps below. |

`templates-reset.json` and `templates-expiry.json` need `t-listed`, a VM defined with `virsh` and never started.
`templates-reset.json` also needs `tpl-scratch.qcow2`, made with
`virsh vol-clone --pool agent-vms tpl-debian13.qcow2 tpl-scratch.qcow2`.

`console-stall.json` rereads from offset 0 with `wait_ms`, as an agent often does. It passes when the reads show the
`Password:` prompt and a root shell prompt. It leaves root logged in on `ttyS0`; send `exit` to log out.

### Disks outside the pool survive reset and expiry

A reset and an expiry delete only volumes in `manage.disk_pool`. This check shows that a disk file outside every pool
stays in place:

1. Run `templates-outside-disk-setup.json`. It creates `t-or`, running, and `t-oe`, stopped with a 1-minute lifetime.

2. Set `DIR` to a directory you own outside every pool, make a file there with `truncate -s 1M "$DIR/outside.img"`,
   and note its `sha256sum`.

3. Attach it to both VMs in their stored configuration only, so QEMU never opens it:

   ```sh
   for vm in t-or t-oe; do
     virsh -c qemu:///system attach-disk "$vm" "$DIR/outside.img" vdd --config --targetbus virtio \
       --driver qemu --subdriver raw --mode readonly
   done
   ```

4. Once `t-oe` has expired, run `templates-outside-disk.json`. The server's first sweep deletes `t-oe` and logs the
   outside disk as not deleted; the reset of `t-or` reports it not deleted either; then `t-or` is deleted.

5. Check that the file still exists, with the same owner and checksum.

### A failed volume deletion during expiry (needs root)

The sweep keeps a VM whose volume it cannot delete, so the next sweep retries it. Producing that failure needs root,
so run this check by hand, from the repository root:

1. Run `templates-expiry-command.json`. It creates `t-exp2` with a 1-minute lifetime and an attached disk.
2. Make the attached disk undeletable: `sudo chattr +i /var/lib/libvirt/images/agent-vms/t-exp2-vdc.qcow2`.
3. Wait 70 s, then run `target/release/virtmanager-mcp --config tests/testbed/config-templates.toml sweep`. It exits 1,
   and its log says it could not delete `t-exp2-vdc.qcow2` and keeps `t-exp2`.
4. Check that `virsh -c qemu:///system dominfo t-exp2` still finds the VM.
5. Make the disk deletable again: `sudo chattr -i /var/lib/libvirt/images/agent-vms/t-exp2-vdc.qcow2`.
6. Run the sweep command again. It deletes `t-exp2` and exits 0, and `virsh -c qemu:///system vol-list agent-vms` lists
   no `t-exp2` volume.

## Run a case

Build the server, then run a case from a scratch directory, because the client saves screenshots as
`shot-N.png` in the current directory:

```sh
cargo build --release --locked
mkdir -p /tmp/testbed-run && cd /tmp/testbed-run
python3 -I "$REPO/tests/testbed/mcp_call.py" "$REPO/tests/testbed/cases/smoke.json" -- \
  "$REPO/target/release/virtmanager-mcp" --config "$REPO/tests/testbed/config.toml"
```

Set `REPO` to the repository root first. A call list may use three special tools: `__list__` prints every advertised
tool with its input properties, `__sleep__` pauses for `args.secs` seconds, and `__time__` prints a timestamp, so the
time between two of them measures the calls in between.

## Recreate the guest

The pool and guest already exist on the development host. Recreate them on a new host as follows. The pool is created
once:

```sh
virsh -c qemu:///system pool-define-as agent-vms dir --target /var/lib/libvirt/images/agent-vms
virsh -c qemu:///system pool-build agent-vms
virsh -c qemu:///system pool-start agent-vms
virsh -c qemu:///system pool-autostart agent-vms
```

Create the disk and run the network install. The installer reads `preseed.cfg` from the initrd:

```sh
virsh -c qemu:///system vol-create-as agent-vms trial-deb13.qcow2 10G --format qcow2
/usr/bin/python3 /usr/bin/virt-install --connect qemu:///system --name trial-deb13 --memory 2048 --vcpus 2 \
  --osinfo debian13 \
  --disk vol=agent-vms/trial-deb13.qcow2,bus=virtio --network network=default,model=virtio \
  --location https://deb.debian.org/debian/dists/trixie/main/installer-amd64/ \
  --initrd-inject "$REPO/tests/testbed/preseed.cfg" \
  --extra-args 'auto=true priority=critical console=ttyS0,115200' \
  --channel unix,target.type=virtio,target.name=org.qemu.guest_agent.0 \
  --graphics spice --noautoconsole
```

The guest powers off when the install finishes, because `preseed.cfg` ends with `debian-installer/exit/poweroff`.
Start it with the server's `start_vm` and `wait_for_agent_secs: 180`; the guest agent then answers.

These commands ran on 2026-11-09 with virt-install 5.1.0, the Debian trixie netboot installer from deb.debian.org,
and guest kernel 6.12.107+deb13-amd64. Run `virt-install` through `/usr/bin/python3` wherever a version manager's
Python shadows the system one, since the system Python provides the `gi` module that `virt-install` needs.

## Credentials

The guest's root password is `trial`, and the SSH server is installed. The guest sits on libvirt's NAT network
`default`, so only this host can reach it. Do not attach the guest to a bridged network with this password.

## Pitfalls

Cloud images did not work on this host. Each attempt uploaded the image with `vol-create-as`, `vol-upload` and
`vol-resize 10G`, then ran `virt-install --import`:

- Debian 13 `genericcloud` and `generic` with `--cloud-init user-data=...` never applied the user data. The hostname
  stayed `localhost`, and no user or guest agent existed. The cause is unknown.
- Debian 13 `nocloud` without cloud-init rejected an empty root password on the console.

During the first boot of a cloud-init import, virt-install powers the VM off when the guest reboots, so a reboot looks
like a crash.
