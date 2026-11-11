# Test bed

This directory holds the manual test bed for `virtmanager-mcp`. A small Python client drives the server over stdio
through scripted tool calls, and the scripts target one Debian 13 guest on `qemu:///system`. The same setup ran the
2026-11-09 trial recorded in
[`docs/research/2026-11-09-vm-management-mcp-evaluation.md`](../../docs/research/2026-11-09-vm-management-mcp-evaluation.md).

| File | Purpose |
| -- | -- |
| `mcp_call.py` | Standard-library MCP stdio client. It runs a JSON list of tool calls and prints each reply. |
| `run-all.sh` | Runs every case with `--check` and the `virsh` steps between them; see "Check a run". |
| `config.toml` | Server config that allows only `trial-deb13`, caps created VMs at one, and uses `agent-vms`. |
| `config-templates.toml` | Server config for VMs made from templates: no `trial-deb13`, at most three 1 GiB VMs, four templates. |
| `preseed.cfg` | Unattended Debian 13 install with `qemu-guest-agent` and a serial getty on `ttyS0`. |
| `t-inst.xml`, `t-win.xml` | Installer guests for the screen cases; see "The installer guests". |
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
| `console-closed-on-power-off.json` | Powers the VM off under an open console, once from inside the guest and once with a forced `stop_vm`; each next read reports the console closed, the one after `stop_vm` with `wait_ms: 0`. The case starts the VM again. |
| `screenshot-region.json` | `WxH` for a full `screenshot`; a quarter region's size and origin; bad regions refused. |
| `screen-settled.json` | `screen_settled` matches the idle login screen and attaches a screenshot; 0.1 s is refused. |
| `screen-no-models.json` | Without `display.ocr_models`, `screen_text` is refused, alone and next to `screen_settled`, before the wait starts; `screen_settled` alone still matches. |

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
| `console-generation.json` | Template config. A forced reopen starts a new generation; a read naming an old generation, or `since` above 0 without one, is refused; `snapshot_revert` of the running VM closes the console. |
| `owrt-pacing.json` | On `t-owrt`, writes 4000 bytes to `cat` once paced (16-byte chunks, 20 ms) and once unpaced, pausing a second after starting `cat` and after each Ctrl-D so the shell has handed the terminal to `cat`; `wc -c` and `md5sum` show both intact (`3f59f6f6…`). Pacing above 1000 ms and a paced write longer than `max_wait_secs` are refused. |
| `owrt-boot.json` | Boots `t-owrt` with the console open and reaches its root shell; see "The OpenWrt appliance". |
| `jobs.json`, then `jobs-reopen.json` | Run both from the same directory: the first saves job IDs to `vars.json` and leaves `t-w` running for the second, which a new server process runs. `jobs.json` creates `t-w`; a job printing a line a second reads `running` with output and `next` growing over two reads; after `systemctl restart qemu-guest-agent` it still reads `running` with more output; a command holding `'$HOME'`, quotes and `${NOT_SET:-literal braces}` prints `$HOME\|it's "quoted"` and `literal braces` and reads `exited` with code 0; `yes \| head -c 10` reads `exited` with code 0 and exactly 10 bytes of output, because jobs keep the default SIGPIPE action; a `sleep 300` job whose wrapper `exec` kills with `kill -9` reads `ended without status`; `job_list` lists the four. `jobs-reopen.json` lists them again; reads the first job; `job_stop` sends TERM, and the next read shows `exited` with code 143; a second `job_stop` is refused as not running; the ID `../../../etc` is refused; with `/var/lib/virtmanager-mcp` replaced by a link to `/tmp`, `job_start` is refused naming the link, and `ls -A /tmp` lists the same entries before and after; `job_start`, `job_read`, `job_stop` and `job_list` on `trial-deb13` are each refused as outside `allowed_vms`; `t-w` is deleted. |
| `job-wait.json` | Creates `t-w` and opens its console. With a job printing `step N` once a second, `[job_output "step 3", console "Kernel panic"]` matches index 0 after about 3 s with `next`; `[job_done]` on `sleep 3; exit 5` matches with "exited with code 5"; a job printing more than 1 MiB of two-byte characters on one line, then `step 3`, matches `[job_output "step 3"]`, which shows a read cut inside a character still moves on; a 3 s `job_output` wait for text never printed times out with a resume point and no generation; `job_done` on a job ID the guest does not have is refused; `t-w` is deleted. |
| `owrt-wait.json` | Boots `t-owrt`; `wait_for` matches "Please press Enter to activate this console", and after Enter matches the root prompt, which reads `root@(none):~#` this early in boot. |
| `mouse-refusals.json` | Starts `t-owrt`, which has no display or tablet: `mouse` is refused for want of a USB tablet, and `mouse` on `trial-deb13` is refused as outside `allowed_vms`; then `t-owrt` is powered off. |
| `wait-for.json` | Creates `t-w` without waiting for its agent. In order: `[agent]` matches once the agent answers; after Enter, `[console "login:"]` matches with `next` and `generation`, and `console_read` still returns that output; the same wait again times out, because the cursor moved past the match; `[console "marker-42", command "test -e /tmp/ready"]` times out after 5 s with a resume point and `last_exit_code` 1; after `exec` creates the file and writes the marker to `/dev/ttyS0`, the command wait matches with its output `ready`, and a console wait matches `marker-42`, written between the calls; with a sysrq crash 3 s away, `[console "login:", console "Kernel panic", command "false"]` matches index 1 within about 3 s while the agent hangs (the `__time__` lines measure it); after a restart and a `poweroff` 5 s away, a wait for text that never appears ends as `closed` because the console stream ended; after another restart and `poweroff`, `[command "false"]` ends as `closed` with "the VM is not running"; `trial-deb13` is refused as outside `allowed_vms`, and a bad pattern is refused; `t-w` is deleted. |
| `templates-outside-disk-setup.json`, `templates-outside-disk.json` | A reset and an expiry leave a disk file outside every pool in place; run with the steps below. |
| `manage-lock.json` | Run it through `run-all.sh`, which holds the management lock from another process with `flock(1)` for 40 s and gives the server its own `HOME`, so servers of live sessions are not held up. Creating `t-lock` fails after 30 s, naming the lock another operation holds; the second create waits for the holder to exit and succeeds; `t-lock` is deleted. |

