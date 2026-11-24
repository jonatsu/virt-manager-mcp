//! The firmware record of a UEFI VM: the code file libvirt resolved at the VM's first start and its SHA-256, kept in
//! the VM's owner element so that a seal can later tell whether the file changed
//! (`docs/plans/custom-templates-design.md`, "Sealing a VM"). This module holds the pure parts: which start to record,
//! what to read from the definition, and the hash. `manage::record_firmware` does the libvirt calls.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::manage::Owner;
use crate::volnames;

/// The largest firmware code file read. OVMF's is under 4 MiB; a bigger file is not firmware the server should hash.
const MAX_CODE_BYTES: u64 = 64 << 20;

/// Whether a start is the first of a VM the server made, with its variables file in the pool, and so the one whose
/// firmware to record. All of these must hold:
///
/// - the management tools may act on the VM (`may_manage`, from `Hypervisor::can_manage`), because the record is a
///   management write: a VM that only `power_vms` lists, or any VM while management is off, is never recorded;
/// - the VM's inactive definition carries the owner element (`owner` is `Some`);
/// - that element has no firmware record yet;
/// - the definition's `<nvram>` names `<vm>.vars` in `pool_dir`, which excludes BIOS VMs and any other file;
/// - the pool listed no `<vm>.vars` before the start (`vars_listed_before`, read after a rescan), so libvirt creates
///   it at this start. A VM started first by another client already has the file, and the server did not see which
///   code file that start used.
pub fn should_record(
    may_manage: bool,
    owner: Option<&Owner>,
    inactive_xml: &str,
    pool_dir: &Path,
    vm: &str,
    vars_listed_before: bool,
) -> bool {
    may_manage
        && owner.is_some_and(|o| !o.has_firmware_record())
        && !vars_listed_before
        && nvram_is_pool_vars(inactive_xml, pool_dir, vm)
}

/// Whether the definition's `<nvram>` names the file `<vm>.vars` directly in `pool_dir`.
fn nvram_is_pool_vars(inactive_xml: &str, pool_dir: &Path, vm: &str) -> bool {
    let Ok(doc) = roxmltree::Document::parse(inactive_xml) else {
        return false;
    };
    let nvram = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("os"))
        .and_then(|os| os.children().find(|n| n.has_tag_name("nvram")))
        .and_then(|n| n.text())
        .map(str::trim);
    nvram.is_some_and(|p| Path::new(p) == pool_dir.join(volnames::vars_name(vm)))
}

/// The firmware code file a running VM's live definition names in `<loader>`.
///
/// # Errors
///
/// Fails when the definition is not XML or has no absolute `<loader>` path.
pub fn loader_path(live_xml: &str) -> Result<String> {
    let doc = roxmltree::Document::parse(live_xml).context("the live definition is not XML")?;
    let path = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("os"))
        .and_then(|os| os.children().find(|n| n.has_tag_name("loader")))
        .and_then(|n| n.text())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .context("the live definition has no <loader> path")?;
    if !Path::new(path).is_absolute() {
        bail!("the live definition's <loader> path '{path}' is not absolute");
    }
    Ok(path.to_string())
}

/// The SHA-256 of the file at `path`, in lower-case hex.
///
/// # Errors
///
/// Fails when the path is not a regular file, is over 64 MiB, or cannot be read.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // The opened file's metadata, not the path's: the file checked is the file read.
    let meta = file
        .metadata()
        .with_context(|| format!("reading {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.len() > MAX_CODE_BYTES {
        bail!(
            "{} is {} bytes, over the {MAX_CODE_BYTES} the firmware record reads",
            path.display(),
            meta.len()
        );
    }
    sha256_reader(file, MAX_CODE_BYTES).with_context(|| format!("reading {}", path.display()))
}

