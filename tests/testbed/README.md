# Test bed

This directory holds the manual test bed for `virtmanager-mcp`. A small Python client drives the server over stdio
with scripted tool calls. The scripts target one Debian 13 guest, `trial-deb13`, on `qemu:///system`. The 2026-11-09
trial in
[`docs/research/2026-11-09-vm-management-mcp-evaluation.md`](../../docs/research/2026-11-09-vm-management-mcp-evaluation.md)
used the same setup.

| File | Purpose |
| -- | -- |
| `mcp_call.py` | MCP stdio client using only the standard library. It runs a JSON list of tool calls and prints each reply. |
| `run-all.sh` | Runs every case with `--check`, with the `virsh` steps between them. See "Check a run". |
| `config.toml` | Server config that allows only `trial-deb13`, caps created VMs at one, and uses `agent-vms`. |
| `config-templates.toml` | Server config for VMs made from templates: no `trial-deb13`, at most three 1 GiB VMs, four templates. |
| `preseed.cfg` | Unattended Debian 13 install with `qemu-guest-agent` and a serial getty on `ttyS0`. |
| `t-inst.xml`, `t-win.xml` | Installer guests for the screen cases. See "The installer guests". |
| `t-owrt.xml` | Serial-only OpenWrt appliance. See "The OpenWrt appliance". |
| `t-win11.xml`, `win11/` | Windows 11 guest and its unattended build, `win11/build.sh` with `autounattend.xml`. See "The Windows guest". |
| `cases/*.json` | Call lists, one per area. |

## Cases

| Case | What it exercises |
| -- | -- |
| `smoke.json` | Tool listing, `guest_info`, `exec` with a timeout, a file round-trip, and refusal of a VM outside `allowed_vms`. |
| `guest-agent-and-snapshots.json` | `exec` exit codes, stderr, stdin and timeout. Literal argument passing, files, and snapshot create, revert and delete. |
| `lifecycle-guardrails.json` | `create_vm` refusals (bad name, vCPU cap, existing name, `max_vms`, ISO outside `iso_dirs`), then start, stop and delete. |
| `console-stall.json` | Logs in as root on the serial console, rereading from offset 0 with `wait_ms`. |
| `console-after-start.json` | Opens the console during boot and reads one byte at a time. |
| `write-file-mode.json` | `write_file` modes: 644 by default, an explicit `600`, a kept `640` on append and overwrite, and a refused `4755`. |
| `start-vm-waits-for-agent.json` | Stops the guest agent, which restarts 20 s later. `start_vm` waits and reports the agent responding. |
| `console-reopen.json` | A second `console_open` reports the session open. With `force: true` it replaces the session, and the new session works. |
| `console-closed-on-power-off.json` | Powers the VM off under an open console, once from inside the guest and once with a forced `stop_vm`. Each next read reports the console closed, and the read after `stop_vm` uses `wait_ms: 0`. The case starts the VM again. |
| `screenshot-region.json` | `WxH` for a full `screenshot`, a quarter region's size and origin, and refusal of bad regions. |
| `screen-settled.json` | `screen_settled` matches the idle login screen and attaches a screenshot. A 0.1 s value is refused. |
| `screen-no-models.json` | Without `display.ocr_models`, `screen_text` is refused before the wait starts, both alone and next to `screen_settled`. `screen_settled` alone still matches. |

The transfer cases also run on `trial-deb13`, with a config that `run-all.sh` writes into its log directory. It adds a
share directory and a pull directory there, and builds a fixed tree in the share to push. They read the
share, the pull directory and the tree as `${share}`, `${pull}` and `${tree}` from `vars.json`.

