# Test bed

This directory holds the manual test bed for `virtmanager-mcp`. A small Python client drives the server over stdio
through scripted tool calls, and the scripts target one Debian 13 guest on `qemu:///system`. The same setup ran the
2026-11-09 trial recorded in
[`docs/research/2026-11-09-vm-management-mcp-evaluation.md`](../../docs/research/2026-11-09-vm-management-mcp-evaluation.md).

| File | Purpose |
| -- | -- |
| `mcp_call.py` | Standard-library MCP stdio client. It runs a JSON list of tool calls and prints each reply. |
| `config.toml` | Server config that allows only `trial-deb13`, caps created VMs at one, and uses `agent-vms`. |
| `preseed.cfg` | Unattended Debian 13 install with `qemu-guest-agent` and a serial getty on `ttyS0`. |
| `cases/*.json` | Call lists from the trial, one per area. |

## Cases

| Case | What it exercises |
| -- | -- |
| `smoke.json` | Tool listing, `guest_info`, `exec` with a timeout, file round-trip, and refusal of a VM outside `allowed_vms`. |
| `guest-agent-and-snapshots.json` | `exec` exit codes, stderr, stdin and timeout; literal argument passing; files; snapshot create, revert and delete. |
| `lifecycle-guardrails.json` | `create_vm` refusals (bad name, vCPU cap, existing name, `max_vms`, ISO outside `iso_dirs`), then start, stop and delete. |
| `console-stall.json` | Reproduces the serial console stall: the echo of `root` arrives, the password prompt never does. |
| `console-after-start.json` | Opens the console during boot and reads one byte at a time. |

`console-stall.json` rereads from offset 0 with `wait_ms`, as an agent often does. It passes when the reads show the
`Password:` prompt and a root shell prompt. It leaves root logged in on `ttyS0`; send `exit` to log out.

## Run a case

Build the server, then run a case from a scratch directory, because the client saves screenshots as
`shot-N.png` in the current directory:

```sh
cargo build --release --locked
mkdir -p /tmp/testbed-run && cd /tmp/testbed-run
python3 -I "$REPO/tests/testbed/mcp_call.py" "$REPO/tests/testbed/cases/smoke.json" -- \
  "$REPO/target/release/virtmanager-mcp" --config "$REPO/tests/testbed/config.toml"
```

Set `REPO` to the repository root first. A call list may use the special tool `__list__`, which prints every
advertised tool with its input properties.

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
