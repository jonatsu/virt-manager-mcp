# Build probes for custom templates (2026-11-24)

Unit 1 of [the implementation plan](../plans/custom-templates-implementation-plan.md) ran these probes before any code.
Probes 1, 2, 4, 5 and 6 ran together, and probes 3 and 7 ran in parallel with them on their own VM and volume.

I ran these on the host's `qemu:///system`: libvirt 12.0.0, QEMU 10.2.1, OVMF from `/usr/share/OVMF`, pool `agent-vms`
at `/var/lib/libvirt/images/agent-vms` (`POOL` below). The guests were qcow2 overlays of `tpl-debian13.qcow2`, with 1 GiB
RAM and 1 vCPU. I reached them through the guest agent (`guest-exec` over `virsh qemu-agent-command`). All VMs and
volumes were named `t-pr1`, `t-pr2`, `t-pr4`, `t-pr5` or `t-pr6` with suffixes, or `tpl@t-probe*`. Probes 3 and 7 have
their own set-up below.

Every VM used this skeleton, with `<os>` varying and `<name>` equal to its overlay's name:

```xml
<domain type='kvm'>
  <name>t-pr1</name>
  <memory unit='MiB'>1024</memory>
  <vcpu>1</vcpu>
  <os>…</os>
  <features><acpi/><apic/><smm state='on'/></features>
  <cpu mode='host-passthrough'/>
  <on_poweroff>destroy</on_poweroff>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='POOL/t-pr1.qcow2'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel>
    <serial type='pty'/>
    <console type='pty'/>
  </devices>
</domain>
```

The overlays came from `virsh vol-create-as agent-vms <name>.qcow2 3G --format qcow2 --backing-vol tpl-debian13.qcow2
--backing-vol-format qcow2`.

I built `TPLVARS` (`POOL/tpl@t-probe.vars`) first. I booted a Secure Boot VM (`t-pr1-seed`, autoselected, enrolled keys,
plain `<nvram format='raw'>`), wrote a marker variable in the guest, shut it down and cloned its variables:

```sh
printf '\007\000\000\000marker-v1' > /sys/firmware/efi/efivars/ProbeVar-12345678-1234-1234-1234-123456789abc   # in the guest
virsh -c qemu:///system shutdown t-pr1-seed
virsh -c qemu:///system pool-refresh agent-vms
virsh -c qemu:///system vol-clone --pool agent-vms t-pr1-seed.vars 'tpl@t-probe.vars'
```

The clone is 540672 bytes, owner `64055:991`. Then I undefined `t-pr1-seed` and deleted its overlay.

## Probe 1: does autoselection (`<os firmware='efi'>`) accept a user `<nvram template=…>`?

Answer: no, not on libvirt 12.0.0. `virsh define` refuses it, for Secure Boot off and on. The plan's form cannot be
used. The form that works is the explicit loader, with no `firmware` attribute and no `<firmware>` features. In that
form libvirt copies the user template at first start and the guest sees the marker variable. libvirt 12.0.0 proves
nothing about 10.10: I did not test an older libvirt, and the autoselection code and the descriptors may differ there.

This feeds unit 21 (VMs from a sealed template).

The plan's form, Secure Boot off:

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='q35'>hvm</type>
  <firmware><feature enabled='no' name='secure-boot'/></firmware>
  <nvram template='POOL/tpl@t-probe.vars' templateFormat='raw' format='raw'>POOL/t-pr1.vars</nvram>
</os>
```

```text
$ virsh -c qemu:///system define t-pr1.xml
error: Failed to define domain from t-pr1.xml
error: operation failed: Unable to find 'efi' firmware that is compatible with the current configuration
```

I varied the XML to find what triggers the refusal. Every row had the same `<nvram>` element as above, with
`TPLVARS` as the template, except where noted.

| Variation | `define` result |
| -- | -- |
| `firmware='efi'`, `secure-boot` off | refused, error above |
| `firmware='efi'`, `secure-boot` off and `enrolled-keys` off | refused, same error |
| `firmware='efi'`, `secure-boot` on and `enrolled-keys` on (the Secure Boot on form) | refused, same error |
| `firmware='efi'` plus an explicit `<loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>` | refused, same error |
| `firmware='efi'`, `secure-boot` off, `template='/usr/share/OVMF/OVMF_VARS_4M.fd' templateFormat='raw'` (stock template named explicitly) | defined |
| the same with the stock template and no `templateFormat` | defined |
| no `firmware` attribute, explicit `<loader>`, `TPLVARS` as the template | defined |

So the refusal comes from the template path: autoselection accepts only a template that a firmware descriptor lists. I
did not read the libvirt source or its debug log, which is root-only, so that is an inference from these seven cases.

The form that works, Secure Boot off (`t-pr1`):

```xml
<os>
  <type arch='x86_64' machine='q35'>hvm</type>
  <loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
  <nvram template='POOL/tpl@t-probe.vars' templateFormat='raw' format='raw'>POOL/t-pr1.vars</nvram>
</os>
```

Secure Boot on (`t-pr1-on`), the sealed form from the earlier probe note:

```xml
<os>
  <type arch='x86_64' machine='q35'>hvm</type>
  <loader readonly='yes' secure='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>
  <nvram template='POOL/tpl@t-probe.vars' templateFormat='raw' format='raw'>POOL/t-pr1-on.vars</nvram>
</os>
```

Both defined, started, and the guest agent answered about 10 seconds after `virsh start`. In the guest:

```text
# Secure Boot off (t-pr1)
$ tail -c 9 /sys/firmware/efi/efivars/ProbeVar-12345678-1234-1234-1234-123456789abc
marker-v1
$ mokutil --sb-state
This system doesn't support Secure Boot
$ ls /sys/firmware/efi/efivars | grep -E '^(PK|KEK|db)-'
KEK-8be4df61-93ca-11d2-aa0d-00e098032b8c
PK-8be4df61-93ca-11d2-aa0d-00e098032b8c
db-d719b2cb-3d3a-4596-a3bc-dad00e67656f

