# VM management

All of it is off by default. Enable each operation separately in the `[manage]` section of the
config file (see `config.example.toml`):

```toml
[manage]
create = true
delete = true
reconfigure = true
snapshots = true
manage_vms = []                     # existing VMs (also in allowed_vms) the tools may act on
disk_pool = "default"               # libvirt pool for new disks, required for create
iso_dirs = ["/var/lib/libvirt/iso"]
networks = ["default"]
max_vms = 2
max_vcpus = 2
max_memory_mib = 4096
max_disk_gib = 32
```

- **Which VMs:** the management tools act on VMs this server created and on VMs listed in
  `manage_vms`. A VM created by the server carries a marker in its libvirt metadata. While any
  `manage` operation is enabled, such VMs are also treated as if they were in `allowed_vms` and
  `power_vms`, so the AI can start them, use `exec` and so on. Other VMs stay off limits.
  `list_vms` shows `managed: true` for VMs the tools may act on.
  [Give agents their own test VMs](how-to/agent-test-vms.md) sets up a server whose agents see only such VMs.

- **Creating:** `create_vm` takes a name, `os` (`linux`/`windows`), vCPUs, memory, disk size, `firmware` (`uefi`, the
  default, or `bios`), an optional TPM 2.0, an install ISO, a second "driver" ISO (e.g. `virtio-win.iso` for Windows),
  up to four `extra_isos` (such as an answer-file ISO) and a network. The driver ISO and each extra ISO are read-only
  SATA CD-ROMs after the install drive, which is the only CD-ROM the VM boots from. q35's SATA controller has six
  ports, one per CD-ROM or SATA disk. A UEFI VM uses the host's firmware with Secure Boot, which refuses an installer
  that is not signed. `secure_boot` takes `"on"` (the default, or `true`), `"off"` (or `false`), which boots an
  unsigned installer, and `"setup"`, which has Secure Boot firmware with no keys enrolled so the guest can enrol its
  own. `"off"` and `"setup"` need `firmware: "uefi"`. The VM gets a q35 machine, a virtio disk (a new qcow2 volume
  `<name>.qcow2` in `disk_pool`), a SATA CD-ROM it boots from after the disk, a virtio NIC, the guest agent channel, a
  serial console, SPICE and spare PCIe ports for hot-plugging. Windows guests also get Hyper-V enlightenments and a
  local-time clock. Every VM the server creates, here or from a template, also gets a panic device (`isa` for Linux,
  `hyperv` for Windows), so a guest crash becomes libvirt's crashed event, a balloon that reports guest memory every 5
  s, and `manage.on_crash` as its crash action: `preserve` (the default) keeps a crashed VM stopped in place, holding
  its memory, until `stop_vm` with `force: true`, and `destroy` stops it. The VM is created shut off. A UEFI VM
  keeps its firmware variables (boot entries, Secure Boot keys) as `<name>.vars` in `disk_pool`. libvirt creates the
  file at the VM's first start, so it is absent until then, and the server rescans the pool after each start. A
  `<name>.vars` that already exists in the pool, a leftover of an earlier VM, makes `create_vm` and
  `create_vm_from_template` refuse the name, because libvirt would give the new VM the old variables. The error
  names the command that removes it, `virsh vol-delete --pool <disk_pool> <name>.vars`. UEFI VMs created by an
  earlier version keep their variables in libvirt's own directory.

- **Hibernation:** `create_vm` with `hibernate: true` turns suspend to disk on and leaves suspend to memory off. A
  guest that hibernates (`systemctl hibernate`) ends up shut off, and `start_vm` resumes it from its swap. libvirt
  keeps no saved state for it, so `vm_events` is where you see it: a `pmsuspended` event with the reason disk comes
  just before the shutdown, and `reset_vm` or `delete_vm` discards the saved state without a warning. The guest needs
  swap at least as large as its memory and the `resume` settings of its own. A UEFI VM with `hibernate` has its
  firmware named by path in its definition, not chosen by libvirt, because libvirt's automatic choice refuses suspend
  to disk; `describe_vm` still reports the mode. A Linux guest with `secure_boot` `"on"` runs under kernel lockdown,
  which refuses hibernation, so use `"off"` or `"setup"` for a VM that has to hibernate. The server does not refuse the
  combination, because another guest may allow it. `describe_vm` shows `hibernate`.

