//! VM management: creating, deleting and reconfiguring VMs, and snapshots.
//!
//! VMs created here carry a metadata element in the `OWNER_NS` namespace. That
//! marker is what makes them accessible, power-controllable and manageable without
//! being listed in the config file.

use std::ffi::OsString;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::domain_snapshot::DomainSnapshot;
use virt::error::ErrorNumber;
use virt::network::Network;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::stream::Stream;
use virt::sys;

use crate::config::{Firmware, GuestOs, Manage, is_hostname};
use crate::hypervisor::{self, Hypervisor};
use crate::seed;

/// XML namespace of the metadata element that marks VMs created by this server.
const OWNER_NS: &str = "http://virtmanager-mcp/owner/1";

/// Undefining a VM also removes its managed save, snapshot and checkpoint metadata, NVRAM and TPM state.
const UNDEFINE_FLAGS: u32 = sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
    | sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
    | sys::VIR_DOMAIN_UNDEFINE_CHECKPOINTS_METADATA
    | sys::VIR_DOMAIN_UNDEFINE_NVRAM
    | sys::VIR_DOMAIN_UNDEFINE_TPM;

/// How long an operation waits for another holder to release the management lock.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Takes the management lock, so that a limit checked here, such as `max_vms`, still holds when the operation acts.
/// The lock is an exclusive `flock` on a lock file. Every open of the file has its own lock, so it excludes other
/// threads of this process as well as other server processes and `sweep` commands of this account. The kernel
/// releases it when the returned file is dropped or the process dies.
fn lock() -> Result<File> {
    lock_file(&lock_path(std::env::var_os("HOME"))?, LOCK_WAIT)
}

/// The lock file, `~/.cache/virtmanager-mcp/manage.lock`. It depends only on the home directory, which every process
/// of one account shares, so a server and a `sweep` timer that lacks a login session's variables find the same file.
fn lock_path(home: Option<OsString>) -> Result<PathBuf> {
    let home = home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            anyhow!("cannot place the management lock file: HOME is not an absolute path")
        })?;
    Ok(home
        .join(".cache")
        .join("virtmanager-mcp")
        .join("manage.lock"))
}

/// Opens `path`, creating it and its directory for this account only, and locks it exclusively, waiting up to `wait`
/// for another holder to let go.
fn lock_file(path: &Path, wait: Duration) -> Result<File> {
    if let Some(dir) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening the management lock file {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => bail!(
                "another virtmanager-mcp operation has held the management lock {} for over {} s; try again once it \
                 ends",
                path.display(),
                wait.as_secs()
            ),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

/// Whether `dom` was created by this server.
pub fn is_owned(dom: &Domain) -> bool {
    dom.get_metadata(sys::VIR_DOMAIN_METADATA_ELEMENT as i32, Some(OWNER_NS), 0)
        .is_ok()
}

/// Attributes of the owner element. VMs made by `create_vm` carry none; VMs made from a template carry the
/// template's name and the disk generation, and an expiry when they have a lifetime. Only the server writes them,
/// when it defines a domain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owner {
    pub template: Option<String>,
    pub generation: Option<u32>,
    /// UTC time in seconds since the epoch after which the VM is deleted.
    pub expires: Option<u64>,
}

impl Owner {
    /// The owner element as it goes into a domain's `<metadata>`.
    fn element(&self) -> String {
        let mut attrs = String::new();
        if let Some(t) = &self.template {
            attrs.push_str(&format!(" template='{}'", esc(t)));
        }
        if let Some(g) = self.generation {
            attrs.push_str(&format!(" generation='{g}'"));
        }
        if let Some(e) = self.expires {
            attrs.push_str(&format!(" expires='{e}'"));
        }
        format!("<vmm:owner xmlns:vmm='{OWNER_NS}'{attrs}/>")
    }

    /// Reads the attributes from an owner element, as libvirt returns it for the `OWNER_NS` metadata. An
    /// attribute that does not parse counts as absent.
    fn from_element(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(xml)?;
        Ok(Self::from_node(doc.root_element()))
    }

    fn from_node(node: roxmltree::Node<'_, '_>) -> Self {
        Self {
            template: node.attribute("template").map(str::to_string),
            generation: node.attribute("generation").and_then(|g| g.parse().ok()),
            expires: node.attribute("expires").and_then(|e| e.parse().ok()),
        }
    }
}

/// The owner attributes of `dom`, or `None` when this server did not create it.
pub fn owner_of(dom: &Domain) -> Option<Owner> {
    let xml = dom
        .get_metadata(sys::VIR_DOMAIN_METADATA_ELEMENT as i32, Some(OWNER_NS), 0)
        .ok()?;
    Owner::from_element(&xml).ok()
}

/// The owner attributes in a domain's XML, or `None` when it has no owner element in `OWNER_NS`.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "a test oracle for the owner element in domain XML; production reads it through libvirt"
    )
)]
fn owner_in_domain_xml(xml: &str) -> Result<Option<Owner>> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(metadata) = child(doc.root_element(), "metadata") else {
        return Ok(None);
    };
    Ok(metadata
        .children()
        .find(|n| n.has_tag_name((OWNER_NS, "owner")))
        .map(Owner::from_node))
}

/// Parameters of a new VM, already validated against the config by `create`.
pub struct NewVm {
    pub name: String,
    pub windows: bool,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub uefi: bool,
    pub tpm: bool,
    pub iso: Option<String>,
    pub driver_iso: Option<String>,
    pub network: Option<String>,
}

pub fn create(hv: &Hypervisor, vm: NewVm) -> Result<String> {
    let m = &hv.config.manage;
    check_name(&vm.name, "VM name")?;
    check_vcpus(m, vm.vcpus)?;
    check_memory(m, vm.memory_mib)?;
    check_disk(m, vm.disk_gib)?;
    let iso = vm
        .iso
        .as_deref()
        .map(|p| hv.config.iso_file(p))
        .transpose()?;
    let driver_iso = vm
        .driver_iso
        .as_deref()
        .map(|p| hv.config.iso_file(p))
        .transpose()?;
    let network = match vm.network.clone() {
        Some(n) => Some(n),
        None => m.networks.first().cloned(),
    };

    let _guard = lock()?;
    hv.with_conn(|conn| {
        if let Some(n) = &network {
            check_network(conn, m, n)?;
        }
        if Domain::lookup_by_name(conn, &vm.name).is_ok() {
            bail!("a VM named '{}' already exists", vm.name);
        }
        let owned = conn.list_all_domains(0)?.iter().filter(|d| is_owned(d)).count();
        if owned >= m.max_vms as usize {
            bail!("this server already has {owned} VMs (manage.max_vms = {}); delete one first", m.max_vms);
        }
        let pool = disk_pool(conn, m)?;
        let vol_name = format!("{}.qcow2", vm.name);
        let vol = create_volume(&pool, &vol_name, vm.disk_gib)?;
        let disk = vol.get_path()?;
        let mut disks = vec![Disk::Qcow2(&disk), Disk::Cdrom(iso.as_deref())];
        if driver_iso.is_some() {
            disks.push(Disk::Cdrom(driver_iso.as_deref()));
        }
        let xml = domain_xml(&DomainSpec {
            name: &vm.name,
            windows: vm.windows,
            vcpus: vm.vcpus,
            memory_mib: vm.memory_mib,
            uefi: vm.uefi,
            tpm: vm.tpm,
            disks,
            network: network.as_deref(),
            uuid: None,
            mac: None,
            owner: Owner::default(),
        });
        if let Err(e) = Domain::define_xml(conn, &xml) {
            let _ = vol.delete(0);
            bail!("cannot define VM '{}': {}", vm.name, e.message());
        }
        let mut msg = format!(
            "created VM '{}' ({} vCPUs, {} MiB, {} GiB disk {}/{}, {}{}); it is shut off, start it with start_vm",
            vm.name,
            vm.vcpus,
            vm.memory_mib,
            vm.disk_gib,
            m.disk_pool,
            vol_name,
            if vm.uefi { "UEFI" } else { "BIOS" },
            if vm.tpm { ", TPM" } else { "" },
        );
        if let Some(n) = &network {
            msg.push_str(&format!("; network '{n}'"));
        }
        Ok(msg)
    })
}

/// Parameters of a new VM made from a template; `None` takes the default.
pub struct NewTemplateVm {
    pub name: String,
    pub template: String,
    pub vcpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    pub network: Option<String>,
    pub lifetime_minutes: Option<u32>,
}

/// A template that passed its checks.
struct UsableTemplate {
    path: String,
    capacity: u64,
    user_data: String,
}

/// Default vCPUs and memory of a VM made from a template, within the configured limits.
const TEMPLATE_VCPUS: u32 = 2;
const TEMPLATE_MEMORY_MIB: u64 = 1024;

/// Lists the templates that pass their checks, with how many VMs this server made from each. A template that
/// fails is left out, and its reason is logged.
pub fn list_templates(hv: &Hypervisor) -> Result<Vec<Value>> {
    let m = &hv.config.manage;
    hv.with_conn(|conn| {
        let owners: Vec<Owner> = conn
            .list_all_domains(0)?
            .iter()
            .filter_map(owner_of)
            .collect();
        let mut out = Vec::new();
        for (name, t) in &m.templates {
            if let Err(e) = check_template(conn, m, name) {
                tracing::warn!("template '{name}' is hidden: {e:#}");
                continue;
            }
            let used_by = owners
                .iter()
                .filter(|o| o.template.as_deref() == Some(name.as_str()))
                .count();
            out.push(json!({
                "name": name,
                "description": t.description,
                "os": match t.os { GuestOs::Linux => "linux", GuestOs::Windows => "windows" },
                "firmware": match t.firmware { Firmware::Bios => "bios", Firmware::Uefi => "uefi" },
                "vms_using_it": used_by,
            }));
        }
        Ok(out)
    })
}

/// Logs every configured template that fails its checks, so the operator sees it at startup.
pub fn log_template_problems(hv: &Hypervisor) {
    let m = &hv.config.manage;
    if !m.create || m.templates.is_empty() {
        return;
    }
    let _ = hv.with_conn(|conn| {
        for name in m.templates.keys() {
            if let Err(e) = check_template(conn, m, name) {
                tracing::warn!("template '{name}' is unusable and hidden: {e:#}");
            }
        }
        Ok(())
    });
}

