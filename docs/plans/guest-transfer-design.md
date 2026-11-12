# Moving code and results in and out: design

Status: accepted, after an architecture review and a plan-critic review.
Once built, the code wins and this design follows it.
Basis: roadmap outcome 3 in [`fork-roadmap.md`](fork-roadmap.md), with evidence in
[`2026-11-11-transfer-probe.md`](../research/2026-11-11-transfer-probe.md) and, for Windows,
[`2026-11-12-windows-tar-probe.md`](../research/2026-11-12-windows-tar-probe.md).

## Summary

An agent pushes a checkout into a guest, pulls files and directories of results back to the host, and finds the
guest's addresses, without passing file contents through its context. Everything travels through the QEMU guest
agent: directories as `tar` archives that the server builds and unpacks itself, so it can enforce its rules on every
entry. Pulled files land only in operator-configured directories, never executable and never outside them. A copy that
outlasts one tool call continues in the server, and `wait_for` waits for it. Four new tools cover it: `copy_from_vm`,
`copy_dir_to_vm`, `copy_dir_from_vm` and `vm_addresses`. The outcome lands in two runs: Linux guests first, then
directory transfers on Windows guests.

## Decisions

| Decision | Chosen | Why | Status |
| -- | -- | -- | -- |
| Scope | Pull one file, push a directory, pull a directory, list a VM's addresses | The host already routes to guests on libvirt's NAT network, so a guest's services are reachable at its address | resolved |
| Port forwarding | Not in this outcome | Needed only where the host cannot route to the guest: `qemu:///session` VMs on user-mode networking (outcome 6), isolated networks (outcome 5), or a remote HTTP client | resolved |
| Where pulls land | Only in a new `host_pull_dirs`, separate from the read-only `host_share_dirs`; no option to pull into the project directory | A guest that writes into a working tree can plant files the host later runs: `.git/hooks/*`, `.envrc`, a `Makefile`, CI configuration | resolved |
| Pulled modes | Files 644 and directories 755, always | A planted executable is the step before code running on the host; a deliberate `chmod` costs little | resolved |
| Directory transport | `tar` over the guest agent, built and unpacked by the server | Needs no network, SSH keys or host packages; 20 MiB/s in the probe. A virtiofs share needs `virtiofsd` on the host and, when writable, lets the guest write host files past every rule here; `rsync` needs `sshd` and keys; one file at a time costs several agent calls per file | resolved |
| Guests without `tar` | The directory tools fail with a clear message; no fallback | A per-file fallback would be slow and a second code path | resolved |
| Push selection | `.gitignore` followed and `.git/` left out by default, both switchable; extra `exclude` globs | Build output stays home; the reply counts ignored files so a missing generated file is noticed | resolved |
| Push target | A new directory, or `replace: true`, which swaps a complete new tree in; no merging | Merging leaves deleted files behind, so the guest drifts from the checkout | resolved |
| Incremental sync | Not now: every push sends the whole selected tree | A checkout takes seconds; incremental sync needs `rsync` in the guest or a manifest protocol of our own | deferred: repeated pushes of a tree of several hundred MiB take tens of seconds each in an edit-and-test loop. It can be added later without changing these tools |
| Links in pulled trees | Skipped and named in the reply | A link can point the host anywhere | deferred: copying links whose target stays inside the tree would keep library trees such as `libfoo.so -> libfoo.so.1` usable |
| Addresses | `vm_addresses`, taking for each interface libvirt's DHCP lease, then the guest agent's answer, then the host's ARP table | A guest cannot forge a lease but controls its agent's answer; where no lease exists, the reply marks the answer as the guest's own claim. Leases and ARP also work without a guest agent, which installers and appliances lack, as long as every NIC has a host-side device name | resolved; an `address` wait condition is deferred until agents need it |
| Long transfers | Every copy tool, including `copy_to_vm`, returns within `limits.max_wait_secs`; a longer copy continues in the server, and `wait_for` gains `transfer_done`, answered whatever the VM's state | Codex and some Claude Code paths cut calls off at 60 s, and 1 GiB takes about 50 s | resolved |
| Excludes on a pull | Applied by the server while unpacking, in gitignore syntax as for pushes; never passed to the guest's `tar` | Guest `tar` builds disagree: GNU tar matches unanchored, and busybox may have no `--exclude`. One host-side dialect gives the same result on every guest, at the cost of excluded files crossing the channel | resolved |
| Cancelling a transfer | No tool | A transfer ends by finishing, failing, or the server exiting | deferred: an agent cannot stop a large mistaken transfer and waits for it |
| Windows | In this outcome; directory transfers through Windows' own `tar.exe` form the second run | Windows is a named target; `copy_from_vm` and `vm_addresses` need nothing OS-specific | resolved |
| Names Windows cannot hold | A push to a Windows guest skips them and names them in the reply, as it does links that leave the tree | `tar.exe` unpacks such an entry without an error but loses it: a file named `CON` vanished in the probe. Refusing the whole push would block a checkout over one odd file | resolved |
| Delivery | Two runs, each landed and pushed before the next | As in outcome 2 | resolved |

