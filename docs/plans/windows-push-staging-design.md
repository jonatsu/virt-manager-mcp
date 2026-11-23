# Windows pushes staged outside the target's parent: design

Status: draft, for approval, revised after an architecture review.
Once built, the code wins and this design follows it.
Basis: roadmap follow-up 23 in [`fork-roadmap.md`](fork-roadmap.md), with evidence in
[`2026-11-23-windows-handle-steps-probe.md`](../research/2026-11-23-windows-handle-steps-probe.md).

## Summary

A Windows push today writes its archive, unpacks it and fixes its access list in `.vmm-<id>` entries beside the
target. A standard user who may delete or rename entries in that parent, such as the owner of `C:\Users\alice`, can
swap one of those entries for a junction between two guest calls, and so make the push act as `SYSTEM` somewhere else.
The probe confirmed that such a user can rename a `SYSTEM`-only directory there.

The push will stage in `%SystemRoot%\Temp`, where other users cannot delete or rename the push's entries. Everything
the push does in the target's parent then happens in one guest call, the `Place` step. A C# helper creates the
missing parents, moves or copies the tree into place, gives it the parent's access list, and deletes a replaced tree.
It holds the parent open throughout and acts only on open handles. Linux pushes do not change.

## Decisions

| Decision | Chosen | Why | Status |
| -- | -- | -- | -- |
| Where a push stages | `%SystemRoot%\Temp\.vmm-<id>.a` for the archive and `.vmm-<id>.d` to unpack into, made by `push-mkdir.ps1` as today | Other users may create entries there but not delete or rename the push's, and `push-mkdir.ps1` already refuses a name another user made first | resolved (roadmap); the test bed checks the premise |
| One guest call in the parent | `Place` does every step in the target's parent and ends with nothing of the push's left there but the new tree | Handles last only as long as the helper's process, so a second call would have to find its entries by name again | resolved |
| How the helper reaches the parent | It opens each component of the parent's path in turn, relative to the one before, with `FILE_OPEN_REPARSE_POINT` and without delete sharing, and creates a missing component the same way | The held handles pin every directory on the path, as the probe showed. Creating the parents here, not in the probe step, leaves no path-based step in the parent | resolved |
| Reparse points on the parent's path | A junction, a symbolic link or a mount point is refused, with a message that names it; other reparse points are opened | A user who owns a folder above the target could point it at a system directory before the push starts. Refusing only these name surrogates keeps cloud-file placeholders, such as OneDrive folders, usable | provisional: a probe with a cloud-file placeholder on `t-win11` confirms that such a folder opens and stays put |
| Helper delivery | A C# source file that the push writes into its private archive directory, compiled there with `Add-Type -Path` | `powershell -EncodedCommand` is limited by the 32,767-character command line, and the source alone would take most of it once encoded as UTF-16 and Base64. The private directory keeps other users from changing the source before it compiles | resolved |
| Steps | `Inherit` and `Swap` become one `Place` step; `Cleanup` deletes only the staging entries in `%SystemRoot%\Temp`, by path as today | `Cleanup` never receives a path in the parent | resolved |
| Inheritance | The helper opens a directory's entries while that directory is still protected, then turns inheritance on for the directory, then for each entry through the handle it already holds, skipping reparse points and files with more than one link | Once a directory inherits, its owner may rename entries in it and put others in their place. Handles opened before that point can only be the push's own entries | resolved |
| A target on another volume | Chosen before anything moves, by comparing the volume serial numbers of the parent and the staging directory. The helper creates `.vmm-<id>.c` in the parent with the protected list `push-mkdir.ps1` sets, copies the staged tree into it through handles, and continues as for a move | A rename cannot cross volumes. Copying into a protected directory keeps other users out until the inheritance pass, so the copy follows the same path as a move. The copy is slower | resolved (roadmap); not yet probed on a second volume |
| Order of a replace | The new tree is ready in the parent, under its staging or copy name, before the old tree moves aside; then the new tree takes the target's name, and the old tree is deleted after the inheritance succeeds | The target is missing only between two renames, as today | resolved |
| A failure after the new tree reached the parent | The helper deletes the new tree through the handles it holds, moves the old tree back, and reports that nothing was replaced. It retries a failed inheritance once first | A tree left in place with a `SYSTEM`-only list is unusable, and a partly inherited tree moved back to staging would be writable by other users while `Cleanup` walks it by path | resolved |
| A failure while deleting the old tree | The push succeeds with a note that names the old tree, as today | The new tree is complete and in place | resolved |
| Windows without full PowerShell language mode | The probe step reports it, and the push fails with that cause | Constrained Language Mode refuses `Add-Type`, so the helper cannot load | resolved |
| Names the NT layer accepts and Win32 does not | The helper refuses a reserved device name, such as `CON` or `NUL`, as the target or a parent | Opens through `NtCreateFile` would create such an entry, which most programs cannot then open | resolved |