| Case | What it exercises |
| -- | -- |
| `transfer-file.json` | `copy_from_vm` pulls a file into a new subdirectory of the pull directory. A second pull is refused without `overwrite` and replaces the file with it. A pull onto a directory, into the staging directory, through `..`, outside `host_pull_dirs`, of a missing guest file and from a VM outside `allowed_vms` is refused. The script checks the pulled file's content and mode 644. |
| `transfer-push.json` | `copy_dir_to_vm` pushes the tree: 4 files with their modes, the symlink that stays inside, and neither the 3 files that `.gitignore` rules in the tree and the share ignore nor the outside link and FIFO, which the reply names. A second push is refused without `replace`, and so are `/usr`, a guest file and a host directory outside `host_share_dirs`. `replace` with `exclude: ["sub/"]` replaces the tree, and no `.vmm-` entry remains. |
| `transfer-pull.json`, then `transfer-pull-caps.json` | `copy_dir_from_vm` pulls a guest tree with a setuid file, a mode 700 directory, a symlink, a hard link and a FIFO. Three files and a directory arrive, the excluded `*.log` and `build/` entries stay out, and the reply names the skipped entries. A second pull is refused without `overwrite`, and so are a host file as the destination and a guest file as the source. `overwrite` then replaces the tree with the guest's `sub`. The caps case runs with `max_pull_bytes` 64 KiB and `max_pull_entries` 10: 12 files and a 100 KiB file are refused through both pull tools, and nothing is left behind. The script checks that every pulled entry is a file with mode 644 or a directory with mode 755, and that `.vmm-staging` is empty. |
| `transfer-background.json`, then `transfer-background-after.json` | With `max_wait_secs` 2, a 128 MiB directory pull and push each reply with a running transfer, and `transfer_done` reports each finished. The pushed file's SHA-256 matches, and the script checks the pulled one. A push cut short by a forced `stop_vm` reports an error. The script starts `trial-deb13` again, checks that its log directory holds no file over 1 MiB, and the second case removes the leftovers in the guest. |
| `addresses.json` | `vm_addresses` names the DHCP lease for `trial-deb13`, also while its guest agent is stopped, when the reply says that the agent is not connected. The agent starts again 30 s later. |

The template cases run with `config-templates.toml`. They need the template volume `tpl-debian13.qcow2`, which
`scripts/make-template.sh` builds ([`docs/templates.md`](../../docs/templates.md)).

