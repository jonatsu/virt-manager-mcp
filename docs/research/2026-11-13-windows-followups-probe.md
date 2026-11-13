# Junctions and non-ASCII paths in Windows directory transfers: probe

Date: 2026-11-13. Host: libvirt 12.0.0, QEMU 10.2.1, `qemu:///system`. Server: release build of `d41fa1c`. Guest:
`t-win11` (Windows 11 Pro, system locale `en-US`, ANSI code page 1252), with the QEMU guest agent from `virtio-win`
0.1.302 running as `SYSTEM`, and `C:\Windows\System32\tar.exe` reporting `bsdtar 3.8.8 - libarchive 3.8.8`. Every step
ran through the server's `exec`, `copy_dir_to_vm` and `copy_dir_from_vm`. The last step deleted everything the probe
created in the guest. It answers roadmap follow-ups 19 and 20.

## Extracting through a junction (follow-up 19)

The probe built an archive in the guest with `tar.exe -c` holding `sub/` and `sub/x.txt`. It created the directory
`C:\Users\Public\vmm-p19\d`, and in it a junction `sub` pointing at `C:\vmm-p19-target`. Then it ran the push's own
unpack command, `tar.exe -x -o -f <archive> -C C:\Users\Public\vmm-p19\d`.

`tar.exe` exited 0 and wrote `x.txt` into `C:\vmm-p19-target`. The junction stayed in place. An archive holding only
the file entry `sub/x.txt`, with no directory entry, did the same. bsdtar's refusal to extract through symbolic links
does not cover junctions.

The access lists of the two parent directories, as `icacls` printed them:

```text
C:\Users\Public BUILTIN\Administrators:(OI)(CI)(F)
                CREATOR OWNER:(OI)(CI)(IO)(F)
                NT AUTHORITY\SYSTEM:(OI)(CI)(F)
                NT AUTHORITY\INTERACTIVE:(OI)(CI)(IO)(M,DC)
                NT AUTHORITY\INTERACTIVE:(RX,WD,AD)
                NT AUTHORITY\SERVICE:(OI)(CI)(IO)(M,DC)
                NT AUTHORITY\SERVICE:(RX,WD,AD)
                NT AUTHORITY\BATCH:(OI)(CI)(IO)(M,DC)
                NT AUTHORITY\BATCH:(RX,WD,AD)

C:\Windows\Temp BUILTIN\Users:(CI)(S,WD,AD,X)
                BUILTIN\Administrators:(F)
                BUILTIN\Administrators:(OI)(CI)(IO)(F)
                NT AUTHORITY\SYSTEM:(F)
                NT AUTHORITY\SYSTEM:(OI)(CI)(IO)(F)
                CREATOR OWNER:(OI)(CI)(IO)(F)
```

In both, a directory that `SYSTEM` creates inherits an entry that lets ordinary users add files and subdirectories to
it: `(OI)(CI)(IO)(M,DC)` for interactive users under `C:\Users\Public`, and `(CI)(S,WD,AD,X)` for `Users` under
`C:\Windows\Temp`. So a logged-on user can create a junction inside a push's staging directory under either parent
before `tar.exe` runs.

## Non-ASCII guest paths (follow-up 20)

`copy_dir_to_vm` pushed a two-file tree to `C:\Windows\Temp\vmm-日本` and to `C:\Windows\Temp\vmm-été`. Both pushes
succeeded, and `Get-ChildItem` listed every file under its expected name. A push passes no guest path to `tar.exe`:
it unpacks into an ASCII staging directory, and the PowerShell steps receive the path inside `-EncodedCommand`.

`copy_dir_from_vm` from either directory failed with:

```text
guest agent 'guest-exec' failed: internal error: failed to parse JSON: invalid utf-8 string
```

The pull passes the guest path to `tar.exe` as a `-C` argument. The same error came back from `exec` with `program`
for every argument list holding a non-ASCII character, `cmd.exe /c echo été` among them, while `cmd.exe /c echo abc`
ran. libvirt reports that the agent's reply is not valid UTF-8, so the guest agent fails such a `guest-exec` and
answers with text in another encoding. `exec` with `command` was unaffected, because the server sends a PowerShell
command line encoded.

Run from PowerShell instead, with the directory set through `Set-Location -LiteralPath`:

- `tar.exe -c -f <archive> .` in `C:\Windows\Temp\vmm-été` exited 0 and listed every entry;
- after the files `été.txt` and `日本.txt` were added to that directory, the same command exited 255 and printed
  nothing to the captured error stream;
- in `C:\Windows\Temp\vmm-日本`, which also held `日本.txt`, it exited 255, also when given the directory's short name
  `C:\Windows\Temp\VMM-~1` (8.3 names are enabled on `C:`).

The 2026-11-12 probe packed a tree holding `été.txt` without error, so `日本.txt` is the likely cause. `tar.exe` packs a
tree whose names all fit the code page, and fails without a message when a name in the tree does not. Whether a
directory outside the code page packs when every name inside it fits was not tested: both such directories held
`日本.txt`.

## A staging directory that ordinary users cannot write