# Secure Boot on (t-pr1-on)
marker-v1
SecureBoot enabled
SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c and SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c present
[    0.000000] Kernel is locked down from EFI Secure Boot; see man kernel_lockdown.7
[    0.000000] secureboot: Secure boot enabled
```

The marker variable was present in both guests, so libvirt copied the user template to the pool path at first start.
The off guest still shows the template's `PK`, `KEK` and `db`, because the template came from a Secure Boot VM. The code
image without Secure Boot ignores them. A sealed template made from a Secure Boot VM therefore carries its keys into
a VM that is meant to run with Secure Boot off.

Open points:

- With the plan's autoselected form refused, unit 21 must build the sealed form (explicit loader and template) for both
  settings, or carry the descriptor choice itself.
- I did not try a template path outside the pool, or a missing template file with autoselection. The earlier note says
  a missing template fails `start`, not `define`, in the explicit form.

## Probe 2: does the inactive definition hold the resolved `<loader>` path after a first start?

Answer: yes, and it holds it from `define`, before any start. libvirt resolves the loader, the `nvram template`, the
`enrolled-keys` feature and the machine type (`q35` becomes `pc-q35-10.2`) at define time. The inactive XML was
byte-identical in the `<os>` element before the first start, after it, and after shutdown. The live XML's `<os>` element
was identical to the inactive one. I did not try an `<os>` form that fails to resolve at `define`, because libvirt
refuses it there.

This feeds unit 8 (firmware record in the owner element). It means unit 8 can read the resolved loader from
`dumpxml --inactive` at any time after `define`, and needs no first start.

I ran `virsh dumpxml --inactive`, then `virsh start`, then `virsh dumpxml` and `virsh dumpxml --inactive`, then
`virsh shutdown` and `virsh dumpxml --inactive` again, and diffed the `<os>` elements.

Autoselected, Secure Boot off (`t-pr2-off`), input:

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='q35'>hvm</type>
  <firmware><feature enabled='no' name='secure-boot'/></firmware>
  <nvram format='raw'>POOL/t-pr2-off.vars</nvram>
</os>
```

Inactive after `define`, before any start (unchanged at every later point):

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='pc-q35-10.2'>hvm</type>
  <firmware>
    <feature enabled='no' name='enrolled-keys'/>
    <feature enabled='no' name='secure-boot'/>
  </firmware>
  <loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
  <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd' templateFormat='raw' format='raw'>POOL/t-pr2-off.vars</nvram>
  <boot dev='hd'/>
</os>
```

Autoselected, Secure Boot on with keys (`t-pr2-on`, `enrolled-keys` and `secure-boot` both `yes`): the same, with
`<loader readonly='yes' secure='yes' …>/usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>` and
`template='/usr/share/OVMF/OVMF_VARS_4M.ms.fd'`.

Explicit loaders (`t-pr1`, `t-pr1-on` from probe 1): the loader and template stay as written, with
`machine='pc-q35-10.2'` and `<boot dev='hd'/>` added at define. Before start, after start and after shutdown were
identical; the live `<os>` element matched too. In those forms `<firmware>` and the `firmware` attribute are absent, so
there is nothing to resolve.

The live XML differs from the inactive one only outside `<os>`: domain `id`, `<resource>`, device aliases, the
`<backingStore>` of the disk, the PTY paths, `<seclabel>` elements and the channel's socket path.

One side case, `t-pr2-nonv`: autoselection with no `<nvram>` element at all (`secure-boot` off). `define` resolved a
different firmware, a ROM image:

```xml
<loader type='rom' format='raw'>/usr/share/ovmf/OVMF.amdsev.fd</loader>
```

and `virsh start` failed:

```text
error: Failed to start domain 't-pr2-nonv'
error: internal error: cannot load AppArmor profile 'libvirt-3a945399-5067-4054-968a-7d46b5704b34'
```

The server always writes an `<nvram>` element, so this does not change a unit, but a firmware record built from an
`<nvram>`-less definition would name `OVMF.amdsev.fd`.

## Probe 3: hibernation (feeds unit 5)

I ran this on 2026-11-24, on libvirt 12.0.0 and QEMU 12.0.0, `qemu:///system`. The probe was conclusive in about five
minutes of work. The two-hour time box was not needed.

### Answer

When the guest hibernates, libvirt reports a shut-off VM. It does not report a suspended VM with a disk reason.

- After `systemctl hibernate`, `virsh domstate --reason t-pr3` says `shut off (shutdown)`. `dominfo` says
  `Managed save: no`.
- libvirt does emit `pmsuspended`, but only for an instant. It sends the `pm-suspend-disk` event and a lifecycle event
  `PMSuspended Disk`, in the same millisecond as `Shutdown Finished after guest request`. About 350 ms later it sends
  `Stopped Shutdown`. The domain never rests in a pmsuspended state.
- `virsh start t-pr3` resumes the guest. Uptime continued, a file in `/dev/shm` survived, and the kernel log shows the
  hibernation exit.
- `virsh dompmsuspend t-pr3 disk` gave the same states and events, and the guest resumed the same way.

For unit 5, `start::decide` needs no new arm for a hibernated VM, because a hibernated VM is plain "shut off" to
libvirt. `start_vm` boots it, and the guest resumes by itself if it holds a hibernation image. The `pmsuspended` state
with a disk reason did not occur in this probe, so no `pm_wakeup` call applies. One caveat: the domain had
`<on_poweroff>destroy</on_poweroff>`. I did not try `preserve` or `restart`, which could leave the state different.

### A finding for unit 5: firmware autoselection refuses `suspend-to-disk`

My first definition used the `<os firmware='efi'>` form from the earlier probe, with
`<pm><suspend-to-disk enabled='yes'/></pm>`. `virsh define` failed:

```
error: operation failed: Unable to find 'efi' firmware that is compatible with the current configuration
```

The same XML without the `<pm>` element defined fine. I checked the descriptors in `/usr/share/qemu/firmware/` for
x86_64. None lists the feature `acpi-s4`; all list `acpi-s3` at most. I infer that libvirt wants `acpi-s4` once S4 is
enabled, so autoselection finds no match. I did not read the libvirt source for this, so the cause is an inference. The
failure itself is certain.

An explicit loader works. So `create_vm` with `hibernate` on UEFI cannot use autoselection on this host. It must write
the loader and the nvram template itself. BIOS has no such problem, but I did not test BIOS (see below).

### VM and commands

I used UEFI, because the template installs its boot loader on the EFI partition and a BIOS boot of it looped in GRUB in
the earlier probe. I did not test BIOS.

```sh
V="virsh -c qemu:///system"
$V vol-create-as agent-vms t-pr3.qcow2 8G --format qcow2 --backing-vol tpl-debian13.qcow2 --backing-vol-format qcow2
$V define t-pr3.xml && $V start t-pr3
```

The overlay is 8 GiB so that the root file system (grown by `x-systemd.growfs`) has room for a 1.5 GiB swap file. The
XML that matters:

```xml
<os>
  <type arch='x86_64' machine='q35'>hvm</type>
  <loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
  <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd' templateFormat='raw' format='raw'>/var/lib/libvirt/images/agent-vms/t-pr3.vars</nvram>
</os>
<features><acpi/><apic/><smm state='off'/></features>
<pm><suspend-to-disk enabled='yes'/></pm>
<on_poweroff>destroy</on_poweroff>
```

The rest was the skeleton from the earlier probe: 1 vCPU, 1024 MiB, `host-passthrough`, a virtio disk `vda` on the
overlay, the `org.qemu.guest_agent.0` channel, a serial and a console. I set `smm` to `off` because the non-secure
descriptor says "optional SMM" and I wanted no SMM in the way. I did not test `smm on` with the explicit loader.

