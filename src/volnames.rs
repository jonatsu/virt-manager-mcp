//! Names of the volumes the server makes in `disk_pool` for firmware variables, TPM state and templates.
//!
//! `@` marks every derived name, and neither `check_name` nor `is_hostname` lets a VM, snapshot or template name hold
//! one. The one clash left is a VM named `tpl`, which `check_vm_name` refuses.

/// The VM name that `check_vm_name` refuses: every template volume starts with `tpl@`, and a VM named `tpl` with a
/// snapshot `s1` would make `tpl@s1.vars`, the variables of a template named `s1`.
pub const RESERVED_VM_NAME: &str = "tpl";

/// What a state volume holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
pub enum StateKind {
    /// UEFI variables, `.vars`.
    Vars,
    /// TPM state, `.tpm`.
    Tpm,
}

/// Which copy of a VM's state a volume is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
pub enum CopyRole {
    /// The state saved with a snapshot.
    Snapshot,
    /// The state a snapshot revert saved before it began.
    Journal,
}

/// A snapshot copy or revert journal, read back from its volume name by `copy_of`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
pub struct StateCopy {
    pub snapshot: String,
    pub kind: StateKind,
    pub role: CopyRole,
}

/// The name of a VM's snapshot copy or revert journal of `kind`.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
fn copy_name(vm: &str, snapshot: &str, kind: StateKind, role: CopyRole) -> String {
    let ext = match kind {
        StateKind::Vars => "vars",
        StateKind::Tpm => "tpm",
    };
    match role {
        CopyRole::Snapshot => format!("{vm}@{snapshot}.{ext}"),
        CopyRole::Journal => format!("{vm}@@revert@{snapshot}.{ext}"),
    }
}

/// `<vm>.vars`, a UEFI VM's own variables.
pub fn vars_name(vm: &str) -> String {
    format!("{vm}.vars")
}

/// `<vm>.tpm`, a VM's own TPM state.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 7"))]
pub fn tpm_name(vm: &str) -> String {
    format!("{vm}.tpm")
}

/// `<vm>@<snapshot>.vars`, the variables saved with a snapshot.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
pub fn snapshot_vars_name(vm: &str, snapshot: &str) -> String {
    copy_name(vm, snapshot, StateKind::Vars, CopyRole::Snapshot)
}

/// `<vm>@<snapshot>.tpm`, the TPM state saved with a snapshot.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
pub fn snapshot_tpm_name(vm: &str, snapshot: &str) -> String {
    copy_name(vm, snapshot, StateKind::Tpm, CopyRole::Snapshot)
}

/// `<vm>@@revert@<snapshot>.vars`, the variables a revert to `snapshot` saved before it began.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 23"))]
pub fn journal_vars_name(vm: &str, snapshot: &str) -> String {
    copy_name(vm, snapshot, StateKind::Vars, CopyRole::Journal)
}

/// `<vm>@@revert@<snapshot>.tpm`, the TPM state a revert to `snapshot` saved before it began.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 23"))]
pub fn journal_tpm_name(vm: &str, snapshot: &str) -> String {
    copy_name(vm, snapshot, StateKind::Tpm, CopyRole::Journal)
}

/// `tpl@<name>.qcow2`, an agent-made template's qcow2 disk.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 16"))]
pub fn template_qcow2_name(template: &str) -> String {
    format!("tpl@{template}.qcow2")
}

/// `tpl@<name>.img`, an agent-made template's raw disk.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 16"))]
pub fn template_raw_name(template: &str) -> String {
    format!("tpl@{template}.img")
}

/// `tpl@<name>.vars`, a UEFI template's firmware variables.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 19"))]
pub fn template_vars_name(template: &str) -> String {
    format!("tpl@{template}.vars")
}

/// `tpl@<name>.meta`, an agent-made template's record.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 13"))]
pub fn template_meta_name(template: &str) -> String {
    format!("tpl@{template}.meta")
}