/// Checks a template as the design requires on every use: its volume is a qcow2 in the disk pool that no domain
/// uses as a disk, and its user data, if any, is readable cloud-config that fits the seed.
fn check_template(conn: &Connect, m: &Manage, name: &str) -> Result<UsableTemplate> {
    let t = m
        .templates
        .get(name)
        .ok_or_else(|| anyhow!("no template named '{name}'; list_templates shows them"))?;
    let pool = disk_pool(conn, m)?;
    let vol = StorageVol::lookup_by_name(&pool, &t.volume).map_err(|e| {
        anyhow!(
            "template volume '{}' is not in pool '{}': {}",
            t.volume,
            m.disk_pool,
            e.message()
        )
    })?;
    let format = volume_format(&vol.get_xml_desc(0)?)?;
    if format != "qcow2" {
        bail!("template volume '{}' is {format}, not qcow2", t.volume);
    }
    let path = vol.get_path()?;
    for dom in conn.list_all_domains(0)? {
        // The live XML too, for a disk hot-plugged into a running VM only. A domain that went away after the
        // listing has no disks left; its error would name it to the agent, so it is logged and skipped.
        let mut disks = Vec::new();
        let mut views = vec![sys::VIR_DOMAIN_XML_INACTIVE];
        if dom.is_active().unwrap_or(false) {
            views.push(0);
        }
        for flags in views {
            match dom.get_xml_desc(flags) {
                Ok(xml) => disks.extend(parse_disks(&xml)?),
                Err(e) => tracing::warn!("template check skipped a VM: {}", e.message()),
            }
        }
        if uses_volume(&disks, &path, &m.disk_pool, &t.volume) {
            // The VM may be one the agent cannot see, so its name goes to the log only.
            tracing::warn!(
                "template '{name}': volume '{}' is a disk of VM '{}'",
                t.volume,
                dom.get_name().unwrap_or_default()
            );
            bail!(
                "template volume '{}' is attached to a VM as a disk, and a template must never be; the server log \
                 names the VM",
                t.volume
            );
        }
    }
    let user_data = match &t.user_data {
        Some(file) => std::fs::read_to_string(file).map_err(|e| {
            tracing::warn!("template '{name}': cannot read user data {}: {e}", file.display());
            anyhow!("the user data file of template '{name}' cannot be read; the server log has the reason")
        })?,
        None => format!("{}\n", seed::CLOUD_CONFIG_HEADER),
    };
    seed::check_user_data(&user_data).with_context(|| format!("user data of template '{name}'"))?;
    Ok(UsableTemplate {
        path,
        capacity: vol.get_info()?.capacity,
        user_data,
    })
}

/// Whether any of `disks` is the volume `volume` of pool `pool_name` at `path`, given either by file path or as a
/// pool-volume reference. A `<backingStore>` inside a disk does not count: `parse_disks` reads only direct sources.
fn uses_volume(disks: &[DiskInfo], path: &str, pool_name: &str, volume: &str) -> bool {
    disks.iter().any(|d| {
        d.file.as_deref() == Some(path)
            || (d.pool.as_deref() == Some(pool_name) && d.volume.as_deref() == Some(volume))
    })
}

/// The `<target><format type=…>` of a volume's XML.
fn volume_format(xml: &str) -> Result<String> {
    let doc = roxmltree::Document::parse(xml)?;
    Ok(child(doc.root_element(), "target")
        .and_then(|t| child(t, "format"))
        .and_then(|f| f.attribute("type"))
        .unwrap_or("unknown")
        .to_string())
}

/// Names of the volumes of disk generation `generation` of VM `name`.
fn generation_volumes(name: &str, generation: u32) -> (String, String) {
    (
        format!("{name}-g{generation}.qcow2"),
        format!("{name}-g{generation}-seed.img"),
    )
}

/// The expiry time for a lifetime starting now, refused above `manage.max_lifetime_minutes`.
fn expiry(m: &Manage, lifetime_minutes: Option<u32>, now: u64) -> Result<Option<u64>> {
    let Some(minutes) = lifetime_minutes else {
        return Ok(None);
    };
    if minutes == 0 || minutes > m.max_lifetime_minutes {
        bail!(
            "lifetime_minutes must be between 1 and {} (manage.max_lifetime_minutes)",
            m.max_lifetime_minutes
        );
    }
    Ok(Some(now + u64::from(minutes) * 60))
}

/// Seconds since the epoch, UTC.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Defines a VM made from a template: an overlay on the template volume, a seed disk with the VM's identity, and
/// the owner element. Volumes created before a failing step are deleted. The VM is left shut off.
pub fn create_from_template(hv: &Hypervisor, vm: NewTemplateVm) -> Result<String> {
    let m = &hv.config.manage;
    if !is_hostname(&vm.name) {
        bail!(
            "a VM made from a template is named like a hostname: lowercase letters, digits and hyphens, starting \
             with a letter, not ending with a hyphen, at most 63 characters"
        );
    }
    let t = m.templates.get(&vm.template).ok_or_else(|| {
        anyhow!(
            "no template named '{}'; list_templates shows them",
            vm.template
        )
    })?;
    let vcpus = vm.vcpus.unwrap_or(TEMPLATE_VCPUS.min(m.max_vcpus));
    let memory_mib = vm
        .memory_mib
        .unwrap_or(TEMPLATE_MEMORY_MIB.min(m.max_memory_mib));
    check_vcpus(m, vcpus)?;
    check_memory(m, memory_mib)?;
    if let Some(gib) = vm.disk_gib {
        check_disk(m, gib)?;
    }
    let expires = expiry(m, vm.lifetime_minutes, now_epoch())?;
    let network = vm.network.clone().or_else(|| m.networks.first().cloned());

    let _guard = lock()?;
    hv.with_conn(|conn| {
        if let Some(n) = &network {
            check_network(conn, m, n)?;
        }
        if Domain::lookup_by_name(conn, &vm.name).is_ok() {
            bail!("a VM named '{}' already exists", vm.name);
        }
        let owned = conn.list_all_domains(0)?.iter().filter(|d| is_owned(d)).count();
        if owned >= m.max_vms as usize {
            bail!("this server already has {owned} VMs (manage.max_vms = {}); delete one first", m.max_vms);
        }
        let template = check_template(conn, m, &vm.template)?;
        let capacity = vm
            .disk_gib
            .map_or(0, |gib| gib << 30)
            .max(template.capacity);
        let pool = disk_pool(conn, m)?;
        let uuid = new_uuid();
        let generation = 1;
        let (overlay_name, seed_name) = generation_volumes(&vm.name, generation);
        let seed_image = seed::build(&format!("{uuid}-{generation}"), &vm.name, &template.user_data)?;

        let overlay = create_overlay(&pool, &overlay_name, capacity, &template.path)?;
        let seed_vol = match create_raw_volume(conn, &pool, &seed_name, &seed_image) {
            Ok(v) => v,
            Err(e) => {
                let _ = overlay.delete(0);
                return Err(e);
            }
        };
        let defined = (|| -> Result<()> {
            let (overlay_path, seed_path) = (overlay.get_path()?, seed_vol.get_path()?);
            let xml = domain_xml(&DomainSpec {
                name: &vm.name,
                windows: t.os == GuestOs::Windows,
                vcpus,
                memory_mib,
                uefi: t.firmware == Firmware::Uefi,
                tpm: false,
                disks: vec![Disk::Qcow2(&overlay_path), Disk::ReadOnlyRaw(&seed_path)],
                network: network.as_deref(),
                uuid: Some(&uuid),
                mac: None,
                owner: Owner {
                    template: Some(vm.template.clone()),
                    generation: Some(generation),
                    expires,
                },
            });
            Domain::define_xml(conn, &xml)
                .map_err(|e| anyhow!("cannot define VM '{}': {}", vm.name, e.message()))?;
            Ok(())
        })();
        if let Err(e) = defined {
            let _ = seed_vol.delete(0);
            let _ = overlay.delete(0);
            return Err(e);
        }
        let mut message = format!(
            "created VM '{}' from template '{}' ({vcpus} vCPUs, {memory_mib} MiB, {} GiB disk {}/{overlay_name})",
            vm.name,
            vm.template,
            capacity >> 30,
            m.disk_pool,
        );
        if let Some(n) = &network {
            message.push_str(&format!("; network '{n}'"));
        }
        match expires {
            Some(at) => message.push_str(&format!(
                "; it expires at {at} (Unix time, in {} minutes) and is then deleted",
                vm.lifetime_minutes.unwrap_or_default()
            )),
            None => message.push_str("; it does not expire"),
        }
        Ok(message)
    })
}

