> Survey lane notes from the `agent-setup` session of 2026-11-09, copied from its untracked working folder.
> The evaluation record, [`2026-11-09-vm-management-mcp-evaluation.md`](2026-11-09-vm-management-mcp-evaluation.md),
> summarizes them.

# VM management for coding agents: alternatives survey (2026-11-09)

Excluded by brief (evaluated by main session): KubeRl/MCP-Server-For-VirtManager-AI, Neanderthal/mcp-qemu-vm.

Tags: (R) read directly in source/LICENSE/API on 2026-11-09; (I) inferred; "not verified" = not checked.
Source files were read at the default branch HEAD via raw.githubusercontent.com; dates come from `commits.atom`
(last default-branch commit) and stars from the GitHub API on 2026-11-09.

## Success criteria

- Ranked top ~5 with operations, libvirt access method, session vs system URI, guardrails, licence (LICENSE file
  read), last commit, and a verdict on both mechanism and runnability.
- Each claim tagged; gaps marked "not verified".
- The strongest pattern to steal.

## Hypotheses and outcome

- H1, no mature libvirt MCP server exists: SUPPORTED. Every libvirt MCP found has 0-11 stars; the best guardrail
  design (akarasulu) has no licence file.
- H2, a mature non-MCP CLI is the practical answer: SUPPORTED. Lima, virt-lightning and testcloud are maintained and
  licensed; Lima also isolates structurally from libvirt.
- H3, a session-URI MCP with prefix scoping exists: PARTLY. kimlherme/qemu-lab-mcp is session-only with one fixed VM;
  akarasulu has a required name prefix but defaults to qemu:///system. No single repo has both and a licence.

## Discovery routes

- Official MCP registry search by name: libvirt/virsh 0 hits; qemu -> 0xmortuex/qemu-mcp; virt -> virtbase/proxbase
  (Proxmox-in-QEMU, out of scope).
- GitHub search API (`search/repositories`) for libvirt/virsh/qemu/kvm/incus/lima/firecracker + mcp. Four of 14
  queries failed (rate limit, inferred). Glama, Smithery and mcp.so not searched (gap).
- Trees checked for `mcp` paths in lima, kcli, incus, multipass, virtme-ng, virt-lightning.

## Ranked shortlist

| # | Candidate | Operations (R) | Access | URI | Guardrails | Licence | Last commit | Stars |
|---|-----------|----------------|--------|-----|-----------|---------|-------------|-------|
| 1 | lima-vm/lima (`limactl`) | create/start/stop/delete, shell, copy, snapshot create/apply/list/delete | QEMU directly, no libvirt (R: `pkg/driver/qemu`) | n/a; unprivileged user process (I, general knowledge) | Structural: manages only its own instances, so cannot see virt-manager VMs (I) | Apache-2.0 (R LICENSE) | 2026-11-09 | 22.3k |
| 2 | virt-lightning/virt-lightning | up/start/stop/down, ssh, exec_ssh, ansible inventory, cloud images | libvirt-python (R `api.py` `libvirt.open`) | default qemu:///system, configurable `libvirt_uri` (R `configuration.py`) | `down` deletes only domains whose `vl` metadata `context` matches (R `api.py` 417-427) | Apache-2.0 (R `LICENSE-2.0.txt`; API says no SPDX) | 2026-10-22 | 255 |
| 3 | teemtee/testcloud | instance create/list/start/stop/force-off/shutdown/remove, cloud-init, SSH port | libvirt-python, plus some `shell=True` (R `instance.py` 330, 475) | `--connection` system (default) or session (R `cli.py` 651-671) | `remove` of a running instance needs `--force` (R `cli.py` 487); tracks own instances under DATA_DIR (R) | GPL-2.0 (R) | 2026-11-04 | 8 |
| 4 | akarasulu/nested-mcp-server | domain lifecycle, define/undefine, snapshots, checkpoints, networks, pools, QMP | libvirt-python (R adapter `libvirt.open`) | default qemu:///system, `MCP_LIBVIRT_SUPPORT_SESSION` (R `config.py` 110-113) | mutations, define, destructive, URI override all OFF by default; per-domain allowlists; `mcp_test_` prefix required for define/undefine; role-tool allowlist; QMP allowlist; audit log (R) | NO LICENCE FILE | 2026-05-13 | 0 |
| 5 | kimlherme/qemu-lab-mcp | provision (Ubuntu cloud image, sha256 checked), start/shutdown/force_stop, guest_exec over SSH, disks, ISO, snapshots, upload/download | virsh via argv list (R `_virsh`), qemu-img | hard-coded qemu:///session, own XDG_RUNTIME_DIR (R lines 30, 94-102) | one fixed VM name `ubuntu-lab`; size caps; SAFE_NAME regex; pinned known_hosts; no destructive cleanup after ambiguous libvirt result (R) | NO LICENCE FILE | 2026-10-16 | 1 |
| 6 | rzippert/libvirt-mcp | lifecycle, force_destroy, snapshots, create/delete domain | libvirt-python; read tools via `openReadOnly` (R `connections.py`) | per-profile URIs from TOML config (R) | `confirm=True` arg on destructive tools (R) - agent sets it itself, so weak | AGPL-3.0 (R) | 2026-06-09 | 2 |