## Behaviour

### Configuration

`host_pull_dirs` lists host directories that pulls may write into. It is empty by default, which hides
`copy_from_vm` and `copy_dir_from_vm`. At start the server resolves each entry to a canonical path, as it does for
`host_share_dirs`. It refuses to start when a pull directory contains, or lies inside, any of these:

- a share directory, the project directory from `share_project_dir` included, so a guest cannot write where a push
  later reads;
- a directory in `manage.iso_dirs`, so a guest cannot plant an install image;
- the target directory of any directory-type storage pool on the libvirt connection, the default pool included, so a
  guest cannot replace a VM's disk.

At start the server also checks that each pull directory's filesystem supports `renameat2` with `RENAME_NOREPLACE`
and `RENAME_EXCHANGE`, by trying both inside its staging directory, and refuses to start with a clear message when it
does not, as on some NFS and FUSE filesystems.

Three limits join `[limits]`:

- `max_pull_bytes`, default 1 GiB, caps the file bytes one pull writes on the host, counted as they are written, and
  the archive a directory pull may build in the guest.
- `max_pull_entries`, default 100,000, caps the entries one directory pull may hold.
- `max_transfers`, default 4, caps the transfers one server process runs at once. A copy beyond it is refused.

Read-only mode hides every copy tool, the existing `copy_to_vm` included, and keeps `vm_addresses` on its lease and
ARP sources.

### Paths

`host_path` and `guest_path` always name the exact destination or source, never a directory to copy into. A copy
never turns a file into a directory or the reverse: `overwrite` and `replace` replace only an entry of the same type,
and refuse otherwise. The three new tools create missing parent directories on the destination side. `copy_to_vm`
keeps its rule that the guest's parent directory must exist.

### Pulling one file: `copy_from_vm`

Arguments: `vm`, `guest_path`, `host_path`, and `overwrite` (default false). The server reads the guest file in
chunks through `guest-file-read` and writes it under the host rules below. It works on any guest with a guest agent.

### Host rules for every pull

Every host file operation starts from a handle on the pull directory and moves one component at a time with
`O_NOFOLLOW`, so a symlink anywhere below the pull directory cannot redirect a write outside it. Missing parent
directories inside the pull directory are created with mode 755. The destination's last component must not be a
symlink.

A pull builds its copy in the pull directory's staging directory, `.vmm-staging/`, under a random name created with
`O_EXCL`. A pull whose destination has a `.vmm-staging` component anywhere is refused, so no pull writes into the
staging directory of a nested pull directory either. Pull directories may nest: a destination that is or holds a
pull directory is refused, and a destination inside nested pull directories belongs to the innermost, so it stages
there and on its filesystem. The server sets every mode with `fchmod`, so
the umask cannot change it: files 644 and directories 755, whatever the guest says. Modification times are the
pull's own. Only a complete copy moves into place:

- without `overwrite`, by `renameat2` with `RENAME_NOREPLACE`, so a destination that appeared meanwhile is never
  replaced and the pull fails instead;
- with `overwrite`, by `renameat2` with `RENAME_EXCHANGE`, which swaps the new copy and the old destination in one
  step, then deletes the old one from the staging directory. No moment exists without a destination, and no stale
  file from an earlier pull survives.

A pull that fails or passes a cap deletes its staged copy. A pull holds an `flock` on a lock file beside its staged
copy while it runs. At start, a server deletes staged copies whose lock no process holds, so it cleans up after a
server that died without touching another process's live pull, and it needs to read only the staging directories.

Within one server process, a pull is refused while another running transfer writes the same destination, a parent of
it, or a path inside it. Between processes, `RENAME_NOREPLACE` and `RENAME_EXCHANGE` keep each destination whole: the
last complete pull wins.

### Pushing a directory: `copy_dir_to_vm`

Arguments: `vm`, `host_path`, `guest_path`, `replace` (default false), `respect_gitignore` (default true),
`include_git` (default false) and `exclude` (glob patterns).

`host_path` must be a directory inside `host_share_dirs`, as for `copy_to_vm`. Selection and reading are two steps:

1. The `ignore` crate's `WalkBuilder` selects the paths, with `follow_links(false)` and `git_global(false)`, because the
   host user's global excludes differ between machines. With `respect_gitignore`, it applies the tree's own
   `.gitignore` files, `.git/info/exclude`, and the `.gitignore` files of parent directories inside the same share, so
   pushing a subdirectory of a repository keeps the repository's rules. `exclude` patterns use the same gitignore
   syntax.
2. The server opens each selected path from a handle on the share directory, one component at a time with
   `O_NOFOLLOW`, so a directory swapped for a symlink after selection cannot lead outside the share.

The archive holds:

- regular files with their host mode, minus setuid and setgid;
- directories;
- symlinks with a relative target that stays inside the pushed tree when resolved through real directories only. A
  target that passes through another symlink is treated as outside, so a pair such as `b -> .` and `a -> b/../x`
  cannot escape.

Other symlinks, device files, FIFOs and sockets are skipped and named in the reply. Every entry is owned by uid and
gid 0 in the archive and unpacked with `tar -x -o`, so files belong to the user the guest agent runs as. GNU tar and
busybox `tar` read `-o` on extraction as "do not restore the archive's owner"; run 2 confirms the same for Windows
`tar.exe`.

In the guest, the archive is written beside the target as `.vmm-<random>.tar`, on the same filesystem, and unpacked
into `.vmm-<random>.d`. Without `replace`, an existing target is refused before anything is sent. With `replace`, the
old tree is moved aside, the new one moved in, and the old one deleted. A failed push deletes its temporary files and
leaves the old tree in place. `replace` refuses `/` and any directory directly below it, and on Windows a drive root
and its direct children, as a guard against mistakes.

The reply gives the files and bytes sent, the entries skipped with their reason, and the number of files that
`.gitignore` kept out.

### Pulling a directory: `copy_dir_from_vm`

Arguments: `vm`, `guest_path`, `host_path`, `overwrite` (default false) and `exclude` (glob patterns).

`guest_path` must be a directory. The guest runs `tar -c -C <guest_path> .` into `.vmm-<random>.tar` in its temporary
directory: `/var/tmp` on Linux, or `/tmp` where `/var/tmp` is missing. While `tar` runs, the server polls the
archive's size and stops `tar` once it passes `max_pull_bytes`, which fails the pull and deletes the archive. A `tar`
that finishes between two polls has its archive measured before anything is sent. A finished archive within the cap
streams out, is deleted in the guest, and is unpacked by the server under the host rules.

The server applies `exclude` itself while unpacking, in the same gitignore syntax as pushes, and never passes it to the
guest's `tar`. Guest `tar` builds disagree on `--exclude`: GNU tar matches unanchored and lets wildcards cross `/`,
and Ubuntu's busybox 1.37.0 has neither `--exclude` nor `-X`. Excluded files therefore cross the channel and count
towards the caps.