## Behaviour

The probe step prints the staging root, `%SystemRoot%\Temp`, on its own line before the path of `tar.exe`, and
reports a guest whose PowerShell runs in a restricted language mode. It checks the target as today but no longer
creates the target's parents.

`MakeDir` creates the archive and staging directories in the staging root. The push writes `helper.cs` and the
archive into the archive directory, and `tar.exe` unpacks into the staging directory.

`Place` runs the helper with the helper's path, the target's path, the staging directory, the names for the copy and
the old tree, and the replace flag. The helper:

1. opens the parent's path component by component, creating missing components, as in the decisions;
2. opens the staging directory with `DELETE` and `WRITE_DAC`, without delete sharing;
3. on a different volume, creates `.vmm-<id>.c` and copies the staged tree into it, skipping reparse points, and from
   here treats `.c` as the new tree; otherwise it renames the staging directory into the parent under its `.d` name;
4. on a replace, opens the target relative to the parent, refuses it unless it is a directory and not a reparse
   point, and renames it to `.vmm-<id>.old` through its handle;
5. renames the new tree to the target's name, without replacing an existing entry;
6. turns inheritance on for the new tree, as in the decisions;
7. deletes the old tree through handles, deleting a reparse point as a link and a read-only file as any other.

A failure in steps 3 to 6 rolls back as the decisions describe. `Place` keeps today's exit statuses and their
messages, adding four: a reparse point on the parent's path, a reserved name, an inheritance that failed so nothing
was replaced, and a helper that could not be compiled. A busy file still gets one retry. `Place` runs under the long
step timeout, and a `Place` that times out reports the target and every `.vmm-<id>` name it may have left in the
parent.

## Invariants

- After the probe step, no step of a Windows push reaches the target's parent by path.
- The helper never follows a reparse point on the parent's path, in the parent, or in the pushed tree.
- Only entries the push created get their access list changed.
- A failed `Place` leaves the target as it was before the push, or names where each tree is.
- Linux pushes are unchanged.

## Code changes

- `src/winsteps/push-helper.cs` (new): the C# helper, included with `include_str!` and written by the push.
- `src/winsteps/push-place.ps1` (new) replaces `push-inherit.ps1` and `push-swap.ps1`; `src/winsteps.rs` changes its
  constants to match.
- `src/winsteps/push-probe.ps1`: prints the staging root, checks the language mode, and stops creating parents.
- `src/push.rs`: a staging root in `Target`, so `.a` and `.d` live there while `.c` and `.old` stay in the parent;
  the probe output's parsing; the helper's write; the `Place` step, its timeout and its messages; and no `.old` in
  `Cleanup`'s list. The tests that expect `Inherit`, the step order and the old tree's deletion by `Cleanup` change.
- `README.md` and its Windows transfer page, `CHANGELOG.md`.

## Test bed

On `t-win11`, the transfer cases keep passing, and new cases check:

- a standard user can neither rename nor delete a push's staging entries in `%SystemRoot%\Temp`;
- a push into a folder that a standard user owns, and a push directly under `C:\`, give the tree the parent's
  inherited list, entry for entry;
- a replace in such a folder, whose old tree holds a junction, a symbolic link, a hard link and a read-only file,
  deletes the old tree and leaves each link's target unchanged;
- a parent reached through a junction, and a target that is a junction, are refused, and the junction's target is
  unchanged;
- a busy file in the old tree gives the busy message after the retry;
- a pushed path longer than 260 characters arrives;
- a push to a second volume copies the tree, on a small extra `t-*` disk in `agent-vms` attached for the case.

The race itself, and an inheritance that fails, cannot be produced reliably in the test bed. The probe record shows
each primitive the helper relies on, and unit tests cover the step order, the arguments and the messages.

## Risks

- The probe step still checks the target by path; `Place` checks it again on handles, so a change in between makes
  the push fail rather than act elsewhere.
- A push needs room on the system volume for the archive and the staged tree, and on a cross-volume push also for the
  copy.
- The helper needs NTFS access lists. A target on a FAT volume fails at the inheritance step and nothing is replaced.
- The helper adds a few hundred lines of C# that no host build compiles; only the Windows test bed exercises it.
