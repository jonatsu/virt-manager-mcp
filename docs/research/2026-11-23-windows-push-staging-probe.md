# Windows push staging: Temp, a second volume and cloud-file folders (2026-11-23)

Probe for unit 1 of the follow-up 23 plan, on `t-win11` (Windows 11 Pro, build 26100), through the server's own
`write_file` and `exec` tools, as `SYSTEM`. It extends
[`2026-11-23-windows-handle-steps-probe.md`](2026-11-23-windows-handle-steps-probe.md), whose C# calls it reuses, with
three questions the accepted design left open: can a standard user take over a push's staging entry in
`%SystemRoot%\Temp`; what happens when the target is on another volume; and does a cloud-file placeholder folder on
the way to a parent open and stay put.

## Setup

A temporary standard user, `vmmprobe`, owned `C:\fu23` and `X:\fu23`. `X:` was a 1 GiB NTFS volume on a `t-*` disk in
`agent-vms`, attached to `t-win11` for the probe and deleted afterwards. The staged tree in `C:\Windows\Temp` had the
access list that `push-mkdir.ps1` sets, a read-only file, and a file at a path of 345 characters. The user, the
folders, the partition and the disk were removed afterwards.

## `%SystemRoot%\Temp`

`C:\Windows\Temp` grants `BUILTIN\Users` only `(CI)(S,WD,AD,X)`: create folders and files, and traverse. Against a
`SYSTEM`-only directory that `SYSTEM` made there, the user's open for `DELETE` alone, the open a rename or a delete
needs, failed with `STATUS_ACCESS_DENIED` (`0xC0000022`). The directory stayed in place.

## A second volume

| Step | Result |
| -- | -- |
| Volume serial numbers through `FileIdInfo` | differ: `C:` and `X:` each report their own |
| Rename of the staged tree into a held directory on `X:` | refused, `STATUS_NOT_SAME_DEVICE` (`0xC00000D4`) |
| Copy through handles: list each directory by handle, create each entry relative to its new parent with `FILE_CREATE`, copy file data through the handles | 8 entries copied, both files with their content, the 345-character path included |

The copy did not carry the read-only attribute or the last-write times, because the probe's copy did not set them. The
helper must set both through each new file's handle after its data.

## Cloud-file folders

A sync root registered with `CfRegisterSyncRoot` (full hydration, full population) at `C:\fu23\u\cloud`, with two
directories below it converted to in-sync placeholders with `CfConvertToPlaceholder`:

| Component | Attributes | Reparse tag |
| -- | -- | -- |
| `cloud`, the sync root | `0x401430` | `0x9000101A` |
| `sub`, a placeholder | `0x410` | `0x9000601A` |
| `inner`, a placeholder | `0x410` | `0x9000601A` |

Both tags are cloud-file tags, `IO_REPARSE_TAG_CLOUD_*`, and neither has the name-surrogate bit (`0x20000000`) that
junctions, symbolic links and mount points carry. A walk that opens each component relative to the one before, with
`FILE_OPEN_REPARSE_POINT`, opened all three as directories. An open of the whole path with `OBJ_DONT_REPARSE` also
succeeded, because the cloud filter, not the I/O manager, handles these tags. A staged tree renamed into `inner`
through the held handle arrived with its file, and `SetSecurityInfo` on its handle gave it the inherited list of the
user's folder. The sync root was unregistered afterwards.

## What this shows

The staging premise holds: a standard user cannot rename or delete the push's entries in `%SystemRoot%\Temp`. A
target on another volume fails the rename with a distinct status, and the volume serial numbers tell the case apart
before anything moves; a handle-based copy reaches long paths. A parent under cloud-file placeholders opens through the
component walk, and refusing only reparse points with the name-surrogate bit keeps such folders usable.

Not probed: a folder that OneDrive itself manages, with a signed-in account and dehydrated placeholders, whose
enumeration would ask the provider for data.