The screen cases below need the OCR models and the installer guests in "The installer guests". They run with a screen
config: `config-templates.toml` with `t-inst` and `t-win` added to `allowed_vms` and `power_vms`, and
`display.ocr_models` set. `run-all.sh` writes it.

| Case | What it exercises |
| -- | -- |
| `screen-installer.json` | Boots `t-inst` from the Debian netinst ISO. `screen_text` matches "Graphical install" on the UEFI GRUB menu and attaches a screenshot; after Enter it matches "Select a language", and `screen_settled` matches. "Italian" is not on screen until a `mouse` scroll of the language list, then it matches. A `mouse` click on "Continue" at (1218, 774) leads to "Select your location". Keys go on through the keyboard, network, hostname, user and time-zone screens to "Partition disks", each one sent once its screen's text matched and the screen held still for 3 s, because the installer drops input that arrives in the first 2 s or so of a new screen. On the changing screen that loads installer components, a `screen_text` wait for text never shown times out after 8 s with its last OCR text (the `__time__` lines measure it). A `screen_text` wait on the title region matches `^Partition disks$`, and a region screenshot of the title reports its size and origin. The VM is then powered off. |
| `screen-windows.json` | Boots `t-win` from the Windows 11 ISO. `screen_text` matches "Press any key to boot from CD", Enter goes three times a second apart, and `screen_text` matches "Windows 11 Setup" with a screenshot attached. The VM is then powered off. |

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
4. Check that `virsh -c qemu:///system dominfo t-exp2` still finds the VM, and that
   `virsh -c qemu:///system vol-list agent-vms` still lists its boot disk `t-exp2-g1.qcow2` and seed
   `t-exp2-g1-seed.img`.
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
time between two of them measures the calls in between. A call may also carry `"save": {"name": "field"}`, which keeps
that field of the reply's JSON under the name; a later argument holding `${name}` gets the value. Saved values persist
in `vars.json` in the current directory, so a later case file run from there can use them.

## Check a run

With `--check` before the case file, `mcp_call.py` checks each reply against the call's `expect` and exits 1 when a
check fails; its docstring describes the format. Every case carries expectations taken from its row in the tables
above. Values that change between runs, such as job IDs, offsets and generations, are matched by pattern. Timings that
`__time__` lines measure stay unchecked; read them from the output.

`run-all.sh` runs every case with `--check`, from the repository root:

```sh
tests/testbed/run-all.sh                            # every case
tests/testbed/run-all.sh templates-reset jobs.json  # a subset
```

