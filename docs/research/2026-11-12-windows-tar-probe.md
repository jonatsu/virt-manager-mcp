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

## Second probe: deletion, links, loops and the archive's size

Later on 2026-11-12, on the same guest and server build.

`Remove-Item -Recurse -Force` deleted a directory that held a junction and a directory symlink to a directory outside
it, and a subdirectory marked `ReadOnly`. The outside directory and its file were still there afterwards.
`[IO.Directory]::Delete(<dir>, $true)` on the same tree failed with "The parameter is incorrect" and left it in place.

`[IO.Directory]::Move` onto an existing directory failed with "Cannot create a file when that file already exists",
and moved nothing into it.

A tree whose junction `back` points to the tree itself made `tar.exe -c` recurse 64 levels, to a path of 317
characters, then print `Cannot stat: Invalid argument` and `Error exit delayed from previous errors`. The archive held
127 entries and took 2 s.

An archive from the host with a file symlink `lf -> f.txt` and a directory symlink `ld -> d` unpacked with exit
status 0. Both became symlinks, but `ld` became a file symlink: listing `ld` as a directory returned `ld` itself, not
`g.txt`. `lf` read as the file's content.

While `tar.exe` wrote a 3 GiB archive, every `guest-file-open` of the archive failed with "The process cannot access
the file because it is being used by another process", until `tar.exe` exited. A PowerShell query of the archive's
length, `[IO.FileInfo]::new(<path>).Length` or `(Get-Item <path>).Length`, reported it growing during the same run:
380 MB, 1.2 GB, 2.0 GB and 2.9 GB at polls about 1.5 s apart, each poll a new `powershell.exe` started through the
agent. `C:` had 39 GiB free.

## Third probe: non-ASCII names into the guest

Later on 2026-11-12, on the same guest, with the server built from the run 2 work in progress. Two host archives held
`été.txt` and `日本語.txt`: one with the UTF-8 names in plain GNU headers, one from GNU tar `--format=pax` with PAX
`path` records. `tar.exe -x -f`, run through the agent, gave these results.

- From the GNU headers, both files unpacked with exit status 0, under names read in code page 437: `été.txt` became
  `├⌐t├⌐.txt` (U+251C U+2310 t U+251C U+2310) and `日本語.txt` became `µùÑµ£¼Φ¬₧.txt`.
- From the PAX records, `été.txt` unpacked under its own name (U+00E9 t U+00E9). For `日本語.txt` it printed
  `Archive entry has empty or unreadable filename ... skipping`, created nothing, and exited with status 1.

The same session pulled a tree with `été.txt`, `sub dir`, an empty directory, a file symlink and a junction: the
hashes on the host matched the guest's, the file symlink was skipped and named, and the junction arrived as a
directory holding the file it points to. A pull with `a.txt` held open with `FileShare` none failed with
`Couldn't open ./a.txt: Permission denied`, and a pull of the self-junction failed with `Cannot stat`. A second pull
to the same host directory was refused without `overwrite`. A pull of a 4 GiB file with `max_pull_bytes` at 64 MiB
failed with the message for an archive that passed the limit while `tar` built it: the size poll, not the final
measurement, stopped `tar.exe`, and the archive was deleted. Afterwards the temporary
directory held no `.vmm-*` entry and no `tar` process ran.
