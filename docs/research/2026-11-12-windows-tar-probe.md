# Directory transfers on Windows through `tar.exe`: probe

Date: 2026-11-12. Host: libvirt 12.0.0, QEMU 10.2.1, `qemu:///system`. Server: release build of `133f84a`. Guest:
`t-win11` (Windows 11 Pro, 4 GiB, no network interface), with the QEMU guest agent from `virtio-win` 0.1.302 running
as `SYSTEM`. Every step ran through the server's `exec`, `copy_to_vm` and `copy_from_vm` under
`C:\Windows\Temp\vmm-probe`, which the last step deleted.

## The guest's `tar`

`C:\Windows\System32\tar.exe --version` printed `bsdtar 3.8.8 - libarchive 3.8.8`.

## Packing a tree

The tree held two files, a file with the non-ASCII name `été.txt`, an empty directory, a directory named `sub dir`, a
directory symlink, a file symlink and a junction, all pointing inside the tree. `tar.exe -c -f <archive> -C <tree> .`
exited 0. GNU tar 1.35 on the host listed the archive as follows:

- every entry is owned by uid and gid 0, with mode `rw-rw-rw-` for files and `rwxrwxrwx` for directories;
- paths use forward slashes and start with `./`, and `été.txt` arrives as UTF-8;
- the two symlinks are symlink entries with absolute targets such as `//?/C:/Windows/Temp/vmm-probe/src/a.txt`;
- the junction is not a link entry: `tar.exe` followed it and stored `./junction/` as a directory holding a copy of
  `sub dir`'s file.

The archive carries PAX headers with `path`, `mtime` and `LIBARCHIVE.symlinktype` records, and no `size` record or
global header.

## Files in use

While the probe held `a.txt` open with no sharing, packing the tree printed `Couldn't open ./a.txt: Permission
denied` and `Error exit delayed from previous errors`, and exited 1.

Renaming a directory failed with "Access to the path ... is denied" while a file inside it was open, both with no
sharing and when opened for reading with read sharing.

## Unpacking

`tar.exe -x -o -f <archive> -C <dir>` accepted `-o` and exited 0. The unpacked files belonged to
`BUILTIN\Administrators`, and their only access entries, `Administrators` and `SYSTEM` with full control, were
inherited from `C:\Windows\Temp`. The symlinks came back as symlinks and the junction as a plain directory.

An archive from the host with a file of mode 444 and a directory of mode 555 unpacked with the file writable and the
directory marked `ReadOnly`. `Remove-Item -Recurse` without `-Force` then failed with "You do not have sufficient
access rights to perform this operation", and with `-Force` it succeeded.

An archive from the host that held one file named `CON` unpacked with exit status 0 and left the target directory
empty. `tar.exe` reported nothing.

## Arguments through the guest agent

`exec` with `program` set to `C:\Windows\System32\tar.exe` ran it directly with a list of arguments. An archive path
containing a space, and a `-C` directory containing a space and ending in a backslash, both reached `tar.exe` intact,
and it packed the expected entries.

## Access to `C:\Windows\Temp`

Its access entries give `SYSTEM`, `Administrators` and `CREATOR OWNER` full control, and `BUILTIN\Users` the right
to create files, append data and execute. The files that `tar.exe` unpacked there, as the agent's `SYSTEM` account,
carried only the `Administrators` and `SYSTEM` entries, so other users cannot read them.