/// Returns a VM made from a template to the template's state, keeping its name, UUID, MAC address, hardware and
/// expiry. The new disks exist before the old ones are deleted, so a failure leaves the VM with one complete set.
/// The VM is left shut off; `start_after_reset` starts it. `before_power_off` runs once every check has passed,
/// just before the VM is forced off, so a refused reset leaves the caller's state, such as its console, alone.
pub fn reset(hv: &Hypervisor, name: &str, before_power_off: impl FnOnce()) -> Result<String> {
    let m = &hv.config.manage;
    let dom = hv.manage_domain(name)?;
    let _guard = lock()?;
    // can_manage also passes VMs listed in manage_vms, so ownership and the template come from the owner element.
    let owner =
        owner_of(&dom).ok_or_else(|| anyhow!("VM '{name}' was not created by this server"))?;
    let (Some(template_name), Some(generation)) = (owner.template.clone(), owner.generation) else {
        bail!(
            "VM '{name}' was not made from a template; only VMs from create_vm_from_template can be reset"
        );
    };
    // Another process's sweep may be deleting it right now, which could orphan the new disks.
    if is_expired(&owner, now_epoch()) {
        bail!("VM '{name}' has expired and is about to be deleted; create a new VM instead");
    };
    hv.with_conn(|conn| {
        // 1. Everything that can fail without changing anything.
        let template = check_template(conn, m, &template_name)?;
        let pool = disk_pool(conn, m)?;
        let (old_overlay_name, _) = generation_volumes(name, generation);
        let old_overlay = StorageVol::lookup_by_name(&pool, &old_overlay_name).map_err(|e| {
            anyhow!(
                "cannot find the VM's disk '{old_overlay_name}': {}; delete_vm it instead",
                e.message()
            )
        })?;
        let capacity = old_overlay.get_info()?.capacity.max(template.capacity);
        let next = generation.checked_add(1).ok_or_else(|| {
            anyhow!("VM '{name}' has run out of generations; delete_vm it instead")
        })?;
        let uuid = dom.get_uuid_string()?;
        let (overlay_name, seed_name) = generation_volumes(name, next);
        for vol in [&overlay_name, &seed_name] {
            if StorageVol::lookup_by_name(&pool, vol).is_ok() {
                bail!(
                    "volume '{vol}' already exists in the disk pool, where the reset would put the VM's new disk; \
                     nothing was changed"
                );
            }
        }
        let seed_image = seed::build(&format!("{uuid}-{next}"), name, &template.user_data)?;

        // 2. Power off and drop saved memory and snapshot metadata; the snapshots' data lives in the old overlay.
        before_power_off();
        if dom.is_active()? {
            dom.destroy()
                .map_err(|e| anyhow!("cannot power off '{name}': {}", e.message()))?;
        }
        let off = "the VM was powered off, and some of its snapshots may be gone; its disks are unchanged";
        // A managed save would restore memory that refers to the old disks.
        if dom.has_managed_save(0).context(off)? {
            dom.managed_save_remove(0)
                .map_err(|e| anyhow!("cannot remove the saved state of '{name}': {}", e.message()))
                .context(off)?;
        }
        for snap in dom.list_all_snapshots(0).context(off)? {
            snap.delete(sys::VIR_DOMAIN_SNAPSHOT_DELETE_METADATA_ONLY)
                .map_err(|e| anyhow!("cannot remove a snapshot of '{name}': {}", e.message()))
                .context(off)?;
        }
        let partial = "the VM was powered off and its snapshots were removed; its disks are unchanged";

        // 3. The next generation's disks.
        let overlay = create_overlay(&pool, &overlay_name, capacity, &template.path)
            .map_err(|e| e.context(partial))?;
        let seed_vol = match create_raw_volume(conn, &pool, &seed_name, &seed_image) {
            Ok(v) => v,
            Err(e) => {
                let _ = overlay.delete(0);
                return Err(e.context(partial));
            }
        };

        // 4. The same domain with the new disks and owner attributes.
        let redefined = (|| -> Result<Vec<DiskInfo>> {
            let old_xml = inactive_xml(&dom)?;
            let (overlay_path, seed_path) = (overlay.get_path()?, seed_vol.get_path()?);
            let new_owner = Owner {
                generation: Some(next),
                ..owner.clone()
            };
            let (xml, old) = replace_disks(
                &old_xml,
                &[Disk::Qcow2(&overlay_path), Disk::ReadOnlyRaw(&seed_path)],
            )?;
            let xml = replace_owner(&xml, &new_owner)?;
            Domain::define_xml(conn, &xml)
                .map_err(|e| anyhow!("cannot redefine '{name}': {}", e.message()))?;
            Ok(old)
        })();
        let old_disks = match redefined {
            Ok(old) => old,
            Err(e) => {
                let _ = seed_vol.delete(0);
                let _ = overlay.delete(0);
                return Err(e.context(partial));
            }
        };

        // 5. Remove every pool disk of the old state.
        let mut msg =
            format!("reset VM '{name}' to template '{template_name}' (generation {next})");
        let mut failed = Vec::new();
        let mut outside = Vec::new();
        for d in &old_disks {
            match pool_volume(conn, &pool, &m.disk_pool, d) {
                Some(vol) => {
                    if let Err(e) = vol.delete(0) {
                        failed.push(format!("{} ({})", d.source(), e.message()));
                    }
                }
                None => outside.push(d.source()),
            }
        }
        if !failed.is_empty() {
            msg.push_str(&format!(
                "; could not delete old disks: {}",
                failed.join(", ")
            ));
        }
        if !outside.is_empty() {
            msg.push_str(&format!(
                "; not deleted, because they are not volumes in manage.disk_pool (already deleted, or outside it): {}",
                outside.join(", ")
            ));
        }
        Ok(msg)
    })
}

/// Starts a VM after a reset. A UEFI VM's firmware variables return to the template's defaults.
pub fn start_after_reset(hv: &Hypervisor, name: &str) -> Result<()> {
    let dom = hv.manage_domain(name)?;
    let uefi = roxmltree::Document::parse(&inactive_xml(&dom)?)?
        .root_element()
        .children()
        .find(|n| n.has_tag_name("os"))
        .is_some_and(|os| os.attribute("firmware") == Some("efi"));
    let flags = if uefi {
        sys::VIR_DOMAIN_START_RESET_NVRAM
    } else {
        0
    };
    dom.create_with_flags(flags)
        .map_err(|e| anyhow!("cannot start '{name}': {}", e.message()))?;
    Ok(())
}

/// Replaces every `device='disk'` disk in a domain's XML with `disks`, keeping CD-ROMs and everything else.
/// Returns the new XML and the disks it removed.
fn replace_disks(xml: &str, disks: &[Disk<'_>]) -> Result<(String, Vec<DiskInfo>)> {
    let removed: Vec<DiskInfo> = parse_disks(xml)?
        .into_iter()
        .filter(|d| d.device == "disk")
        .collect();
    let Some(first) = removed.first() else {
        bail!("the VM has no disk to replace");
    };
    let insert_at = first.range.start;
    let mut out = String::with_capacity(xml.len());
    let mut pos = 0;
    for d in &removed {
        out.push_str(&xml[pos..d.range.start]);
        if d.range.start == insert_at {
            out.push_str(&disk_xml(disks).concat());
        }
        pos = d.range.end;
    }
    out.push_str(&xml[pos..]);
    Ok((out, removed))
}

/// Replaces the owner element in a domain's XML. Refuses XML without one: the owner element is never added to a
/// VM that lacks it.
fn replace_owner(xml: &str, owner: &Owner) -> Result<String> {
    let doc = roxmltree::Document::parse(xml)?;
    let node = child(doc.root_element(), "metadata")
        .and_then(|md| md.children().find(|n| n.has_tag_name((OWNER_NS, "owner"))))
        .ok_or_else(|| anyhow!("the VM has no owner element"))?;
    let range = node.range();
    Ok(format!(
        "{}{}{}",
        &xml[..range.start],
        owner.element(),
        &xml[range.end..]
    ))
}

/// Owned VMs with an expiry, as (UUID, owner) pairs, for the sweep to choose from.
pub fn owned_with_expiry(hv: &Hypervisor) -> Result<Vec<(String, Owner)>> {
    hv.with_conn(|conn| {
        Ok(conn
            .list_all_domains(0)?
            .iter()
            .filter_map(|d| {
                let owner = owner_of(d)?;
                owner.expires?;
                Some((d.get_uuid_string().ok()?, owner))
            })
            .collect())
    })
}

/// Deletes the VM with `uuid` if it is still owned and expired at `now`, re-read just before acting, so a VM
/// deleted and recreated meanwhile is left alone. Powers it off first, then deletes its pool volumes, then
/// undefines it, so a failure part-way leaves a domain the next sweep finds again. Returns what was done, or
/// `None` when there was nothing to do.
pub fn expire(hv: &Hypervisor, uuid: &str, now: u64) -> Result<Option<String>> {
    let m = &hv.config.manage;
    let _guard = lock()?;
    hv.with_conn(|conn| {
        // Another process may delete the VM at any step; finding it gone means the work is done.
        let gone = |e: &virt::error::Error| e.code() == ErrorNumber::NoDomain;
        let dom = match Domain::lookup_by_uuid_string(conn, uuid) {
            Ok(dom) => dom,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => bail!("cannot look up VM {uuid}: {}", e.message()),
        };
        if !should_expire(owner_of(&dom).as_ref(), now) {
            return Ok(None);
        }
        let name = dom.get_name()?;
        match dom.is_active() {
            Ok(true) => match dom.destroy() {
                Ok(()) => {}
                Err(e) if gone(&e) => return Ok(None),
                Err(e) => bail!("cannot power off '{name}': {}", e.message()),
            },
            Ok(false) => {}
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => bail!("cannot read the state of '{name}': {}", e.message()),
        }
        let xml = match dom.get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE) {
            Ok(xml) => xml,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) => bail!("cannot read '{name}': {}", e.message()),
        };
        let disks: Vec<DiskInfo> = parse_disks(&xml)?
            .into_iter()
            .filter(|d| d.device == "disk")
            .collect();
        let pool = disk_pool(conn, m)?;
        let outcomes = delete_disks_boot_last(&disks, |d| {
            match pool_volume(conn, &pool, &m.disk_pool, d) {
                Some(vol) => match vol.delete(0) {
                    Ok(()) => DiskDeletion::Deleted,
                    Err(e) => DiskDeletion::Failed(e.message().to_string()),
                },
                None => DiskDeletion::NotInPool,
            }
        });
        // A volume left behind keeps the domain, so the next sweep finds the VM again and retries it.
        let msg = expiry_report(&name, uuid, &outcomes)?;
        match dom.undefine_flags(UNDEFINE_FLAGS) {
            Ok(_) => Ok(Some(msg)),
            Err(e) if gone(&e) => Ok(None),
            Err(e) => bail!("cannot undefine '{name}': {}", e.message()),
        }
    })
}

/// What happened to one disk of an expiring VM.
#[derive(Debug, PartialEq, Eq)]
enum DiskDeletion {
    Deleted,
    /// Not a volume in `manage.disk_pool`: already deleted, or outside it. It is left in place.
    NotInPool,
    Failed(String),
}

/// Deletes `disks` with `delete` in reverse document order, so the boot disk, which comes first, goes last, and stops
/// at the first failure. A VM kept after a failed deletion therefore still has its boot disk and every disk before
/// the one that failed. Returns each tried disk's source and outcome.
fn delete_disks_boot_last(
    disks: &[DiskInfo],
    mut delete: impl FnMut(&DiskInfo) -> DiskDeletion,
) -> Vec<(String, DiskDeletion)> {
    let mut outcomes = Vec::new();
    for d in disks.iter().rev() {
        let outcome = delete(d);
        let failed = matches!(outcome, DiskDeletion::Failed(_));
        outcomes.push((d.source(), outcome));
        if failed {
            break;
        }
    }
    outcomes
}