/// Reads `volume` as a snapshot copy or revert journal of `vm`, or `None` when it is neither. A match rebuilds the
/// name from the parsed snapshot, kind and role and compares it with `volume`, so a prefix of `<vm>@` alone never
/// matches.
#[cfg_attr(not(test), expect(dead_code, reason = "first caller: unit 22"))]
pub fn copy_of(vm: &str, volume: &str) -> Option<StateCopy> {
    let rest = volume.strip_prefix(vm)?;
    // A snapshot name holds no `@`, so the journal prefix cannot also open a snapshot copy.
    let (role, rest) = match rest.strip_prefix("@@revert@") {
        Some(rest) => (CopyRole::Journal, rest),
        None => (CopyRole::Snapshot, rest.strip_prefix('@')?),
    };
    let (kind, snapshot) = match (rest.strip_suffix(".vars"), rest.strip_suffix(".tpm")) {
        (Some(snapshot), _) => (StateKind::Vars, snapshot),
        (None, Some(snapshot)) => (StateKind::Tpm, snapshot),
        (None, None) => return None,
    };
    if snapshot.is_empty() || snapshot.contains('@') {
        return None;
    }
    (copy_name(vm, snapshot, kind, role) == volume).then(|| StateCopy {
        snapshot: snapshot.to_owned(),
        kind,
        role,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::manage::check_vm_name;

    type Builder = fn(&str, &str) -> String;

    fn vm_names() -> Vec<String> {
        [
            "a",
            "vm",
            "vm2",
            "a.b",
            "a_b",
            "a-b",
            "vm.vars",
            "vm.tpm",
            "a.vars.tpm",
            "tpl2",
            "my-tpl",
            "T-Vm_1.2",
        ]
        .map(String::from)
        .into_iter()
        .chain(["x".repeat(64)])
        .collect()
    }

    fn snapshot_names() -> Vec<String> {
        let mut names = vm_names();
        names.extend(
            ["tpl", "s1", "debian-13", "vars", "tpm", "revert", "x.vars"].map(String::from),
        );
        names
    }

    /// Valid template names, which follow the hostname rule.
    fn template_names() -> Vec<String> {
        ["s1", "a", "debian-13", "t-x", "tpl", "vm", "revert"]
            .map(String::from)
            .into_iter()
            .chain(["a".repeat(63)])
            .collect()
    }

    #[test]
    fn each_name_has_the_designed_shape() {
        assert_eq!(vars_name("vm"), "vm.vars");
        assert_eq!(tpm_name("vm"), "vm.tpm");
        assert_eq!(snapshot_vars_name("vm", "s1"), "vm@s1.vars");
        assert_eq!(snapshot_tpm_name("vm", "s1"), "vm@s1.tpm");
        assert_eq!(journal_vars_name("vm", "s1"), "vm@@revert@s1.vars");
        assert_eq!(journal_tpm_name("vm", "s1"), "vm@@revert@s1.tpm");
        assert_eq!(template_qcow2_name("deb"), "tpl@deb.qcow2");
        assert_eq!(template_raw_name("deb"), "tpl@deb.img");
        assert_eq!(template_vars_name("deb"), "tpl@deb.vars");
        assert_eq!(template_meta_name("deb"), "tpl@deb.meta");
    }

    /// Every volume name the server derives for a VM, snapshot or template, with what it is.
    fn derived_names() -> Vec<(String, String)> {
        let mut out = Vec::new();
        for vm in vm_names() {
            out.push((format!("vars of {vm}"), vars_name(&vm)));
            out.push((format!("tpm of {vm}"), tpm_name(&vm)));
            for s in snapshot_names() {
                out.push((
                    format!("snapshot vars {vm} {s}"),
                    snapshot_vars_name(&vm, &s),
                ));
                out.push((format!("snapshot tpm {vm} {s}"), snapshot_tpm_name(&vm, &s)));
                out.push((format!("journal vars {vm} {s}"), journal_vars_name(&vm, &s)));
                out.push((format!("journal tpm {vm} {s}"), journal_tpm_name(&vm, &s)));
            }
        }
        for t in template_names() {
            out.push((format!("template qcow2 {t}"), template_qcow2_name(&t)));
            out.push((format!("template raw {t}"), template_raw_name(&t)));
            out.push((format!("template vars {t}"), template_vars_name(&t)));
            out.push((format!("template meta {t}"), template_meta_name(&t)));
        }
        out
    }

    /// The disk names `create`, `create_from_template` and `attach_disk` make for a VM.
    fn disk_names(vm: &str) -> Vec<String> {
        let mut out = vec![format!("{vm}.qcow2")];
        for n in 1..=3 {
            out.push(format!("{vm}-g{n}.qcow2"));
            out.push(format!("{vm}-g{n}-seed.img"));
        }
        for c in ['b', 'c', 'z'] {
            out.push(format!("{vm}-vd{c}.qcow2"));
        }
        out
    }

    #[test]
    fn no_valid_name_produces_another_kinds_or_vms_volume_name() {
        let mut seen: HashMap<String, String> = HashMap::new();
        for (what, name) in derived_names() {
            if let Some(other) = seen.insert(name.clone(), what.clone()) {
                panic!("{name} is both {other} and {what}");
            }
        }
        let disks: HashSet<String> = vm_names().iter().flat_map(|vm| disk_names(vm)).collect();
        for (what, name) in derived_names() {
            assert!(!disks.contains(&name), "{what} equals the disk name {name}");
        }
    }

    #[test]
    fn a_journal_name_is_never_a_snapshot_copy_name() {
        // A snapshot copy `<vm>@<s>.<ext>` equal to the journal `<vm>@@revert@<t>.<ext>` would need the snapshot
        // `@revert@<t>`, and a snapshot name cannot hold `@`.
        assert!(crate::manage::check_name("@revert@s1", "snapshot name").is_err());
        for vm in vm_names() {
            for s in snapshot_names() {
                let journal = journal_vars_name(&vm, &s);
                for t in snapshot_names() {
                    assert_ne!(journal, snapshot_vars_name(&vm, &t));
                }
            }
        }
    }

    #[test]
    fn the_vm_tpl_clashes_with_a_template_and_check_vm_name_refuses_it() {
        for t in template_names() {
            assert_eq!(
                snapshot_vars_name("tpl", &t),
                template_vars_name(&t),
                "a snapshot {t} of a VM tpl lands on the template's variables"
            );
        }
        assert!(check_vm_name("tpl", "VM name").is_err());
    }

    #[test]
    fn copy_of_accepts_exactly_the_names_the_builders_make() {
        let builders: [(StateKind, CopyRole, Builder); 4] = [
            (StateKind::Vars, CopyRole::Snapshot, snapshot_vars_name),
            (StateKind::Tpm, CopyRole::Snapshot, snapshot_tpm_name),
            (StateKind::Vars, CopyRole::Journal, journal_vars_name),
            (StateKind::Tpm, CopyRole::Journal, journal_tpm_name),
        ];
        for vm in vm_names() {
            for s in snapshot_names() {
                for (kind, role, build) in builders {
                    assert_eq!(
                        copy_of(&vm, &build(&vm, &s)),
                        Some(StateCopy {
                            snapshot: s.clone(),
                            kind,
                            role
                        }),
                        "{vm} {s}"
                    );
                    // Another VM never owns the copy.
                    for other in vm_names().iter().filter(|o| **o != vm) {
                        assert_eq!(copy_of(other, &build(&vm, &s)), None, "{other} {vm} {s}");
                    }
                }
            }
            // The names of every other kind are not copies.
            let mut others = vec![vars_name(&vm), tpm_name(&vm)];
            others.extend(disk_names(&vm));
            others.extend(template_names().iter().flat_map(|t| {
                [
                    template_qcow2_name(t),
                    template_raw_name(t),
                    template_vars_name(t),
                    template_meta_name(t),
                ]
            }));
            for name in others {
                assert_eq!(copy_of(&vm, &name), None, "{vm} {name}");
            }
        }
    }

    #[test]
    fn copy_of_refuses_malformed_names() {
        for bad in [
            "",
            "vm",
            "vm@",
            "vm@.vars",
            "vm@@revert@.tpm",
            "vm@s",
            "vm@s.qcow2",
            "vm@s.vars.bak",
            "vm@s@t.vars",
            "vm@@s.vars",
            "vm@@revert@s@t.tpm",
            "vm@@@revert@s.vars",
            "vm@revert@s.vars",
            "vm2@s.vars",
            "VM@s.vars",
        ] {
            assert_eq!(copy_of("vm", bad), None, "{bad}");
        }
    }
}