| Case | What it exercises |
| -- | -- |
| `templates-create.json` | `list_templates` hides a missing volume. Three VMs come from templates, one of them UEFI, each timed to a responding agent. Hostname, size and lifetime refusals and `max_vms` follow, then deletion of overlays, seeds and attached disks. |
| `templates-reset.json` | Resets a VM that holds a marker file, a snapshot and an attached disk. Refuses the unowned `t-listed`, a `create_vm` VM and `trial-deb13`. Leaves `t-s` running on the `scratch` template. |
| `templates-reset-missing.json` | Run it after `virsh vol-delete --pool agent-vms tpl-scratch.qcow2`. The reset of `t-s` fails with its disks in place, then the case deletes its VMs. |
| `templates-reset-uefi.json` | Creates `t-u` from `debian13-uefi` and resets it. The reset closes the console opened before it, and the VM boots under UEFI without its marker file. |
| `templates-reset-saved.json` | Run it after `virsh managedsave t-u`. The reset discards the saved state and boots fresh. A `create_vm` VM named `t-u-g4` then holds the next disk name, so the next reset refuses and changes nothing. The console opened before that reset still reads output. Both VMs are then deleted. |
| `templates-expiry.json` | Creates `t-exp` with a 1-minute lifetime and an attached disk, and `t-keep` without a lifetime. After 130 s the server's sweep has deleted `t-exp` and its volumes, and `t-keep` and `t-listed` remain. |
| `templates-expiry-command.json` | Creates `t-exp2` with a 1-minute lifetime and an attached disk, and exits. Wait 70 s, then run `virtmanager-mcp --config tests/testbed/config-templates.toml sweep`. The command deletes `t-exp2` and its volumes and exits 0. |
| `console-generation.json` | Uses the template config. A forced reopen starts a new generation. A read naming an old generation is refused, and so is `since` above 0 without a generation. `snapshot_revert` of the running VM closes the console. |
| `owrt-pacing.json` | On `t-owrt`, writes 4000 bytes to `cat` twice: paced (16-byte chunks, 20 ms) and unpaced. It pauses a second after starting `cat` and after each Ctrl-D, so the shell has handed the terminal to `cat`. `wc -c` and `md5sum` show both writes intact (`3f59f6f6…`). Pacing above 1000 ms is refused, and so is a paced write longer than `max_wait_secs`. |
| `owrt-boot.json` | Boots `t-owrt` with the console open and reaches its root shell. See "The OpenWrt appliance". |
| `jobs.json`, then `jobs-reopen.json` | Run both from the same directory. The first saves job IDs to `vars.json` and leaves `t-w` running, and a new server process runs the second. `jobs.json` creates `t-w`. A job that prints a line a second reads `running`, with output and `next` growing over two reads. After `systemctl restart qemu-guest-agent` it still reads `running` with more output. A command holding `'$HOME'`, quotes and `${NOT_SET:-literal braces}` prints `$HOME\|it's "quoted"` and `literal braces`, and reads `exited` with code 0. `yes \| head -c 10` reads `exited` with code 0 and exactly 10 bytes of output, because jobs keep the default SIGPIPE action. A `sleep 300` job whose wrapper `exec` kills with `kill -9` reads `ended without status`. `job_list` lists the four jobs. `jobs-reopen.json` lists them again and reads the first job. `job_stop` sends TERM, and the next read shows `exited` with code 143. A second `job_stop` is refused as not running, and the ID `../../../etc` is refused. With `/var/lib/virtmanager-mcp` replaced by a link to `/tmp`, `job_start` is refused naming the link, and `ls -A /tmp` lists the same entries before and after. `job_start`, `job_read`, `job_stop` and `job_list` are each refused on `trial-deb13` as outside `allowed_vms`. `t-w` is deleted. |
| `job-wait.json` | Creates `t-w` and opens its console. With a job printing `step N` once a second, `[job_output "step 3", console "Kernel panic"]` matches index 0 after about 3 s, with `next`. `[job_done]` on `sleep 3; exit 5` matches with "exited with code 5". A job prints more than 1 MiB of two-byte characters on one line, then `step 3`, and `[job_output "step 3"]` matches. That match shows that a read cut inside a character still moves on. A 3 s `job_output` wait for text never printed times out with a resume point and no generation. `job_done` on a job ID the guest does not have is refused. `t-w` is deleted. |
| `owrt-wait.json` | Boots `t-owrt`. `wait_for` matches "Please press Enter to activate this console". After Enter it matches the root prompt, which reads `root@(none):~#` this early in boot. |
| `mouse-refusals.json` | Starts `t-owrt`, which has no display or tablet. `mouse` is refused because the VM has no USB tablet, and `mouse` on `trial-deb13` is refused as outside `allowed_vms`. Then `t-owrt` is powered off. |
| `wait-for.json` | Creates `t-w` without waiting for its agent. In order: `[agent]` matches once the agent answers. After Enter, `[console "login:"]` matches with `next` and `generation`, and `console_read` still returns that output. The same wait again times out, because the cursor moved past the match. `[console "marker-42", command "test -e /tmp/ready"]` times out after 5 s with a resume point and `last_exit_code` 1. Then `exec` creates the file and writes the marker to `/dev/ttyS0`. The command wait now matches with its output `ready`, and a console wait matches `marker-42`, written between the calls. With a sysrq crash 3 s away, `[console "login:", console "Kernel panic", command "false"]` matches index 1 within about 3 s while the agent hangs. The `__time__` lines measure that time. After a restart and a `poweroff` 5 s away, a wait for text that never appears ends as `closed`, because the console stream ended. After another restart and `poweroff`, `[command "false"]` ends as `closed` with "the VM is not running". `trial-deb13` is refused as outside `allowed_vms`, and a bad pattern is refused. `t-w` is deleted. |
| `templates-outside-disk-setup.json`, `templates-outside-disk.json` | A reset and an expiry leave a disk file outside every pool in place. Run them with the steps below. |
| `manage-lock.json` | Run it through `run-all.sh`. The script holds the management lock from another process with `flock(1)` for 40 s. It also gives the server its own `HOME`, so the servers of live sessions are not held up. Creating `t-lock` fails after 30 s, naming the lock another operation holds. The second create waits for the holder to exit and succeeds. `t-lock` is deleted. Without `flock`, `run-all.sh` skips the case. |
| `win-exec.json` | Runs on `t-win11` (see "The Windows guest"), with `t-win11` added to `allowed_vms` and `power_vms`. Starts the VM and waits for the agent. `whoami` returns `nt authority\system` with PowerShell, without CLIXML on stderr, and with `cmd`. The guest is Windows 11 Pro with Secure Boot on, a TPM present and a signed virtio serial driver. A file round-trips through `write_file` and `read_file`. Then the VM shuts down. |
| `win-jobs.json`, then `win-jobs-reopen.json` | Run both on a running `t-win11` from one directory, like `jobs.json` and its pair. A job printing `step N` a second reads `running` with output growing. A job printing `päivää ✓ 日本` and a line on stderr returns both as UTF-8, and `exit 5` reads `exited` with code 5. `job_stop` with `force` ends a `ping` job and says that `force` makes no difference on Windows. The next read shows `ended without status`. `job_list` lists the three jobs. The reopen case runs in a new server process. It lists the jobs again and reads the first with `max_bytes` 40. It stops that job, which leaves `ended without status`, and refuses a second stop as not running. Then it clears the jobs directory. |
| `win-jobs-refusals.json` | With the jobs directory replaced by a junction, `job_start` is refused naming the junction. With an access entry for `Users` added, it is refused naming the access list. With the directory restored, a job runs again. A malformed job ID is refused, and every job tool is refused on `trial-deb13` as outside `allowed_vms`. |
| `win-wait.json` | `[command "exit [int](-not (Test-Path C:\ready))"]` times out with `last_exit_code` 1, then matches once `exec` creates the file. `[job_output "step 3"]` matches while the job runs, and `[job_done]` matches with "exited with code 0". |
| `win-transfer.json` | Runs last, with the share and pull directories of the transfer cases. A file goes to `C:\Windows\Temp` through `copy_to_vm` with a matching `Get-FileHash`, and comes back through `copy_from_vm`, which the script checks. `vm_addresses` lists no interfaces, because the guest has no network interface. `copy_dir_to_vm` and `copy_dir_from_vm` are refused as not supporting Windows guests yet, and the script checks that the refused pull created no host directory. The file is then removed. |
| `win-jobs-restart.json` | Manual: `run-all.sh` does not run it. A job keeps running through a restart of the guest agent, started through WMI. See "The Windows guest" for why. |

