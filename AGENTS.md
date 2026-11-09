# virtmanager-mcp

This repository is a Rust MCP server that gives AI agents guarded access to libvirt VMs. It is a GPL-2.0 fork of
`KubeRl/MCP-Server-For-VirtManager-AI` that diverges from upstream and takes upstream changes case by case.

## Ground rules

- Track planned work in `docs/plans/fork-roadmap.md`, and mark each item done in the commit that lands it.
- Keep dated evidence in `docs/research/`. Cite it from the roadmap rather than copying it.
- Treat `example-global-CLAUDE.md` and `example-global-AGENTS.md` as product templates for the server's users. They
  are not instructions for working on this repository.
- Use Conventional Commits, one logical change per commit.

## Build gate

Building needs the libvirt development files (`libvirt-dev` on Debian and Ubuntu). Every commit passes:

```sh
cargo fmt --check
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
pre-commit run --all-files
```

Formatting uses default rustfmt settings. The unit tests never touch libvirt. Run `pre-commit install` once per
clone. Leave `docs/research/` and `example-global-*.md` out of the Markdown formatters: they are verbatim copies
and prompt templates.

## Guardrails

The server's safety comes from operator config that defaults to deny. Do not weaken it as a side effect.

- `Config::is_allowed` in `src/config.rs` hides every VM that `allowed_vms` does not list.
- `is_owned` in `src/manage.rs` reads the owner element in the `OWNER_NS` metadata namespace. Only `create_vm` sets
  it, inside the new domain's XML. No tool may write it later, because an agent could then claim another VM.
- `Hypervisor::can_manage` in `src/hypervisor.rs` limits management tools to owned VMs and to `manage_vms`.
- Deletion removes only volumes in the configured `disk_pool`.

A change to any of these needs a test that shows a refusal still happens.

## Test bed

Manual and libvirt-backed testing uses `tests/testbed/`; its README explains how to run a case. Never touch a VM
other than `trial-deb13`, or a pool other than `agent-vms`, on `qemu:///system`. The host runs the user's own VMs
there, and membership of the `libvirt` group is effectively root.

Run `virt-install` as `/usr/bin/python3 /usr/bin/virt-install` wherever a version manager's Python shadows the
system Python, which provides the `gi` module.
