# Custom templates and images: design

Status: draft, awaiting approval. Its decisions came from an interview, and two of them are provisional until the
test-bed probes under Checks settle them. Once built, the code wins and this design follows it.
Basis: roadmap outcome 5 in [`fork-roadmap.md`](fork-roadmap.md). It extends
[`disposable-vms-design.md`](disposable-vms-design.md), whose overlays, seeds, reset, expiry and invariants still hold
unless this design changes them. The terms image, volume and template are used as the
[README's Terms section](../../README.md#terms) defines them.

## Summary

Agents test images built anywhere and keep prepared VMs as templates. Behind a config switch, an agent seals a
shut-off VM the server created into a template, or imports a disk image file from a share directory as one, and deletes
templates agents made. A template is a disk together with the UEFI variables and the hardware it was built with, so a
fully updated Windows keeps its Secure Boot certificates in every VM made from it and after every reset. `create_vm`
boots a built image directly, takes more CD-ROMs, and offers Secure Boot setup mode and hibernation. Every UEFI VM the
server creates keeps its variables as a volume in the pool, which is what lets the server copy them for seals and
snapshots.

Two needs drive it. A dotfiles repository builds a Windows 11 VM with every update, WSL and SSH, keeps that state as a
template, and runs each test in a fresh VM from it. A Nix configuration repository boots each ISO and disk image it
builds, under BIOS, UEFI and UEFI with its own Secure Boot keys and a TPM, and tests hibernation.

## Decisions

Device passthrough was settled in the same interview but is its own roadmap item, follow-up 36, outside this design.

| Decision | Recommended | Chosen | Why | Status |
| -- | -- | -- | -- | -- |
| Where agent-made templates are recorded | A metadata volume `tpl-<name>.meta` in `disk_pool`, beside `tpl-<name>.qcow2` and `tpl-<name>.vars` | As recommended | Keeps every part of a template in one known location that every server using the pool sees, and that outlives any server's own state | resolved |
| Switch and caps for agent-made templates | `manage.agent_templates` (off by default, also needs `manage.create`), `max_templates = 4`, `max_template_gib = 100` counting allocated space of every agent-made template; checked before a seal or import, which is refused with the numbers | As recommended | Space is the real limit; a virtual-size cap would block a mostly empty 80 GiB Windows disk | resolved |
| Deleting templates | `template_delete` removes an agent-made template's disk, variables and record; needs `manage.delete`; refuses while any VM on the host backs onto it (count in the reply, names in the log); config templates are never deleted by a tool; any agent may delete any agent-made template | As recommended | Versioned templates pile up otherwise and the caps would block new seals; sharing matches how VMs work | resolved |
| Which VMs can be sealed | Only VMs the server created (owned), not `manage_vms` | As recommended | Sharing an operator VM's contents with every agent must be an explicit act: import its disk, or seal a copy a create tool made | resolved |
| What a seal copies | The boot disk as one standalone qcow2, chain merged and internal snapshots dropped; no seed disk, no CD-ROMs; a VM with a second writable disk is refused until it is detached; hardware the template cannot record, such as a host device added by hand, is left out and named in the reply | As recommended | The template must not depend on an older one, nothing the guest relies on is lost silently, and host devices belong to one host and moment | resolved |
| Seal in the call or in the background | Background: `template_create` returns a transfer ID, the agent waits with `wait_for` `transfer_done`; the template is listed only when complete; a failure deletes the partial volumes; starting the VM being sealed is refused | As recommended | A copy of 30 GB or more can outlast an MCP client's call timeout | resolved |
| Host device passthrough | Its own roadmap item, outside this design: operator-named devices in `[manage.devices.<name>]`, either `serial` (a host serial device such as a `/dev/serial/by-id/` path, shown to the guest as a serial port, attach while shut off) or `usb` (vendor, product, optional serial number resolved to the bus address at each attach, hot-pluggable); `list_devices`, `attach_device`, `detach_device` on owned VMs with `manage.reconfigure`; one VM per device across the host; templates never carry devices; tested with a USB-to-serial adapter and a spare USB device; PCI passthrough stays parked | As recommended | A guest driving physical hardware is a new trust boundary, so each device is named by the operator with no wildcards | resolved |
| Tying a template to its firmware build | Variables stored as `tpl-<name>.vars`; each VM names the sealed VM's firmware code file and starts from a copy through `<nvram template=…>`; the code file's path is pinned and its hash recorded; a changed hash warns in the reply and the VM still starts | As recommended | A routine `ovmf` upgrade must not break every template, and the variable store format is stable; the Secure Boot template test catches a breaking firmware change | resolved |
| Where VM variables live, and snapshots | Each UEFI VM the server creates keeps its variables as `<vm>.vars` in `disk_pool`, named in `<nvram template=…>`; seal copies it to `tpl-<name>.vars`; a snapshot of a shut-off UEFI VM copies it to `<vm>-snap-<snapshot>.vars`, revert copies it back, snapshot and VM deletion delete the copies; UEFI VMs made earlier cannot be sealed and their snapshots stay without variables | As recommended | libvirt's own NVRAM path is root-owned and has no API to read or write it, so the server could neither seal nor snapshot variables there | provisional: a test-bed probe must show that libvirt accepts a variable file in the pool, copies the template into it, and that the per-VM AppArmor profile lets QEMU write it; a failure returns the design to the owner |
| Hardware a template records, and per-VM overrides | A template records OS, firmware, Secure Boot mode (off, Microsoft keys, setup), TPM, memory and vCPUs (disk size from the volume); seal takes them from the VM, import from the call, config templates may set them; a VM defaults to them and may override memory and vCPUs within limits, grow the disk, add or drop the TPM; firmware and Secure Boot are fixed for a sealed template and chosen per VM for an imported or config template; values above a limit are refused with the limit named | As recommended | The Windows template needs its build hardware back, one imported image must boot in every mode, and a sealed template's variables belong to one firmware | resolved |
| Cloud-init seed per template | `seed = "nocloud"` or `"none"`, default `nocloud` for Linux and `none` for Windows, set at seal, import or in the config; with `none` the VM keeps the template's hostname and identity and `user_data` is refused; `nocloud` on Windows is the cloudbase-init opt-in, which its first user verifies | As recommended | Windows shows the seed as a stray drive, and images without cloud-init gain nothing from it | resolved |
| Importing an image, and `disk_image` | Source inside `host_share_dirs`, resolved like `copy_to_vm` (a `/nix/store` result is copied in with `cp -L`); raw or qcow2 kept as-is, so a template may be raw; a qcow2 naming a backing file or an external data file is refused; virtual size within `max_disk_gib`, space counted toward the template cap; runs in the background with a transfer ID; `create_vm` `disk_image` makes the copy the VM's own disk, deleted with it, with `disk_bus` `virtio` (default), `sata` or `usb`; every upload is sparse: zero runs and holes go as holes through `virStreamSendHole` with `VIR_STORAGE_VOL_UPLOAD_SPARSE_STREAM`, and a seal's `qemu-img convert` copy already skips zeros; non-sparse sources are accepted | As recommended, with sparse uploads | One copy path serves both uses; a backing reference would let QEMU open host files; sparse uploads keep raw images small without asking agents to prepare them | resolved |
| Secure Boot setup mode, and TPM state on reset | `secure_boot` takes `"off"`, `"on"` (Microsoft keys) or `"setup"` (no keys enrolled), with `true` and `false` still read as on and off, in `create_vm`, templates and per-VM choices; setup mode combines with `tpm: true`, and keys the guest enrolls live in its variables, so a seal carries them; `reset_vm` gives an empty TPM by default and keeps it with `keep_tpm: true`; TPM state persists across reboots and shutdowns | As recommended | Lanzaboote and `sbctl` need firmware with no keys; a reset must return to the template, which never carries TPM state, while TPM-bound disks opt in to keep it | provisional: a test-bed check settles how a reset clears TPM state, since a reset keeps the UUID that libvirt files TPM state under |
| More CD-ROMs | `create_vm` `extra_isos`: up to 4 ISOs from `iso_dirs`, each a read-only SATA CD-ROM after the installer and driver drives, never booted from; `set_cdrom` on a shut-off VM adds a drive at a free named target | As recommended | An unattended Windows install needs the installer, virtio-win and an answer-file ISO; an existing VM can only gain a drive through `set_cdrom` | resolved |
| Hibernation per VM | `hibernate` on `create_vm` and templates, off by default, sets `<suspend-to-disk enabled='yes'/>` with suspend to memory still off; a hibernating guest shuts the VM off, `vm_events` records it, `start_vm` resumes it; a seal records it; `list_vms` shows the hibernated state where libvirt reports it | As recommended | nix-config's hibernate-and-resume path with LUKS swap needs a VM to cover it | resolved |
| Nested virtualization | No new setting: `host-passthrough` with the host's `kvm_intel nested=Y` already exposes it; a test-bed check shows `/dev/kvm` in a Linux template VM, and the dotfiles WSL 2 run covers Windows | As recommended | It already works but nothing proves it | resolved |

## Behaviour

### What a template is

A template is two or three volumes in `manage.disk_pool` and a record:

| Volume | Holds |
| -- | -- |
| `tpl-<name>.qcow2` or `tpl-<name>.img` | The disk, qcow2 or raw. VMs back onto it and never write it. |
| `tpl-<name>.vars` | UEFI templates only: the firmware variables every VM starts from. |
| `tpl-<name>.meta` | Agent-made templates only: a small JSON record the server writes. |

The record holds the template's origin (`sealed` or `imported`), its state (`building` or `ready`), its OS, firmware,
the firmware code file's path and SHA-256, its Secure Boot mode, TPM, memory, vCPUs, seed choice and hibernation
setting, the names of its volumes, and its creation time. A template from the config has no record and keeps working as
today; it may now set the same hardware fields, `seed`, `hibernate`, and a raw disk. A template is usable when its
record says `ready` (or it comes from the config), every volume it names exists, and no VM uses its disk as a writable
disk. Template names follow the hostname rule, and one name space covers config and agent-made templates: a seal or
import refuses a name the config uses or whose volumes exist.

`list_templates` shows, for each template, its origin, hardware, firmware, Secure Boot mode, seed choice, space on
disk and the number of owned VMs using it. A `building` record from a seal or import that did not finish shows as
incomplete.

### Sealing a VM

`template_create` with `vm` and `name` seals a VM. It needs `manage.agent_templates` and `manage.create`.

```mermaid
flowchart LR
  A[Check VM, name,<br/>caps, pool space] --> B[Mark VM sealing<br/>in its owner element]
  B --> C[Write record<br/>state building]
  C --> D[Copy boot disk<br/>to tpl-name]
  D --> E[Copy variables<br/>to tpl-name.vars]
  E --> F[Rewrite record<br/>state ready]
  F --> G[Clear the mark,<br/>transfer done]
  C -. failure .-> X[Delete volumes made,<br/>clear the mark]
  D -. failure .-> X
  E -. failure .-> X
```

1. The checks refuse: a VM the server did not create; a VM that is not shut off; a UEFI VM whose variables are not a
   pool volume, which covers UEFI VMs made before this design; a VM with a second writable disk; a name in use; and a
   seal that would pass `max_templates`, `max_template_gib` or the pool's free space. The space estimate is the boot
   disk's allocated size, its whole chain included.
2. The VM's owner element gains a `sealing` attribute, so `start_vm`, `reset_vm` and `delete_vm` refuse it, in any
   server process, until the seal ends. Only the server writes owner attributes, as for `generation` and `expires`.
3. The record is written first, with state `building`, so a seal cut short by a crash leaves volumes a record names,
   which `template_delete` can remove. Volumes no record names are never deleted by a tool.
4. The boot disk is copied with libvirt's volume copy into a standalone qcow2: the copy merges the overlay chain, drops
   internal snapshots and skips zero blocks. The seed disk and CD-ROMs are not copied.
5. The VM's `<vm>.vars` is copied to `tpl-<name>.vars`, and the record takes the hardware from the VM's definition.
   Hardware the record cannot hold, such as a host device added by hand, is left out and named in the result.
6. The call returns a transfer ID at once, and `wait_for` with `transfer_done` ends with the result or the failure. A
   failure deletes the volumes the seal made and clears the mark.

### Importing an image

`template_create` with `image`, `name`, `os` and the hardware fields imports a file from `host_share_dirs`, resolved as
`copy_to_vm` resolves its source. It needs the same switches as a seal and runs in the background the same way.

- The server reads the file's header. A qcow2 image that names a backing file or an external data file is refused, so
  QEMU never opens a host path the image chose. Anything that is neither qcow2 nor raw is refused.
- The virtual size must fit `max_disk_gib`, and the data size the caps and the pool's free space.
- The upload streams the file in libvirt's sparse mode: holes, and runs of zero blocks found while reading, travel as
  holes through `virStreamSendHole` with `VIR_STORAGE_VOL_UPLOAD_SPARSE_STREAM`, so the volume is sparse whatever the
  source was. `virt` 0.4.3 has no wrapper for it, so the call goes through `virt-sys` 0.3.1. The stream is aborted on
  every error path.
- An imported template has no variables. Its VMs start from the firmware's stock variables, and each VM may choose
  firmware and Secure Boot mode.

### Creating VMs

`create_vm_from_template` takes the template's recorded values as defaults:

- memory and vCPUs may be overridden within the limits, the disk grown but not shrunk, and `tpm` and `hibernate` set
  either way;
- for an imported or config template, `firmware` and `secure_boot` may be chosen per VM; for a sealed template they are
  fixed, because its variables belong to that firmware;
- a recorded value above a current limit refuses the call and names the limit, rather than shrinking the VM;
- a template with `seed = "none"` gets no seed disk.

A VM from a sealed UEFI template names the template's code file in `<loader>` and starts its own `<vm>.vars` from
`tpl-<name>.vars` through `<nvram template=…>`. When the code file's SHA-256 differs from the record, the result warns
that the firmware changed since the seal, and the VM still starts. `reset_vm` already starts with
`VIR_DOMAIN_START_RESET_NVRAM`, which copies the template's variables again.

`create_vm` gains:

- `disk_image`: a file from `host_share_dirs`, checked and uploaded as for an import, which becomes the VM's own disk in
  place of a new empty one, and goes with `delete_vm`;
- `disk_bus`: `virtio` (the default), `sata` or `usb`, for that disk;
- `extra_isos`: up to 4 ISOs from `iso_dirs`, each a read-only SATA CD-ROM after the installer and driver drives, never
  booted from;
- `hibernate`: off by default, which writes `<suspend-to-disk enabled='yes'/>` and leaves suspend to memory off.

`secure_boot` in `create_vm`, in templates and in per-VM choices takes `"off"`, `"on"` (Microsoft's keys enrolled) or
`"setup"` (Secure Boot firmware with no keys enrolled), and still reads `true` and `false` as on and off. The firmware
features are `secure-boot` and `enrolled-keys` both on, `secure-boot` on with `enrolled-keys` off, and both off. Setup
mode combines with `tpm: true`, and keys a guest enrolls stay in its variables, so a seal carries them.

`set_cdrom` on a shut-off VM adds a drive at a free target it names, where it refuses today.

### Where a VM's variables live

Every UEFI VM the server creates, from an ISO, an image or a template, keeps its variables in the pool as `<vm>.vars`,
named in its definition. libvirt's default place, under `/var/lib/libvirt/qemu/nvram/`, is owned by root, and libvirt
has no call to read or write it, so a server running as the operator could neither seal nor snapshot variables there.
`delete_vm` already undefines with `VIR_DOMAIN_UNDEFINE_NVRAM`, which removes the file.

### Snapshots

A snapshot of a shut-off UEFI VM also copies `<vm>.vars` to `<vm>-snap-<snapshot>.vars`. `snapshot_revert` copies it
back while the VM is off, `snapshot_delete` deletes the copy, and `delete_vm` and `reset_vm` delete every copy. A
running UEFI VM still cannot be snapshotted on hosts whose firmware variables are raw files, as today. A UEFI VM made
before this design keeps its variables where libvirt put them; its snapshots stay without them, and the result says so.

### TPM state

A template never carries TPM state. `reset_vm` gives the VM an empty TPM, and keeps the old one with `keep_tpm: true`,
for a guest whose disk unlocks through the TPM. TPM state persists across reboots and shutdowns as today.

### Hibernation

A guest that hibernates shuts its VM off; `vm_events` records the `pmsuspended` event, and `start_vm` boots the VM,
whose guest resumes from its swap. `list_vms` shows the hibernated state wherever libvirt reports it, because
`reset_vm` or `delete_vm` on such a VM discards the saved state.

### Deleting templates

`template_delete` needs `manage.agent_templates` and `manage.delete`. It deletes an agent-made template's volumes and
its record, last, and refuses a template from the config. It refuses while any domain on the host uses the template's
disk as a backing file; the result gives the count and the server log names the domains. It deletes an incomplete
template's volumes too. Any agent may delete any agent-made template.

### Gating

`template_create` and `template_delete` are advertised only when `manage.agent_templates` is on with `create` or
`delete` respectively, and read-only mode hides them. Configs without `agent_templates` behave as today, apart from the
new `create_vm` options and the variables volume of new UEFI VMs.

## Invariants

- No code path writes, boots or attaches a template volume. Only `template_delete` deletes one, and only volumes an
  agent-made record names, while no domain backs onto them.
- An agent-made template comes only from a copy of an owned VM's disk or of a file in `host_share_dirs`, into new
  volumes. No call turns an existing volume into a template.
- No imported image or `disk_image` names a backing file or an external data file.
- A template is usable only when its record says `ready` and every volume it names exists.
- Templates never carry TPM state or host devices.
- A VM being sealed cannot be started, reset or deleted.
- The guardrails in `AGENTS.md` hold: `is_owned`, `can_manage` and deletion only within `disk_pool` are unchanged,
  and the `sealing` attribute is written only by the server, on a VM that already carries the owner element.

## Risks

1. Variables in the pool are a departure from libvirt's default. libvirt must accept the path and copy the template
   into it, and the AppArmor profile libvirt writes for each VM must let QEMU write it. The first probe checks this,
   and a failure returns the design to its owner.
2. An `ovmf` upgrade changes the code file under sealed templates. The variable store format is stable, so VMs usually
   still boot, and the warning names the cause when one does not.
3. Secure Boot certificate and revocation updates live in the variables. A sealed template keeps them; an imported
   template starts from the host's stock variables, which may refuse an updated Windows boot manager.
4. A broken or tampered template spreads to every VM made from it. That stays inside the agents' own VMs and the pool.
5. `max_template_gib` can exceed the pool's free space, which the free-space check covers; other users of the
   filesystem can still fill it during a copy.
6. Any agent can delete a template another agent's workflow relies on.
7. A hibernated VM looks shut off to an agent that does not read `list_vms`.
8. cloudbase-init reading a NoCloud seed is unverified; its first user checks it.

## Open questions

| Question | Kind | Recommendation |
| -- | -- | -- |
| Snapshot names in variable volume names | Detail | Name the copy after the snapshot when the name fits the volume-name rule, and refuse other names for snapshots of UEFI VMs, with the rule in the error. |
| How a reset clears TPM state | Probe | Redefine the VM with libvirt's flag that removes TPM state, if a reset that keeps the UUID would otherwise keep it. |

## Checks

- First, a probe on the test bed: a UEFI VM whose `<nvram>` names a volume in `agent-vms` boots, libvirt copies the
  template into it, QEMU writes it (a variable set in the guest survives a restart), and the volume copies with
  libvirt's storage calls.
- A Windows 11 VM with every update applied shows Microsoft's 2023 certificate in its firmware database; it is sealed,
  and a VM from the template boots with Secure Boot on, before and after `reset_vm`.
- A seal of a template VM gives a standalone qcow2: `qemu-img info` shows no backing file, and the source template's
  checksum is unchanged.
- An imported raw image of 4 GiB with 1 GiB of data occupies about 1 GiB in the pool.
- A qcow2 naming a backing file, an image outside the share directories, a seal of an unowned or running VM, a seal
  over the caps, and `template_delete` of a config template or of a template in use are each refused; a start of a VM
  being sealed is refused.
- One imported image boots in three VMs: BIOS, UEFI without Secure Boot, and UEFI in setup mode with a TPM, where
  `sbctl status` reports setup mode.
- A shut-off UEFI VM's snapshot keeps a boot entry made after it: after the revert, the entry is gone.
- `reset_vm` leaves an empty TPM, and one with `keep_tpm` keeps a sealed TPM object.
- A VM with `hibernate` resumes a process after `systemctl hibernate` and `start_vm`.
- A Linux VM from a template shows `/dev/kvm`.
- Unit tests cover the record's parsing, the qcow2 header check, the hole detection, the caps, the hardware defaults
  and limits, the `secure_boot` values, and the XML for each firmware mode.