The screen cases need the OCR models and the guests in "The installer guests". They run with a screen config:
`config-templates.toml` with `t-inst`, `t-win` and `t-win11` added to `allowed_vms` and `power_vms`, and
`display.ocr_models` set. `run-all.sh` writes this config.

| Case | What it exercises |
| -- | -- |
| `screen-installer.json` | Boots `t-inst` from the Debian netinst ISO. `screen_text` matches "Graphical install" on the UEFI GRUB menu and attaches a screenshot. After Enter it matches "Select a language", and `screen_settled` matches. "Italian" is not on screen until a `mouse` scroll of the language list, and then it matches. A `mouse` click on "Continue" at (1218, 774) leads to "Select your location". Keys then go through the keyboard, network, hostname, user and time-zone screens to "Partition disks". The installer drops input that arrives in the first 2 s or so of a new screen. Each key is therefore sent once its screen's text matched and the screen held still for 3 s. On the changing screen that loads installer components, a `screen_text` wait for text never shown times out after 8 s with its last OCR text. The `__time__` lines measure that time. A `screen_text` wait on the title region matches `^Partition disks$`, and a region screenshot of the title reports its size and origin. The VM is then powered off. |
| `screen-windows.json` | Boots `t-win` from the Windows 11 ISO. `screen_text` matches "Press any key to boot from CD", and Enter goes three times, a second apart. `screen_text` then matches "Windows 11 Setup" with a screenshot attached. The VM is then powered off. |
| `win-screen.json` | Restarts a running `t-win11` with `shutdown /r` and checks that it booted in the last 3 minutes. Waits until the desktop holds still for 10 s, then clicks the Start button at (418, 776) with `mouse`. Matches "Pinned" in the Start menu with a screenshot attached. |