I reached the guest through the guest agent, with `guest-exec` of `/bin/sh -c` as root.

### Guest set-up

The guest runs Debian 13, kernel `6.12.115+deb13.1-cloud-amd64`, with `CONFIG_HIBERNATION=y`. `/sys/power/state` lists
`freeze mem disk`. The root file system is ext4 with UUID `2d5cddec-8777-4aef-9b8d-1d7093e60982`, on `vda1`.

```sh
fallocate -l 1536M /swapfile; chmod 600 /swapfile; mkswap /swapfile; swapon /swapfile
echo "/swapfile none swap sw 0 0" >> /etc/fstab
# first physical block of the swap file, in 4 KiB blocks:
filefrag -v /swapfile          # first extent starts at 370688
echo "RESUME=UUID=2d5cddec-8777-4aef-9b8d-1d7093e60982" > /etc/initramfs-tools/conf.d/resume
# in /etc/default/grub:
GRUB_CMDLINE_LINUX_DEFAULT="resume=UUID=2d5cddec-8777-4aef-9b8d-1d7093e60982 resume_offset=370688"
update-initramfs -u            # warns "no matching swap device is available"; harmless for a swap file
update-grub
systemctl reboot
```

The swap file has several extents. That did not matter. After the reboot, `/proc/cmdline` held both resume parameters
and `swapon --show` listed `/swapfile`. On that first boot the kernel logged `PM: Image not found (code -22)`, which is
normal when no image exists.

### The hibernate and the events

I ran `virsh event --all --loop --timestamp > events.log &` before the hibernate. It logs every domain on the host, so I
kept only the `t-pr3` lines. Before hibernating, I wrote `marker-618` to `/dev/shm/marker` and started `sleep 100000`.
Guest uptime was 50.66 s.

```sh
$V domstate --reason t-pr3     # running (booted)
systemctl hibernate            # in the guest, through the agent
$V domstate --reason t-pr3     # polled every 4 s for 32 s, each time:
shut off (shutdown)
```

Events for the first hibernate, verbatim:

```
2026-11-24 19:20:09.926+0000: event 'agent-lifecycle' for domain 't-pr3': state: 'disconnected' reason: 'channel event'
2026-11-24 19:20:10.655+0000: event 'pm-suspend-disk' for domain 't-pr3'
2026-11-24 19:20:10.655+0000: event 'lifecycle' for domain 't-pr3': PMSuspended Disk
2026-11-24 19:20:10.655+0000: event 'lifecycle' for domain 't-pr3': Shutdown Finished after guest request
2026-11-24 19:20:11.001+0000: event 'lifecycle' for domain 't-pr3': Stopped Shutdown
```

### The resume

`virsh start t-pr3` printed `Domain 't-pr3' started`. Events for the start:

```
2026-11-24 19:20:52.206+0000: event 'agent-lifecycle' for domain 't-pr3': state: 'disconnected' reason: 'domain started'
2026-11-24 19:20:52.223+0000: event 'lifecycle' for domain 't-pr3': Resumed Unpaused
2026-11-24 19:20:52.223+0000: event 'lifecycle' for domain 't-pr3': Started Booted
2026-11-24 19:20:52.552+0000: event 'rtc-change' for domain 't-pr3': 0
2026-11-24 19:20:57.045+0000: event 'agent-lifecycle' for domain 't-pr3': state: 'connected' reason: 'channel event'
```

The guest agent answered about five seconds after the start. In the guest:

- `/proc/uptime` read `99.09`, so it continued from the 50 s before the hibernate. A fresh boot would have started
  near 0.
- `uptime -s` still said `19:19:17`, the earlier boot time, and `/dev/shm/marker` still held `marker-618`.
- The kernel log held `PM: hibernation: hibernation entry` at 19:20:08, then `Basic memory bitmaps freed` and
  `hibernation exit` after the restore.

### Second path: `dompmsuspend`

With the guest running again, I ran `virsh dompmsuspend t-pr3 disk` at 19:21:02. It printed
`Domain 't-pr3' successfully suspended` with exit 0. Every poll said `shut off (shutdown)`. The events matched the first
path:

```
2026-11-24 19:21:02.627+0000: event 'agent-lifecycle' for domain 't-pr3': state: 'disconnected' reason: 'channel event'
2026-11-24 19:21:03.113+0000: event 'pm-suspend-disk' for domain 't-pr3'
2026-11-24 19:21:03.113+0000: event 'lifecycle' for domain 't-pr3': PMSuspended Disk
2026-11-24 19:21:03.113+0000: event 'lifecycle' for domain 't-pr3': Shutdown Finished after guest request
2026-11-24 19:21:03.426+0000: event 'lifecycle' for domain 't-pr3': Stopped Shutdown
```

`virsh start` resumed it again: uptime 133.23, marker still present, and a second `hibernation exit` in the kernel log.
Here the guest agent in the guest performed the hibernate, so this path needs the agent running in the guest.

### What I did not test

- BIOS firmware.
- `smm on` with an explicit loader.
- `on_poweroff` set to `preserve`, `restart` or `rename`.
- A VM with `<pm>` absent: `dompmsuspend disk` and `systemctl hibernate` there.
- `virsh start` of a VM whose swap file holds a stale image.
- Whether `hibernate` as a `create_vm` flag needs the guest set-up above. The XML alone does not make a guest able to
  hibernate; the guest needs the swap file and resume parameters.

### Feeds unit 5

The probe was conclusive, so unit 5 runs. `start::decide` gets no new arm: a hibernated VM is shut off, and `start_vm`
boots it. The unit has one new design point. With `hibernate: true` on a UEFI VM, `create_vm` and the seal cannot use
`<os firmware='efi'>` autoselection on this host. They need an explicit loader and nvram template, or a refusal with a
clear message.

## Probe 4: does `vol-clone` of an overlay give a flat volume, and does the template stay unchanged?

Answer: the clone is a flat, self-contained qcow2 with no backing file, and the template is unchanged (same SHA-256).
But two points matter for unit 19. First, `vol-dumpxml` on the new clone lists a `<backingStore>` that points at the
template. That entry is stale: `qemu-img info` shows no backing file, and a `pool-refresh` removes the element. The seal's
check has to refresh the pool, or read the file with `qemu-img`, before it decides that the clone has no backing
reference. Second, the allocation is not "about the data's". The clone holds the template's data as well as the overlay's.

This feeds unit 19 (seal in the background). Per the plan, the unit stops if the clone keeps a backing reference. It does
not, so I report that the unit is not stopped; the stale `<backingStore>` is a risk for a check that reads the dumpxml
right after the clone.

Setup: `t-pr4` was an overlay of `tpl-debian13.qcow2`. I booted it, wrote 64 MiB of random data in the guest
(`dd if=/dev/urandom of=/root/data.bin bs=1M count=64; sync`) and shut it down.

