# Cloud-init and qcow2 overlays for disposable VMs

Date: 2026-11-09. Host: Ubuntu 26.04.1, libvirt 12.0.0, QEMU 10.2.1, virt-install 5.1.0, pool `agent-vms` on
`qemu:///system`. Guest: Debian 13 `genericcloud` amd64 from `cloud.debian.org/images/cloud/trixie/latest`, SHA-512
verified against that directory's `SHA512SUMS`. Cloud-init in the guest: 25.1.4.

This probe answers the first question of roadmap outcome 1, "a clean VM in seconds": can disposable VMs come from a
base image plus cloud-init, after the 2026-11-09 trial saw cloud-init never apply its user data?

## Method

1. Upload the image into the pool as a base volume, then create a 10 GiB qcow2 overlay on it with
   `virsh vol-create-as ... --backing-vol <base> --backing-vol-format qcow2`. The overlay allocated 200 KB.
2. Build a NoCloud seed ISO with `xorriso -as mkisofs -V cidata -J -r user-data meta-data`. `user-data` starts with
   `#cloud-config`. It sets the hostname, a root password, installs `qemu-guest-agent`, enables it and writes
   `/root/ci-marker`. `meta-data` holds `instance-id` and `local-hostname`.
3. Define the VM with `virt-install --import --print-xml`, with the overlay as a virtio disk, the seed as a CD-ROM and
   the guest-agent channel, then `virsh define`. Start it through `virtmanager-mcp` and read the serial console from
   boot.
4. Time power-on to "guest agent is responding" with `start_vm` and `wait_for_agent_secs`, between two `__time__`
   marks of the test-bed driver, for two variants:
   - **A:** a fresh overlay on the plain cloud image, where cloud-init installs the guest agent.
   - **B:** an overlay on the disk from A, where the agent is already installed, with a second seed that has a new
     `instance-id` and hostname and no packages.

## Results

- **Cloud-init works.** `cloud-init status --long` reported `status: done`, `detail: DataSourceNoCloud
  [seed=/dev/sr0]` and no errors. The hostname, the root password, the guest agent and the marker file were all in
  place, and the console showed `ci-probe login:`.
- **A plain cloud image takes about 44 s.** Variant A answered after 43.6 s, mostly cloud-init running `apt` to
  install the guest agent. `systemd-analyze` reported 41.6 s to the end of boot.
- **A prepared base takes about 8 s.** Variant B answered after 7.6 s, and its hostname changed to `ci-probe-b`, so
  cloud-init applied the new seed on a disk that had already booted once.
- **The overlay path works on this host.** `virt-install` referenced both volumes by file path, and AppArmor allowed
  the backing chain in the pool directory.

## Inferences

- The trial's failures came from its `virt-install --cloud-init user-data=...` attempts, not from the image, the host
  or cloud-init. The exact cause is unknown, because the trial's files were deleted. A missing `#cloud-config` header
  would explain "hostname stayed `localhost`", but that is untested.
- Outcome 1 can use a base image plus a NoCloud seed for each VM. Booting from a base that already has the guest agent
  is about five times faster than installing the agent at boot, so the design should prepare templates once, not
  install per VM.
- Variant B reused a disk that had been powered off hard and still held A's machine identity. A real template needs
  cleaning, such as `cloud-init clean` and a reset machine ID, before it is used as a base.
