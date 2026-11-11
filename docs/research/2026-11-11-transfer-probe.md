# Moving files through the guest agent: probe

Date: 2026-11-11. Host: libvirt 12.0.0, QEMU 10.2.1, `qemu:///system`. Server: release build of `5aca5b6`. Guest:
`trial-deb13` (Debian 13, 2 vCPUs, 1 GiB) on the `default` NAT network.

## Throughput

`copy_to_vm` copied a 64 MiB file of random bytes to `/tmp` in the guest in 3.2 s, twice, measured with the test-bed
client's `__time__` markers. That is about 20 MiB/s. The guest's `sha256sum` matched the host's
(`fe4099c42df818ea1d092fc44197610eeb7bfbbb1120fa4cde250cb8d9f45f6c`). The server writes in 64 KiB chunks through
`guest-file-write`.

At that rate, a pull of 1 GiB takes about 50 s, which is the default `limits.max_wait_secs`.

Pulls run at the same rate. Later on 2026-11-11, with the server's `copy_dir_from_vm` built, a pull of a 128 MiB
directory from `trial-deb13` took 5.5 s, including the guest's `tar` packing it: about 23 MiB/s.

## Guest tools

`trial-deb13` has `/usr/bin/tar`, but no `rsync`, `python3` or `sshd`. Its `/tmp` is a 987 MiB tmpfs.

## Reaching the guest

The host reaches the guest's address directly: `ping 192.168.122.141` answered in 0.5 ms through `virbr0`
(192.168.122.1/24). `virsh domifaddr` reported the address from libvirt's DHCP leases, without the guest agent, and
from the agent with `--source agent`. The Rust `virt` crate 0.4.3 exposes the same call as
`Domain::interface_addresses`.

## Shared folders

The host has no `virtiofsd` (`/usr/libexec/virtiofsd` is absent), so a virtiofs shared folder would need a host
package installed first.

## A side finding

`exec` given `command` together with `args` ignores `args` without an error: `sha256sum` with `args` ran with no
argument and hashed empty input.
