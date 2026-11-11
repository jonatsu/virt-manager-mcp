# Moving code and results in and out: design

Status: accepted, after an architecture review; a plan-critic review follows.
Once built, the code wins and this design follows it.
Basis: roadmap outcome 3 in [`fork-roadmap.md`](fork-roadmap.md), with evidence in
[`2026-11-11-transfer-probe.md`](../research/2026-11-11-transfer-probe.md).

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
| Addresses | `vm_addresses`, taking for each interface libvirt's DHCP lease, then the guest agent's answer, then the host's ARP table | A guest cannot forge a lease but controls its agent's answer, so a compromised guest cannot steer the agent to another VM's or a host address; leases and ARP also work without a guest agent, which installers and appliances lack | resolved; an `address` wait condition is deferred until agents need it |
| Long transfers | Every copy tool, including `copy_to_vm`, returns within `limits.max_wait_secs`; a longer copy continues in the server, and `wait_for` gains `transfer_done`, answered whatever the VM's state | Codex and some Claude Code paths cut calls off at 60 s, and 1 GiB takes about 50 s | resolved |
| Excludes on a pull | Applied by the server while unpacking, and also by the guest's `tar` when it is GNU tar or bsdtar | Busybox `tar` may have no `--exclude`; filtering on the host keeps the result the same on every guest | resolved |
| Cancelling a transfer | No tool | A transfer ends by finishing, failing, or the server exiting | deferred: an agent cannot stop a large mistaken transfer and waits for it |
| Windows | In this outcome; directory transfers through Windows' own `tar.exe` form the second run | Windows is a named target; `copy_from_vm` and `vm_addresses` need nothing OS-specific | resolved |
| Delivery | Two runs, each landed and pushed before the next | As in outcome 2 | resolved |

## Behaviour

### Configuration

`host_pull_dirs` lists host directories that pulls may write into. It is empty by default, which hides
`copy_from_vm` and `copy_dir_from_vm`. At start the server resolves each entry to a canonical path, as it does for
`host_share_dirs`. It refuses to start when a pull directory contains, or lies inside, any of these:

- a share directory, the project directory from `share_project_dir` included, so a guest cannot write where a push
  later reads;
- a directory in `manage.iso_dirs`, so a guest cannot plant an install image;
- the target directory of `manage.disk_pool`, read from libvirt, so a guest cannot replace another VM's disk.

Three limits join `[limits]`:

- `max_pull_bytes`, default 1 GiB, caps the file bytes one pull writes on the host, counted as they are written.
- `max_pull_entries`, default 100,000, caps the entries one directory pull may hold.
- `max_transfers`, default 4, caps the transfers one server process runs at once. A copy beyond it is refused.

Read-only mode hides every copy tool, the existing `copy_to_vm` included, and keeps `vm_addresses` on its lease and
ARP sources.

### Pulling one file: `copy_from_vm`

Arguments: `vm`, `guest_path`, `host_path`, and `overwrite` (default false). The server reads the guest file in
chunks through `guest-file-read` and writes it under the host rules below. It works on any guest with a guest agent.

### Host rules for every pull

Every host file operation starts from a handle on the pull directory and moves one component at a time with
`O_NOFOLLOW`, so a symlink anywhere below the pull directory cannot redirect a write outside it. Missing parent
directories inside the pull directory are created with mode 755. The destination's last component must not be a
symlink.

A pull writes into a temporary entry beside the destination, named `.vmm-partial-<random>` and created with `O_EXCL`.
It sets every mode with `fchmod`, so the umask cannot change it: files 644 and directories 755, whatever the guest
says. Only a complete copy moves into place:

- without `overwrite`, by `renameat2` with `RENAME_NOREPLACE`, so a destination that appeared meanwhile is never
  replaced and the pull fails instead;
- with `overwrite`, by `renameat2` with `RENAME_EXCHANGE`, which swaps the new copy and the old destination in one
  step, then deletes the old one under its temporary name. No moment exists without a destination, and no stale file
  from an earlier pull survives.

A pull that fails or passes a cap deletes its temporary entry. Temporary entries left by a server that died are
removed by the next server start once they are more than a day old, so a live transfer of another process is never
touched.

Within one server process, a pull is refused while another running transfer writes the same destination, a parent of
it, or a path inside it. Between processes, `RENAME_NOREPLACE` and `RENAME_EXCHANGE` keep each destination whole: the
last complete pull wins.

