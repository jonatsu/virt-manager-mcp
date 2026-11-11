# Booting a Debian 13 cloud image with a `fatfs` seed

Date: 2026-11-10. Host: as in [`2026-11-09-cloud-init-overlay-probe.md`](2026-11-09-cloud-init-overlay-probe.md),
after a host restart. Guest: Debian 13 `genericcloud` amd64, build `20261103-2651` from
`cloud.debian.org/images/cloud/trixie/latest`, SHA-512 verified; kernel `6.12.115+deb13.1-cloud-amd64`.

This note answers unit 1 of [`disposable-vms-implementation-plan.md`](../plans/disposable-vms-implementation-plan.md):
does cloud-init accept a seed image built in memory with the `fatfs` crate?

## Method

1. Build a 2 MiB seed with `seed::build` in `src/seed.rs`, through its ignored test `write_seed_for_boot_test`. The
   user data installs and enables `qemu-guest-agent`; `meta-data` sets the hostname `t-seed`. `blkid` reports
   `LABEL="CIDATA" TYPE="vfat" VERSION="FAT12"`.
2. Upload the cloud image as `t-seed-base.qcow2`, add a 10 GiB qcow2 overlay `t-seed.qcow2` on it, and upload the
   seed as the raw volume `t-seed-seed.img`, all in `agent-vms`.
3. Define `t-seed` with `virt-install --import --print-xml` (q35, BIOS, 1 GiB) with the overlay as `vda` and the seed
   as a read-only virtio disk `vdb`, and start it.

## Results

- cloud-init accepts the `fatfs` seed. The guest agent answered 55 s after power-on. `cloud-init status --long`
  reported `status: done`, `detail: DataSourceNoCloud [seed=/dev/vdb]` and `errors: []`. `hostname` printed `t-seed`,
  and `lsblk` showed `vdb` with label `CIDATA`, type `vfat`, read-only.
- The fresh disk reset in a loop on its first boots with 2 vCPUs. The serial console showed GRUB's
  `Booting 'Debian GNU/Linux'` repeating, with no kernel output and no DHCP lease, while a vCPU stayed busy. It
  happened with and without the seed, with 1 and 2 GiB, and with no graphics and with SPICE graphics. With 1 vCPU the
  same disk booted to `localhost login:`. After that first successful boot, the same disk booted with 2 vCPUs under
  both `host-passthrough` and `host-model` in 20 s. `trial-deb13`, installed from the netinst image, boots with 2
  vCPUs.
- A seed disk given as a pool volume failed on a later start. The disk used `<disk type='volume'>` and
  `<source pool='agent-vms' volume='t-seed-seed.img'/>`. The first start worked. A start after a redefinition failed
  with `Could not open '…/t-seed-seed.img': Permission denied`, read-only or not. The file was owned by `libvirt-qemu`
  with mode 0600. The kernel log showed no AppArmor denial, though it reported suppressed audit callbacks. With
  `<disk type='file'>` and the file path, two starts in a row worked.

## Inferences

- The design's choice of a `fatfs` seed holds; the ISO fallback is not needed.
- The reset loop's cause is unknown. It is tied to the cloud image's first boots, not to the CPU mode, memory or the
  seed. The template script should do its single first boot with 1 vCPU; VMs made from a template boot a disk that has
  already booted once.
- Template VMs should reference their disks by file path, as `create_vm` already does, not as pool volumes.
