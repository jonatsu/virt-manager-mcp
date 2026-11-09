//! The NoCloud seed disk that gives a VM made from a template its identity.
//!
//! cloud-init's NoCloud data source reads a `vfat` filesystem labelled `CIDATA` that holds `meta-data` and
//! `user-data`. The image is built in memory, so the host needs no `mkfs` or ISO tool.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "create_vm_from_template, a later change, calls this module"
    )
)]

use std::io::{Cursor, Write};

use anyhow::{Context, Result, bail};
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

/// Size of every seed image: small, and enough for a FAT12 filesystem with room for user data.
pub const SEED_BYTES: u64 = 2 * 1024 * 1024;

/// Largest user data accepted, leaving the rest of the image for the filesystem itself.
pub const MAX_USER_DATA: usize = 1024 * 1024;

/// The FAT volume label cloud-init looks for, padded to the 11 bytes the boot sector holds.
const LABEL: [u8; 11] = *b"CIDATA     ";

/// The header cloud-init requires on cloud-config user data.
pub const CLOUD_CONFIG_HEADER: &str = "#cloud-config";

/// Builds a seed image holding `meta-data` with the instance ID and hostname, and `user-data`.
///
/// The instance ID and hostname go into `meta-data` unquoted, so each must be a plain token: letters, digits, `-`,
/// `_` and `.` only.
///
/// # Errors
///
/// Fails when the instance ID or hostname is not a plain token, or the user data lacks the `#cloud-config` header
/// line or exceeds [`MAX_USER_DATA`].
pub fn build(instance_id: &str, hostname: &str, user_data: &str) -> Result<Vec<u8>> {
    check_token(instance_id, "instance ID")?;
    check_token(hostname, "hostname")?;
    check_user_data(user_data)?;
    let size = usize::try_from(SEED_BYTES).expect("the seed size fits in memory");
    let mut disk = Cursor::new(vec![0u8; size]);
    fatfs::format_volume(&mut disk, FormatVolumeOptions::new().volume_label(LABEL))
        .context("formatting the seed image")?;
    disk.set_position(0);
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new()).context("opening the seed image")?;
        let meta = format!("instance-id: {instance_id}\nlocal-hostname: {hostname}\n");
        for (name, body) in [("meta-data", meta.as_str()), ("user-data", user_data)] {
            let mut file = fs
                .root_dir()
                .create_file(name)
                .with_context(|| format!("creating {name} in the seed image"))?;
            file.write_all(body.as_bytes())
                .with_context(|| format!("writing {name} to the seed image"))?;
            // Dropping the file would also flush its directory entry, but would only log a failure.
            file.flush()
                .with_context(|| format!("closing {name} in the seed image"))?;
        }
        fs.unmount().context("closing the seed image")?;
    }
    Ok(disk.into_inner())
}

/// Checks that user data's first line is the `#cloud-config` header and that it fits in the seed image.
///
/// Other headers that share the prefix, such as `#cloud-config-archive`, are different content types to cloud-init,
/// so the first line must be the header alone.
///
/// # Errors
///
/// Returns the reason the user data is unusable.
pub fn check_user_data(user_data: &str) -> Result<()> {
    let first_line = user_data.lines().next().unwrap_or_default().trim_end();
    if first_line != CLOUD_CONFIG_HEADER {
        bail!("user data must start with a {CLOUD_CONFIG_HEADER} line");
    }
    if user_data.len() > MAX_USER_DATA {
        bail!(
            "user data is {} bytes; the seed holds at most {MAX_USER_DATA}",
            user_data.len()
        );
    }
    Ok(())
}

fn check_token(value: &str, what: &str) -> Result<()> {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if value.is_empty() || !value.chars().all(plain) {
        bail!("the seed's {what} {value:?} may hold only letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn read_file(image: &[u8], name: &str) -> String {
        let fs = FileSystem::new(Cursor::new(image.to_vec()), FsOptions::new()).unwrap();
        let mut text = String::new();
        fs.root_dir()
            .open_file(name)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn seed_holds_label_and_both_files() {
        let image = build("abc-1", "t-vm1", "#cloud-config\npackages: [curl]\n").unwrap();
        assert_eq!(image.len() as u64, SEED_BYTES);
        let fs = FileSystem::new(Cursor::new(image.clone()), FsOptions::new()).unwrap();
        assert_eq!(fs.volume_label().trim_end(), "CIDATA");
        assert_eq!(
            read_file(&image, "meta-data"),
            "instance-id: abc-1\nlocal-hostname: t-vm1\n"
        );
        assert_eq!(
            read_file(&image, "user-data"),
            "#cloud-config\npackages: [curl]\n"
        );
    }

    #[test]
    fn seed_is_fat12() {
        let image = build("abc-1", "t-vm1", "#cloud-config\n").unwrap();
        let fs = FileSystem::new(Cursor::new(image), FsOptions::new()).unwrap();
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat12);
    }

    #[test]
    fn user_data_needs_header() {
        for data in [
            "packages: [curl]\n",
            "#cloud-config-archive\n- type: text/cloud-config\n",
            "",
        ] {
            let err = build("abc-1", "t-vm1", data).unwrap_err();
            assert!(err.to_string().contains("#cloud-config"), "{err}");
        }
        assert!(check_user_data("#cloud-config\r\nhostname: x\n").is_ok());
        assert!(check_user_data("#cloud-config").is_ok());
    }

    #[test]
    fn meta_data_values_are_plain_tokens() {
        for (id, host) in [
            ("abc-1\nlocal-hostname: evil", "t-vm1"),
            ("abc-1", "t-vm1 x"),
            ("", "t-vm1"),
            ("abc-1", ""),
        ] {
            assert!(
                build(id, host, "#cloud-config\n").is_err(),
                "{id:?} {host:?}"
            );
        }
    }

    #[test]
    fn user_data_has_a_size_cap() {
        let big = format!("#cloud-config\n{}", "#".repeat(MAX_USER_DATA));
        let err = build("abc-1", "t-vm1", &big).unwrap_err();
        assert!(err.to_string().contains("at most"), "{err}");
    }

    #[test]
    fn largest_user_data_fits() {
        let mut data = String::from("#cloud-config\n");
        data.push_str(&"#".repeat(MAX_USER_DATA - data.len()));
        let image = build("abc-1", "t-vm1", &data).unwrap();
        assert_eq!(read_file(&image, "user-data").len(), MAX_USER_DATA);
    }

    /// Writes a seed image for a manual boot test: `SEED_OUT=path SEED_HOSTNAME=name SEED_USER_DATA=file`.
    #[test]
    #[ignore = "writes a file for the test-bed boot check"]
    fn write_seed_for_boot_test() {
        let out = std::env::var("SEED_OUT").expect("SEED_OUT names the output file");
        let host = std::env::var("SEED_HOSTNAME").expect("SEED_HOSTNAME names the guest");
        let data_path = std::env::var("SEED_USER_DATA").expect("SEED_USER_DATA names a file");
        let data = std::fs::read_to_string(data_path).unwrap();
        let image = build(&format!("{host}-1"), &host, &data).unwrap();
        std::fs::write(out, image).unwrap();
    }
}
