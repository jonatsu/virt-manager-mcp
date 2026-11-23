# Project goals

The purpose and scope of virtmanager-mcp live in the [Goals section of the README](../README.md#goals). This page holds
the rest of the goals: the signals that show they are met, the method, and how to change them.

## Success signals

- An agent takes a VM from a template to a tested result and back without operator help.
- Asked to do a task in an existing VM that the operator has opted in, an agent finishes it without operator
  help.
- Asked to set up a new VM from an installer ISO with no template, such as a CachyOS VM for a test, an agent
  installs it and leaves it booted and usable.
- `tests/testbed/run-all.sh` passes every case.
- No tool reaches a VM or storage pool outside the operator's config, and the guardrail refusal tests prove it.

## Method

`AGENTS.md` fixes the language, the build gate, the guardrails and the commit rules. Within those, agents choose
the approach and the tools, as long as these goals hold.

## Changing these goals

A change to these goals needs the project owner's approval. Agents stop and ask before any work that would break
a goal or widen the scope.
