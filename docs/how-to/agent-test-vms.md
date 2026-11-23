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

3. Copy each installer ISO into that directory. You and the agents can do so without sudo.

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

## Check it

Ask an agent to do each step, and check its answer:

1. `list_vms` shows none of your VMs.
2. `list_isos` shows the ISOs in `/var/lib/libvirt/images/vm-isos`.
3. `create_vm` with one of those ISOs, `firmware: "uefi"` and `secure_boot: false` creates a VM that is shut off. Keep
   Secure Boot on for an installer that is signed for it.
4. `start_vm` boots it, and `screenshot` shows the installer.
5. `delete_vm` removes the VM and its disk.

An installer without the QEMU guest agent can only be driven through the screen tools and, where it writes to one,
the serial console, which [Screen and mouse](../screen-and-input.md) covers. `exec` and the file tools work once the
running system has the guest agent.

## Know what agents can reach

Agents reach only the VMs this server created, the volumes in `disk_pool`, and the ISOs in `iso_dirs`. Every agent
that reaches this server shares those VMs: through a shared MCP proxy, any session can stop or delete a VM that
another session made. Guests on libvirt's `default` network get NAT addresses that the host can reach directly, so
`vm_addresses` gives an address for SSH from the host.
