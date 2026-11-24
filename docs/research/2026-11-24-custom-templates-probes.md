# UEFI variables in the pool and how a reset clears TPM state (2026-11-24)

Two probes for the provisional decisions in [`custom-templates-design.md`](../plans/custom-templates-design.md): "Where VM
variables live, and snapshots", and "Secure Boot setup mode, and TPM state on reset". Host: libvirt 12.0.0
(`libvirt-daemon-system 12.0.0-1ubuntu5.6`), QEMU 10.2.1, OVMF `2025.11-3ubuntu7.3`, swtpm 0.10.1, AppArmor enforcing,
all on `qemu:///system`. Guests were qcow2 overlays of `tpl-debian13.qcow2` (Debian 13, kernel
`6.12.115+deb13.1-cloud-amd64`), 1 GiB, 1 vCPU, driven through `virsh qemu-agent-command` with `guest-exec`. Every VM
and volume was named `t-*` or `tpl@probe*`, and all were deleted afterwards. The tool installs in the guest were not
needed: UEFI variables were written through `/sys/firmware/efi/efivars`, and the TPM was driven with raw TPM 2.0
commands through `/dev/tpmrm0` from a short Python script.

## Probe 1: variables in the pool

Every guest used this skeleton, with the `<os>` element varying (below) and `<name>` equal to the overlay's name:

```xml
<domain type='kvm'>
  <name>t-nv-off</name>
  <memory unit='MiB'>1024</memory>
  <vcpu>1</vcpu>
  <os>…</os>
  <features><acpi/><apic/><smm state='on'/></features>
  <cpu mode='host-passthrough'/>
  <on_poweroff>destroy</on_poweroff>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='/var/lib/libvirt/images/agent-vms/t-nv-off.qcow2'/>
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

### The four `<os>` forms that worked

Secure Boot off (autoselection, no keys needed, `OVMF_CODE_4M.fd`):

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='q35'>hvm</type>
  <firmware><feature enabled='no' name='secure-boot'/></firmware>
  <nvram format='raw'>/var/lib/libvirt/images/agent-vms/t-nv-off.vars</nvram>
</os>
```

Secure Boot on with Microsoft keys (descriptor `40-edk2-x86_64-secure-enrolled.json`, `OVMF_CODE_4M.ms.fd`):

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='q35'>hvm</type>
  <firmware>
    <feature enabled='yes' name='enrolled-keys'/>
    <feature enabled='yes' name='secure-boot'/>
  </firmware>
  <nvram format='raw'>/var/lib/libvirt/images/agent-vms/t-nv-on.vars</nvram>
</os>
```

Setup mode (descriptor `50-edk2-x86_64-secure.json`: `secure-boot` without `enrolled-keys`, `OVMF_CODE_4M.secboot.fd`
with the empty `OVMF_VARS_4M.fd`):

```xml
<os firmware='efi'>
  <type arch='x86_64' machine='q35'>hvm</type>
  <firmware>
    <feature enabled='no' name='enrolled-keys'/>
    <feature enabled='yes' name='secure-boot'/>
  </firmware>
  <nvram format='raw'>/var/lib/libvirt/images/agent-vms/t-nv-setup.vars</nvram>
</os>
```

The sealed form, with an explicit loader and no autoselection. `tpl@probe.vars` was a `vol-clone` of the Secure Boot VM's
variables after it had set a marker variable:

```xml
<os>
  <type arch='x86_64' machine='q35'>hvm</type>
  <loader readonly='yes' secure='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>
  <nvram template='/var/lib/libvirt/images/agent-vms/tpl@probe.vars' templateFormat='raw' format='raw'>/var/lib/libvirt/images/agent-vms/t-nv-sealed.vars</nvram>
