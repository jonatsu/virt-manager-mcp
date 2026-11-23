# Windows push archive: access list and crafted entries (2026-11-23)

Probe for roadmap follow-up 25, on `t-win11` (Windows 11 Pro 26H2, `tar.exe` as shipped), through the server's own
`write_file` and `exec` tools. The question: an interactive user can rewrite a push's `.vmm-<id>.tar` beside a target
under `C:\Users\Public` before `tar.exe` unpacks it as `SYSTEM`. Can a crafted archive then write outside the staging
directory?

## The archive's access list

A file that the guest agent writes as `SYSTEM` in `C:\Users\Public` gets this list, all inherited:

```text
C:\Users\Public\vmm-acl25.tar BUILTIN\Administrators:(I)(F)
                              NT AUTHORITY\SYSTEM:(I)(F)
                              NT AUTHORITY\INTERACTIVE:(I)(M,DC)
                              NT AUTHORITY\SERVICE:(I)(M,DC)
                              NT AUTHORITY\BATCH:(I)(M,DC)
```

So an interactive, service or batch logon may modify the archive between the write and the unpack.

## Crafted entries

Each archive was written to `C:\Windows\Temp\vmm-case25.tar` and unpacked as the push does it,
`tar.exe -x -o -f <archive> -C C:\Windows\Temp\vmm-stage25`, into an empty staging directory. A victim file
`C:\Windows\Temp\vmm-victim25.txt` held "victim intact" before each case.

| Entries | `tar.exe` | Outside the staging directory | Inside it |
| -- | -- | -- | -- |
| symlink `sl` -> `C:/Windows/Temp/vmm-out25`, then `sl/x.txt` | `Can't create ...\sl\x.txt: No such file or directory` | nothing | `sl`, a symbolic link to `C:\Windows\Temp\vmm-out25` |
| symlink `rl` -> `../vmm-out25`, then `rl/y.txt` | the same refusal | nothing | `rl`, a symbolic link to `..\vmm-out25` |
| hard link `hl` -> `../vmm-victim25.txt`, then file `hl` | `Can't create ...\hl: Unknown error` | victim intact | `hl`, a plain file |
| hard link `ha` -> `C:/Windows/Temp/vmm-victim25.txt`, then file `ha` | drive letter removed; `Hard-link target 'Windows/Temp/vmm-victim25.txt' does not exist` | victim intact | `ha`, a plain file |
| file `../vmm-dotdot25.txt` | `Path contains '..'` | nothing | nothing |
| file `C:/Windows/Temp/vmm-absfile25.txt` | drive letter removed | nothing | `Windows\Temp\vmm-absfile25.txt` |
| symlink `s2` -> the victim, then file `s2` | no error | victim intact | `s2`, a plain file that replaced the link |

## What this shows

`tar.exe` kept every write inside the staging directory: it refuses to extract through a symbolic link, refuses hard
links whose target lies outside, refuses `..`, and strips a drive letter. A crafted archive can still change what the
push delivers, and it can leave symbolic links that point anywhere in the pushed tree, which a host tree never
contains because a push skips symbolic links. Not probed: whether `push-inherit.ps1`'s access-list propagation, or a
later reader of the tree, follows such a link.