/// The log line for an expiry whose disks fared as `outcomes`, given as (source path, outcome) pairs.
///
/// # Errors
///
/// Fails when any pool volume could not be deleted, so the caller keeps the domain for the next sweep to retry.
fn expiry_report(name: &str, uuid: &str, outcomes: &[(String, DiskDeletion)]) -> Result<String> {
    let failed: Vec<String> = outcomes
        .iter()
        .filter_map(|(source, o)| match o {
            DiskDeletion::Failed(why) => Some(format!("{source}: {why}")),
            _ => None,
        })
        .collect();
    if !failed.is_empty() {
        bail!(
            "could not delete disk {}; VM '{name}' is kept so the next sweep retries it",
            failed.join(", disk ")
        );
    }
    let mut msg = format!("expired VM '{name}' ({uuid}) deleted");
    for (source, o) in outcomes {
        match o {
            DiskDeletion::Deleted => msg.push_str(&format!("; deleted disk {source}")),
            DiskDeletion::NotInPool => msg.push_str(&format!(
                "; disk {source} is not a volume in manage.disk_pool (already deleted, or outside it), so it was not deleted"
            )),
            DiskDeletion::Failed(_) => {}
        }
    }
    Ok(msg)
}

/// Whether the sweep acts on a VM with this owner element: only a VM this server made, with an expiry that has
/// passed at `now`.
fn should_expire(owner: Option<&Owner>, now: u64) -> bool {
    owner.is_some_and(|o| is_expired(o, now))
}

/// Whether an owner's expiry has passed at `now`. VMs without an expiry never expire.
pub fn is_expired(owner: &Owner, now: u64) -> bool {
    owner.expires.is_some_and(|at| at <= now)
}

/// Undefines a shut-off VM and deletes its disk volumes that are in the disk pool.
pub fn delete(hv: &Hypervisor, name: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    if dom.is_active()? {
        bail!("VM '{name}' is running; shut it down with stop_vm first");
    }
    let _guard = lock()?;
    let disks: Vec<DiskInfo> = parse_disks(&inactive_xml(&dom)?)?
        .into_iter()
        .filter(|d| d.device == "disk")
        .collect();
    hv.with_conn(|conn| {
        let m = &hv.config.manage;
        let pool = if m.disk_pool.is_empty() {
            None
        } else {
            Some(disk_pool(conn, m)?)
        };
        let mut remove = Vec::new();
        let mut kept = Vec::new();
        for d in &disks {
            match pool
                .as_ref()
                .and_then(|p| pool_volume(conn, p, &m.disk_pool, d))
            {
                Some(vol) => remove.push(vol),
                None => kept.push(d.source()),
            }
        }
        dom.undefine_flags(UNDEFINE_FLAGS)
            .map_err(|e| anyhow!("cannot undefine '{name}': {}", e.message()))?;

        let mut msg = format!("deleted VM '{name}'");
        for vol in remove {
            let path = vol.get_path().unwrap_or_default();
            match vol.delete(0) {
                Ok(()) => msg.push_str(&format!("; deleted disk {path}")),
                Err(e) => msg.push_str(&format!("; could not delete disk {path}: {}", e.message())),
            }
        }
        if !kept.is_empty() {
            msg.push_str(&format!(
                "; left disks outside manage.disk_pool in place: {}",
                kept.join(", ")
            ));
        }
        Ok(msg)
    })
}

