# Disposable VMs from templates: brainstorm draft

Status: incomplete brainstorm draft, 2026-11-09. It shapes outcome 1 of
[`fork-roadmap.md`](fork-roadmap.md), "a clean VM in seconds, not minutes", for a technical design to continue. Each
item is marked as a **user decision**, a **recommendation** or **provisional**.

## Original idea and purpose

The fork exists so that coding agents can create, snapshot and destroy throwaway test VMs on a libvirt host. Today
`create_vm` makes a blank disk plus an ISO install, and the trial's only working guest took a full Debian network
install. An agent that wants a fresh VM for each task cannot wait that long.

**Desired outcome (user decision, through the outcome ordering):** an agent gets a fresh, disposable VM in seconds,
uses it, and throws it away without leaving anything behind.

## Evidence

[`2026-11-09-cloud-init-overlay-probe.md`](../research/2026-11-09-cloud-init-overlay-probe.md) holds the probe:

- Cloud-init applies a NoCloud seed disk on this host.
- qcow2 overlays on a base volume in the `agent-vms` pool work under AppArmor.
- An overlay on a base that already has the guest agent answers about 8 s after power-on, against 44 s when cloud-init
  installs the agent at boot.

## Current direction

- **Templates are prepared by the operator (user decision).** The operator lists template volumes in the server
  config. Agents cannot create, change or promote templates in this version.
- **Each disposable VM is a thin qcow2 overlay on a template plus its own NoCloud seed (recommendation, from the
  evidence).** The seed carries at least the VM's identity, such as its hostname. Cloning a full disk adds a copy
  without a speed gain.
- **The VMs live on `qemu:///system` (user decision).** That is where the evidence, the test bed and the existing
  guardrails are. The connection URI stays configurable, so a later move to `qemu:///session` costs no rework.
- **The existing guardrails apply (recommendation).** These are the owner metadata on VMs the server creates,
  `max_vms`, pool-only deletion and the per-operation switches in `[manage]`.
- **A documented recipe makes a template (recommendation).** It covers installing the guest agent and cleaning the
  image (`cloud-init clean`, a reset machine ID), because the probe's base still carried the previous VM's identity.
- **Reset recreates the overlay rather than reverting a snapshot (recommendation).** A new overlay is instant and
  leaves no snapshot state behind.
- **An optional lifetime for each VM, after which the server deletes it (recommendation).** It removes VMs that an
  agent abandoned.

## Scope

In scope: listing the templates, creating a VM from a template, resetting it, letting it expire, and deleting it with
its overlay and seed, plus the template recipe.

Out of scope: agent-built or promoted templates, server-downloaded images, `qemu:///session`, network policies,
several VMs together, and the existing ISO-based `create_vm`, which stays as it is.

## Parked ideas

- **Agents promote a configured VM to a template.** It builds on this version, but adds a trust boundary, because a
  bad template spreads to every VM built on it.
- **The server builds templates from a cloud image URL and checksum.** It means trusting outside images and needs
  network access for the first boot.
- **`qemu:///session` scope and network policies** belong to outcome 6, operator trust and control.

## Consequential constraints and assumptions

- A template must not change while overlays depend on it. Changing or deleting a base corrupts every VM built on it
  (provisional; the design decides how to protect templates).
- `max_vms` is enforced only within one process today (roadmap outcome 6). Disposable VMs make several concurrent
  agent sessions more likely, so the race matters more here (provisional).
- Expiry needs something that runs when no agent calls the server. With stdio, each agent session has its own
  server process, which may not be running (provisional; this is an open question).

## Observable success signal

An agent creates a VM from a template and gets a responding guest agent within about 10 s. Deleting it, or letting it
expire, leaves no volume of it in the pool (recommendation).

## Questions for the technical design

1. How does the config name templates, and how does the server protect a template volume while overlays use it?
2. What goes into the seed by default, and may an agent add user data, such as packages or an SSH key, within
   limits?
3. Which process enforces expiry when the server runs as several short-lived stdio instances?
4. Is reset a new tool or an option of an existing one, and does a reset keep the VM's name and MAC address?

## Route

A technical design comes next, without a separate spec (recommendation). The behaviour an agent sees is small and
named above, and the success signal is concrete.