```sh
virsh -c qemu:///system vol-download --pool agent-vms tpl-debian13.qcow2 /dev/stdout | sha256sum
# 200fadadceb09736250de6c27787ab35f395f7431cf0ab78df3282f0d8e3a5d6  -
virsh -c qemu:///system vol-clone --pool agent-vms t-pr4.qcow2 t-pr4-seal.qcow2     # 0.9 s
virsh -c qemu:///system vol-dumpxml --pool agent-vms t-pr4-seal.qcow2
virsh -c qemu:///system vol-download --pool agent-vms t-pr4-seal.qcow2 seal.qcow2
qemu-img info -U seal.qcow2
virsh -c qemu:///system pool-refresh agent-vms
virsh -c qemu:///system vol-download --pool agent-vms tpl-debian13.qcow2 /dev/stdout | sha256sum
```

| Volume | Capacity | Allocation | Physical | Owner | Backing |
| -- | -- | -- | -- | -- | -- |
| `tpl-debian13.qcow2` before and after | 3221225472 | 696782848 | 696778752 | `64055:991` | none |
| overlay `t-pr4.qcow2` (after the guest wrote 64 MiB) | 3221225472 | 77209600 | 77266944 | `0:0` | `tpl-debian13.qcow2` |
| clone `t-pr4-seal.qcow2` | 3221225472 | 1355878400 | 1355808768 | `0:0` | see below |

- The template's SHA-256 is `200fadad…a5d6` before the clone and after it, and again after the seal VM booted. Its
  `<mtime>` stayed `1794263945.055113477`.
- `vol-dumpxml` on the clone, run right after `vol-clone`, listed `<backingStore><path>POOL/tpl-debian13.qcow2</path>…`. I
  cloned a second time (`t-pr4-seal2`) and read it three times in a row: the element was there each time. After
  `virsh pool-refresh agent-vms` the element was gone. On the first clone, a read after I had downloaded the volume
  already showed no element, without an explicit refresh; I do not know what cleared it.
- `qemu-img info -U` on the downloaded clone: `file format: qcow2`, `virtual size: 3 GiB`, `disk size: 1.26 GiB`, and no
  `backing file` line (`grep -c backing` returned 0). The overlay's download shows
  `backing file: /var/lib/libvirt/images/agent-vms/tpl-debian13.qcow2`.
- The clone's allocation (1.26 GiB) is larger than the template's physical size plus the overlay's (about 0.76 GiB). It is
  a full flat copy, and the template's clusters are probably stored compressed or sparse in the template and expanded in
  the clone. I did not check why.
- I defined `t-pr4-seal` on the clone and booted it. The guest's `sha256sum /root/data.bin` was
  `29bd108dc24e58f85bb218f21e25fccc87ead84354a6a1a03e8a97d91a616bcb`, the same as in the overlay's guest.

## Probe 5: do `<mtime>` values in `vol-dumpxml` move when the guest writes?

Answer: yes, for both the boot disk and the `.vars` volume, and the values are read live: `vol-dumpxml` showed new
numbers without a `pool-refresh`. Three details matter for the seal's confirm step. The disk's `<mtime>` moves again at
shutdown, after the guest's last write, so the confirm step must read it after the VM is shut off. The `.vars`
`<mtime>` also moves during an ordinary boot, with no guest write, so it cannot show that the guest wrote a variable. The
`<ctime>` of both volumes moves at every start and stop because libvirt changes the owner, so it cannot serve either.

This feeds unit 19, step 5 of the seal (the confirm step compares those times). The plan stops the unit if the times do
not move. They move, so the unit is not stopped.

`t-pr5` used the autoselected Secure Boot on form with a plain `<nvram format='raw'>POOL/t-pr5.vars</nvram>`. I read both
volumes with `virsh vol-dumpxml --pool agent-vms <volume>` at each point. The guest wrote
`echo probe > /root/probe5.txt; sync` and `printf '\007\000\000\000write2' > /sys/firmware/efi/efivars/Probe5Var-12345678-1234-1234-1234-123456789abc; sync`.
Times are epoch seconds with the leading `17955` removed.

| Point | `t-pr5.qcow2` mtime | `t-pr5.vars` mtime | `t-pr5.vars` ctime | Allocation, disk |
| -- | -- | -- | -- | -- |
| A: first boot finished, running, after refresh | 48040.208 | 48034.977 (file created at first start) | 48034.977 | 7483392 |
| B: after shutdown, no refresh | 48044.119 | 48034.977 | 48044.420 | 9969664 |
| B2: same, after `pool-refresh` | 48044.119 | 48034.977 | 48044.420 | 9969664 |
| C: second boot, running, before the guest wrote anything, no refresh | 48053.494 | 48048.574 | 48048.574 | 10498048 |
| D: after the guest wrote the file and the variable, still running, no refresh | 48054.216 | 48054.213 | 48054.213 | 10825728 |
| E: after shutdown, no refresh | 48060.288 | 48054.213 | 48060.586 | 11018240 |
| F: same, after `pool-refresh` | 48060.288 | 48054.213 | 48060.586 | 11018240 |

Notes on the table:

- Between C and D the disk's `<mtime>` rose from 48053.494 to 48054.216, and the `.vars` `<mtime>` from 48048.574 to
  48054.213. Both moved because of the guest's writes. The `.vars` value 48054.213 is later than the guest's write command.
- Between D and E (shutdown only) the disk's `<mtime>` rose again, to 48060.288, but the `.vars` `<mtime>` stayed at
  48054.213. QEMU flushes the disk at shutdown.
- The `.vars` `<mtime>` between A and B did not move at first boot's end (it still showed the creation time), but it moved
  during the second boot (C, 48048.574) before the guest ran any command. A boot alone changes it.
- `<ctime>` moved at shutdown (B, E) on both volumes, to the same instant: libvirt restored the owner then.
- `vol-dumpxml` also lists the backing template's timestamps in `<backingStore>`; I did not count those.

## Probe 6: does `vol-clone` onto an existing name work, and what does a shorter `vol-upload` leave?

Answer: `vol-clone` onto an existing name fails, so a revert or reset that uses it must delete the target first.
`vol-upload` of a shorter file over a longer raw volume overwrites the first bytes only: the size is unchanged, the old
tail stays as it was, and the owner is unchanged. An upload of a longer file grows the volume to the new length. For a
qcow2 volume the shorter upload leaves the old file's size and tail too, while the virtual size follows the new header.
So a revert and a TPM reset cannot rely on `vol-upload` alone to put back a shorter file: they must delete and recreate
the volume, or upload a file of the same length. A recreate through `vol-clone` gives the right owner; a recreate through
`vol-create-as` and `vol-upload` gives `0:0`.

This feeds unit 23 (snapshot revert with a journal) and unit 24 (`reset_vm` returns variables and TPM state). It decides
that they replace a volume by delete and recreate, not in place.