`templates-reset.json` and `templates-expiry.json` need `t-listed`, a VM defined with `virsh` and never started.
`templates-reset.json` also needs `tpl-scratch.qcow2`. Make it with
`virsh vol-clone --pool agent-vms tpl-debian13.qcow2 tpl-scratch.qcow2`.

`console-stall.json` rereads from offset 0 with `wait_ms`, as an agent often does. It passes when the reads show the
`Password:` prompt and a root shell prompt. The case leaves root logged in on `ttyS0`, so send `exit` to log out.

### Disks outside the pool survive reset and expiry

A reset and an expiry delete only volumes in `manage.disk_pool`. This check shows that a disk file outside every pool
stays in place.

1. Run `templates-outside-disk-setup.json`. It creates `t-or`, running, and `t-oe`, stopped with a 1-minute lifetime.

2. Set `DIR` to a directory you own outside every pool. Make a file there with `truncate -s 1M "$DIR/outside.img"`.
   Note its `sha256sum`.

3. Attach the file to both VMs in their stored configuration only, so QEMU never opens it:

   ```sh
   for vm in t-or t-oe; do
     virsh -c qemu:///system attach-disk "$vm" "$DIR/outside.img" vdd --config --targetbus virtio \
       --driver qemu --subdriver raw --mode readonly
   done
   ```

4. Once `t-oe` has expired, run `templates-outside-disk.json`. The server's first sweep deletes `t-oe` and logs the
   outside disk as not deleted. The reset of `t-or` also reports the disk as not deleted, and then `t-or` is deleted.

5. Check that the file still exists, with the same owner and checksum.

### A failed volume deletion during expiry (needs root)

The sweep keeps a VM whose volume it cannot delete, so the next sweep retries it. Producing that failure needs root,
so this check runs by hand. Run every step from the repository root.

1. Run `templates-expiry-command.json`. It creates `t-exp2` with a 1-minute lifetime and an attached disk.
2. Make the attached disk undeletable: `sudo chattr +i /var/lib/libvirt/images/agent-vms/t-exp2-vdc.qcow2`.
3. Wait 70 s, then run `target/release/virtmanager-mcp --config tests/testbed/config-templates.toml sweep`. It exits 1.
   Its log says that it could not delete `t-exp2-vdc.qcow2` and keeps `t-exp2`.
4. Check that `virsh -c qemu:///system dominfo t-exp2` still finds the VM. Check that
   `virsh -c qemu:///system vol-list agent-vms` still lists its boot disk `t-exp2-g1.qcow2` and seed
   `t-exp2-g1-seed.img`.
5. Make the disk deletable again: `sudo chattr -i /var/lib/libvirt/images/agent-vms/t-exp2-vdc.qcow2`.
6. Run the sweep command again. It deletes `t-exp2` and exits 0, and `virsh -c qemu:///system vol-list agent-vms` lists
   no `t-exp2` volume.

## Run a case

The client saves screenshots as `shot-N.png` in the current directory, so run each case from a scratch directory. Set
`REPO` to the repository root, then build the server and run the case:

```sh
cargo build --release --locked
mkdir -p /tmp/testbed-run && cd /tmp/testbed-run
python3 -I "$REPO/tests/testbed/mcp_call.py" "$REPO/tests/testbed/cases/smoke.json" -- \
  "$REPO/target/release/virtmanager-mcp" --config "$REPO/tests/testbed/config.toml"
```

A call list may use three special tools. `__list__` prints every advertised tool with its input properties.
`__sleep__` pauses for `args.secs` seconds. `__time__` prints a timestamp, so the time between two of them measures
the calls in between.

A call may also carry `"save": {"name": "field"}`, which keeps that field of the reply's JSON under the name. A later
argument holding `${name}` gets the value. Saved values persist in `vars.json` in the current directory, so a later
case file run from there can use them.

## Check a run

With `--check` before the case file, `mcp_call.py` checks each reply against the call's `expect` and exits 1 when a
check fails. Its docstring describes the format. Every case carries expectations taken from its row in the tables
above. Values that change between runs, such as job IDs, offsets and generations, are matched by pattern. The timings
that `__time__` lines measure are not checked, so read them from the output.

`run-all.sh` runs every case with `--check`. Run it from the repository root:

