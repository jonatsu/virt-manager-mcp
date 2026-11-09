# Building a VM template

A template is a qcow2 volume that disposable VMs are made from: each VM gets a thin overlay on it and a small seed
disk that sets its identity. `scripts/make-template.sh` builds one from the official Debian 13 cloud image, with the
QEMU guest agent installed and the image's cloud-init state and machine ID cleaned.

## Build one

Run the script on the libvirt host, with access to `qemu:///system` (membership of the `libvirt` group):

```sh
scripts/make-template.sh --pool agent-vms --volume tpl-debian13.qcow2
```

It needs `virsh`, `virt-install`, `curl`, `sha512sum`, `xorriso`, `jq` and `base64`, and network access for the
download and for the guest's package install. It takes about a minute. If `virt-install` fails with a missing `gi`
module because a version manager's Python shadows the system one, run the script with
`VIRT_INSTALL="/usr/bin/python3 /usr/bin/virt-install"`.

The script:

1. downloads `debian-13-genericcloud-amd64.qcow2` into `~/.cache/virtmanager-mcp/images` and checks it against the
   Debian `SHA512SUMS`;
2. uploads it as the new volume, refusing a name that already exists;
3. boots it once as the temporary VM `t-tpl-build`, with 1 vCPU, 1 GiB and a seed that installs and enables
   `qemu-guest-agent`;
4. runs `cloud-init clean --logs --machine-id` in the guest, so each VM made from the template gets its own identity;
5. shuts the guest down and removes the temporary VM and its seed volume.

On any failure it removes the unfinished volume and the temporary VM, so a half-built template never stays in the
pool.

## Keep templates unchanged

Every VM made from a template reads the template volume as its backing file. Changing, booting or deleting the volume
while VMs use it corrupts all of them. To update a template, build a new volume under a new name, such as
`tpl-debian13-2.qcow2`, point the config at it, and delete the old volume once no VM uses it.