I used raw volumes with the patterns below. `A.bin` is 540672 bytes of `0x41`, `B.bin` is 200000 bytes of `0x42` and
`L.bin` is 600000 bytes of `0x4c`. The first volume `t-pr6-a.vars` was created root-owned
(`vol-create-as agent-vms t-pr6-a.vars 540672b --format raw`), and `t-pr6-c.vars` was a `vol-clone` of `t-pr5.vars`, so it
was owned by `64055:991` (`libvirt-qemu:kvm`). I read each volume with `vol-dumpxml` and `vol-download`.

Replace a volume with `vol-upload` (shorter file over a longer volume):

| Step | Capacity | Allocation | Physical | Owner | Content after |
| -- | -- | -- | -- | -- | -- |
| `t-pr6-a.vars` created, empty | 540672 | 540672 | 540672 | `0:0` | all zero |
| upload `A.bin` (540672) | 540672 | 540672 | 540672 | `0:0` | tail 16 bytes `41…41` |
| upload `B.bin` (200000) over it | 540672 | 540672 | 540672 | `0:0` | download 540672 bytes; first 200000 bytes equal `B.bin`; bytes 200001 to 540672 are `A` (tail `41414141414141414141414141414141`) |
| `t-pr6-c.vars` cloned from `t-pr5.vars` | 540672 | 540672 | 540672 | `64055:991` | `.vars` data |
| upload `A.bin` | 540672 | 540672 | 540672 | `64055:991` | tail `41…41` |
| upload `B.bin` (200000) | 540672 | 540672 | 540672 | `64055:991` | `cmp` of the first 200000 bytes with `B.bin`: equal; `cmp` of bytes 200001 on with `A.bin`'s: equal (stale tail); `xxd` of the last 32 bytes: all `0x41` |
| upload `L.bin` (600000, longer than the 540672 capacity) | 600000 | 602112 | 600000 | `64055:991` | the volume grew; no error |
| upload `B.bin` (200000) again | 600000 | 602112 | 600000 | `64055:991` | first 200000 bytes `B`, bytes 200001 to 600000 equal `L.bin`'s tail (`4c…4c`) |

The `vol-upload` calls returned 0 and printed nothing. The `<mtime>` of the volume moved to the upload's time
(1795548189.261 after an upload at 1795548189.254).

A qcow2 volume (`t-pr6-q.qcow2`, 64 MiB, created with `vol-create-as … --format qcow2`):

- I uploaded `big.qcow2`, a 64 MiB qcow2 of 196616 bytes of metadata plus 4 MiB of `Z` appended (4390920 bytes in all).
  Capacity 67108864, allocation 4395008, physical 4390920, owner `0:0`.
- I then uploaded `small.qcow2` (a 1 MiB qcow2 of 196616 bytes). Capacity became 1048576 (read from the new header),
  allocation 4395008, physical 4390920, owner `0:0`. The download was 4390920 bytes, `qemu-img info` read it as a valid
  1 MiB qcow2, and the bytes after the small file's end equalled the big file's bytes: the tail was `5a5a…5a` (`Z`).

Replace a volume by creating over an existing name:

```text
$ virsh -c qemu:///system vol-clone --pool agent-vms t-pr5.vars t-pr6-a.vars
error: Failed to clone vol from t-pr5.vars
error: internal error: storage volume name 't-pr6-a.vars' already in use.

$ virsh -c qemu:///system vol-create-from --pool agent-vms --inputpool agent-vms cf.xml t-pr5.vars
error: Failed to create vol from cf.xml
error: internal error: storage volume name 't-pr6-c.vars' already in use.
```

The target kept its size and owner after both failures, and its download had the content it held before.

Delete and recreate, with the owner afterwards:

| Method | Result |
| -- | -- |
| `vol-delete` then `vol-clone` from `t-pr5.vars` | 540672 bytes, `cmp` against the source: identical, owner `64055:991` |
| `vol-delete` then `vol-create-as … 540672b --format raw` and `vol-upload` | 540672 bytes, owner `0:0` |

Overlays and clones in probes 4 and 5 sat at `0:0` between runs and booted, so I expect a VM to start from a `0:0`
volume, because libvirt changes the owner at start and restores it at shutdown (the `<ctime>` values in probe 5). I did
not start a VM from a recreated volume in probe 6.

## Probe 7: FFI spike (feeds units 7, 15 and 19)

I built a throwaway crate pinned to `virt` 0.4.3 (features `qemu`) and `virt-sys` 0.3.1, the versions `Cargo.lock` resolves. I copied the repo's `Cargo.lock` into the crate and built offline with its own `target/` directory. The host runs libvirt 12.0.0. I read the crate sources under `~/.local/share/cargo/registry/src/index.crates.io-*/`.

### Signatures, verbatim

```rust
// virt 0.4.3, src/stream.rs
pub fn as_ptr(&self) -> sys::virStreamPtr
pub fn new(conn: &Connect, flags: sys::virStreamFlags) -> Result<Stream, Error>
pub fn send(&self, data: &[u8]) -> Result<usize, Error>
pub fn finish(self) -> Result<(), Error>
pub fn abort(self) -> Result<(), Error>

// virt 0.4.3, src/storage_vol.rs
pub fn create_xml_from(
    pool: &StoragePool,
    xml: &str,
    vol: &StorageVol,
    flags: sys::virStorageVolCreateFlags,
) -> Result<StorageVol, Error>
pub fn upload(&self, stream: &Stream, offset: u64, length: u64, flags: u32) -> Result<(), Error>

// virt 0.4.3, src/connect.rs
pub fn get_lib_version(&self) -> Result<u32, Error>

// virt-sys 0.3.1, bindgen/bindings.rs
pub fn virStreamSendHole(st: virStreamPtr, length: ::libc::c_longlong, flags: ::libc::c_uint) -> ::libc::c_int;
pub const VIR_STORAGE_VOL_UPLOAD_SPARSE_STREAM: virStorageVolUploadFlags = 1;
```

`create_xml_from` is an associated function, not a method: `StorageVol::create_xml_from(&pool, xml, &src_vol, 0)`. `get_lib_version` returned `Ok(12000000)` here, so 10.10.0 reads 10010000 and fits `u32`.

### Spike code, key lines

The crate compiles these coercions, which fail to build if a signature differs:

```rust
let _a: fn(&Stream) -> virt_sys::virStreamPtr = Stream::as_ptr;
let _b: fn(&StoragePool, &str, &StorageVol, virt_sys::virStorageVolCreateFlags)
    -> Result<StorageVol, virt::error::Error> = StorageVol::create_xml_from;
let _c: fn(&Connect) -> Result<u32, virt::error::Error> = Connect::get_lib_version;
let _d: unsafe extern "C" fn(virt_sys::virStreamPtr, libc::c_longlong, libc::c_uint) -> libc::c_int =
    virt_sys::virStreamSendHole;
```

