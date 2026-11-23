# CachyOS installed from scratch through the server's tools (2026-11-23)

Recorded run for roadmap follow-up 26: an agent sets up a new VM from an installer ISO with no template, and leaves it
booted and usable. The ISO was `cachyos-desktop-linux-260809.iso` (3.0 GiB). Every step went through the server's
tools, driven by `tests/testbed/mcp_call.py` call lists, with screenshots read between steps. The server ran with the
test bed's `config-templates.toml`, changed in four places: `iso_dirs = ["/var/tmp/vmm-iso"]`, `max_vcpus = 4`,
`max_memory_mib = 4096`, `max_disk_gib = 30`, and the OCR models under `[display]`.

## Result

The install succeeded. The installed system booted from its own disk, showed its login screen 22 s after
`start_vm`, accepted the password typed with `type_text`, and opened KDE Plasma 6.8.3 on Wayland, kernel
`7.3.5-2-cachyos`. A terminal opened with `ctrl+alt+t` ran `echo marker-$(hostname)-$(uname -r | cut -d- -f1-2)`,
and OCR read `marker-t-cachy-7.3.5-2`. The VM and its disk were deleted afterwards.

## Steps

| Step | Tools | Observation |
| -- | -- | -- |
| ISO where QEMU and the server can read it | none (host copy) | The pool directory is root-only, so the server cannot list it as an `iso_dirs` entry; the home directory is closed to `libvirt-qemu`. The run copied the ISO to `/var/tmp/vmm-iso/`. |
| Create, UEFI | `list_isos`, `create_vm` (`firmware: "uefi"`), `start_vm`, `wait_for` | Rejected: `Access Denied -- rejected probably by Secure Boot`. libvirt picks the `secure-enrolled` firmware descriptor (`OVMF_CODE_4M.ms.fd`), and `create_vm` has no way to turn Secure Boot off. |
| Create, BIOS | `stop_vm` (`force`), `delete_vm`, `create_vm` (`firmware: "bios"`), `start_vm` | The CachyOS boot menu appeared within 2 s. |
| Live desktop | `send_keys` (`enter`), `wait_for` (`screen_text`), `screenshot` | The desktop took about 2.5 min; each `wait_for` stops at `max_wait_secs` (50 s), so the call was repeated. |
| Installer | `mouse` clicks, `wait_for`, `screenshot` | A notice about an older ISO; the installer then opened in Finnish, from its location lookup, and was switched with the language list and `type_text "American English"`. |
| Pages | `mouse`, `type_text`, `send_keys` (`ctrl+a`) | Location Helsinki, keyboard English (US), Limine, erase disk with btrfs, Plasma, default packages, user `tester`, host `t-cachy`. One `type_text` straight after `ctrl+a` kept only its first two characters; retyped after a 1 s pause, it held. |
| Install | `mouse` (Install, Install Now), `wait_for` | "All done" after about 9 min, packages downloaded through the `default` network. |
| First boot | `stop_vm`, `set_cdrom` (eject), `start_vm`, `type_text`, `wait_for` | A graceful `stop_vm` worked from the live desktop. |

## Gaps

- `create_vm` cannot make a UEFI VM without Secure Boot, so an unsigned installer needs `firmware: "bios"`.
- The tools ignore unknown arguments: a `screenshot` given `x`, `y`, `width` and `height` instead of `region` returned
  the whole screen without an error.
- An installer ISO must sit in a directory both the server and QEMU can read, which the test bed has none of.