/// Hardware summary of a VM's persistent configuration.
pub fn describe(hv: &Hypervisor, name: &str) -> Result<Value> {
    let dom = hv.manage_domain(name)?;
    let xml = inactive_xml(&dom)?;
    let doc = roxmltree::Document::parse(&xml)?;
    let root = doc.root_element();
    let text = |tag: &str| {
        child(root, tag)
            .and_then(|n| n.text())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let vcpu = child(root, "vcpu");
    let os = child(root, "os");
    let uefi =
        os.is_some_and(|o| o.attribute("firmware") == Some("efi") || child(o, "loader").is_some());
    let devices = child(root, "devices");
    let has = |tag: &str| devices.is_some_and(|d| child(d, tag).is_some());

    let m = &hv.config.manage;
    let disks: Vec<Value> = hv.with_conn(|conn| {
        let pool = if m.disk_pool.is_empty() { None } else { disk_pool(conn, m).ok() };
        Ok(parse_disks(&xml)?
            .iter()
            .map(|d| {
                let in_pool = d.device == "disk" && pool.as_ref().and_then(|p| pool_volume(conn, p, &m.disk_pool, d)).is_some();
                let mut v = json!({ "target": d.target, "device": d.device, "bus": d.bus, "source": d.source() });
                if d.device == "disk" {
                    v["in_disk_pool"] = json!(in_pool);
                }
                v
            })
            .collect())
    })?;
    Ok(json!({
        "name": name,
        "state": hypervisor::state_name(&dom),
        "created_by_this_server": is_owned(&dom),
        "vcpus": vcpu.and_then(|v| v.attribute("current")).map(str::to_string).unwrap_or_else(|| text("vcpu")),
        "max_vcpus": text("vcpu"),
        "memory_mib": kib_text(&xml, "currentMemory").or_else(|| kib_text(&xml, "memory")).map(|k| k / 1024),
        "firmware": if uefi { "uefi" } else { "bios" },
        "tpm": has("tpm"),
        "disks": disks,
        "interfaces": parse_nics(&xml)?,
        "note": "vcpus/memory are the persistent configuration and take effect at the next boot",
    }))
}

/// Changes vCPUs and/or memory in the persistent configuration (applies at next boot).
pub fn set_resources(
    hv: &Hypervisor,
    name: &str,
    vcpus: Option<u32>,
    memory_mib: Option<u64>,
) -> Result<String> {
    let m = &hv.config.manage;
    if vcpus.is_none() && memory_mib.is_none() {
        bail!("provide vcpus and/or memory_mib");
    }
    let dom = hv.manage_domain(name)?;
    let xml = inactive_xml(&dom)?;
    let doc = roxmltree::Document::parse(&xml)?;
    let cfg = sys::VIR_DOMAIN_AFFECT_CONFIG;
    let mut done = Vec::new();
    if let Some(n) = vcpus {
        check_vcpus(m, n)?;
        let max: u32 = child(doc.root_element(), "vcpu")
            .and_then(|v| v.text())
            .and_then(|t| t.trim().parse().ok())
            .context("cannot read the VM's vcpu count")?;
        // The current count may never exceed the maximum, so order the two calls.
        let steps = if n >= max {
            [cfg | sys::VIR_DOMAIN_VCPU_MAXIMUM, cfg]
        } else {
            [cfg, cfg | sys::VIR_DOMAIN_VCPU_MAXIMUM]
        };
        for flags in steps {
            dom.set_vcpus_flags(n, flags)
                .map_err(|e| anyhow!("cannot set vCPUs: {}", e.message()))?;
        }
        done.push(format!("{n} vCPUs"));
    }
    if let Some(mib) = memory_mib {
        check_memory(m, mib)?;
        let kib = mib * 1024;
        let max = kib_text(&xml, "memory").context("cannot read the VM's memory size")?;
        let steps = if kib >= max {
            [cfg | sys::VIR_DOMAIN_MEM_MAXIMUM, cfg]
        } else {
            [cfg, cfg | sys::VIR_DOMAIN_MEM_MAXIMUM]
        };
        for flags in steps {
            dom.set_memory_flags(kib, flags)
                .map_err(|e| anyhow!("cannot set memory: {}", e.message()))?;
        }
        done.push(format!("{mib} MiB memory"));
    }
    let mut msg = format!("VM '{name}' configured with {}", done.join(" and "));
    if dom.is_active()? {
        msg.push_str(
            "; the VM is running, so this takes effect after it is shut down and started again",
        );
    }
    Ok(msg)
}

/// Creates a new volume in the disk pool and attaches it as a virtio disk.
pub fn attach_disk(hv: &Hypervisor, name: &str, size_gib: u64) -> Result<String> {
    let m = &hv.config.manage;
    check_disk(m, size_gib)?;
    let dom = hv.manage_domain(name)?;
    let _guard = lock()?;
    let used: Vec<String> = parse_disks(&inactive_xml(&dom)?)?
        .into_iter()
        .map(|d| d.target)
        .collect();
    let target = ('b'..='z')
        .map(|c| format!("vd{c}"))
        .find(|t| !used.contains(t))
        .context("no free virtio disk target left")?;
    hv.with_conn(|conn| {
        let pool = disk_pool(conn, m)?;
        let vol_name = format!("{name}-{target}.qcow2");
        let vol = create_volume(&pool, &vol_name, size_gib)?;
        // A file path rather than a pool/volume reference, like virt-manager writes it:
        // some distributions' AppArmor setup does not label volume-type disks.
        let xml = format!(
            "<disk type='file' device='disk'><driver name='qemu' type='qcow2' discard='unmap'/>\
             <source file='{}'/><target dev='{target}' bus='virtio'/></disk>",
            esc(&vol.get_path()?)
        );
        if let Err(e) = dom.attach_device_flags(&xml, live_and_config(&dom)?) {
            let _ = vol.delete(0);
            bail!("cannot attach disk: {}{}", e.message(), running_hint(&dom));
        }
        Ok(format!(
            "attached new {size_gib} GiB disk {}/{vol_name} to '{name}' as {target}",
            m.disk_pool
        ))
    })
}

/// Detaches a disk from a shut-off VM and deletes its volume if it is in the disk pool.
pub fn detach_disk(hv: &Hypervisor, name: &str, target: &str) -> Result<String> {
    let m = &hv.config.manage;
    let dom = hv.manage_domain(name)?;
    if dom.is_active()? {
        bail!(
            "VM '{name}' is running; shut it down with stop_vm first (the disk is deleted after detaching)"
        );
    }
    let _guard = lock()?;
    let xml = inactive_xml(&dom)?;
    let disk = parse_disks(&xml)?
        .into_iter()
        .find(|d| d.target == target && d.device == "disk")
        .with_context(|| format!("VM '{name}' has no disk with target '{target}' (CD-ROMs are changed with set_cdrom)"))?;
    hv.with_conn(|conn| {
        let vol = if m.disk_pool.is_empty() {
            None
        } else {
            pool_volume(conn, &disk_pool(conn, m)?, &m.disk_pool, &disk)
        };
        dom.detach_device_flags(&xml[disk.range.clone()], sys::VIR_DOMAIN_AFFECT_CONFIG)
            .map_err(|e| anyhow!("cannot detach {target}: {}", e.message()))?;
        let mut msg = format!("detached {target} from '{name}'");
        match vol {
            Some(vol) => match vol.delete(0) {
                Ok(()) => msg.push_str(&format!("; deleted {}", disk.source())),
                Err(e) => msg.push_str(&format!(
                    "; could not delete {}: {}",
                    disk.source(),
                    e.message()
                )),
            },
            None => msg.push_str(&format!(
                "; {} is outside manage.disk_pool and was left in place",
                disk.source()
            )),
        }
        Ok(msg)
    })
}

/// Inserts an ISO into a CD-ROM drive, or ejects it with `iso: None`. Without
/// `target` the first CD-ROM drive is used; a shut-off VM without one gets a new drive.
pub fn set_cdrom(
    hv: &Hypervisor,
    name: &str,
    target: Option<&str>,
    iso: Option<&str>,
) -> Result<String> {
    let iso = iso.map(|p| hv.config.iso_file(p)).transpose()?;
    let dom = hv.manage_domain(name)?;
    let _guard = lock()?;
    let active = dom.is_active()?;
    // The live definition carries the fields libvirt compares when changing media.
    let xml = if active {
        dom.get_xml_desc(0)?
    } else {
        inactive_xml(&dom)?
    };
    let disks = parse_disks(&xml)?;
    let source = iso
        .as_deref()
        .map(|p| format!("<source file='{}'/>", esc(&p.to_string_lossy())))
        .unwrap_or_default();
    let what = match &iso {
        Some(p) => format!("inserted {}", p.display()),
        None => "ejected the medium".to_string(),
    };
    let cdrom = disks
        .iter()
        .find(|d| d.device == "cdrom" && target.is_none_or(|t| d.target == t));
    let Some(cdrom) = cdrom else {
        if let Some(t) = target {
            bail!("VM '{name}' has no CD-ROM drive '{t}'");
        }
        if active {
            bail!("VM '{name}' has no CD-ROM drive; shut it down to add one");
        }
        let used: Vec<&str> = disks.iter().map(|d| d.target.as_str()).collect();
        let t = ('a'..='z')
            .map(|c| format!("sd{c}"))
            .find(|t| !used.contains(&t.as_str()))
            .context("no free SATA target left")?;
        let dev = format!(
            "<disk type='file' device='cdrom'><driver name='qemu' type='raw'/>{source}<target dev='{t}' bus='sata'/><readonly/></disk>"
        );
        dom.attach_device_flags(&dev, sys::VIR_DOMAIN_AFFECT_CONFIG)
            .map_err(|e| anyhow!("cannot add a CD-ROM drive: {}", e.message()))?;
        return Ok(format!("added CD-ROM drive {t} to '{name}' and {what}"));
    };

    // Keep the drive's definition and swap only its <source>.
    let el = &xml[cdrom.range.clone()];
    let doc = roxmltree::Document::parse(el)?;
    let root = doc.root_element();
    let body_start = el.find('>').context("malformed disk element")? + 1;
    // Replace the old <source> in place, or insert the new one before <target>.
    let cut = match (child(root, "source"), child(root, "target")) {
        (Some(s), _) => s.range(),
        (None, Some(t)) => t.range().start..t.range().start,
        (None, None) => body_start..body_start,
    };
    let dev = format!(
        "<disk type='file' device='cdrom'>{}{source}{}",
        &el[body_start..cut.start],
        &el[cut.end..]
    );
    dom.update_device_flags(&dev, live_and_config(&dom)?)
        .map_err(|e| anyhow!("cannot change media in {}: {}", cdrom.target, e.message()))?;
    Ok(format!("{what} in {} of '{name}'", cdrom.target))
}

pub fn attach_nic(hv: &Hypervisor, name: &str, network: &str) -> Result<String> {
    let m = &hv.config.manage;
    let dom = hv.manage_domain(name)?;
    hv.with_conn(|conn| check_network(conn, m, network))?;
    let before: Vec<String> = parse_nics(&inactive_xml(&dom)?)?
        .into_iter()
        .map(|n| n.mac)
        .collect();
    let xml = format!(
        "<interface type='network'><source network='{}'/><model type='virtio'/></interface>",
        esc(network)
    );
    dom.attach_device_flags(&xml, live_and_config(&dom)?)
        .map_err(|e| {
            anyhow!(
                "cannot attach network interface: {}{}",
                e.message(),
                running_hint(&dom)
            )
        })?;
    let mac = parse_nics(&inactive_xml(&dom)?)?
        .into_iter()
        .map(|n| n.mac)
        .find(|mac| !before.contains(mac))
        .unwrap_or_default();
    Ok(format!(
        "attached a network interface on '{network}' to '{name}' (MAC {mac})"
    ))
}

pub fn detach_nic(hv: &Hypervisor, name: &str, mac: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    let xml = inactive_xml(&dom)?;
    let nic = parse_nics(&xml)?
        .into_iter()
        .find(|n| n.mac.eq_ignore_ascii_case(mac))
        .with_context(|| format!("VM '{name}' has no network interface with MAC {mac}"))?;
    dom.detach_device_flags(&xml[nic.range.clone()], live_and_config(&dom)?)
        .map_err(|e| anyhow!("cannot detach network interface: {}", e.message()))?;
    Ok(format!(
        "detached network interface {} from '{name}'",
        nic.mac
    ))
}

pub fn snapshot_create(
    hv: &Hypervisor,
    name: &str,
    snapshot: &str,
    description: Option<&str>,
) -> Result<String> {
    check_name(snapshot, "snapshot name")?;
    let dom = hv.manage_domain(name)?;
    let xml = format!(
        "<domainsnapshot><name>{}</name><description>{}</description></domainsnapshot>",
        esc(snapshot),
        esc(description.unwrap_or_default())
    );
    DomainSnapshot::create_xml(&dom, &xml, 0).map_err(|e| {
        let mut msg = format!("cannot create snapshot '{snapshot}': {}", e.message());
        if e.message().contains("pflash") {
            msg.push_str(
                " (a running UEFI VM cannot be snapshotted here; shut it down with stop_vm first)",
            );
        }
        anyhow!(msg)
    })?;
    Ok(format!(
        "created snapshot '{snapshot}' of '{name}' ({})",
        hypervisor::state_name(&dom)
    ))
}

pub fn snapshot_list(hv: &Hypervisor, name: &str) -> Result<Vec<Value>> {
    let dom = hv.manage_domain(name)?;
    let mut out = Vec::new();
    for snap in dom.list_all_snapshots(0)? {
        let xml = snap.get_xml_desc(0)?;
        let doc = roxmltree::Document::parse(&xml)?;
        let root = doc.root_element();
        let text = |tag: &str| child(root, tag).and_then(|n| n.text()).map(str::to_string);
        out.push(json!({
            "name": text("name"),
            "description": text("description"),
            "vm_state": text("state"),
            "created": text("creationTime").and_then(|t| t.parse::<i64>().ok()),
            "parent": child(root, "parent").and_then(|p| child(p, "name")).and_then(|n| n.text()),
            "current": snap.is_current(0).unwrap_or(false),
        }));
    }
    out.sort_by_key(|v| v["created"].as_i64());
    Ok(out)
}

pub fn snapshot_revert(hv: &Hypervisor, name: &str, snapshot: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    let snap = DomainSnapshot::lookup_by_name(&dom, snapshot, 0)
        .map_err(|e| anyhow!("snapshot '{snapshot}' not found: {}", e.message()))?;
    snap.revert(0)
        .map_err(|e| anyhow!("cannot revert to '{snapshot}': {}", e.message()))?;
    Ok(format!(
        "reverted '{name}' to snapshot '{snapshot}'; the VM is now {}",
        hypervisor::state_name(&dom)
    ))
}

pub fn snapshot_delete(hv: &Hypervisor, name: &str, snapshot: &str) -> Result<String> {
    let dom = hv.manage_domain(name)?;
    let snap = DomainSnapshot::lookup_by_name(&dom, snapshot, 0)
        .map_err(|e| anyhow!("snapshot '{snapshot}' not found: {}", e.message()))?;
    snap.delete(0)
        .map_err(|e| anyhow!("cannot delete snapshot '{snapshot}': {}", e.message()))?;
    Ok(format!("deleted snapshot '{snapshot}' of '{name}'"))
}

#[derive(Serialize)]
pub struct IsoFile {
    path: String,
    size_bytes: u64,
}

/// `.iso` files in `manage.iso_dirs` (searched up to three directory levels deep).
pub fn list_isos(m: &Manage) -> Vec<IsoFile> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<IsoFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() && depth < 3 {
                walk(&path, depth + 1, out);
            } else if meta.is_file()
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
            {
                out.push(IsoFile {
                    path: path.to_string_lossy().into_owned(),
                    size_bytes: meta.len(),
                });
            }
        }
    }
    let mut out = Vec::new();
    for dir in &m.iso_dirs {
        walk(dir, 0, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---- helpers ------------------------------------------------------------------

fn check_name(name: &str, what: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with(['.', '-']);
    if !ok {
        bail!(
            "{what} must be 1-64 characters of A-Z, a-z, 0-9, '.', '_', '-' and not start with '.' or '-'"
        );
    }
    Ok(())
}

fn check_vcpus(m: &Manage, n: u32) -> Result<()> {
    if n == 0 || n > m.max_vcpus {
        bail!(
            "vcpus must be between 1 and {} (manage.max_vcpus)",
            m.max_vcpus
        );
    }
    Ok(())
}

fn check_memory(m: &Manage, mib: u64) -> Result<()> {
    if !(256..=m.max_memory_mib).contains(&mib) {
        bail!(
            "memory_mib must be between 256 and {} (manage.max_memory_mib)",
            m.max_memory_mib
        );
    }
    Ok(())
}

fn check_disk(m: &Manage, gib: u64) -> Result<()> {
    if gib == 0 || gib > m.max_disk_gib {
        bail!(
            "disk size must be between 1 and {} GiB (manage.max_disk_gib)",
            m.max_disk_gib
        );
    }
    Ok(())
}

fn check_network(conn: &Connect, m: &Manage, network: &str) -> Result<()> {
    if !m.networks.iter().any(|n| n == network) {
        bail!(
            "network '{network}' is not in manage.networks ({})",
            m.networks.join(", ")
        );
    }
    Network::lookup_by_name(conn, network)
        .map_err(|e| anyhow!("network '{network}': {}", e.message()))?;
    Ok(())
}

fn disk_pool(conn: &Connect, m: &Manage) -> Result<StoragePool> {
    if m.disk_pool.is_empty() {
        bail!("manage.disk_pool is not set");
    }
    let pool = StoragePool::lookup_by_name(conn, &m.disk_pool)
        .map_err(|e| anyhow!("storage pool '{}': {}", m.disk_pool, e.message()))?;
    if !pool.is_active()? {
        bail!("storage pool '{}' is not active", m.disk_pool);
    }
    let _ = pool.refresh(0);
    Ok(pool)
}

fn create_volume(pool: &StoragePool, name: &str, gib: u64) -> Result<StorageVol> {
    if StorageVol::lookup_by_name(pool, name).is_ok() {
        bail!("volume '{name}' already exists in the disk pool");
    }
    let xml = format!(
        "<volume><name>{}</name><capacity unit='GiB'>{gib}</capacity><allocation>0</allocation>\
         <target><format type='qcow2'/></target></volume>",
        esc(name)
    );
    StorageVol::create_xml(pool, &xml, 0)
        .map_err(|e| anyhow!("cannot create volume '{name}': {}", e.message()))
}

/// Creates a qcow2 overlay of `capacity` bytes whose backing file is the qcow2 at `backing`.
fn create_overlay(
    pool: &StoragePool,
    name: &str,
    capacity: u64,
    backing: &str,
) -> Result<StorageVol> {
    if StorageVol::lookup_by_name(pool, name).is_ok() {
        bail!("volume '{name}' already exists in the disk pool");
    }
    StorageVol::create_xml(pool, &overlay_xml(name, capacity, backing), 0)
        .map_err(|e| anyhow!("cannot create volume '{name}': {}", e.message()))
}

fn overlay_xml(name: &str, capacity: u64, backing: &str) -> String {
    format!(
        "<volume><name>{}</name><capacity unit='bytes'>{capacity}</capacity><allocation>0</allocation>\
         <target><format type='qcow2'/></target>\
         <backingStore><path>{}</path><format type='qcow2'/></backingStore></volume>",
        esc(name),
        esc(backing)
    )
}

/// Creates a raw volume holding exactly `data`, uploaded through a libvirt stream.
fn create_raw_volume(
    conn: &Connect,
    pool: &StoragePool,
    name: &str,
    data: &[u8],
) -> Result<StorageVol> {
    if StorageVol::lookup_by_name(pool, name).is_ok() {
        bail!("volume '{name}' already exists in the disk pool");
    }
    let xml = format!(
        "<volume><name>{}</name><capacity unit='bytes'>{}</capacity><allocation>0</allocation>\
         <target><format type='raw'/></target></volume>",
        esc(name),
        data.len()
    );
    let vol = StorageVol::create_xml(pool, &xml, 0)
        .map_err(|e| anyhow!("cannot create volume '{name}': {}", e.message()))?;
    if let Err(e) = upload(conn, &vol, data) {
        let _ = vol.delete(0);
        return Err(e.context(format!("cannot upload volume '{name}'")));
    }
    Ok(vol)
}

/// Writes `data` to the start of `vol`. Every error path aborts the stream: the `virt` crate's `Stream` panics on
/// drop when freeing an unfinished stream fails.
fn upload(conn: &Connect, vol: &StorageVol, data: &[u8]) -> Result<()> {
    let stream = Stream::new(conn, 0).map_err(|e| anyhow!("{}", e.message()))?;
    if let Err(e) = vol.upload(&stream, 0, data.len() as u64, 0) {
        let _ = stream.abort();
        bail!("{}", e.message());
    }
    let mut sent = 0;
    while sent < data.len() {
        match stream.send(&data[sent..]) {
            Ok(0) => {
                let _ = stream.abort();
                bail!("the stream accepted no data");
            }
            Ok(n) => sent += n,
            Err(e) => {
                let _ = stream.abort();
                bail!("{}", e.message());
            }
        }
    }
    stream.finish().map_err(|e| anyhow!("{}", e.message()))
}

/// A new random UUID for a domain, chosen before the domain exists because its seed's instance ID contains it.
fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The pool volume backing `disk`, if it lives in the pool named `pool_name`.
fn pool_volume(
    conn: &Connect,
    pool: &StoragePool,
    pool_name: &str,
    disk: &DiskInfo,
) -> Option<StorageVol> {
    if let (Some(p), Some(v)) = (&disk.pool, &disk.volume) {
        return if p == pool_name {
            StorageVol::lookup_by_name(pool, v).ok()
        } else {
            None
        };
    }
    let vol = StorageVol::lookup_by_path(conn, disk.file.as_deref()?).ok()?;
    let owner = StoragePool::lookup_by_volume(&vol).ok()?.get_name().ok()?;
    (owner == pool_name).then_some(vol)
}

/// Device changes go to the persistent config, and to the live VM too if it is running.
fn live_and_config(dom: &Domain) -> Result<u32> {
    let mut flags = sys::VIR_DOMAIN_AFFECT_CONFIG;
    if dom.is_active()? {
        flags |= sys::VIR_DOMAIN_AFFECT_LIVE;
    }
    Ok(flags)
}

/// Suffix for hot-plug errors: the same change usually works on a shut-off VM.
fn running_hint(dom: &Domain) -> &'static str {
    if dom.is_active().unwrap_or(false) {
        " (the VM is running; shutting it down with stop_vm and retrying may work)"
    } else {
        ""
    }
}

fn inactive_xml(dom: &Domain) -> Result<String> {
    Ok(dom.get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)?)
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, tag: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(tag))
}