```sh
tests/testbed/run-all.sh                            # every case
tests/testbed/run-all.sh templates-reset jobs.json  # a subset
```

The script builds the release binary and runs the cases in a fixed order. Between cases it does the `virsh` steps
that this README describes. It defines `t-listed`, clones and deletes `tpl-scratch.qcow2`, shuts `trial-deb13` off
for `console-after-start.json`, and saves `t-u`. It also attaches the outside disk, waits out expiries, runs the
`sweep` command, and logs root out of `ttyS0`. For the transfer cases it builds the share and pull directories in its
log directory and checks what arrives in the pull directory.

Naming one case of a dependent pair, such as `jobs.json` and `jobs-reopen.json`, runs both. Logs go to a new
directory under `/tmp`, which the script prints. At the end the script removes the `t-*` VMs and volumes it created
and leaves `trial-deb13` running. It then prints a summary and exits 1 if a case failed.

The script prints `skipped: <reason>` and exits 0 when `virsh` cannot reach `qemu:///system`. It does the same when
pool `agent-vms`, `trial-deb13` or `tpl-debian13.qcow2` is missing. Without `t-owrt`, it skips the OpenWrt cases and
`mouse-refusals.json`. It skips the Windows cases without `t-win11`, or with less than 5 GiB of memory available for
it. Otherwise it writes `config-windows.toml` with `t-win11` allowed. It starts the VM for the cases after
`win-exec.json` and shuts it down through its agent afterwards.

The root check, "A failed volume deletion during expiry", stays manual. A full run takes about 11 minutes, or about 13
with the screen cases. The Windows cases add about 3.5 minutes.

The screen cases need the OCR models. `VIRTMANAGER_OCR_MODELS` names their directory, such as the one that
`scripts/fetch-ocr-models.sh` fills, as an absolute path. Without the variable, the script uses the directory that a
server started with `VIRTMANAGER_OCR_DOWNLOAD=1` fills, `~/.local/share/virtmanager-mcp/ocr-models`, when it holds
both models. The script itself never downloads them. With neither, the summary lists the screen cases as skipped with
that reason. With models, the script writes the screen config into its `/tmp` directory as `config-screen.toml`. It
defines `t-inst` and `t-win` from their XML files and creates `t-inst.qcow2`, each unless it exists. It runs one of
the two VMs at a time, after checking that the host has the VM's memory plus 1 GiB available. At the end it removes
the VMs and the disk it created:

```sh
VIRTMANAGER_OCR_MODELS=~/.local/share/virtmanager-mcp/ocr-models tests/testbed/run-all.sh screen-installer
```

## Recreate the guest

The pool and guest already exist on the development host. On a new host, create the pool once:

```sh
virsh -c qemu:///system pool-define-as agent-vms dir --target /var/lib/libvirt/images/agent-vms
virsh -c qemu:///system pool-build agent-vms
virsh -c qemu:///system pool-start agent-vms
virsh -c qemu:///system pool-autostart agent-vms
```

Wherever a version manager's Python shadows the system Python, run `virt-install` through `/usr/bin/python3`. The
system Python provides the `gi` module that `virt-install` needs.

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
Start it with the server's `start_vm` and `wait_for_agent_secs: 180`, and the guest agent then answers.

These commands ran on 2026-11-09 with virt-install 5.1.0, the Debian trixie netboot installer from deb.debian.org,
and guest kernel 6.12.107+deb13-amd64.

## The OpenWrt appliance

`t-owrt` is an OpenWrt x86-64 VM whose only channel is the serial console. It has no guest agent, no network and no
display, and `config-templates.toml` allows it. Create it once, from a scratch directory:

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

`owrt-boot.json` starts the VM and opens the console during boot. After Enter, the boot log ends in a
`root@OpenWrt:~#` prompt, and the case then powers the VM off. This ran with OpenWrt 25.12.6, which booted on the q35
machine type.

## The installer guests

`t-inst` and `t-win` boot installer ISOs for the screen cases. Their XML files came from
`/usr/bin/python3 /usr/bin/virt-install ... --print-xml` and are kept as generated.