The upload creates the volume from XML (raw, capacity 67108864 bytes, allocation 0), then streams the content:

```rust
let vol = StorageVol::create_xml(&pool, VOL_XML, 0)?;
let stream = Stream::new(&conn, 0)?;
vol.upload(&stream, 0, 64 * MIB as u64, virt_sys::VIR_STORAGE_VOL_UPLOAD_SPARSE_STREAM)?;

let hole = |len: usize| {
    let r = unsafe { virt_sys::virStreamSendHole(stream.as_ptr(), len as i64, 0) };
    assert_eq!(r, 0);
};
data(0xAA, 4 * MIB);  // 0..4 MiB data, via stream.send in a loop
hole(28 * MIB);       // 4..32 MiB hole
data(0x55, 4 * MIB);  // 32..36 MiB data
hole(28 * MIB);       // 36..64 MiB trailing hole
stream.finish()?;
```

The stream is blocking (flags 0). A trailing hole followed by `finish` worked, and the volume kept its full 64 MiB size.

### Result

```
$ virsh -c qemu:///system vol-info --pool agent-vms t-pr7.img
Name:           t-pr7.img
Type:           file
Capacity:       64.00 MiB
Allocation:     8.00 MiB
```

The allocation is 8 MiB, which is exactly the two 4 MiB data runs. `vol-dumpxml` shows `<allocation>` 8388608. I downloaded the volume with `vol-download`: it is 67108864 bytes, and `cmp` against a file built as 4 MiB of 0xAA, 28 MiB of zeros, 4 MiB of 0x55 and 28 MiB of zeros reports no difference. The data is where I wrote it and the holes read as zeros. (The downloaded copy is not sparse, so it says nothing about allocation; `vol-info` does.)

### Answer

All four call checks hold. `Stream::as_ptr`, `StorageVol::create_xml_from` and `Connect::get_lib_version` exist and compile as the table above shows, and `virStreamSendHole` links through `virt-sys` and works. The sparse upload gives the right capacity, an allocation that covers only the data, and the right content. Probe 7 does not stop unit 15.

I found no assumption in units 7, 15 or 19 that the real signatures contradict.

- Unit 7 (the libvirt floor): `get_lib_version` returns `Result<u32, Error>`, and 10010000 fits. No contradiction.
- Unit 15: `upload_sparse` uses `Stream::send` for data and `virStreamSendHole` for zero runs, as spiked. Two details the unit should keep in mind. `Stream::send` returns the bytes sent, which can be fewer than the buffer, so the code loops. `as_ptr` calls `unwrap` on the stream's pointer, so it must not run after `finish` or `free`. `finish(self)` and `abort(self)` consume the stream; on an error return, `Drop` frees the stream and panics only if the free itself fails. The plan's note that `Stream` panics on drop of an unfinished stream matches this: a drop frees the stream and panics only on a free error, so aborting on each error path stays the safe choice.
- Unit 19: `create_xml_from` takes `&StoragePool` plus the source `&StorageVol` and a flags value of type `sys::virStorageVolCreateFlags`, so the seal copy needs both a pool handle and the source volume handle on its own connection.

## Probe 8: UEFI hibernation through an explicit firmware path (feeds unit 5)

I ran this on 2026-11-24, on libvirt 12.0.0 and QEMU 10.2.1 (`virsh version` reports API "QEMU 12.0.0"), on
`qemu:///system`.
Guest: Debian 13, kernel `6.12.115+deb13.1-cloud-amd64`, the same image as probe 3.

### Answer

Hibernation works through the explicit loader for Secure Boot off and for setup mode. It does not work for Secure Boot
on, and the refusal comes from the guest, not from libvirt.

- The define-read-redefine sequence works for all three modes. The redefine succeeded, the UUID stayed the same, and
  libvirt resolved exactly the files in the design's mode table.
- Off and setup hibernated and resumed. Uptime continued and a `/dev/shm` marker survived.
- On (enforced Secure Boot) boots, but the kernel starts in lockdown `integrity`, and lockdown forbids hibernation. The
  guest lists no `disk` in `/sys/power/state`, and `systemctl hibernate` fails inside the guest. libvirt stays out
  of it: the domain keeps running and no event arrives.
- Setup mode is not enforced Secure Boot. The guest reports `SecureBoot disabled`, lockdown is `none`, and
  hibernation works. So for unit 5, `hibernate` is possible with Secure Boot off and in setup mode, and impossible
  with Secure Boot on, whatever the host does.

### Table

| Mode | Resolved code file / variables template | Redefine | UUID kept | Start | Guest SB state | Lockdown | Hibernate | Resume |
| -- | -- | -- | -- | -- | -- | -- | -- | -- |
| off | `OVMF_CODE_4M.fd` / `OVMF_VARS_4M.fd` | ok | yes | ok | `This system doesn't support Secure Boot` | `[none] integrity confidentiality` | ok | ok |
| setup | `OVMF_CODE_4M.secboot.fd` / `OVMF_VARS_4M.fd` | ok | yes | ok | `SecureBoot disabled`, `Platform is in Setup Mode` | `[none] integrity confidentiality` | ok | ok |
| on | `OVMF_CODE_4M.ms.fd` / `OVMF_VARS_4M.ms.fd` | ok | yes | ok | `SecureBoot enabled` | `none [integrity] confidentiality` | refused by the guest | not applicable |

All three resolved files match the design's mode table.

### VMs and commands

Each VM `t-ph-<mode>` was a qcow2 overlay of `tpl-debian13.qcow2`, 8 GiB, 1 GiB RAM, one vCPU, `host-passthrough`, a
guest agent channel, `<on_poweroff>destroy</on_poweroff>`. `POOL` is `/var/lib/libvirt/images/agent-vms`.

```sh
V="virsh -c qemu:///system"
$V vol-create-as agent-vms t-ph-<mode>.qcow2 8G --format qcow2 --backing-vol tpl-debian13.qcow2 --backing-vol-format qcow2
$V define t-ph-<mode>.stage1.xml
$V dumpxml --inactive t-ph-<mode> > t-ph-<mode>.inactive1.xml     # read loader, template, uuid
$V define t-ph-<mode>.stage2.xml                                  # same name and uuid
$V dumpxml --inactive t-ph-<mode>
```

Stage 1, the autoselected definition. The `<os>` element, with `<smm state='on'/>` for on and setup and `<smm
state='off'/>` for off, and no `<pm>`:

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='q35'>hvm</type>
  <firmware>
    <!-- off:   <feature enabled='no' name='secure-boot'/> -->
    <!-- on:    <feature enabled='yes' name='secure-boot'/><feature enabled='yes' name='enrolled-keys'/> -->
    <!-- setup: <feature enabled='yes' name='secure-boot'/><feature enabled='no' name='enrolled-keys'/> -->
  </firmware>
  <loader type='pflash'/>
  <nvram format='raw'>POOL/t-ph-<mode>.vars</nvram>
