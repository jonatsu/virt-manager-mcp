# Windows push final steps through handles (2026-11-23)

Probe for roadmap follow-up 23, on `t-win11` (Windows 11 Pro, build 26100, Windows PowerShell 5.1), through the
server's own `write_file` and `exec` tools, as `SYSTEM`. The questions: can a standard user who owns a push's parent
directory take over a `SYSTEM`-only directory in it, and do the handle-based final steps work under the guest agent?

## Method

A C# class compiled with `Add-Type -Path` wrapped these calls:

- `NtCreateFile` with `FILE_OPEN_REPARSE_POINT`, optionally relative to a directory handle (`RootDirectory`), and
  optionally with `OBJ_DONT_REPARSE`;
- `NtSetInformationFile` with `FileRenameInformation`, renaming the open entry into the directory open as
  `RootDirectory`, and with `FileDispositionInformationEx` (delete, POSIX semantics, ignore the read-only attribute);
- `GetFileInformationByHandleEx` for the attributes (`FileAttributeTagInfo`) and to list a directory through its
  handle (`FileFullDirectoryRestartInfo`, then `FileFullDirectoryInfo`);
- `SetSecurityInfo` with `DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION` and an empty access list;
- `LogonUserW` and `WindowsIdentity.RunImpersonated`, for the other user's steps.

A temporary standard user, `vmmprobe`, owned `C:\fu23` and had full control of it, inherited by `C:\fu23\u`, which
stood in for a folder such as `C:\Users\alice`. The staged tree sat in `C:\Windows\Temp` with the access list that
`push-mkdir.ps1` sets, and held a read-only file, a subdirectory, a file in it, and a junction to
`C:\fu23\elsewhere`, which held a victim file. The user, the folders and the staged tree were removed afterwards.

## Results

| Step | Result |
| -- | -- |
| `Add-Type -Path` of the class, under the guest agent | compiled and loaded in 0.1 to 0.15 s |
| The user renames a `SYSTEM`-only directory in its folder with `[IO.Directory]::Move` | access denied: .NET also asks for `SYNCHRONIZE`, which only the directory's own list could grant |
| The same, with a handle opened for `DELETE` alone and `FileRenameInformation` | renamed: the parent's delete-child right grants `DELETE` |
| `SYSTEM` holds the parent open without delete sharing; the user renames the parent | refused, sharing violation |
| The same; the user renames the parent's parent | refused, access denied |
| The same; the user creates an entry in the parent | allowed |
| Open by path with `OBJ_DONT_REPARSE` through a junction | refused, `STATUS_STOPPED_ON_SYMLINK` (`0xC000050B`) |
| Open by path with `OBJ_DONT_REPARSE` where the last element is a junction | opens the junction itself, which reports `FILE_ATTRIBUTE_REPARSE_POINT` |
| Rename the staged tree from `C:\Windows\Temp` into the held parent, by handle | moved, contents intact |
| Open a junction at the target name relative to the parent | opens the junction itself, reported as a reparse point |
| Rename a tree onto a name that exists, without replace | refused, `STATUS_OBJECT_NAME_COLLISION` (`0xC0000035`) |
| `SetSecurityInfo` on the moved root's handle | the root now inherits from the parent, entry for entry |
| The same on each entry, walked through handles relative to their directory, skipping reparse points | every entry matches a directory and file created in place; the junction's target list is unchanged |
| `SYSTEM` holds the moved root open without delete sharing; the user renames it | refused, sharing violation |
| Rename the moved root aside by handle | renamed |
| Delete the tree through handles, the read-only file and the junction included | deleted; the junction's target and the victim file are intact |

`[Fu23]::Delete` could not be called from PowerShell ("does not contain a method named 'Delete'"), although the class
has the method and the C# code in the same class called it. A helper that does each step inside C# avoids the
question.

## What this shows

The threat holds: a standard user who owns the parent can rename a `SYSTEM`-only entry in it, so any step that reaches
a push's entries by path can be redirected. Holding the parent open without delete sharing pins the parent and every
directory above it. Opening each entry relative to that handle, without following a reparse point, then acting on the
open handle, keeps every step on the entry the push made: the move into place, the inheritance, the move-aside and the
deletion. `SetSecurityInfo` on a handle computes the inherited entries from the parent itself, so the per-entry walk
gives the same result as a path-based propagation.

Not probed: a target on another volume than `C:\Windows\Temp`, where the rename fails and the push must copy; paths
longer than 260 characters through relative opens; and Windows PowerShell's Constrained Language Mode, which would
refuse `Add-Type`.