GNU tar exits with status 1 when a file changed while it was read, as a growing log does. A status 1 whose message
says so lets the pull succeed with `tar`'s warning in the reply. Any other non-zero status fails the pull.

The archive is untrusted. The unpacker reads it with the `tar` crate in raw mode and merges long-name and PAX headers
itself, so it can check every entry:

- an absolute path or a `..` component fails the whole pull, which writes nothing and names the entry;
- a path seen twice, or a file and a directory at the same path, fails the pull;
- a sparse entry fails the pull, whether GNU-style or marked by PAX `GNU.sparse.*` keys. A GNU sparse entry declares a
  size it does not carry, and the `tar` crate would pad it with zeros on the host; the crate ignores the PAX keys, so
  such a file would arrive corrupted;
- a PAX `size` record or a PAX global header fails the pull. In raw mode the `tar` crate 0.4.46 reads past each entry
  by the size in its ustar header and ignores a PAX `size` record, so honouring one would lose the unpacker's place
  in the stream. GNU tar and busybox write neither, and bsdtar writes `size` only for files over 8 GiB;
- a PAX or long-name header larger than 64 KiB fails the pull, so metadata cannot fill memory;
- symlinks, hard links, device files, FIFOs and sockets are skipped and named in the reply;
- a path longer than 4096 bytes or deeper than 256 levels fails the pull, so a deep tree stays deletable with one
  handle per level;
- passing `max_pull_bytes` or `max_pull_entries` fails the pull. The directories an entry's path implies count as
  entries too, so one long name cannot create thousands of directories.

### Long transfers

Each copy tool starts its copy as a transfer inside the server and waits for it up to `limits.max_wait_secs`. A
transfer that finishes in time returns its result as the call's reply. Otherwise the reply holds the transfer's ID,
the bytes moved so far, and the state `running`, and the copy continues. A cancelled tool call does not cancel its
transfer. Transfer IDs are random, so a client sharing an HTTP-transport server cannot guess another's. Temporary file
names are random too and never derived from the ID, because several server processes may serve one guest at once.

`wait_for` gains a `transfer_done` condition with a transfer ID. It must be the call's only condition. `server.rs`
answers it from the transfer table before it looks up the VM, so a transfer cut off by a stopped, expired or deleted
VM still reports its failure. The table stores each transfer's VM name and UUID, and a `transfer_done` that names
another VM is refused. The condition holds once the transfer has finished or failed, and its result is the reply the
copy tool would have given. A process keeps the results of its 64 most recent finished transfers.

Transfer IDs belong to one server process. The transfers end when their process exits, and an ID the process does not
know is reported as unknown, with that reason. A transfer runs on a blocking thread and sends its chunks as separate
guest-agent commands, so other calls to the same guest interleave with it. A transfer that panics finishes with an
error and frees its slot.

### Addresses: `vm_addresses`

Argument: `vm`. For each interface, the reply gives its name where known, its MAC address, its IPv4 and IPv6
addresses with prefix lengths, and the source of the answer: `lease`, `agent` or `arp`. For each interface, matched by
MAC address, the server takes libvirt's DHCP lease first, then the guest agent's answer, then the host's ARP table, and
uses the first source that has an address for it. Loopback and IPv6 link-local addresses are left out. The tool does
not wait: an empty list means no source knows an address yet.

A lease comes from libvirt's own DHCP server, so the guest cannot forge it. Leases exist only for interfaces on
libvirt-managed networks, though, and a guest with a static address has none. The agent's answer and the ARP table are
then the guest's own claims, and a compromised guest can name another VM's or a host address. The tool description
says so, and the source field shows which kind each answer is.