</os>
```

`virsh dumpxml --inactive` right after the first define, `<os>` element per mode (libvirt printed double quotes; I
show it as printed):

```xml
<!-- off -->
<os firmware="efi">
  <type arch="x86_64" machine="pc-q35-10.2">hvm</type>
  <firmware>
    <feature enabled="no" name="enrolled-keys"/>
    <feature enabled="no" name="secure-boot"/>
  </firmware>
  <loader readonly="yes" type="pflash" format="raw">/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
  <nvram template="/usr/share/OVMF/OVMF_VARS_4M.fd" templateFormat="raw" format="raw">POOL/t-ph-off.vars</nvram>
  <boot dev="hd"/>
</os>

<!-- setup -->
  <firmware>
    <feature enabled="no" name="enrolled-keys"/>
    <feature enabled="yes" name="secure-boot"/>
  </firmware>
  <loader readonly="yes" secure="yes" type="pflash" format="raw">/usr/share/OVMF/OVMF_CODE_4M.secboot.fd</loader>
  <nvram template="/usr/share/OVMF/OVMF_VARS_4M.fd" templateFormat="raw" format="raw">POOL/t-ph-setup.vars</nvram>

<!-- on -->
  <firmware>
    <feature enabled="yes" name="enrolled-keys"/>
    <feature enabled="yes" name="secure-boot"/>
  </firmware>
  <loader readonly="yes" secure="yes" type="pflash" format="raw">/usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>
  <nvram template="/usr/share/OVMF/OVMF_VARS_4M.ms.fd" templateFormat="raw" format="raw">POOL/t-ph-on.vars</nvram>
```

(For setup and on I show only the lines that differ from off; the `<type>` and `<boot>` lines were the same.) Note
that setup resolves `secure="yes"` on the loader, although the guest does not enforce Secure Boot (see the start
results).

Stage 2, the redefine. I took `uuid`, the loader path, `secure` and the template path from the stage 1 dump. The `<os>`
has no `firmware` attribute and no `<firmware>` element:

```xml
<uuid>UUID-FROM-STAGE-1</uuid>
<os>
  <type arch='x86_64' machine='q35'>hvm</type>
  <loader readonly='yes' secure='yes|no' type='pflash' format='raw'>LOADER-FROM-STAGE-1</loader>
  <nvram template='TEMPLATE-FROM-STAGE-1' templateFormat='raw' format='raw'>POOL/t-ph-<mode>.vars</nvram>
</os>
<features><acpi/><apic/><smm state='on|off'/></features>
<pm><suspend-to-disk enabled='yes'/><suspend-to-mem enabled='no'/></pm>
```

I used `secure='no'` for off, and `secure='yes'` for setup and on, copying what libvirt resolved. The rest of the
domain was the probe 3 skeleton.

### Results of the sequence

Each redefine printed `Domain 't-ph-<mode>' defined from t-ph-<mode>.stage2.xml` with exit 0. The UUIDs before and
after were equal:

| Mode | UUID before and after the redefine |
| -- | -- |
| off | `c29b872d-e13c-4c6a-8477-154fd6b9940e` |
| setup | `ce6888a2-15aa-47d9-9910-c4b4fc924c52` |
| on | `691e894f-7b30-4d75-b477-7ca59d45ac1b` |

`virsh dumpxml --inactive` after the redefine kept the explicit form, with `machine='pc-q35-10.2'` and
`<boot dev="hd"/>` added, and `<pm>` held `suspend-to-mem enabled="no"` and
`suspend-to-disk enabled="yes"`. Example for on:

```xml
<os>
  <type arch="x86_64" machine="pc-q35-10.2">hvm</type>
  <loader readonly="yes" secure="yes" type="pflash" format="raw">/usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>
  <nvram template="/usr/share/OVMF/OVMF_VARS_4M.ms.fd" templateFormat="raw" format="raw">POOL/t-ph-on.vars</nvram>
  <boot dev="hd"/>
</os>
<pm>
  <suspend-to-mem enabled="no"/>
  <suspend-to-disk enabled="yes"/>
</pm>
```

`<pm>` with `suspend-to-disk enabled='yes'` is accepted in the explicit form for every mode. That is the case the
autoselected form refused in probe 3.

### Start and guest state

All three VMs started and the guest agent answered within about 10 seconds. In the guest:

```text
# off
$ mokutil --sb-state
This system doesn't support Secure Boot
(no SecureBoot-* or SetupMode-* variables in /sys/firmware/efi/efivars)
$ cat /sys/kernel/security/lockdown
[none] integrity confidentiality
[    0.000000] secureboot: Secure boot disabled
$ cat /sys/power/state ; cat /sys/power/disk
freeze mem disk
[platform] shutdown reboot suspend test_resume

# setup
$ mokutil --sb-state
SecureBoot disabled
Platform is in Setup Mode
SecureBoot variable data: 6 0 0 0 0     SetupMode variable data: 6 0 0 0 1
$ cat /sys/kernel/security/lockdown
[none] integrity confidentiality
[    0.000000] secureboot: Secure boot disabled
$ cat /sys/power/state ; cat /sys/power/disk
freeze mem disk
[platform] shutdown reboot suspend test_resume

