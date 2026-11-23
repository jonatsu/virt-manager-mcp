# File transfer

## Host directories

Copies read and write only in host directories that the config file lists.

- Pushes: list the host directories the AI may copy files from in `host_share_dirs`. To allow copying only from the
  project you are working in, set `share_project_dir = true` instead: Claude Code starts the server in the session's
  project directory, and that directory becomes the only share. It is ignored when that directory is `/` or your home
  directory. In HTTP mode there is one shared server, so the "project" is wherever you started it.
- Pulls: list the host directories that pulls from a guest may write into in `host_pull_dirs`. Keep them apart from
  anything else: the server refuses to start when a pull directory equals, contains or lies inside a share
  directory, the shared project directory, an ISO directory, or the target of any libvirt storage pool, because a
  guest controls what a pull writes. The check compares resolved paths, so it cannot see a bind mount that shows
  the same directory at two places; do not bind-mount a pull directory elsewhere, and mount nothing inside one,
  because an overwrite deletes the replaced tree with whatever is mounted in it. Pull directories may nest: a pull
  never writes to a pull directory itself or to a path that holds one, and a path inside nested pull directories
  belongs to the innermost. Each pull directory gets a private `.vmm-staging/` directory, where pulls build their
  copies; its filesystem must support `renameat2` with `RENAME_NOREPLACE` and `RENAME_EXCHANGE`, as local
  filesystems such as ext4, xfs, btrfs and tmpfs do.

## Moving files

Four tools move files between the host and a guest without passing the data through the conversation. Pushes read
only from `host_share_dirs` (or the shared project directory), and pulls write only into `host_pull_dirs`, set up as
[Host directories](#host-directories) describes. The server hides each tool while its directories are not configured,
and read-only mode hides all four.

- `copy_to_vm` streams one file in 64 KiB guest-agent requests. The host path is resolved (symlinks, `..`) before the
  share check, so a link pointing outside a share is refused. On a Linux guest a new file gets mode 644 and an existing
  one keeps its mode; run `chmod +x` through `exec` for scripts. The parent directory must exist.
- `copy_from_vm` copies one guest file into a pull directory. The file arrives with mode 644 and appears only once
  complete. Missing parent directories are created with mode 755, symlinks on the host side are never followed, and
  an existing file is replaced only with `overwrite: true`. A pull never replaces a directory with a file.
- `copy_dir_to_vm` pushes a host directory into a guest, for example a checkout. It follows the
  `.gitignore` rules in the tree and in its parent directories inside the share (`respect_gitignore`, on by default),
  leaves `.git` out unless `include_git` is set, and takes more `exclude` patterns. A symlink goes along only when it
  stays inside the tree; other links, FIFOs, sockets and devices are skipped and named in the reply. The guest
  unpacks the archive beside the target as `.vmm-<random>.d`. Files keep their host modes without setuid and setgid
  bits, and belong to the user the guest agent runs as. An existing target is replaced as a whole only with
  `replace: true`, never `/` or a directory directly below it, and a failed push leaves the old target in place.
- `copy_dir_from_vm` pulls a directory from a guest. A Linux guest packs it in `/var/tmp`, or `/tmp` when that is
  missing, and the server deletes the archive afterwards. The server unpacks the archive itself, because
  the guest controls it: files arrive with mode 644 and directories with 755, and links, devices and FIFOs are
  skipped and named. An archive with absolute or `..` paths, paths over 4096 bytes or 256 levels, repeated paths or
  sparse files fails the pull, which then writes nothing. `exclude` takes gitignore patterns, applied on the host. An
  existing directory is replaced as a whole only with `overwrite: true`. A file that changed while `tar` read it
  gives a warning, not a failure.

`limits.max_pull_bytes` (default 1 GiB) caps one pull: the archive while the guest builds it, and the files written.
`limits.max_pull_entries` (default 100,000) caps the entries of a directory pull, counting the directories their
paths imply. The server stops a guest `tar` whose archive grows past the byte cap.

Every copy waits up to `limits.max_wait_secs`. A longer copy goes on in the background and replies with a transfer
ID, and `wait_for` with a lone `transfer_done` condition reports how it ended. At most `limits.max_transfers`
(default 4) copies run at once, and the server keeps the newest 64 results. A background copy lives in the server
process, so it ends when the server does. A push cut short, for example by the VM stopping, can leave `.vmm-*`
entries beside its target in the guest; delete them through `exec`.

`vm_addresses` tells an agent where to reach a guest for other ways of moving files, such as `scp`. For each
interface it takes libvirt's DHCP lease, then the guest agent's answer, then the host's ARP table, and names the
source. Only a lease comes from outside the guest. The agent's answer and the ARP table are the guest's own claims,
so a compromised guest can name another VM's address or a host address. When a VM has a network interface without a
host-side device name, such as a passed-through device, the server skips the lease and ARP sources, and only the
guest agent can answer.

### Directory transfers on Windows

A Linux guest needs `tar`. A Windows guest uses its own `%SystemRoot%\System32\tar.exe`, which Windows 10 1803 and
later ship; the server runs it directly through the guest agent and runs PowerShell only for the steps around it.
The guest agent fails a command whose arguments are not ASCII, so a pull from such a path, such as
`C:\Windows\Temp\vmm-été`, runs `tar.exe` from PowerShell instead.

- A Windows `guest_path` is a drive path such as `C:\work\app`, with `\` or `/` between components. The
  server refuses relative and drive-relative paths (`C:work`), UNC and device paths (`\\server\share`, `\\?\C:\`),
  `..`, control characters and `< > " | ? *`, and a component that ends in a dot or a space. A push never replaces a
  drive root, and `replace` also refuses the directories directly below one, compared without regard to case.
- `tar.exe` unpacks some names without an error but onto a device, another file or
  another name, so a push to Windows skips and names them: reserved device names such as `CON` or `nul.txt`, names
  with a control character or one of `< > : " | ? * \`, names ending in a dot or a space, names that are not valid
  UTF-8, names shaped like 8.3 short names such as `LONGFI~1.TXT`, and a name that differs from an earlier one in its
  directory only in case. It also skips every symlink, because the archive cannot say whether a link points to a
  file or a directory. A name outside the guest's code page, such as a Japanese name on a Western guest, fails the
  push in `tar.exe`.
- Every pushed file and directory arrives writable; host modes do not carry over. While `tar.exe` unpacks, only
  `SYSTEM` and `Administrators` can write the push's staging directory, so no other guest user can plant a junction
  in it. The pushed tree then gets the access list its parent passes on. A guest user who may delete or rename
  entries in the target's parent, such as its owner, can still redirect a push.
- Windows refuses to rename a directory while a file in it is open, so a `replace` of a tree in
  use fails after one retry and leaves the old tree in place.
- The guest packs in `%SystemRoot%\Temp`, usually `C:\Windows\Temp`, where only `SYSTEM` and `Administrators` can
  read the archive. `tar.exe` follows junctions, so a junction in the pulled tree brings its target's content along
  until a cap stops it; a junction to the tree itself fails the pull. Any non-zero exit of `tar.exe`, including a
  file another program holds open, fails the pull, as does a name in the tree outside the guest's code page, for
  which `tar.exe` prints no reason and the pull's error names that cause. The server polls the archive's size
  through PowerShell, so an archive can pass `max_pull_bytes` by about a second of writing before `tar.exe` stops;
  the host still writes nothing past the cap.
- `tar.exe` creates paths over 260 characters, which programs without long-path support, such as
  Explorer, cannot open.