A second run checked a fix for follow-up 19 under `C:\Windows\Temp`. `[IO.Directory]::CreateDirectory` with a
`DirectorySecurity` that is protected from inheritance and grants full control, inherited by subdirectories and files,
only to `SYSTEM` (`S-1-5-18`) and `Administrators` (`S-1-5-32-544`) created a directory whose `icacls` listing held
exactly those two entries. A file created inside it inherited the same two. Called again on the existing directory,
`CreateDirectory` returned without an error and left the directory as it was, so it cannot detect a directory that
already exists.

The probe then created a junction inside the directory pointing at `C:\vmm-p4-target`, which held a file. It turned
inheritance back on for the directory with `SetAccessRuleProtection($false, $false)`, removed the two explicit
entries, and applied the result with `Set-Acl`. The directory's listing then matched what `C:\Windows\Temp` passes to a
new subdirectory, `Users:(I)(CI)(S,WD,AD,X)` included. The target file's security descriptor was unchanged, so the
propagation did not pass through the junction. The file inside the directory listed `SYSTEM` and `Administrators`
before and after, which is also what `C:\Windows\Temp` passes to files, so this run cannot tell whether inheritance
reached it.

## The protected staging directory as built

A later run on `t-win11` exercised `push-mkdir.ps1` and `push-inherit.ps1` from the fix for follow-up 19, through
`exec` and `copy_dir_to_vm`, from a release build with the two scripts in place.

`push-mkdir.ps1` created `C:\Windows\Temp\vmm-m1` with this list, owned by `BUILTIN\Administrators`, and accepted the
directory again when run a second time:

```text
C:\Windows\Temp\vmm-m1 NT AUTHORITY\SYSTEM:(OI)(CI)(F)
                       BUILTIN\Administrators:(OI)(CI)(F)
```

It exited 2 for a directory made beforehand with `New-Item` ("its access list inherits from its parent"), for a
directory given the same two rules with `icacls /inheritance:r /grant:r` and then the owner `BUILTIN\Users` ("its
owner is S-1-5-32-545"), and for a junction at the path ("it is a junction or link").

A junction to `C:\vmm-jt` placed inside a directory that `push-mkdir.ps1` made, then `push-inherit.ps1` on that
directory, left the SDDL of `C:\vmm-jt` and of the file in it unchanged.

A push of a two-file tree to `C:\Users\Public\vmm-acl` gave its root, its subdirectory and both files the same
`icacls` lists as a reference tree made there with `New-Item` and `Set-Content`. Each file listed:

```text
BUILTIN\Administrators:(I)(F)
NT AUTHORITY\SYSTEM:(I)(F)
NT AUTHORITY\INTERACTIVE:(I)(M,DC)
NT AUTHORITY\SERVICE:(I)(M,DC)
NT AUTHORITY\BATCH:(I)(M,DC)
```

Running `push-inherit.ps1` a second time on that tree changed none of the four lists. A push of `share/win-deep` to
`C:\Users\Public\vmm-deep` gave its file, whose full path is 292 characters long, the inherited `INTERACTIVE`,
`SERVICE` and `BATCH` rules as well. On a pushed tree of 5,000 small files in 50 directories, protecting the root again
with `SetAccessControl` took 0.26 s, and `push-inherit.ps1` then took 0.27 s.

## Packing from PowerShell

A third run on `t-win11` tried `pull-pack.ps1` from the fix for follow-up 20 through `exec`, before the server used
it. The script enters the directory with `Set-Location -LiteralPath`, runs `tar.exe -c -f <archive> .` with
`$ErrorActionPreference = 'Continue'`, and writes each stderr line of `tar.exe` to `[Console]::Error`. In that
PowerShell, `[Console]::OutputEncoding` was code page 65001 and `[Console]::InputEncoding` code page 437; the system's
ANSI code page was 1252.

| Directory | Contents | Exit | Stderr |
| -- | -- | -- | -- |
| `C:\Windows\Temp\vmm-été` | `été.txt`, `a.txt` | 0 | none |
| `C:\Windows\Temp\vmm-jp` | `日本.txt`, `a.txt` | 255 | none |
| `C:\Windows\Temp\vmm-日本` | `a.txt` | 0 | none |
| `C:\Windows\Temp\vmm-loop` | `sub\x.txt`, and the junction `back` to the directory itself | 1 | `tar.exe: ./back/back/…/back: Cannot stat: Invalid argument` and `tar.exe: Error exit delayed from previous errors`, as plain text |
| `C:\Windows\Temp\vmm-missing` | does not exist | 30 | `Cannot find path 'C:\Windows\Temp\vmm-missing' because it does not exist.` |

So a directory whose own name lies outside the code page packs when every name inside it fits, and `tar.exe` gives no
message for a name that does not fit, even with its stderr relayed. With `C:\Windows\System32\no-tar.exe` as the
program, the script exited 31, as planned, but the error reached stderr as CLIXML, because PowerShell's own error for a
missing command does not pass through the relay. The script now checks that the program exists first.

With the server then using the script, `copy_dir_from_vm` pulled `vmm-été` and `vmm-日本`, and the pulled files'
SHA-256 sums matched those that `Get-FileHash` printed in the guest. The pull of `vmm-jp` failed with the code-page
message, the pull of `vmm-loop` still failed with `Cannot stat`, and no `.vmm-*` entry was left in
`C:\Windows\Temp`.