The `virt` crate 0.4.3 reads two fields of libvirt's answer without a NULL check, and either crash would take the
server down. The server therefore asks the guest agent through its own agent channel, with
`guest-network-get-interfaces`, instead of libvirt's agent source, which may return an interface without a MAC
address. It asks for leases and ARP entries only when every NIC in the VM's live XML has a `<target dev>`, because
libvirt returns a NULL device name for a NIC without one, such as a passed-through device. For such a VM the lease
and ARP sources report that they were skipped, and only the guest agent can answer.

### Windows guests

Run 1 makes `copy_from_vm`, `copy_to_vm` and `vm_addresses` work on Windows guests, and tests them on `t-win11`. Run 2
adds the directory tools, through `C:\Windows\System32\tar.exe`, which Windows 10 1803 and later ship. On `t-win11` it
is bsdtar 3.8.8, which accepts `-o` and writes forward slashes
([probe note](../research/2026-11-12-windows-tar-probe.md)). The server runs `tar.exe` directly through the guest agent,
with each path a separate argument, and runs PowerShell only for the steps around it, with every path quoted as a
literal.

A Windows guest path is a drive path such as `C:\work\app`, with `\` or `/` between components. The server refuses a
relative path, a drive-relative path such as `C:work`, a UNC or device path such as `\\server\share` or `\\?\C:\`,
`..`, a `:` after the drive, and a component that ends in a dot or a space, because Windows drops those and a guard
could then be passed with `C:\Windows.`. With `replace`, it refuses a drive root and the directories directly below
it, compared without regard to case.

A push to a Windows guest writes every file with mode 644 and every directory with 755 into the archive, so the host's
modes are dropped and `tar.exe` marks no directory read-only. It skips, and names in the reply, every entry that
Windows cannot hold under its name: a reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`, `COM0` to
`COM9`, `LPT0` to `LPT9`, and `COM` or `LPT` with `¹`, `²` or `³`), also with an extension, as in `CON.txt`; a name
with a control character or one of `< > : " | ? * \`; a name that ends in a dot or a space; a name that is not valid
UTF-8, since Windows names are UTF-16; a name shaped like a generated 8.3 short name, such as `LONGFI~1.TXT`, which
Windows may already have given to a longer name in the same push; and an entry whose name differs from an earlier one
in the same directory only in case. A skipped directory takes its subtree with it. `tar.exe` would otherwise unpack
such an entry onto a device, onto another file, into another directory, or under another name, without an error.
The push also skips every symlink and names it: a tar archive does not say whether a link points to a file or a
directory, and `tar.exe` turns a directory link into a broken file link.

`tar.exe` reads a name in a plain tar header in the guest's code page, so the push gives each entry whose path is not
ASCII a PAX `path` record, which `tar.exe` reads as UTF-8. It still cannot unpack a name that the guest's code page
cannot hold, such as a Japanese name on a guest set to a Western code page: it skips the entry, exits with status 1,
and the push fails with its message.

A pull from a Windows guest packs into `C:\Windows\Temp`, where only `SYSTEM` and `Administrators` can read the files
that the agent creates. Any non-zero exit status of `tar.exe` fails the pull: Windows has no counterpart of GNU tar's
changed-file warning, and its status 1 also covers a file that another program holds open.

## Invariants

- A pull writes only inside `host_pull_dirs`, never through a symlink, and never leaves a partial file at its
  destination.
- No pulled file is executable, setuid or setgid.
- A pull never writes more file bytes than `max_pull_bytes`, sparse entries included.
- A pull directory never overlaps a share directory, the project directory, an ISO directory or a storage pool.
- A push sends only files inside `host_share_dirs` and never carries a symlink that resolves outside the pushed tree.
- A failed push or `replace` leaves the guest's old tree in place.
- A copy never replaces a file with a directory or the reverse.
- No copy call blocks longer than `limits.max_wait_secs`.
- Read-only mode advertises no copy tool.

Each needs a test that shows the refusal still happens, as `CLAUDE.md` asks for guardrails.

## Code changes

- `src/config.rs`: `host_pull_dirs`, the overlap checks, and the three limits. The pool and `renameat2` checks run
  once the server has connected to libvirt.
- `src/pulldir.rs`: the pull directories' staging directories, their locks, the start-up cleanup and the
  `renameat2` probe.
- `src/pull.rs`: the host writer, with its handle-based walk, staged copies and the `renameat2` commit, for files and
  directories.
- `src/push.rs`: the push walk, the archive builder and the guest steps of `copy_dir_to_vm`.
- `src/dirpull.rs`: the guest steps of `copy_dir_from_vm` and the untrusted unpacker. It reads entries with the `tar`
  crate in raw mode but writes every file itself, never through the crate's `unpack`.
- `src/transfer.rs`: the transfer table, and the `transfer_done` condition, answered in `src/server.rs` before the VM
  lookup.
- `src/addresses.rs`: `vm_addresses`, its sources and their merge.
- `src/agent.rs`: streaming reads to a writer, mirroring `write_from`, and a file's size.
- `src/hypervisor.rs`: the storage pools' target paths for the overlap check.
- `src/server.rs`: the four new tools, `copy_to_vm` moved onto transfers, and the read-only rules.

The run adds three crates: `tar`, `ignore`, and `rustix` for `openat`, `fchmod` and `renameat_with`. Each passes
`cargo deny check advisories`.

## Test bed

`config.toml` gains a pull directory under the run's scratch directory. New cases cover a file pull and its
refusals, a push with `.gitignore`, `exclude`, `replace` and a skipped outside symlink, a push of a repository's
subdirectory, a pull with a skipped symlink and FIFO, the caps, a pull of a tree larger than the cap, a transfer that
outlasts `max_wait_secs` (set low in the case's config), a VM stopped during a transfer, and `vm_addresses` with and
without a guest agent. Run 1 adds a file pull and `vm_addresses` on `t-win11`, and run 2 the directory cases.

Unit tests feed the unpacker crafted archives, because a real guest `tar` does not produce them: absolute paths, `..`,
repeated paths, a file and directory at one path, GNU and PAX sparse entries, oversized headers, links and devices.
Unit tests also cover the config overlap refusals, the staging cleanup with a held and a free lock, and a push walk
that meets a symlinked directory.

## Checks

- A pushed checkout's files match the host's by checksum, and ignored files are absent.
- A pulled tree matches the guest's by checksum, with every mode 644 or 755.
- Every refusal in the invariants shows in a case or a unit test.
- A large push finishes, through `transfer_done` when it outlasts the call. The test bed pushes 128 MiB, because its
  work directory lives on the host's tmpfs.
- Pulls run at a measured rate, recorded in the probe note, since the probe measured only pushes.

## Risks

1. A server that exits mid-transfer leaves `.vmm-*` temporary files in the guest. Their names start with `.vmm-`, so
   an agent can find and delete them.
2. A guest with little temporary space cannot take a push of a tree larger than half its free space, because the
   archive and the unpacked tree exist together. A pull's archive can fill the guest's temporary directory up to
   `max_pull_bytes` before the cap stops it. On guests whose `/tmp` lives in RAM, such as OpenWrt, that costs memory.
3. A push to a Windows guest skips the names Windows cannot hold, so a checkout with such a file arrives without it.
   The reply names each skipped entry. Windows also refuses to rename a directory while a file in it is open, so a
   `replace` of a tree in use fails and leaves the old tree in place.
4. A guest's `tar -c` follows the guest's view of the tree, which the agent controls. Windows `tar.exe` follows
   junctions, so a junction to a directory outside the tree, or to one of its own parents, brings that content into
   the pull until a cap stops it. The host rules do not depend on it, because the server unpacks every pull itself and
   the caps bound it.
5. Concurrent guest-agent commands from several server processes rely on libvirt serializing them per VM. The
   background-transfer case checks it under one process only.
6. An overwrite deletes the swapped-out tree with `std::fs::remove_dir_all`, which does not stop at mount boundaries.
   A filesystem that an operator mounts inside a pulled tree would lose its contents, so the README asks operators to
   mount nothing inside a pull directory.

## Open questions

None block planning. The deferred items are in the decisions table.