- `t-inst.xml`: 1 GiB, 2 vCPUs, UEFI on q35, no network, a USB tablet, SPICE with virtio video, the Debian 13 netinst
  ISO `tpl-debian13-netinst.iso` as its CD-ROM, and the 8 GiB disk `t-inst.qcow2`. It also has the emulated TPM 2.0,
  random number generator and guest-agent channel that `virt-install` adds.
- `t-win.xml`: 4 GiB, 2 vCPUs, UEFI with Secure Boot and SMM, an emulated TPM 2.0, a USB tablet, SPICE with virtio
  video, the Windows 11 ISO `tpl-win11-26h2.iso` as its CD-ROM, and no disk.

Both guests' TPMs need `swtpm` on the host. Both ISOs are volumes in `agent-vms`. Define the guests by hand as
follows, which `run-all.sh` also does when they are missing:

```sh
virsh -c qemu:///system vol-create-as agent-vms t-inst.qcow2 8G --format qcow2
virsh -c qemu:///system define "$REPO/tests/testbed/t-inst.xml"
virsh -c qemu:///system define "$REPO/tests/testbed/t-win.xml"
```

Run a screen case with a config that allows the guests and sets `display.ocr_models`, such as the one `run-all.sh`
writes. To remove the guests, run `virsh -c qemu:///system undefine --nvram t-inst` and the same for `t-win`. Then run
`virsh -c qemu:///system vol-delete --pool agent-vms t-inst.qcow2`.

## The Windows guest

`t-win11` is a persistent Windows 11 Pro VM for the Windows jobs, waits and screen cases. `t-win11.xml` gives it 4 GiB,
2 vCPUs, UEFI with Secure Boot and Microsoft's keys, an emulated TPM 2.0, and a 64 GiB SATA disk `t-win11.qcow2`. It
also has a USB tablet, SPICE with virtio video, the guest-agent channel, no network and no serial console. Its disk
boots first, so the DVD's "press any key" prompt shows only while the disk is empty.

`win11/build.sh` installs the VM unattended from `tpl-win11-26h2.iso` with `win11/autounattend.xml`. The answer file
selects Pro by Microsoft's public KMS client key, so the guest stays unactivated. At first logon, it installs the
`virtio-win` serial driver with `pnputil` and the QEMU guest agent from its MSI.

The script needs 5 GiB of available memory and 70 GiB free in the pool. It also needs a local copy of
`virtio-win-0.1.302.iso`. The script pins that ISO's SHA-256, because the `virtio-win` project publishes checksums
only for its RPMs. Download the ISO from the `archive-virtio/virtio-win-0.1.302-1/` directory on `fedorapeople.org`.
Then run the script:

```sh
tests/testbed/win11/build.sh ~/Downloads/virtio-win-0.1.302.iso
```

The script uploads the ISO and the answer ISO as `tpl-virtio-win-0.1.302.iso` and `tpl-win11-answer.iso`. It reuses
an existing `tpl-virtio-win-0.1.302.iso` only when it hashes to the pinned sum, and an interrupted upload leaves no
volume. It then installs Windows, which takes about 10 minutes on this host. Finally it waits for the guest agent,
shuts the VM down and ejects the ISOs. `win-exec.json` checks the result.

The guest's local administrator `tester` logs on automatically. Its password in the answer file is a test value for
this offline VM only. The serial driver is signed, and Windows enforces that signature under Secure Boot.
`qemu-ga.exe` itself carries no Authenticode signature, so its integrity rests on the pinned SHA-256 of the ISO.

Windows ignores the ACPI power button while `tester` is logged on. Shut the VM down through its agent:
`virsh -c qemu:///system shutdown t-win11 --mode agent`.

A restart of the `QEMU-GA` service sometimes leaves the agent unreachable until the VM is powered off and on. The new
agent process runs, but libvirt reports the channel disconnected. This happened in 4 of about 22 restarts with
`virtio-win` 0.1.302 ([probe note](../../docs/research/2026-11-11-windows-jobs-probe.md)). For that reason
`run-all.sh` leaves `win-jobs-restart.json` out.

To remove the guest, run `virsh -c qemu:///system undefine t-win11 --nvram --tpm`. Then run
`virsh -c qemu:///system vol-delete --pool agent-vms t-win11.qcow2`.

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