/// The SHA-256 of what `reader` yields, in lower-case hex. A reader that yields more than `max` bytes, such as a file
/// that grew after its size was checked, is refused.
fn sha256_reader(reader: impl Read, max: u64) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut limited = reader.take(max.saturating_add(1));
    let mut buf = vec![0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = limited.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max {
            bail!("more than {max} bytes");
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const POOL: &str = "/var/lib/libvirt/images/agent-vms";

    fn definition(os: &str) -> String {
        format!("<domain><name>t-x</name><os>{os}</os></domain>")
    }

    fn uefi_definition() -> String {
        definition(&format!(
            "<loader readonly='yes' secure='yes' type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.secboot.fd</loader>\
             <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd'>{POOL}/t-x.vars</nvram>"
        ))
    }

    fn owner() -> Owner {
        Owner::default()
    }

    #[test]
    fn the_first_start_of_an_owned_uefi_vm_without_a_vars_volume_is_recorded() {
        let xml = uefi_definition();
        let pool = Path::new(POOL);
        assert!(should_record(
            true,
            Some(&owner()),
            &xml,
            pool,
            "t-x",
            false
        ));
    }

    #[test]
    fn no_other_start_is_recorded() {
        let xml = uefi_definition();
        let pool = Path::new(POOL);
        let recorded = Owner {
            firmware_code: Some("/c.fd".into()),
            firmware_sha256: Some("00".into()),
            ..owner()
        };
        // A second start: the record exists, and the variables file is listed.
        assert!(!should_record(
            true,
            Some(&recorded),
            &xml,
            pool,
            "t-x",
            true
        ));
        // A record alone, or a listed file alone, already rules a start out.
        assert!(!should_record(
            true,
            Some(&recorded),
            &xml,
            pool,
            "t-x",
            false
        ));
        assert!(!should_record(
            true,
            Some(&owner()),
            &xml,
            pool,
            "t-x",
            true
        ));
        // A VM the server did not make.
        assert!(!should_record(true, None, &xml, pool, "t-x", false));
        // BIOS has no variables file.
        let bios = definition("");
        assert!(!should_record(
            true,
            Some(&owner()),
            &bios,
            pool,
            "t-x",
            false
        ));
        // Variables kept elsewhere, or under another name, are not the pool's `<vm>.vars`.
        let elsewhere = definition("<nvram>/var/lib/libvirt/qemu/nvram/t-x_VARS.fd</nvram>");
        assert!(!should_record(
            true,
            Some(&owner()),
            &elsewhere,
            pool,
            "t-x",
            false
        ));
        let other_vm = definition(&format!("<nvram>{POOL}/t-y.vars</nvram>"));
        assert!(!should_record(
            true,
            Some(&owner()),
            &other_vm,
            pool,
            "t-x",
            false
        ));
        let nested = definition(&format!("<nvram>{POOL}/sub/t-x.vars</nvram>"));
        assert!(!should_record(
            true,
            Some(&owner()),
            &nested,
            pool,
            "t-x",
            false
        ));
        // Garbage is not a definition.
        assert!(!should_record(
            true,
            Some(&owner()),
            "<domain",
            pool,
            "t-x",
            false
        ));
    }

    #[test]
    fn a_vm_the_management_tools_may_not_act_on_is_never_recorded() {
        // The guardrail: `can_manage` is false for a VM that only `power_vms` lists, and for every VM while
        // management is off, and the record is a management write. Everything else qualifies.
        let xml = uefi_definition();
        let pool = Path::new(POOL);
        assert!(should_record(
            true,
            Some(&owner()),
            &xml,
            pool,
            "t-x",
            false
        ));
        assert!(!should_record(
            false,
            Some(&owner()),
            &xml,
            pool,
            "t-x",
            false
        ));
    }

    #[test]
    fn the_loader_path_comes_from_the_definition() {
        assert_eq!(
            loader_path(&uefi_definition()).unwrap(),
            "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd"
        );
        for xml in [
            definition(""),
            definition("<loader/>"),
            definition("<loader>  </loader>"),
            definition("<loader>OVMF_CODE.fd</loader>"),
            "<domain".to_string(),
        ] {
            assert!(loader_path(&xml).is_err(), "{xml}");
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "virtmanager-mcp-firmware-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_hash_is_the_files_sha256_in_lower_case_hex() {
        let dir = scratch_dir("hash");
        let file = dir.join("CODE.fd");
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_over_the_cap_is_not_hashed() {
        let dir = scratch_dir("cap");
        // A sparse file one byte over the cap, which the size check refuses.
        let big = dir.join("BIG.fd");
        File::create(&big)
            .unwrap()
            .set_len(MAX_CODE_BYTES + 1)
            .unwrap();
        let err = format!("{:#}", sha256_file(&big).unwrap_err());
        assert!(err.contains("over the"), "{err}");
        // One at the cap is read.
        let exact = dir.join("EXACT.fd");
        File::create(&exact)
            .unwrap()
            .set_len(MAX_CODE_BYTES)
            .unwrap();
        assert!(sha256_file(&exact).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reader_that_yields_more_than_the_cap_is_refused() {
        // A file that grew after its size was checked.
        let bytes = [7u8; 11];
        assert!(sha256_reader(&bytes[..], 10).is_err());
        assert_eq!(
            sha256_reader(&bytes[..10], 10).unwrap(),
            sha256_reader(&bytes[..10], 100).unwrap()
        );
    }

    #[test]
    fn only_a_regular_file_is_hashed() {
        let dir = scratch_dir("regular");
        assert!(sha256_file(&dir).is_err());
        assert!(sha256_file(&dir.join("missing.fd")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
