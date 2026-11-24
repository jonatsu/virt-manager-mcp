# Windows guest with networking: address, SSH, WSL 2 and revert (2026-11-24)

Probe for roadmap follow-up 35. The question: does a Windows 11 guest that the server's own `create_vm` installs on
libvirt's `default` network show its address in `vm_addresses`, accept SSH from the host with a key, run WSL 2 nested
inside it, and revert from a snapshot of the shut-off VM with its TPM still present? All four held.

## Setup

The host runs libvirt 12.0.0 and QEMU 10.2.1 on kernel 7.0.0-44, on an Intel Core Ultra 7 255HX. Nested virtualisation
is on: `/sys/module/kvm_intel/parameters/nested` reads `Y`. The server was the release build of commit `ddd41df`
(0.4.0), run through `tests/testbed/mcp_call.py` with a test config. The config lists `/var/lib/libvirt/images/vm-isos`
in `iso_dirs` and `default` in `networks`, and allows 8192 MiB, 4 vCPUs and 64 GiB.

`tests/testbed/win11/build-net-iso.sh` merged `tests/testbed/win11/autounattend-net.xml` into
`virtio-win-0.1.302.iso` (SHA-256 `303f7ae4...867949d`, as pinned in the script) in 2.6 s. The answer file adapts
`autounattend.xml`: Setup loads `viostor` and `NetKVM` from drive letters D to G, the first logon installs the virtio
drivers and the QEMU guest agent, and a second command installs the OpenSSH server capability and starts `sshd`. The
call, with both ISOs in `vm-isos`, was:

```json
{"tool": "create_vm", "args": {"name": "t-win11net", "os": "windows", "vcpus": 4, "memory_mib": 8192, "disk_gib": 64,
  "firmware": "uefi", "secure_boot": true, "tpm": true, "network": "default",
  "iso": "/var/lib/libvirt/images/vm-isos/win11-26h2.iso",
  "driver_iso": "/var/lib/libvirt/images/vm-isos/virtio-win-0.1.302-net.iso"}}
```

`start_vm` followed, and 40 presses of the space bar in 20 s through `virsh send-key` answered the DVD's boot prompt, as
`build.sh` does. The guest is Windows 11 Pro 10.0.26300.9457.

## Install

Setup found the virtio disk and the NIC with no manual step. The guest agent answered 9.2 minutes after `start_vm`
(552 s, polled every 10 s); the first install took 604 s. The OpenSSH server follows the agent, because
`Add-WindowsCapability` downloads from Windows Update: its files appeared 17 minutes after the start in the first
install, and the SSH port was open between 15 and 22 minutes after the start in the second.

The first install showed a fault in the answer file. The capability's firewall rule `OpenSSH-Server-In-TCP` applies to
the Private profile only, and the guest's NIC sits on an unidentified network, which Windows classes as Public, so the
host's connections to port 22 timed out although `sshd` listened. The answer file now opens the rule to every profile.
That install was deleted with `delete_vm`, and the results below come from a new install with the fixed answer file.
No install failed.

## Results

(a) `vm_addresses` on the running VM:

```text
{"interfaces":[{"addresses":[{"address":"192.168.122.232","prefix":24}],"mac":"52:54:00:f9:74:7a","name":"Ethernet","source":"lease"}],"unavailable":{}}
```

(b) A case wrote a fresh `ed25519` public key to `C:\ProgramData\ssh\administrators_authorized_keys` with `write_file`
and reset its access list with `icacls`. Then the host ran
`ssh -i <key> -o BatchMode=yes -o IdentitiesOnly=yes tester@192.168.122.232 whoami`, which printed
`t-win11net\tester` in 0.45 s.

(c) Nesting is on, so the check could run. `wsl --install --no-distribution` did not work: on the fresh guest, every
`wsl.exe` call over SSH, including `--install` and `--update`, printed "The Windows Subsystem for Linux is not
installed" in under 0.2 s and did nothing. This was tried only over SSH, not in a console session. The fallback ran:

1. `Enable-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform,Microsoft-Windows-Subsystem-Linux -All -NoRestart`, then a restart.
2. `winget install --id Microsoft.WSL --source winget`, 87 s. winget checked the installer's hash; the package is the
   MSI from `github.com/microsoft/WSL` release 3.0.5.
3. `wsl --install -d Debian --no-launch --web-download`, 30 s.

Then `wsl --version` showed WSL 3.0.5.0 with kernel 6.18.58.1 and `Default Version: 2`, `wsl --list --verbose` showed
`* Debian Running 2`, and `wsl -d Debian -u root -- uname -a` printed
`Linux t-win11net 6.18.58.1-microsoft-standard-WSL2 ... x86_64 GNU/Linux` on Debian 13 (trixie).

(d) With the ISOs ejected through `set_cdrom` and the VM shut off, `snapshot_create` made `clean-wsl`: "created snapshot
'clean-wsl' of 't-win11net' (shutoff)". The guest then started, `C:\vmm-revert-marker.txt` and `C:\vmm-revert-dir`
were created, and the guest was shut down. `snapshot_revert` printed "reverted 't-win11net' to snapshot 'clean-wsl'; the
VM is now shutoff". After the next start, both paths were absent, `Get-Tpm` reported `TpmPresent: True` and
`TpmReady: True`, `Confirm-SecureBootUEFI` returned `True`, and WSL still ran. `snapshot_delete` then removed the
snapshot.

## Test-bed case

`win-network.json` checks (a) and the SSH server, and `run-all.sh` checks (b) from the host. The first version started
the VM in the case and failed: the guest agent answers early in a boot, and the 10 s agent timeout then expired on a
`Get-CimInstance` call, and a `write_file` failed on the agent's synchronisation. The unit now starts the VM, waits for
its SSH port to open, and only then runs the case. `run-all.sh win-network` passed in 35 s with the VM shut off
beforehand. It shuts the VM off again at the end.

No server bug turned up. The three surprises were in the guest: the firewall profile, the early agent, and the
`wsl.exe` stub.