/// Value of a top-level memory element (`memory`, `currentMemory`) in KiB.
fn kib_text(xml: &str, tag: &str) -> Option<u64> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let n = child(doc.root_element(), tag)?;
    let v: u64 = n.text()?.trim().parse().ok()?;
    let factor = match n.attribute("unit").unwrap_or("KiB") {
        "b" | "bytes" => return Some(v / 1024),
        "KiB" | "k" => 1,
        "MiB" | "M" => 1024,
        "GiB" | "G" => 1024 * 1024,
        _ => return None,
    };
    Some(v * factor)
}

struct DiskInfo {
    device: String,
    target: String,
    bus: String,
    file: Option<String>,
    pool: Option<String>,
    volume: Option<String>,
    /// Byte range of the `<disk>` element in the XML it was parsed from.
    range: std::ops::Range<usize>,
}

impl DiskInfo {
    fn source(&self) -> String {
        match (&self.file, &self.pool, &self.volume) {
            (Some(f), _, _) => f.clone(),
            (None, Some(p), Some(v)) => format!("{p}/{v}"),
            _ => "(empty)".into(),
        }
    }
}

fn parse_disks(xml: &str) -> Result<Vec<DiskInfo>> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(devices) = child(doc.root_element(), "devices") else {
        return Ok(Vec::new());
    };
    Ok(devices
        .children()
        .filter(|n| n.has_tag_name("disk"))
        .map(|d| {
            let source = child(d, "source");
            let src = |a: &str| source.and_then(|s| s.attribute(a)).map(str::to_string);
            let target = child(d, "target");
            DiskInfo {
                device: d.attribute("device").unwrap_or("disk").to_string(),
                target: target
                    .and_then(|t| t.attribute("dev"))
                    .unwrap_or_default()
                    .to_string(),
                bus: target
                    .and_then(|t| t.attribute("bus"))
                    .unwrap_or_default()
                    .to_string(),
                file: src("file").or_else(|| src("dev")),
                pool: src("pool"),
                volume: src("volume"),
                range: d.range(),
            }
        })
        .collect())
}

#[derive(Serialize)]
struct NicInfo {
    mac: String,
    #[serde(rename = "type")]
    kind: String,
    source: String,
    model: String,
    #[serde(skip)]
    range: std::ops::Range<usize>,
}

fn parse_nics(xml: &str) -> Result<Vec<NicInfo>> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(devices) = child(doc.root_element(), "devices") else {
        return Ok(Vec::new());
    };
    Ok(devices
        .children()
        .filter(|n| n.has_tag_name("interface"))
        .map(|i| {
            let attr = |tag: &str, a: &str| {
                child(i, tag)
                    .and_then(|n| n.attribute(a))
                    .unwrap_or_default()
                    .to_string()
            };
            let source = child(i, "source")
                .and_then(|s| {
                    s.attribute("network")
                        .or(s.attribute("bridge"))
                        .or(s.attribute("dev"))
                })
                .unwrap_or_default()
                .to_string();
            NicInfo {
                mac: attr("mac", "address"),
                kind: i.attribute("type").unwrap_or_default().to_string(),
                source,
                model: attr("model", "type"),
                range: i.range(),
            }
        })
        .collect())
}

/// Escapes text for use in XML content and single- or double-quoted attributes.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