It builds the release binary, then runs the cases in a fixed order and does the `virsh` steps this README describes
between them. Those steps define `t-listed`, clone and delete `tpl-scratch.qcow2`, shut `trial-deb13` off for
`console-after-start.json`, and save `t-u`. They also attach the outside disk, wait out expiries, run the `sweep`
command, and log root out of `ttyS0`. Naming one case of a pair that depends on the other, such as `jobs.json` and
`jobs-reopen.json`, runs both. Logs go to a new directory under `/tmp`, which the script prints. At the end it
removes the `t-*` VMs and volumes it created, leaves `trial-deb13` running, prints a summary, and exits 1 if a case
failed.

It prints `skipped: <reason>` and exits 0 when `virsh` cannot reach `qemu:///system`, or when pool `agent-vms`,
`trial-deb13` or `tpl-debian13.qcow2` is missing. Without `t-owrt`, it skips the OpenWrt cases and
`mouse-refusals.json`. The check that needs root, "A failed volume deletion during expiry", stays manual. A full run
takes about 11 minutes, and about 13 with the screen cases.

The screen cases run only when `VIRTMANAGER_OCR_MODELS` holds the absolute path of the directory that
`scripts/fetch-ocr-models.sh` fills; otherwise the summary lists them as skipped with that reason. With it set, the
script writes the screen config into its `/tmp` directory as `config-screen.toml`, defines `t-inst` and `t-win` from
their XML files unless they exist, and creates `t-inst.qcow2` unless it exists. It runs one of the two VMs at a time,
after checking that the host has the VM's memory plus 1 GiB available, and at the end removes the VMs and the disk it
created:

```sh
VIRTMANAGER_OCR_MODELS=~/.local/share/virtmanager-mcp/ocr-models tests/testbed/run-all.sh screen-installer
```

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

## The OpenWrt appliance

`t-owrt` is an OpenWrt x86-64 VM whose only channel is the serial console: it has no guest agent, no network and no
display. `config-templates.toml` allows it. Create it once, from a scratch directory:

```sh
B=https://downloads.openwrt.org/releases/25.12.6/targets/x86/64
F=openwrt-25.12.6-x86-64-generic-ext4-combined.img.gz
curl -fsSLO "$B/sha256sums" && curl -fsSLO "$B/$F"
grep -F "$F" sha256sums | sha256sum -c -
gunzip -c "$F" > t-owrt.img || [ $? -eq 2 ]   # some releases carry trailing data, which gunzip reports with 2
virsh -c qemu:///system vol-create-as agent-vms t-owrt.img "$(stat -c %s t-owrt.img)" --format raw
virsh -c qemu:///system vol-upload --pool agent-vms t-owrt.img t-owrt.img
virsh -c qemu:///system define "$REPO/tests/testbed/t-owrt.xml"
```

`owrt-boot.json` starts it, opens the console during boot, and shows the boot log ending in a `root@OpenWrt:~#`
prompt after Enter; then it powers the VM off. This ran with OpenWrt 25.12.6, which booted on the q35 machine type.

## The installer guests

`t-inst` and `t-win` boot installer ISOs for the screen cases. Their XML files came from
`/usr/bin/python3 /usr/bin/virt-install ... --print-xml` and are kept as generated.

- `t-inst.xml`: 1 GiB, 2 vCPUs, UEFI on q35, no network, a USB tablet, SPICE with virtio video, the Debian 13 netinst
  ISO `tpl-debian13-netinst.iso` as its CD-ROM, and the 8 GiB disk `t-inst.qcow2`. It also has the emulated TPM 2.0,
  random number generator and guest-agent channel that `virt-install` adds.
- `t-win.xml`: 4 GiB, 2 vCPUs, UEFI with Secure Boot and SMM, an emulated TPM 2.0, a USB tablet, SPICE with virtio
  video, the Windows 11 ISO `tpl-win11-26h2.iso` as its CD-ROM, and no disk.

Both guests' TPMs need `swtpm` on the host.

Both ISOs are volumes in `agent-vms`. Define the guests by hand as follows; `run-all.sh` does the same when they are
missing:

```sh
virsh -c qemu:///system vol-create-as agent-vms t-inst.qcow2 8G --format qcow2
virsh -c qemu:///system define "$REPO/tests/testbed/t-inst.xml"
virsh -c qemu:///system define "$REPO/tests/testbed/t-win.xml"
```

Run a screen case with a config that allows them and sets `display.ocr_models`, such as the one `run-all.sh` writes.
Remove them with `virsh -c qemu:///system undefine --nvram t-inst`, the same for `t-win`, and
`virsh -c qemu:///system vol-delete --pool agent-vms t-inst.qcow2`.

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
