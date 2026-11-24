# Give agents their own test VMs, and none of yours

This guide sets up the server so that agents can create, install, run and delete their own VMs, while every VM you
already have stays hidden from them. The agents boot installer ISOs that you or they place in one directory, under
UEFI. You need a libvirt storage pool of type `dir` for the agents' disks, and sudo once to create the ISO directory.

The server marks each VM it creates as its own, in the VM's libvirt metadata. While any management operation is on,
it treats its own VMs as if they were listed in `allowed_vms` and `power_vms`. So you do not need to know the names
of the agents' VMs in advance, and `allowed_vms` can stay empty. No tool can add that mark to a VM the server did
not create.

## Prepare the pool and the ISO directory

1. Create a `dir` storage pool for the agents' disks, if you have none. This guide calls it `agent-vms`:

   ```sh
   virsh -c qemu:///system pool-define-as agent-vms dir --target /var/lib/libvirt/images/agent-vms
   virsh -c qemu:///system pool-build agent-vms
   virsh -c qemu:///system pool-start agent-vms
   virsh -c qemu:///system pool-autostart agent-vms
   ```

2. Create a directory for installer ISOs that you own, outside your home directory:

   ```sh
   sudo install -d -o "$USER" -g "$USER" -m 755 /var/lib/libvirt/images/vm-isos
   ```

   QEMU runs as another user under `qemu:///system`. It cannot open an ISO inside a home directory that gives other
   users no access, which is the usual case.

3. Copy each installer ISO into that directory. You and the agents can do so without sudo. For example, copy an ISO
   that is already a volume in a pool:

   ```sh
   virsh -c qemu:///system vol-download --pool agent-vms debian-13-netinst.iso \
     /var/lib/libvirt/images/vm-isos/debian-13-netinst.iso
   ```

### Who owns the files

The pool directory belongs to `root` with mode `0711`, the mode `pool-build` gives a `dir` pool, and each volume has
mode `0600`. `ls` therefore cannot list the pool or read its volumes. Use libvirt's commands instead:

```sh
virsh -c qemu:///system vol-list agent-vms                # names and paths
virsh -c qemu:///system vol-info --pool agent-vms NAME    # size and space used on disk
virsh -c qemu:///system vol-download --pool agent-vms NAME FILE
virsh -c qemu:///system vol-delete --pool agent-vms NAME
```

A new volume belongs to `root`. When a VM starts with it, libvirt gives it to `libvirt-qemu:kvm`, the user QEMU runs
as, which is why some volumes in the list belong to `root` and others to `libvirt-qemu`.

libvirt does the same to an ISO in the ISO directory: at the first start of a VM that uses it, the file goes to
`libvirt-qemu:kvm`, and it stays that way after the VM stops. You can still read it through the `kvm` group and delete
it, because you own the directory, but you cannot overwrite it. To replace an ISO, delete it and copy the new file
under the same name, once no VM holds the old file in a CD-ROM. A copy you make gets the mode your umask allows, such as
`640` with umask `0027`; QEMU reads it anyway, because libvirt changes its owner first.

## Write the config

1. Write `~/.config/virtmanager-mcp/config.toml`:

   ```toml
   uri = "qemu:///system"
   allowed_vms = []       # none of your existing VMs
   power_vms = []

   [manage]
   create = true
   delete = true                                  # deletes only disks in disk_pool, of VMs the server created
   reconfigure = true
   snapshots = true
   manage_vms = []
   disk_pool = "agent-vms"
   iso_dirs = ["/var/lib/libvirt/images/vm-isos"]
   networks = ["default"]
   max_vms = 2
   max_vcpus = 2
   max_memory_mib = 4096
   max_disk_gib = 40
   ```

   Choose `max_vms` and `max_memory_mib` so that their product fits in the memory your host can spare. When the
   server starts, it logs that only VMs it creates will be accessible.

2. If you want agents to wait for text on a VM's screen, such as an installer's prompt, add
   `VIRTMANAGER_OCR_DOWNLOAD=1` to the server's environment.
   [Install, update and remove the OCR models](ocr-models.md) describes this and the alternative.

3. Restart the server, or the client or proxy that starts it, so it reads the new file.

## Make disposable VMs from a template

A template gives agents a running VM with the guest agent in seconds, instead of an install from an ISO. Its disk is a
volume in the pool, so it lives at `/var/lib/libvirt/images/agent-vms/tpl-<name>.qcow2`, and you inspect it with the
`virsh vol-*` commands above.

1. Build the Debian 13 template volume:

   ```sh
   scripts/make-template.sh --pool agent-vms --volume tpl-debian13.qcow2
   ```

   [Building a VM template](../templates.md) describes the script.

2. Add one entry per boot mode to the config. All three share the volume, because the Debian cloud image boots under
   both BIOS and UEFI, and its boot loader is signed for Secure Boot:

   ```toml
   [manage.templates.debian13]
   volume = "tpl-debian13.qcow2"
   firmware = "bios"
   description = "Debian 13 with qemu-guest-agent"

   [manage.templates.debian13-uefi]
   volume = "tpl-debian13.qcow2"
   firmware = "uefi"
   description = "Debian 13, UEFI boot with Secure Boot"

   [manage.templates.debian13-uefi-nosb]
   volume = "tpl-debian13.qcow2"
   firmware = "uefi"
   secure_boot = false
   description = "Debian 13, UEFI boot without Secure Boot"
   ```

3. Add `max_lifetime_minutes` to `[manage]` if agents should keep VMs longer than a day; 1440 is the default.

4. Restart the server. Agents create VMs with `create_vm_from_template`, and `lifetime_minutes` deletes each VM when
   its time is up.

Never change, boot or delete a template volume while VMs use it. To update one, build a new volume under a new name and
point the config at it.

## Check it

Ask an agent to do each step, and check its answer:

1. `list_vms` shows none of your VMs.
2. `list_isos` shows the ISOs in `/var/lib/libvirt/images/vm-isos`.
3. `create_vm` with one of those ISOs, `firmware: "uefi"` and `secure_boot: false` creates a VM that is shut off. Keep
   Secure Boot on for an installer that is signed for it.
4. `start_vm` boots it, and `screenshot` shows the installer.
5. `delete_vm` removes the VM and its disk.
6. `list_templates` shows `debian13`, `debian13-uefi` and `debian13-uefi-nosb`, and `create_vm_from_template` with
   one of them and `lifetime_minutes: 60` reports the guest agent responding.

An installer without the QEMU guest agent can only be driven through the screen tools and, where it writes to one,
the serial console, which [Screen and mouse](../screen-and-input.md) covers. `exec` and the file tools work once the
running system has the guest agent.

## Know what agents can reach

Agents reach only the VMs this server created, the volumes in `disk_pool`, and the ISOs in `iso_dirs`. Every agent
that reaches this server shares those VMs: through a shared MCP proxy, any session can stop or delete a VM that
another session made. Guests on libvirt's `default` network get NAT addresses that the host can reach directly, so
`vm_addresses` gives an address for SSH from the host.