/// A disk of a new domain. Every disk refers to its file by path: a seed given as a pool-volume reference failed
/// to open on a later start (`docs/research/2026-11-10-fatfs-seed-boot.md`).
enum Disk<'a> {
    /// A writable qcow2 disk on the virtio bus.
    Qcow2(&'a str),
    /// A read-only raw disk on the virtio bus, such as a NoCloud seed.
    ReadOnlyRaw(&'a str),
    /// A SATA CD-ROM, empty when `None`.
    Cdrom(Option<&'a Path>),
}

/// Everything `domain_xml` puts into a new domain.
struct DomainSpec<'a> {
    name: &'a str,
    windows: bool,
    vcpus: u32,
    memory_mib: u64,
    uefi: bool,
    tpm: bool,
    /// In order: the first writable disk boots first, the first CD-ROM second.
    disks: Vec<Disk<'a>>,
    network: Option<&'a str>,
    /// Kept by a reset; libvirt picks one when `None`.
    uuid: Option<&'a str>,
    /// MAC address of the network interface; libvirt picks one when `None`.
    mac: Option<&'a str>,
    owner: Owner,
}

fn disk_xml(disks: &[Disk<'_>]) -> Vec<String> {
    let (mut virtio, mut sata) = (b'a', b'a');
    let (mut disk_boot, mut cdrom_boot) = (false, false);
    disks
        .iter()
        .map(|d| match d {
            Disk::Qcow2(path) | Disk::ReadOnlyRaw(path) => {
                let dev = format!("vd{}", char::from(virtio));
                virtio += 1;
                let (driver, tail) = match d {
                    Disk::Qcow2(_) if !disk_boot => {
                        disk_boot = true;
                        ("type='qcow2' discard='unmap'", "<boot order='1'/>")
                    }
                    Disk::Qcow2(_) => ("type='qcow2' discard='unmap'", ""),
                    _ => ("type='raw'", "<readonly/>"),
                };
                format!(
                    "<disk type='file' device='disk'><driver name='qemu' {driver}/>\
                     <source file='{}'/><target dev='{dev}' bus='virtio'/>{tail}</disk>",
                    esc(path)
                )
            }
            Disk::Cdrom(iso) => {
                let dev = format!("sd{}", char::from(sata));
                sata += 1;
                let source = iso
                    .map(|p| format!("<source file='{}'/>", esc(&p.to_string_lossy())))
                    .unwrap_or_default();
                let boot = if cdrom_boot {
                    ""
                } else {
                    cdrom_boot = true;
                    "<boot order='2'/>"
                };
                format!(
                    "<disk type='file' device='cdrom'><driver name='qemu' type='raw'/>{source}\
                     <target dev='{dev}' bus='sata'/><readonly/>{boot}</disk>"
                )
            }
        })
        .collect()
}

fn domain_xml(spec: &DomainSpec<'_>) -> String {
    // Root ports for every PCIe device plus spares, so disks and NICs can be hot-plugged.
    let mut devices = vec!["<controller type='pci' index='0' model='pcie-root'/>".to_string()];
    devices.extend(std::iter::repeat_n(
        "<controller type='pci' model='pcie-root-port'/>".to_string(),
        14,
    ));
    devices.extend(disk_xml(&spec.disks));
    if let Some(n) = spec.network {
        let mac = spec
            .mac
            .map(|m| format!("<mac address='{}'/>", esc(m)))
            .unwrap_or_default();
        devices.push(format!(
            "<interface type='network'>{mac}<source network='{}'/><model type='virtio'/></interface>",
            esc(n)
        ));
    }
    devices.extend(
        [
            "<controller type='usb' model='qemu-xhci'/>",
            "<input type='tablet' bus='usb'/>",
            "<channel type='unix'><target type='virtio' name='org.qemu.guest_agent.0'/></channel>",
            "<serial type='pty'><target type='isa-serial'/></serial>",
            "<console type='pty'><target type='serial'/></console>",
            "<graphics type='spice' autoport='yes'><listen type='address'/><image compression='off'/></graphics>",
            "<video><model type='virtio'/></video>",
            "<memballoon model='virtio'/>",
            "<rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>",
        ]
        .map(String::from),
    );
    if spec.tpm {
        devices.push("<tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>".into());
    }
    let features = if spec.windows {
        "<acpi/><apic/><hyperv mode='custom'><relaxed state='on'/><vapic state='on'/>\
         <spinlocks state='on' retries='8191'/><vpindex state='on'/><synic state='on'/>\
         <stimer state='on'/><frequencies state='on'/></hyperv>"
    } else {
        "<acpi/><apic/>"
    };
    let clock = if spec.windows {
        "<clock offset='localtime'><timer name='rtc' tickpolicy='catchup'/><timer name='pit' tickpolicy='delay'/>\
         <timer name='hpet' present='no'/><timer name='hypervclock' present='yes'/></clock>"
    } else {
        "<clock offset='utc'><timer name='rtc' tickpolicy='catchup'/><timer name='pit' tickpolicy='delay'/>\
         <timer name='hpet' present='no'/></clock>"
    };
    format!(
        "<domain type='kvm'>\
         <name>{name}</name>{uuid}\
         <metadata>{owner}</metadata>\
         <memory unit='MiB'>{mem}</memory><currentMemory unit='MiB'>{mem}</currentMemory>\
         <vcpu>{vcpus}</vcpu>\
         <os{firmware}><type arch='x86_64' machine='q35'>hvm</type></os>\
         <features>{features}</features>\
         <cpu mode='host-passthrough'/>\
         {clock}\
         <pm><suspend-to-mem enabled='no'/><suspend-to-disk enabled='no'/></pm>\
         <devices>{devices}</devices>\
         </domain>",
        name = esc(spec.name),
        uuid = spec
            .uuid
            .map(|u| format!("<uuid>{}</uuid>", esc(u)))
            .unwrap_or_default(),
        owner = spec.owner.element(),
        mem = spec.memory_mib,
        vcpus = spec.vcpus,
        firmware = if spec.uefi { " firmware='efi'" } else { "" },
        devices = devices.concat(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one lock test, removed by the test.
    fn lock_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "virtmanager-mcp-lock-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_held_lock_file_makes_another_holder_wait_and_then_fail() {
        let dir = lock_test_dir("held");
        let path = dir.join("virtmanager-mcp").join("manage.lock");
        // Each open file has its own lock, as in another process, so a second open conflicts with the first.
        let first = lock_file(&path, Duration::ZERO).unwrap();

        let started = Instant::now();
        let err = lock_file(&path, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(
            err.contains("another virtmanager-mcp operation has held the management lock"),
            "{err}"
        );

        drop(first);
        let second = lock_file(&path, Duration::ZERO);
        assert!(second.is_ok(), "{second:?}");
        drop(second);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_waiting_holder_gets_the_lock_file_once_it_is_released() {
        let dir = lock_test_dir("released");
        let path = dir.join("manage.lock");
        let first = lock_file(&path, Duration::ZERO).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(first);
        });
        assert!(lock_file(&path, Duration::from_secs(10)).is_ok());
        releaser.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_lock_file_and_its_directory_belong_to_this_account_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = lock_test_dir("modes");
        let path = dir.join("virtmanager-mcp").join("manage.lock");
        drop(lock_file(&path, Duration::ZERO).unwrap());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_lock_file_sits_in_the_home_directory_cache() {
        assert_eq!(
            lock_path(Some("/home/me".into())).unwrap(),
            Path::new("/home/me/.cache/virtmanager-mcp/manage.lock")
        );
        for home in [None, Some(OsString::new()), Some("home/me".into())] {
            let err = lock_path(home).unwrap_err().to_string();
            assert!(err.contains("HOME is not an absolute path"), "{err}");
        }
    }

    #[test]
    fn two_threads_of_one_process_exclude_each_other() {
        let dir = lock_test_dir("threads");
        let path = dir.join("manage.lock");
        let first = lock_file(&path, Duration::ZERO).unwrap();
        let other = path.clone();
        let err = std::thread::spawn(move || lock_file(&other, Duration::ZERO).map(drop))
            .join()
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(err.contains("has held the management lock"), "{err}");
        drop(first);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names() {
        assert!(check_name("test-vm_1.2", "n").is_ok());
        for bad in ["", "-x", ".x", "a b", "a/b", "a'b", &"x".repeat(65)] {
            assert!(check_name(bad, "n").is_err(), "{bad}");
        }
    }

    fn spec<'a>(disks: Vec<Disk<'a>>) -> DomainSpec<'a> {
        DomainSpec {
            name: "t",
            windows: false,
            vcpus: 2,
            memory_mib: 2048,
            uefi: false,
            tpm: false,
            disks,
            network: Some("net"),
            uuid: None,
            mac: None,
            owner: Owner::default(),
        }
    }

    #[test]
    fn domain_xml_parses_and_escapes() {
        let xml = domain_xml(&DomainSpec {
            windows: true,
            uefi: true,
            tpm: true,
            ..spec(vec![
                Disk::Qcow2("/pool/t.qcow2"),
                Disk::Cdrom(Some(Path::new("/isos/a'b&c.iso"))),
                Disk::Cdrom(None),
            ])
        });
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let disks = parse_disks(&xml).unwrap();
        assert_eq!(disks[0].file.as_deref(), Some("/pool/t.qcow2"));
        assert_eq!(disks[1].file.as_deref(), Some("/isos/a'b&c.iso"));
        let targets: Vec<_> = disks.iter().map(|d| d.target.as_str()).collect();
        assert_eq!(targets, ["vda", "sda", "sdb"]);
        let boots: Vec<_> = doc
            .descendants()
            .filter(|n| n.has_tag_name("boot"))
            .filter_map(|n| n.attribute("order"))
            .collect();
        assert_eq!(boots, ["1", "2"]);
        assert!(doc.descendants().any(|n| n.has_tag_name("tpm")));
        assert!(doc.descendants().any(|n| n.has_tag_name("hyperv")));
        assert_eq!(
            child(doc.root_element(), "os")
                .unwrap()
                .attribute("firmware"),
            Some("efi")
        );
        assert!(child(doc.root_element(), "uuid").is_none());
        assert_eq!(kib_text(&xml, "memory"), Some(2048 * 1024));
        assert_eq!(owner_in_domain_xml(&xml).unwrap(), Some(Owner::default()));
    }

    #[test]
    fn template_domain_xml() {
        let owner = Owner {
            template: Some("debian13".into()),
            generation: Some(2),
            expires: Some(1_794_300_000),
        };
        let xml = domain_xml(&DomainSpec {
            uuid: Some("8a3f0c55-1d2e-4c6b-9f00-5b6d1e2f3a4b"),
            mac: Some("52:54:00:12:34:56"),
            owner: owner.clone(),
            ..spec(vec![
                Disk::Qcow2("/pool/t-g2.qcow2"),
                Disk::ReadOnlyRaw("/pool/t-g2-seed.img"),
            ])
        });
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let disks = parse_disks(&xml).unwrap();
        assert_eq!(disks.len(), 2);
        assert_eq!(
            (disks[1].target.as_str(), disks[1].file.as_deref()),
            ("vdb", Some("/pool/t-g2-seed.img"))
        );
        let seed = doc
            .descendants()
            .filter(|n| n.has_tag_name("disk"))
            .nth(1)
            .unwrap();
        assert!(child(seed, "readonly").is_some());
        assert_eq!(
            child(seed, "driver").unwrap().attribute("type"),
            Some("raw")
        );
        assert!(child(seed, "boot").is_none());
        assert_eq!(
            child(doc.root_element(), "uuid").unwrap().text(),
            Some("8a3f0c55-1d2e-4c6b-9f00-5b6d1e2f3a4b")
        );
        assert_eq!(parse_nics(&xml).unwrap()[0].mac, "52:54:00:12:34:56");
        assert!(
            child(doc.root_element(), "os")
                .unwrap()
                .attribute("firmware")
                .is_none()
        );
        assert_eq!(owner_in_domain_xml(&xml).unwrap(), Some(owner));
    }

    #[test]
    fn owner_element_round_trips() {
        let owner = Owner {
            template: Some("a'b".into()),
            generation: Some(7),
            expires: Some(42),
        };
        assert_eq!(Owner::from_element(&owner.element()).unwrap(), owner);
        assert_eq!(
            Owner::from_element(&Owner::default().element()).unwrap(),
            Owner::default()
        );
        let garbled = format!("<o xmlns='{OWNER_NS}' generation='x' expires='-1'/>");
        assert_eq!(Owner::from_element(&garbled).unwrap(), Owner::default());
    }

    #[test]
    fn domains_without_our_owner_element_are_not_owned() {
        let plain = "<domain><name>x</name></domain>";
        assert_eq!(owner_in_domain_xml(plain).unwrap(), None);
        let other = "<domain><name>x</name><metadata>\
                     <o:owner xmlns:o='http://example.com/owner/1' template='debian13'/>\
                     </metadata></domain>";
        assert_eq!(owner_in_domain_xml(other).unwrap(), None);
        let renamed =
            format!("<domain><metadata><v:creator xmlns:v='{OWNER_NS}'/></metadata></domain>");
        assert_eq!(owner_in_domain_xml(&renamed).unwrap(), None);
    }

    #[test]
    fn overlay_xml_names_backing_file() {
        let xml = overlay_xml("t-g1.qcow2", 10 << 30, "/pool/tpl&.qcow2");
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let backing = doc
            .descendants()
            .find(|n| n.has_tag_name("backingStore"))
            .unwrap();
        assert_eq!(
            child(backing, "path").unwrap().text(),
            Some("/pool/tpl&.qcow2")
        );
        assert_eq!(
            child(doc.root_element(), "capacity").unwrap().text(),
            Some("10737418240")
        );
    }

    #[test]
    fn nested_backing_store_is_not_a_disk_source() {
        // A running overlay VM's live XML names its template inside <backingStore>; only the direct source counts.
        let xml = "<domain><devices><disk type='file' device='disk'>\
                   <source file='/pool/t-g1.qcow2'/>\
                   <backingStore type='file'><format type='qcow2'/><source file='/pool/tpl.qcow2'/></backingStore>\
                   <target dev='vda' bus='virtio'/></disk></devices></domain>";
        let disks = parse_disks(xml).unwrap();
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].file.as_deref(), Some("/pool/t-g1.qcow2"));
    }

    #[test]
    fn template_in_use_by_path_or_pool_volume() {
        let by_path = "<domain><devices><disk type='file' device='disk'>\
                       <source file='/pool/tpl.qcow2'/><target dev='vda'/></disk></devices></domain>";
        let by_volume = "<domain><devices><disk type='volume' device='disk'>\
                         <source pool='agent-vms' volume='tpl.qcow2'/><target dev='vda'/></disk></devices></domain>";
        let other_pool = "<domain><devices><disk type='volume' device='disk'>\
                          <source pool='elsewhere' volume='tpl.qcow2'/><target dev='vda'/></disk></devices></domain>";
        let uses = |xml: &str| {
            uses_volume(
                &parse_disks(xml).unwrap(),
                "/pool/tpl.qcow2",
                "agent-vms",
                "tpl.qcow2",
            )
        };
        assert!(uses(by_path));
        assert!(uses(by_volume));
        assert!(!uses(other_pool));
        let overlay = "<domain><devices><disk type='file' device='disk'><source file='/pool/t-g1.qcow2'/>\
                       <backingStore type='file'><source file='/pool/tpl.qcow2'/></backingStore>\
                       <target dev='vda'/></disk></devices></domain>";
        assert!(!uses(overlay));
    }

    #[test]
    fn sweep_acts_only_on_owned_expired_vms() {
        let owned = |expires| Owner {
            template: Some("debian13".into()),
            generation: Some(1),
            expires,
        };
        // expire() re-reads the owner just before acting, so these are the states it can find.
        assert!(should_expire(Some(&owned(Some(100))), 100));
        assert!(
            !should_expire(None, u64::MAX),
            "a VM this server did not make"
        );
        assert!(
            !should_expire(Some(&owned(None)), u64::MAX),
            "a VM without an expiry"
        );
        assert!(
            !should_expire(Some(&owned(Some(1000))), 100),
            "a VM recreated with a later expiry"
        );
    }

    #[test]
    fn expiry_with_a_failed_volume_deletion_keeps_the_vm() {
        let outcomes = [
            ("/pool/t-g1.qcow2".to_string(), DiskDeletion::Deleted),
            (
                "/pool/t-vdc.qcow2".to_string(),
                DiskDeletion::Failed("Permission denied".into()),
            ),
        ];
        let err = expiry_report("t", "u-1", &outcomes)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/pool/t-vdc.qcow2: Permission denied"),
            "{err}"
        );
        assert!(
            err.contains("is kept so the next sweep retries it"),
            "{err}"
        );
    }

    fn disk_at(file: &str) -> DiskInfo {
        DiskInfo {
            device: "disk".into(),
            target: String::new(),
            bus: String::new(),
            file: Some(file.into()),
            pool: None,
            volume: None,
            range: 0..0,
        }
    }

    #[test]
    fn expiry_deletes_the_boot_disk_last_and_stops_at_a_failure() {
        let disks = [
            disk_at("/pool/t-g1.qcow2"),
            disk_at("/pool/t-g1-seed.img"),
            disk_at("/pool/t-vdc.qcow2"),
        ];
        let mut tried = Vec::new();
        let outcomes = delete_disks_boot_last(&disks, |d| {
            tried.push(d.source());
            DiskDeletion::Deleted
        });
        assert_eq!(
            tried,
            [
                "/pool/t-vdc.qcow2",
                "/pool/t-g1-seed.img",
                "/pool/t-g1.qcow2"
            ]
        );
        assert_eq!(outcomes.len(), 3);

        let mut tried = Vec::new();
        let outcomes = delete_disks_boot_last(&disks, |d| {
            tried.push(d.source());
            DiskDeletion::Failed("Operation not permitted".into())
        });
        assert_eq!(
            tried,
            ["/pool/t-vdc.qcow2"],
            "the seed and the boot disk stay"
        );
        assert!(expiry_report("t", "u-1", &outcomes).is_err());
    }

    #[test]
    fn expiry_report_names_deleted_and_outside_disks() {
        let outcomes = [
            ("/pool/t-g1.qcow2".to_string(), DiskDeletion::Deleted),
            ("/home/u/extra.img".to_string(), DiskDeletion::NotInPool),
        ];
        let msg = expiry_report("t", "u-1", &outcomes).unwrap();
        assert!(msg.starts_with("expired VM 't' (u-1) deleted; deleted disk /pool/t-g1.qcow2"));
        assert!(msg.contains("disk /home/u/extra.img is not a volume in manage.disk_pool"));
        assert!(msg.ends_with("so it was not deleted"));
    }

    #[test]
    fn lifetime_is_capped() {
        let m = Manage {
            max_lifetime_minutes: 60,
            ..Manage::default()
        };
        assert_eq!(expiry(&m, None, 1000).unwrap(), None);
        assert_eq!(expiry(&m, Some(1), 1000).unwrap(), Some(1060));
        assert_eq!(expiry(&m, Some(60), 1000).unwrap(), Some(4600));
        for bad in [0, 61, u32::MAX] {
            assert!(expiry(&m, Some(bad), 1000).is_err(), "{bad}");
        }
    }

    #[test]
    fn generation_volume_names() {
        assert_eq!(
            generation_volumes("t-vm1", 3),
            ("t-vm1-g3.qcow2".into(), "t-vm1-g3-seed.img".into())
        );
    }

    #[test]
    fn volume_format_reads_target() {
        let xml = "<volume><name>v</name><target><path>/p/v</path><format type='qcow2'/></target></volume>";
        assert_eq!(volume_format(xml).unwrap(), "qcow2");
        assert_eq!(
            volume_format("<volume><name>v</name></volume>").unwrap(),
            "unknown"
        );
    }

    /// A domain as libvirt returns it for a VM made from a template, after an agent attached a disk.
    fn template_vm_xml() -> String {
        let mut xml = domain_xml(&DomainSpec {
            uuid: Some("8a3f0c55-1d2e-4c6b-9f00-5b6d1e2f3a4b"),
            mac: Some("52:54:00:12:34:56"),
            owner: Owner {
                template: Some("debian13".into()),
                generation: Some(1),
                expires: Some(99),
            },
            ..spec(vec![
                Disk::Qcow2("/pool/t-g1.qcow2"),
                Disk::ReadOnlyRaw("/pool/t-g1-seed.img"),
                Disk::Cdrom(None),
            ])
        });
        let extra = "<disk type='file' device='disk'><source file='/pool/t-vdc.qcow2'/>\
                     <target dev='vdc' bus='virtio'/></disk>";
        xml = xml.replacen(
            "<controller type='usb'",
            &format!("{extra}<controller type='usb'"),
            1,
        );
        xml
    }

    #[test]
    fn reset_replaces_every_disk_and_keeps_the_rest() {
        let old = template_vm_xml();
        let (new, removed) = replace_disks(
            &old,
            &[
                Disk::Qcow2("/pool/t-g2.qcow2"),
                Disk::ReadOnlyRaw("/pool/t-g2-seed.img"),
            ],
        )
        .unwrap();
        let removed: Vec<_> = removed.iter().map(|d| d.source()).collect();
        assert_eq!(
            removed,
            [
                "/pool/t-g1.qcow2",
                "/pool/t-g1-seed.img",
                "/pool/t-vdc.qcow2"
            ]
        );
        let disks = parse_disks(&new).unwrap();
        let now: Vec<_> = disks
            .iter()
            .map(|d| (d.device.as_str(), d.source()))
            .collect();
        assert_eq!(
            now,
            [
                ("disk", "/pool/t-g2.qcow2".to_string()),
                ("disk", "/pool/t-g2-seed.img".to_string()),
                ("cdrom", "(empty)".to_string()),
            ]
        );
        assert_eq!(parse_nics(&new).unwrap()[0].mac, "52:54:00:12:34:56");
        let doc = roxmltree::Document::parse(&new).unwrap();
        assert_eq!(
            child(doc.root_element(), "uuid").unwrap().text(),
            Some("8a3f0c55-1d2e-4c6b-9f00-5b6d1e2f3a4b")
        );
        assert_eq!(kib_text(&new, "memory"), Some(2048 * 1024));
    }

    #[test]
    fn reset_rewrites_owner_attributes_only_where_present() {
        let next = Owner {
            template: Some("debian13".into()),
            generation: Some(2),
            expires: Some(99),
        };
        let new = replace_owner(&template_vm_xml(), &next).unwrap();
        assert_eq!(owner_in_domain_xml(&new).unwrap(), Some(next.clone()));
        // A VM without our owner element never gains one.
        let plain = "<domain><name>x</name><metadata><o:owner xmlns:o='http://example.com/o'/></metadata></domain>";
        assert!(replace_owner(plain, &next).is_err());
        assert!(replace_owner("<domain><name>x</name></domain>", &next).is_err());
    }

    #[test]
    fn new_uuids_differ() {
        let (a, b) = (new_uuid(), new_uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }
}