# on
$ mokutil --sb-state
SecureBoot enabled
SecureBoot variable data: 6 0 0 0 1     SetupMode variable data: 6 0 0 0 0
(PK, KEK and db variables present)
$ cat /sys/kernel/security/lockdown
none [integrity] confidentiality
[    0.000000] Kernel is locked down from EFI Secure Boot; see man kernel_lockdown.7
[    0.000000] secureboot: Secure boot enabled
[    0.654117] Lockdown: swapper/0: hibernation is restricted; see man kernel_lockdown.7
$ cat /sys/power/state ; cat /sys/power/disk
freeze mem
[disabled]
```

In setup mode the `secure='yes'` on the loader gives the SMM-capable, Secure Boot capable firmware build, but with no
PK enrolled the firmware does not enforce anything, so Linux does not lock down. The `on` and `setup` rows differ in
the guest, even though libvirt gives both `secure="yes"`.

### Guest set-up for hibernation

The same recipe as probe 3, once per VM (the overlay starts from the template each time). The values came out the same
as in probe 3 for every VM: root UUID `2d5cddec-8777-4aef-9b8d-1d7093e60982`, first swap file block `370688`.

```sh
fallocate -l 1536M /swapfile; chmod 600 /swapfile; mkswap /swapfile; swapon /swapfile
echo "/swapfile none swap sw 0 0" >> /etc/fstab
echo "RESUME=UUID=$UUID" > /etc/initramfs-tools/conf.d/resume
# GRUB_CMDLINE_LINUX_DEFAULT="resume=UUID=$UUID resume_offset=$OFF"
update-initramfs -u; update-grub; systemctl reboot
```

After the reboot, `/proc/cmdline` held both resume parameters and `swapon --show` listed `/swapfile` in all three
guests. I set up the `on` guest the same way, so its refusal is not a set-up gap.

### Hibernate and resume: off and setup

I started `virsh event --domain t-ph-<mode> --all --loop --timestamp`, wrote a marker to `/dev/shm/marker`, and
hibernated. The guest agent call returned nothing useful because the agent dies with the guest, so I used
`virsh domstate --reason`, polled every 4 s. Both modes gave `running (booted)` once, then `shut off (shutdown)` on
every poll.

Events, off (verbatim):

```text
2026-11-24 19:32:34.191+0000: event 'agent-lifecycle' for domain 't-ph-off': state: 'disconnected' reason: 'channel event'
2026-11-24 19:32:34.684+0000: event 'pm-suspend-disk' for domain 't-ph-off'
2026-11-24 19:32:34.684+0000: event 'lifecycle' for domain 't-ph-off': PMSuspended Disk
2026-11-24 19:32:34.684+0000: event 'lifecycle' for domain 't-ph-off': Shutdown Finished after guest request
2026-11-24 19:32:35.053+0000: event 'lifecycle' for domain 't-ph-off': Stopped Shutdown
```

Events, setup (the same pattern):

```text
2026-11-24 19:34:13.712+0000: event 'agent-lifecycle' for domain 't-ph-setup': state: 'disconnected' reason: 'channel event'
2026-11-24 19:34:14.183+0000: event 'pm-suspend-disk' for domain 't-ph-setup'
2026-11-24 19:34:14.183+0000: event 'lifecycle' for domain 't-ph-setup': PMSuspended Disk
2026-11-24 19:34:14.183+0000: event 'lifecycle' for domain 't-ph-setup': Shutdown Finished after guest request
2026-11-24 19:34:14.521+0000: event 'lifecycle' for domain 't-ph-setup': Stopped Shutdown
```

This matches probe 3: libvirt reports a shut-off VM, with a `PMSuspended Disk` event for an instant. Then `virsh start`:

| Mode | Uptime before | Uptime after resume | `uptime -s` | Marker | Kernel log |
| -- | -- | -- | -- | -- | -- |
| off | 136.33 s | 188.25 s | `19:30:14`, unchanged | `marker-627` survived | `hibernation entry`, then `hibernation exit` |
| setup | 16.74 s | 68.14 s | `19:33:53`, unchanged | `marker-615` survived | `hibernation entry`, then `hibernation exit` |

Kernel log after the resume, off (setup had the same lines): `PM: Image not found (code -22)` at boot,
`PM: hibernation: hibernation entry`, then `PM: hibernation: hibernation exit`. The timestamps after the restore read
`[18446744050.508384]`, which is a kernel clock artefact of the restore, not an error. After the start, `virsh domstate
--reason` said `running (booted)`. Lockdown was still `[none]` in both guests after the resume.

Start events, off:

```text
2026-11-24 19:33:18.153+0000: event 'agent-lifecycle' for domain 't-ph-off': state: 'disconnected' reason: 'domain started'
2026-11-24 19:33:18.175+0000: event 'lifecycle' for domain 't-ph-off': Resumed Unpaused
2026-11-24 19:33:18.178+0000: event 'lifecycle' for domain 't-ph-off': Started Booted
2026-11-24 19:33:18.793+0000: event 'rtc-change' for domain 't-ph-off': 0
2026-11-24 19:33:24.219+0000: event 'agent-lifecycle' for domain 't-ph-off': state: 'connected' reason: 'channel event'
```

### Hibernate: on (refused by the guest)

I ran `systemctl hibernate` in the guest through the guest agent, after the swap file and resume parameters were in
place and had survived a reboot:

```text
$ systemctl hibernate; echo "systemctl exit=$?"
Call to Hibernate failed: Sleep verb 'hibernate' is not configured or configuration is not supported by kernel
systemctl exit=1
```

`journalctl -b` in the guest (kernel and systemd lines about hibernation; the guest's hostname is `t-tpl-build`):

```text
Nov 24 19:35:33 t-tpl-build kernel: Kernel is locked down from EFI Secure Boot; see man kernel_lockdown.7
Nov 24 19:35:33 t-tpl-build kernel: Lockdown: swapper/0: hibernation is restricted; see man kernel_lockdown.7
Nov 24 19:35:33 t-tpl-build kernel: Lockdown: resume: hibernation is restricted; see man kernel_lockdown.7
Nov 24 19:35:47 t-tpl-build kernel: Lockdown: systemd-logind: hibernation is restricted; see man kernel_lockdown.7
```

The domain stayed `running (booted)` at every poll, and `virsh event` logged nothing for it. libvirt was never asked
to power the VM off. The refusal is the kernel lockdown in the guest, which enforced Secure Boot switches on. It does
not depend on libvirt or on the `<pm>` element, which was set to `suspend-to-disk enabled='yes'` in the definition.

One more observation. I ran `virsh dompmsuspend t-ph-on disk`. The command had not returned after 120 s, and I stopped
it. The domain stayed `running (booted)`, no event arrived, and the guest agent no longer answered `guest-ping` within
20 s afterwards. I did not find out why, and I did not repeat it. So for the on mode, `dompmsuspend` can hang. A caller
should not use it as the hibernate path without a timeout, or should refuse `hibernate` for Secure Boot on up front.

### Feeds unit 5

- The design's firmware section can say that an explicit loader and nvram template, read back from the first
  `dumpxml --inactive`, give hibernation-capable VMs for Secure Boot off and for setup mode. libvirt accepts `<pm>`
  with `suspend-to-disk` in the explicit form for every mode, and `start_vm` needs no arm for a hibernated VM.
- `hibernate: true` with Secure Boot on cannot work with this guest, because the kernel locks hibernation down when
  Secure Boot is enforced. The server should refuse that combination at create time with a clear message, or document
  it, instead of defining a VM that cannot hibernate.
- The redefine keeps the UUID only if the stage 2 XML carries the same `<uuid>` and name. I always passed it.

### What I did not test

- Whether a guest without enforced lockdown can hibernate with Secure Boot on (for example `lockdown=none` on the
  kernel command line, which Secure Boot overrides on most distribution kernels). I did not try changing the guest's
  lockdown mode.
- Other guests, kernels, or BIOS firmware.
- `smm off` with the on or setup loaders (I used `smm on` there).
- `on_poweroff` other than `destroy`.
- A second hibernate and resume cycle per mode.

### Cleanup

I destroyed each VM, ran `virsh undefine --nvram t-ph-<mode>`, deleted `t-ph-<mode>.qcow2`, ran `virsh pool-refresh
agent-vms`, and confirmed with `virsh list --all` and `virsh vol-list agent-vms` that no `t-ph-*` VM or volume remains.
The `.vars` volumes went with `undefine --nvram`.