- **Disposable VMs from templates:** `create_vm_from_template` makes a VM in seconds from a template
  volume the operator prepared, such as a Debian 13 cloud image with the guest agent installed
  ([docs/templates.md](templates.md) builds one with `scripts/make-template.sh`). The VM gets a
  thin qcow2 overlay on the template (`<name>-g1.qcow2`) and a small read-only seed disk
  (`<name>-g1-seed.img`) that cloud-init reads for the VM's identity; its name is also its hostname.
  It starts and waits for the guest agent by default. With `lifetime_minutes` (at most
  `max_lifetime_minutes`), the server deletes the VM and its disks once the time has passed: every
  running server checks once a minute, and `virtmanager-mcp sweep` does one check, for a systemd timer
  when no server runs. A running server's `vm_events` records its own expiry as cause `expiry`, and one
  by `virtmanager-mcp sweep` as `outside`. `reset_vm` returns such a VM to the template's state: new
  disks, no snapshots, same name, MAC address, hardware and expiry. `list_templates` shows the usable
  templates; one whose volume is missing, not qcow2 or attached to any VM, or whose user data is
  unreadable, does not start with a `#cloud-config` line or exceeds 1 MiB, is hidden, and the server
  log says why. Never change or delete a template volume while VMs use it.

  ```toml
  [manage]
  create = true
  delete = true               # also needed for reset_vm
  disk_pool = "agent-vms"
  max_lifetime_minutes = 1440

  [manage.templates.debian13]
  volume = "tpl-debian13.qcow2" # qcow2 volume in disk_pool
  firmware = "bios"             # or "uefi"; os = "linux" (default) or "windows"
  # secure_boot = "off"                           # UEFI only: "on" (default), "off" or "setup"
  description = "Debian 13 with qemu-guest-agent"
  # user_data = "/etc/virtmanager-mcp/debian13.yaml"   # optional cloud-config for every new VM
  ```

- **Deleting:** `delete_vm` needs the VM to be shut off. It removes the VM with its snapshots,
  NVRAM and TPM state, and deletes the VM's disks that are volumes in `disk_pool`. Disks
  elsewhere and ISOs are never deleted. The same applies to VMs in `manage_vms`.

- **Reconfiguring:** `configure_vm` changes vCPUs and memory in the persistent configuration
  (takes effect at the next boot). `attach_disk` creates a new volume in `disk_pool` and attaches
  it, also while the VM is running. `detach_disk` needs the VM shut off and **deletes** the volume
  if it is in `disk_pool`. `set_cdrom` inserts or ejects ISOs; on a shut-off VM, a
  `target` of the form `sd<letter>` that no device uses adds a new CD-ROM drive there, within the six SATA ports.
  `attach_nic`/`detach_nic` add and
  remove interfaces on allowed networks. `describe_vm` shows targets and MAC addresses.

- **Limits:** `max_vcpus`, `max_memory_mib` and `max_disk_gib` apply to create and reconfigure. `max_total_memory_mib`,
  unset by default, caps the memory of every VM created by the server that runs, is paused or is suspended, taken
  together. Every start of such a VM checks it, from `start_vm`, `create_vm_from_template`, `reset_vm` or a revert to a
  running or paused snapshot, which counts the snapshot's memory, and a start that would pass it is refused with the
  running VMs named; a VM larger than the cap is refused at create and reconfigure. Other VMs on the host do not count.
  `max_vms` caps how many VMs created by the server exist at once. It holds across every server process and `sweep`
  command of one account, such as several Claude Code sessions: management operations take the lock file
  `~/.cache/virtmanager-mcp/manage.lock`, and wait up to 30 s for another operation to release it. Processes running as
  different accounts, or with a different `HOME`, do not share it.

- **Paths:** ISOs must be inside `iso_dirs` (resolved like `host_share_dirs`, symlinks
  included), and QEMU (user `libvirt-qemu`/`qemu`) must be able to read them. `list_isos` lists the
  `.iso` files found there.

- **Snapshots** are libvirt's internal qcow2 snapshots. A running VM's snapshot includes its
  memory. A *running* UEFI VM cannot be snapshotted on hosts whose OVMF firmware has no qcow2
  NVRAM variant (Arch, Ubuntu): libvirt refuses, so shut it down first. BIOS VMs work either way.