</os>
```

In the three autoselected forms libvirt accepts the bare `<nvram format='raw'>` path at `virsh define` and writes the
`template` attribute and `templateFormat` itself (`/usr/share/OVMF/OVMF_VARS_4M.fd` for off and setup,
`OVMF_VARS_4M.ms.fd` for on). No `template` attribute, `type='file'` form or pre-created volume is needed.

### Results

| Question | Result | Evidence |
| -- | -- | -- |
| 1. A pool path in `<nvram>` is accepted, and libvirt creates the file from the descriptor's template at first start | Holds, in all three autoselected forms | `define` succeeds; the file does not exist after it. After `start` the file exists, 540672 bytes, mode 0600, owner `64055:991` (`libvirt-qemu:kvm`). The pool lists it only after `virsh pool-refresh agent-vms`; before that, `vol-list` and `vol-clone` do not see it |
| 2. QEMU writes it under the generated AppArmor profile | Holds, in all four forms | `/etc/apparmor.d/libvirt/libvirt-<uuid>.files` lists `".../t-nv-off.vars" rwk`, `".../tpl-debian13.qcow2" rk` (the backing file) and the loader `rk`; `/usr/share/OVMF/**` also falls under `abstractions/libvirt-qemu`. The file's mtime moved when the guest wrote. A variable created in the guest (`ProbeVar` under a private GUID, attributes NV, BS, RT) was read back unchanged after `virsh shutdown` and `virsh start`, once for off, on, setup and the sealed copy |
| 3. libvirt's storage calls copy it, and the copy serves as another VM's `<nvram>` | Holds | `vol-clone --pool agent-vms t-nv-off.vars t-nv-copy.vars` and `vol-create-from` both worked; the `vol-clone` copy kept the owner `64055:991`. A VM defined with the copy as its `<nvram>` booted with `ProbeVar` present, so libvirt used the existing file and did not recreate it. `vol-download` followed by `vol-upload` over an existing `.vars` volume replaced it byte for byte (same md5 on download), which is the revert path. `vol-clone` onto an existing name was not tried |
| 4. Exact XML for off, on, setup and the sealed form | Holds | The four `<os>` elements above. Off: `dmesg` reports "Secure boot disabled" and `mokutil` that Secure Boot is unsupported, with no `SecureBoot` variable. On: `SecureBoot` = 1, `SetupMode` = 0, `PK` and a `db` holding Microsoft certificates, kernel lockdown by Secure Boot. Setup: `SecureBoot` = 0, `SetupMode` = 1, `mokutil` says "Platform is in Setup Mode", and no `PK`, `KEK` or `db` variable exists. Sealed: started from the template (the marker variable was present), and `mokutil --sb-state` said "SecureBoot enabled" |
| 5. An overlay backed by `tpl@probe.qcow2` starts under the profile | Holds | `tpl@probe.qcow2` was a `vol-clone` of `tpl-debian13.qcow2`; `vol-create-as … --backing-vol 'tpl@probe.qcow2'` made the overlay; the VM booted, and the profile lists `"…/tpl@probe.qcow2" rk,` |
| 6. q35's SATA controller has 6 ports | Holds | With 6 empty SATA CD-ROMs the guest logs "ahci 0000:00:1f.2: 6/6 ports implemented" and has 6 `ata_port` entries and 6 `/dev/sr*`. The address `unit='6'` on controller 0 fails at start: `Bus 'ide.6' not found`. A seventh drive without an address makes libvirt add a second SATA controller (index 1, a separate PCI device with another 6 ports), and the guest then shows 12 `ata_port` entries and 7 `/dev/sr*`. The 6-drive limit is therefore the built-in controller's, not a libvirt refusal |

The SATA check ran under UEFI. A first attempt under BIOS looped in GRUB's "Booting Debian GNU/Linux" for a reason I did not
diagnose, so the BIOS case is untested.

### Other facts the probe turned up

- `virsh undefine <vm>` of a UEFI domain with an `<nvram>` fails with "cannot undefine domain with nvram". `--nvram`
  deletes the file, also at a pool path (the volume was gone after a pool refresh), and `--keep-nvram` leaves it. A
  server that deletes the volume itself through the pool must pass `--keep-nvram`; one that relies on libvirt passes
  `--nvram`.
- A missing template volume fails the start, not the define: `Failed to open file '…/tpl@probe.vars': No such file or
  directory`.
- With the sealed form, the `.files` list named `OVMF_CODE_4M.secboot.fd` while the running QEMU used
  `OVMF_CODE_4M.ms.fd` (visible in its `-blockdev` arguments). The abstraction allows both, so the start was unaffected; I
  did not find why the list differs.
- Persistence was shown across a graceful shutdown. A forced stop (`virsh destroy`) of a guest that had just written a
  variable was not tested.

### What it means for the design

The provisional decision "Where VM variables live, and snapshots" holds on this host: libvirt accepts the pool path,
creates the file at first start, QEMU writes it under the generated profile, and libvirt's storage calls copy it,
overwrite it and use a copy as another VM's variables. Three points belong in the implementation. The server must
refresh the pool after a first start before it can see or copy the file. `delete_vm` must pass `--keep-nvram` or `--nvram`,
since a plain undefine is refused. A snapshot revert can copy back with `vol-download` plus `vol-upload`,
which keeps the volume's owner, or delete and `vol-clone`, which also gave the right owner. Nothing here sends the design
back to the owner.

## Probe 2: how a reset clears TPM state

The guest was UEFI with Secure Boot off and `<tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>`. The
guest's TPM was read with raw TPM 2.0 commands: `TPM2_GetCapability` for NV indices (`0x01000000`) and persistent handles
(`0x81000000`), `TPM2_ReadPublic` of the endorsement key at `0x81010001` (its response hashed, so a new key shows as
a new hash), and `TPM2_NV_DefineSpace` of owner index `0x01500016` as the marker of earlier state. Fresh state shows NV
indices `0x1c00002`, `0x1c00016` and `0x1c08000`, and persistent handles `0x81010001` and `0x81010016`. The marker
counts as cleared when `0x1500016` is gone and the endorsement key hash differs. I could not read
`/var/lib/libvirt/swtpm` (root-only), so the state's location under the UUID is inferred from the results below.

| Candidate | Cleared? | Evidence |
| -- | -- | -- |
| State across `virsh shutdown` and `start` | No change, as the design says | Marker and key hash kept |
| `persistent_state='no'` on the `<backend>` | No effect on a persistent domain | The libvirt documentation says the attribute governs whether state survives a transient domain's power-off or undefine. `define` with it succeeded, libvirt dropped it from the stored XML as the default, and the marker was kept |
| Remove `<tpm>`, start, shut down, define it again | No | The guest had no `/dev/tpm*` in between; after the second definition, marker and key hash were the same as before |
| `undefine --keep-nvram --keep-tpm`, then `define` of the saved XML | No | Marker and key hash kept |
| `undefine --keep-nvram --tpm`, then `define` of the saved XML (same UUID) | Yes | Marker gone, key hash changed, fresh NV list. The VM is undefined between the two calls |
| `<source type='file' path='…/t-tp-b.tpm'/>` or `type='dir'` in the pool (libvirt docs: since 10.10.0) | Not tested to a result | `start` fails: `Could not run '/usr/bin/swtpm_setup'. exitstatus: 1`, with a missing file, a 1 MiB and an 8 MiB raw volume, and a directory volume owned by `64055:991` mode 0700. The error log `/var/log/swtpm/libvirt/qemu/<vm>-swtpm.log` is root-only, so I could not read the cause |
| Guest-side `TPM2_Clear` with the lockout hierarchy | Partly | The owner marker went; the endorsement key hash and the platform NV indices stayed. It needs a running guest that cooperates, so it does not give a fresh TPM |
| New UUID without a gap: define a copy of the XML under a temporary name and a new UUID, `undefine --keep-nvram --tpm` the old domain, `domrename` the copy to the old name | Yes | Marker gone, key hash changed, and a UEFI variable set earlier survived because the variables file was kept. The VM is defined at every moment |

The method that works without leaving the VM undefined, with exact steps (VM shut off, with `OLD` its UUID, `NEW` a
fresh one from `uuidgen`):

```sh
virsh dumpxml --inactive t-tp-a > a.xml
sed -e "s#<uuid>$OLD</uuid>#<uuid>$NEW</uuid>#" -e "s#<name>t-tp-a</name>#<name>t-tp-a-x</name>#" a.xml > a2.xml
virsh define a2.xml
virsh undefine --keep-nvram --tpm t-tp-a
virsh domrename t-tp-a-x t-tp-a
```

Two limits showed up. The old domain cannot be undefined while it has snapshots (`cannot delete inactive domain with 1
snapshots`), so delete the snapshots first; `--snapshots-metadata` would drop only their metadata and leave the internal
snapshots in the qcow2. The UUID changes, and `virsh domuuid` returned the new one after the rename. The
disks, the `<nvram>` path and the XML's other content stay as they were.

### What it means for the design

The provisional decision "Secure Boot setup mode, and TPM state on reset" asked for a way to give an empty TPM that never
leaves the VM undefined while its disks exist. Such a way exists, but only by giving the VM a new UUID: the
swtpm state follows the UUID, and nothing I tried clears it while the UUID stays, short of the undefine-and-define
sequence that leaves a gap. The design's premise that a reset keeps the UUID therefore cannot hold for the default
empty-TPM reset. With `keep_tpm: true` the reset needs no extra step, because a shutdown and start keep the state. The
owner has to decide whether a changing UUID is acceptable for `reset_vm`, and check what else keys on it (the event
stream, console capture, anything the server stores). An alternative worth a later probe is the `<source>` form, which
would let the server wipe the state as a pool volume; it needs the root-only swtpm log to diagnose, for example
`sudo tail -n 20 /var/log/swtpm/libvirt/qemu/<vm>-swtpm.log` after a failed start.