## Rejected or niche (with reason)

- karmab/kcli + `kvirt/kmcp.py`: Apache-2.0, 659 stars, active 2026-11-09. MCP runs `transport: http, host: 0.0.0.0`
  (R line 672) with no auth seen; `delete_vm`/`delete_pool` have no guardrails (R). The kvm provider builds many
  `os.system` and `shell=True` strings, including `sudo sed` with a host entry (R line 2327); injection not traced.
  Supports `session=True` (R line 125-130). CLI usable; MCP not.
- alexandremahdhaoui/testenv-vm: Go, go-libvirt (R go.mod), Apache-2.0, cloud-init, generated MCP. `VMCreate`
  silently destroys and undefines any existing domain with the requested name (R `domain.go` 35-38). Default
  qemu:///system (R). Dangerous on a host with user VMs.
- EF-Code/boxes-mcp: MIT, virsh argv, GUI input. Default URIs are system AND session (R `virsh.ts` 22); no scoping.
- amineutron/fedora-agents: MIT, active 2026-11-08. virsh via `spawn(shell:false)` with zod name regexes and
  `dry_run` (R), cloud-init create. Uses qemu:///system (R common.sh 113) and sudoers-run scripts for destroy (R
  config.ts 148-149). Personal Fedora toolkit, French messages.
- 0xmortuex/qemu-mcp: MIT, active 2026-11-09, tests. Direct qemu-system, QMP/HMP savevm snapshots, serial, screen.
  Only touches VMs it spawned (in-process `_vms` registry, R vm.py 423). Accepts arbitrary `extra_args` (R) so an
  agent can attach any host file the user can read (I). GUI-driving focus, no cloud-init/SSH.
- MatiasVara/libvirt-mcp: no licence file, 2025-07-23, hard-coded qemu:///system, `destroy_vm` destroys and
  undefines (R).
- moonlightelite/kvm_mcp: MIT, 2026-03. HTTP app with CORS `*` (R line 37); snapshot uses
  `domain.createWithXML` (R) which is not the snapshot API (I: likely broken).
- xddxdd/mcp-libvirt-vm-use: GPL-3.0, Rust, SPICE screen control, default qemu:///system (R).
- steveydevey/kvm-mcp: no licence file, 2025-04-30.
- arighi/virtme-ng `mcp.py`: GPL-2.0, kernel build/test only. Tool text tells the agent "do NOT wait for user
  confirmation" (R lines 953, 1007, 1027, 1079): an embedded directive, treat as a finding.
- Lima's own MCP (`limactl mcp`): guest-sandbox file/shell tools only, no lifecycle (R docs page and toolset.go).
- lxc/incus: Apache-2.0, active. Not libvirt; restricted projects would give strong scoping (I, not verified in
  source). nikitatsym/incus-mcp has no licence file.
- vagrant-libvirt: MIT, last push 2024-08-17 (R API). Stale.
- quickemu (MIT, 2026-10-29), multipass (GPL-3.0, active; root daemon, I), virtme-ng CLI: mechanism not read.
- Firecracker sandboxes (nehemiah, pullrun, etc.): agent-sandbox runtimes, not libvirt/test-VM managers; not read.

## Patterns worth stealing

1. Structural scoping first: run agent VMs on qemu:///session (kimlherme) or outside libvirt (Lima), so the user's
   qemu:///system VMs in virt-manager are unreachable whatever the agent passes.
2. Ownership tag in libvirt domain metadata (virt-lightning `setMetadata(..., "vl", ...)`), checked before any
   mutation; stronger than a name prefix because renaming does not forge it (I). Note virt-lightning builds the
   metadata XML with an f-string (R line 872).
3. Default-deny capability tiers set by the operator in env, not by the agent (akarasulu). Contrast rzippert's
   `confirm=True`, which the agent can always set.
4. No silent overwrite: refuse a name that already exists (amineutron, kimlherme) rather than destroy it (testenv-vm).