### Pushing a directory: `copy_dir_to_vm`

Arguments: `vm`, `host_path`, `guest_path`, `replace` (default false), `respect_gitignore` (default true),
`include_git` (default false) and `exclude` (glob patterns).

`host_path` must be a directory inside `host_share_dirs`, as for `copy_to_vm`. The server walks it from directory
handles with `O_NOFOLLOW`, so a directory swapped for a symlink during the walk cannot lead outside the share. The
`ignore` crate supplies the rules: with `respect_gitignore`, the tree's own `.gitignore` files and
`.git/info/exclude` apply; the host user's global excludes do not, because they differ between machines. The archive
holds:

- regular files with their host mode, minus setuid and setgid;
- directories;
- symlinks with a relative target that stays inside the pushed tree when resolved through real directories only. A
  target that passes through another symlink is treated as outside, so a pair such as `b -> .` and `a -> b/../x`
  cannot escape.

Other symlinks, device files, FIFOs and sockets are skipped and named in the reply. Every entry is owned by uid and
gid 0 in the archive and unpacked with `tar -x -o`, so files belong to the user the guest agent runs as. GNU tar,
busybox and bsdtar all read `-o` on extraction as "do not restore the archive's owner".

In the guest, the archive is written beside the target as `.vmm-<random>.tar`, on the same filesystem, and unpacked
into `.vmm-<random>.d`. Without `replace`, an existing target is refused before anything is sent. With `replace`, the
old tree is moved aside, the new one moved in, and the old one deleted. A failed push deletes its temporary files and
leaves the old tree in place. `replace` refuses `/` and any directory directly below it, and on Windows a drive root
and its direct children, as a guard against mistakes.

The reply gives the files and bytes sent, the entries skipped with their reason, and the number of files that
`.gitignore` kept out.

### Pulling a directory: `copy_dir_from_vm`

Arguments: `vm`, `guest_path`, `host_path`, `overwrite` (default false) and `exclude` (glob patterns).

The guest runs `tar -c` over `guest_path` into `.vmm-<random>.tar` in its temporary directory, `/var/tmp` on Linux. The
server streams the archive out, deletes it in the guest, and unpacks it itself under the host rules. It applies
`exclude` while unpacking. When the guest's `tar` is GNU tar or bsdtar, the server also passes each pattern as
`--exclude`, placed before the file arguments as GNU tar 1.29 and later require, so excluded files do not cross the
channel. Busybox `tar` may lack `--exclude`: Ubuntu's 1.37.0 build has neither `--exclude` nor `-X`. There the
excluded files cross and count towards the caps.

GNU tar exits with status 1 when a file changed while it was read, as a growing log does. The pull then succeeds,
and its reply carries `tar`'s warning. Any other non-zero status fails the pull.

The archive is untrusted, and the unpacker checks every entry:

- an absolute path or a `..` component fails the whole pull, which writes nothing and names the entry;
- a path seen twice, or a file and a directory at the same path, fails the pull;
- a GNU sparse entry fails the pull, because it declares a size it does not carry, and the `tar` crate would pad it
  with zeros on the host;
- a PAX or long-name header larger than 64 KiB fails the pull, so metadata cannot fill memory;
- symlinks, hard links, device files, FIFOs and sockets are skipped and named in the reply;
- passing `max_pull_bytes` or `max_pull_entries` fails the pull.

### Long transfers

Each copy tool starts its copy as a transfer inside the server and waits for it up to `limits.max_wait_secs`. A
transfer that finishes in time returns its result as the call's reply. Otherwise the reply holds the transfer's ID,
such as `x3`, the bytes moved so far, and the state `running`, and the copy continues. A cancelled tool call does not
cancel its transfer. Temporary file names are random and never derived from the ID, because several server processes
may serve one guest at once.

`wait_for` gains a `transfer_done` condition with a transfer ID. The server answers it from its own transfer table,
whatever state the VM is in, so a transfer cut off by a stopped or expired VM still reports its failure. The condition
names the VM too, and a transfer of another VM is refused. It holds once the transfer has finished or failed, and its
result is the reply the copy tool would have given. A process keeps the results of its 64 most recent finished
transfers.

Transfer IDs belong to one server process. The transfers end when their process exits, and an ID the process does not
know is reported as unknown, with that reason. A transfer runs on a blocking thread and sends its chunks as separate
guest-agent commands, so other calls to the same guest interleave with it.

### Addresses: `vm_addresses`

Argument: `vm`. For each interface, the reply gives its name where known, its MAC address, its IPv4 and IPv6
addresses with prefix lengths, and the source of the answer: `lease`, `agent` or `arp`. For each interface, matched by
MAC address, the server takes libvirt's DHCP lease first, then the guest agent's answer, then the host's ARP table, and
uses the first source that has an address for it. A lease comes from libvirt's own DHCP server, so a guest cannot
forge it, while the guest controls the agent's answer; the agent still fills in interfaces with static addresses.
Loopback and IPv6 link-local addresses are left out. The tool does not wait: an empty list means no source knows an
address yet.

### Windows guests (run 2)

`copy_from_vm`, `copy_to_vm` and `vm_addresses` work on Windows from run 1. Run 2 adds the directory tools, through
`C:\Windows\System32\tar.exe`, which Windows 10 1803 and later ship. Archives use forward slashes, pushed modes are
dropped, and the pull archive goes to `C:\Windows\Temp`, since the guest agent runs as `SYSTEM`. Run 2 confirms that
this bsdtar accepts `-o` and `--exclude` as the Linux builds do.

## Invariants

- A pull writes only inside `host_pull_dirs`, never through a symlink, and never leaves a partial file at its
  destination.
- No pulled file is executable, setuid or setgid.
- A pull never writes more file bytes than `max_pull_bytes`, sparse entries included.
- A pull directory never overlaps a share directory, the project directory, an ISO directory or the disk pool.
- A push reads only inside `host_share_dirs` and never carries a symlink that resolves outside the pushed tree.
- A failed push or `replace` leaves the guest's old tree in place.
- No copy call blocks longer than `limits.max_wait_secs`.
- Read-only mode advertises no copy tool.

Each needs a test that shows the refusal still happens, as `CLAUDE.md` asks for guardrails.

## Code changes

- `src/config.rs`: `host_pull_dirs`, the overlap checks, and the three limits. The disk-pool check runs once the
  server has connected to libvirt.
- A new module for the host side: the handle-based walks for pulls and pushes, temporary names and `renameat2`, the
  archive builder for pushes, and the untrusted unpacker for pulls. It uses the `tar` and `ignore` crates
  (provisional: `rustix` or `cap-std` for the handle-based calls, chosen in the plan). The unpacker reads entries with
  the `tar` crate but writes every file itself, never through the crate's `unpack`.
- `src/agent.rs`: streaming reads to a writer, mirroring `write_from`.
- A transfer table beside the jobs, with the `transfer_done` condition in `src/wait.rs`, answered before the VM's
  state is checked.
- `src/server.rs`: the four tools, `copy_to_vm` moved onto transfers, and the read-only rules.
- `src/hypervisor.rs`: interface addresses through `Domain::interface_addresses`.

## Test bed

`config.toml` gains a pull directory under the run's scratch directory. New cases cover a file pull and its
refusals, a push with `.gitignore`, `exclude`, `replace` and a skipped outside symlink, a pull with a skipped symlink
and FIFO, the caps, a transfer that outlasts `max_wait_secs` (set low in the case's config), a VM stopped during a
transfer, and `vm_addresses` with and without a guest agent. Unit tests feed the unpacker crafted archives with
absolute paths, `..`, repeated paths, sparse entries, oversized headers, links and devices, since a real guest `tar`
does not produce them. Unit tests also cover the config overlap refusals and a push walk that meets a symlinked
directory. Run 2 adds the directory cases on `t-win11`.

## Checks

- A pushed checkout's files match the host's by checksum, and ignored files are absent.
- A pulled tree matches the guest's by checksum, with every mode 644 or 755.
- Every refusal in the invariants shows in a case or a unit test.
- A 200 MiB push finishes, through `transfer_done` when it outlasts the call.

## Risks

1. A server that exits mid-transfer leaves `.vmm-*` temporary files in the guest. Their names start with `.vmm-`, so
   an agent can find and delete them.
2. A guest with little temporary space cannot take a push of a tree larger than half its free space, because the
   archive and the unpacked tree exist together.
3. Windows `tar.exe` cannot create files with reserved names such as `CON`; such a push fails with `tar`'s message.
4. A guest's `tar -c` follows the guest's view of the tree, which the agent controls. The host rules do not depend on
   it, because the server unpacks every pull itself.

## Open questions

None block planning. The deferred items are in the decisions table.
