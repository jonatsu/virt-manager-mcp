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

use crate::config::{Firmware, GuestOs, Manage, OnCrash, SecureBoot, is_hostname};
use crate::firmware;
use crate::hypervisor::{self, Hypervisor};
use crate::seed;
use crate::tpm::{self, SwtpmIds};
use crate::volnames;

/// XML namespace of the metadata element that marks VMs created by this server.
const OWNER_NS: &str = "http://virtmanager-mcp/owner/1";

/// Undefining a VM also removes its managed save, snapshot and checkpoint metadata, NVRAM and TPM state.
const UNDEFINE_FLAGS: u32 = sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
    | sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
    | sys::VIR_DOMAIN_UNDEFINE_CHECKPOINTS_METADATA
    | sys::VIR_DOMAIN_UNDEFINE_NVRAM
    | sys::VIR_DOMAIN_UNDEFINE_TPM;

/// The flags to undefine a VM whose TPM state outside `manage.disk_pool` is `outside`, as `tpm::state_outside_pool`
/// lists it: `UNDEFINE_FLAGS`, except that libvirt keeps that state instead of deleting it.
fn undefine_flags_for(outside: &[String]) -> u32 {
    if outside.is_empty() {
        UNDEFINE_FLAGS
    } else {
        (UNDEFINE_FLAGS & !sys::VIR_DOMAIN_UNDEFINE_TPM) | sys::VIR_DOMAIN_UNDEFINE_KEEP_TPM
    }
}

/// The report text for TPM state kept outside `manage.disk_pool`, empty when there is none.
fn tpm_kept_note(outside: &[String]) -> String {
    if outside.is_empty() {
        String::new()
    } else {
        format!(
            "; left TPM state outside manage.disk_pool in place: {}",
            outside.join(", ")
        )
    }
}

/// How long an operation waits for another holder to release the management lock.
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Takes the management lock, so that a limit checked here, such as `max_vms`, still holds when the operation acts.
/// The lock is an exclusive `flock` on a lock file. Every open of the file has its own lock, so it excludes other
/// threads of this process as well as other server processes and `sweep` commands of this account. It is released
/// when the returned guard is dropped or the process dies.
fn lock() -> Result<Held> {
    lock_file(&lock_path(std::env::var_os("HOME"))?, LOCK_WAIT)
}

/// A held management lock, released on drop. The release is explicit: a child that another thread forks meanwhile
/// shares the open file, and closing ours alone would leave the lock held until that child execs.
#[derive(Debug)]
pub struct Held(File);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
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
fn lock_file(path: &Path, wait: Duration) -> Result<Held> {
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
            Ok(()) => return Ok(Held(file)),
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

/// Attributes of the owner element. A UEFI VM carries the Secure Boot mode it was asked for; VMs made from a template
/// carry the template's name and the disk generation, and an expiry when they have a lifetime; and a UEFI VM the
/// server started first carries the firmware record. Only the server writes them: the create tools when they define
/// a domain, and, on a VM that already carries the element, a reset (`generation`, `expires`) and the first start
/// (the firmware record).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owner {
    pub template: Option<String>,
    pub generation: Option<u32>,
    /// UTC time in seconds since the epoch after which the VM is deleted.
    pub expires: Option<u64>,
    /// The Secure Boot mode a UEFI VM was made with. A loader pinned by path cannot tell setup from on, so this does.
    /// Absent for BIOS and for VMs made before it was recorded.
    pub secure_boot: Option<SecureBoot>,
    /// The firmware code file libvirt resolved at the VM's first start, which the server made.
    pub firmware_code: Option<String>,
    /// The SHA-256 of that file at that start, in lower-case hex.
    pub firmware_sha256: Option<String>,
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
        if let Some(mode) = self.secure_boot {
            attrs.push_str(&format!(" secure_boot='{}'", mode.as_str()));
        }
        if let Some(code) = &self.firmware_code {
            attrs.push_str(&format!(" firmware_code='{}'", esc(code)));
        }
        if let Some(sum) = &self.firmware_sha256 {
            attrs.push_str(&format!(" firmware_sha256='{}'", esc(sum)));
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
            secure_boot: node
                .attribute("secure_boot")
                .and_then(SecureBoot::from_word),
            firmware_code: node.attribute("firmware_code").map(str::to_string),
            firmware_sha256: node.attribute("firmware_sha256").map(str::to_string),
        }
    }

    /// The owner element of a new VM made by `create_vm`: no template, and for a UEFI VM the Secure Boot mode asked
    /// for. A template VM adds its own attributes to this.
    fn for_new_vm(uefi: bool, secure_boot: SecureBoot) -> Self {
        Self {
            secure_boot: uefi.then_some(secure_boot),
            ..Self::default()
        }
    }

    /// Whether the element carries any part of the firmware record.
    pub fn has_firmware_record(&self) -> bool {
        self.firmware_code.is_some() || self.firmware_sha256.is_some()
    }
}

/// The owner attributes of `dom`, or `None` when this server did not create it.
pub fn owner_of(dom: &Domain) -> Option<Owner> {
    let xml = dom
        .get_metadata(sys::VIR_DOMAIN_METADATA_ELEMENT as i32, Some(OWNER_NS), 0)
        .ok()?;
    Owner::from_element(&xml).ok()
}

/// The owner attributes in a domain's XML, or `None` when it has no owner element in `OWNER_NS`. Given the inactive
/// definition, it reads the persistent element, which `owner_of` does not for a running VM: libvirt's metadata call
/// reads the live definition then, and a redefinition changes only the persistent one.
pub(crate) fn owner_in_domain_xml(xml: &str) -> Result<Option<Owner>> {
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
    /// Only a UEFI VM may have it off or in setup mode.
    pub secure_boot: SecureBoot,
    pub iso: Option<String>,
    pub driver_iso: Option<String>,
    /// More read-only SATA CD-ROMs after the installer and driver drives, at most `MAX_EXTRA_ISOS`.
    pub extra_isos: Vec<String>,
    /// Writes `<suspend-to-disk enabled='yes'/>`, so a guest that hibernates shuts the VM off and `start_vm` resumes it.
    pub hibernate: bool,
    pub network: Option<String>,
}

pub fn create(hv: &Hypervisor, vm: NewVm) -> Result<String> {
    let m = &hv.config.manage;
    check_vm_name(&vm.name, "VM name")?;
    check_vcpus(m, vm.vcpus)?;
    check_memory(m, vm.memory_mib)?;
    check_disk(m, vm.disk_gib)?;
    check_secure_boot(vm.uefi, vm.secure_boot)?;
    check_extra_isos(&vm.extra_isos)?;
    // The installer drive is always there, empty without an ISO.
    check_sata_ports(
        &vm.name,
        1 + usize::from(vm.driver_iso.is_some()) + vm.extra_isos.len(),
    )?;
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
    let extra_isos = vm
        .extra_isos
        .iter()
        .map(|p| hv.config.iso_file(p))
        .collect::<Result<Vec<_>>>()?;
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
        if vm.uefi {
            check_no_leftover_vars(hv, &pool, &vm.name)?;
        }
        // The TPM state is made before anything is created in the pool, so a missing swtpm-tools or swtpm user fails
        // a create that has nothing to undo.
        let tpm_state = if vm.tpm {
            tpm::check_libvirt(conn.get_lib_version()?)?;
            check_no_leftover_tpm(hv, &pool, &vm.name)?;
            Some((tpm::swtpm_ids()?, tpm::generate_state()?))
        } else {
            None
        };
        let vol_name = format!("{}.qcow2", vm.name);
        let vol = create_volume(&pool, &vol_name, vm.disk_gib)?;
        let tpm_vol = match make_tpm_volume(conn, &pool, &vm.name, tpm_state.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                let _ = vol.delete(0);
                return Err(e);
            }
        };
        let disk = match vol.get_path() {
            Ok(path) => path,
            Err(e) => {
                let _ = vol.delete(0);
                if let Some(tpm_vol) = &tpm_vol {
                    let _ = tpm_vol.delete(0);
                }
                return Err(e.into());
            }
        };
        let nvram = vm.uefi.then(|| vars_path(&disk, &vm.name));
        let tpm_file = tpm_vol.is_some().then(|| tpm_path(&disk, &vm.name));
        let disks = create_disks(&disk, iso.as_deref(), driver_iso.as_deref(), &extra_isos);
        let spec = DomainSpec {
            name: &vm.name,
            windows: vm.windows,
            vcpus: vm.vcpus,
            memory_mib: vm.memory_mib,
            uefi: vm.uefi,
            tpm: tpm_file.as_deref(),
            secure_boot: vm.secure_boot,
            hibernate: vm.hibernate,
            pinned: None,
            nvram: nvram.as_deref(),
            on_crash: m.on_crash,
            disks,
            network: network.as_deref(),
            uuid: None,
            mac: None,
            owner: Owner::for_new_vm(vm.uefi, vm.secure_boot),
        };
        let mut volumes = vec![&vol];
        volumes.extend(tpm_vol.as_ref());
        define_new_vm(&spec, &mut LibvirtDefiner { conn, name: &vm.name, volumes })?;
        let mut msg = format!(
            "created VM '{}' ({} vCPUs, {} MiB, {} GiB disk {}/{}, {}{}); it is shut off, start it with start_vm",
            vm.name,
            vm.vcpus,
            vm.memory_mib,
            vm.disk_gib,
            m.disk_pool,
            vol_name,
            match (vm.uefi, vm.secure_boot) {
                (false, _) => "BIOS",
                (true, SecureBoot::On) => "UEFI",
                (true, SecureBoot::Off) => "UEFI without Secure Boot",
                (true, SecureBoot::Setup) => "UEFI with Secure Boot in setup mode, no keys enrolled",
            },
            if vm.tpm { ", TPM" } else { "" },
        );
        if let Some(n) = &network {
            msg.push_str(&format!("; network '{n}'"));
        }
        Ok(msg)
    })
}

/// Makes the volume `<vm>.tpm` from `state`, the swtpm user and the state `create` made, or nothing for a VM without
/// a TPM.
fn make_tpm_volume(
    conn: &Connect,
    pool: &StoragePool,
    vm: &str,
    state: Option<&(SwtpmIds, Vec<u8>)>,
) -> Result<Option<StorageVol>> {
    state
        .map(|(ids, bytes)| tpm::create_volume(conn, pool, vm, bytes, *ids))
        .transpose()
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
                "secure_boot": t.firmware == Firmware::Uefi && t.secure_boot == SecureBoot::On,
                "secure_boot_mode": if t.firmware == Firmware::Uefi { t.secure_boot.as_str() } else { "off" },
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
    check_vm_name(&vm.name, "VM name")?;
    let t = m.templates.get(&vm.template).ok_or_else(|| {
        anyhow!(
            "no template named '{}'; list_templates shows them",
            vm.template
        )
    })?;
    let vcpus = vm.vcpus.unwrap_or(TEMPLATE_VCPUS.min(m.max_vcpus));
    let memory_mib = vm.memory_mib.unwrap_or(
        TEMPLATE_MEMORY_MIB
            .min(m.max_memory_mib)
            .min(m.max_total_memory_mib.unwrap_or(u64::MAX)),
    );
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
        if t.firmware == Firmware::Uefi {
            check_no_leftover_vars(hv, &pool, &vm.name)?;
        }
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
            let nvram = (t.firmware == Firmware::Uefi).then(|| vars_path(&overlay_path, &vm.name));
            let xml = domain_xml(&DomainSpec {
                name: &vm.name,
                windows: t.os == GuestOs::Windows,
                vcpus,
                memory_mib,
                uefi: t.firmware == Firmware::Uefi,
                tpm: None,
                secure_boot: t.secure_boot,
                hibernate: false,
                pinned: None,
                nvram: nvram.as_deref(),
                on_crash: m.on_crash,
                disks: vec![Disk::Qcow2(&overlay_path), Disk::ReadOnlyRaw(&seed_path)],
                network: network.as_deref(),
                uuid: Some(&uuid),
                mac: None,
                owner: Owner {
                    template: Some(vm.template.clone()),
                    generation: Some(generation),
                    expires,
                    ..Owner::for_new_vm(t.firmware == Firmware::Uefi, t.secure_boot)
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

/// The VM to start after a reset, and the libvirt start flags that return a UEFI VM's firmware variables to the
/// template's defaults.
pub fn reset_start(hv: &Hypervisor, name: &str) -> Result<(Domain, u32)> {
    let dom = hv.manage_domain(name)?;
    let flags = reset_flags(&inactive_xml(&dom)?)?;
    Ok((dom, flags))
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

/// `xml` with the firmware record in its owner element: the code file `path` and its SHA-256 `sha256`. The other
/// attributes stay. Refuses a definition without an owner element, which no record may create, and one that already
/// holds a record, which describes the first start only.
fn with_firmware_record(xml: &str, path: &str, sha256: &str) -> Result<String> {
    let owner = owner_in_domain_xml(xml)?.ok_or_else(|| anyhow!("the VM has no owner element"))?;
    if owner.has_firmware_record() {
        bail!("the VM's owner element already holds a firmware record");
    }
    replace_owner(
        xml,
        &Owner {
            firmware_code: Some(path.to_string()),
            firmware_sha256: Some(sha256.to_string()),
            ..owner
        },
    )
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
        // libvirt deletes the TPM state its definition names, so state outside the pool is kept.
        let tpm_outside = tpm::state_outside_pool(&xml, pool_dir(&pool)?.as_deref())?;
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
        let msg = expiry_report(&name, uuid, &outcomes, &tpm_outside)?;
        match dom.undefine_flags(undefine_flags_for(&tpm_outside)) {
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
fn expiry_report(
    name: &str,
    uuid: &str,
    outcomes: &[(String, DiskDeletion)],
    tpm_outside: &[String],
) -> Result<String> {
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
    msg.push_str(&tpm_kept_note(tpm_outside));
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
    let xml = inactive_xml(&dom)?;
    let disks: Vec<DiskInfo> = parse_disks(&xml)?
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
        // libvirt deletes the TPM state its definition names, so state outside the pool is kept.
        let dir = match &pool {
            Some(p) => pool_dir(p)?,
            None => None,
        };
        let tpm_outside = tpm::state_outside_pool(&xml, dir.as_deref())?;
        dom.undefine_flags(undefine_flags_for(&tpm_outside))
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
        msg.push_str(&tpm_kept_note(&tpm_outside));
        Ok(msg)
    })
}

/// The directory of the pool `manage.disk_pool` names, or `None` when it is unset, missing or has no directory. It
/// serves a hint, so a lookup that fails reads as no directory.
pub fn disk_pool_dir(hv: &Hypervisor) -> Option<PathBuf> {
    let name = &hv.config.manage.disk_pool;
    if name.is_empty() {
        return None;
    }
    hv.with_conn(|conn| {
        let pool = StoragePool::lookup_by_name(conn, name)?;
        pool_dir(&pool)
    })
    .ok()
    .flatten()
}

/// The directory `pool` keeps its volumes in, from its XML target path; `None` when the pool has none.
fn pool_dir(pool: &StoragePool) -> Result<Option<PathBuf>> {
    hypervisor::pool_target(&pool.get_xml_desc(0)?)
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
    // The persistent element: a running VM's live one lacks what a redefinition added.
    let recorded = owner_in_domain_xml(&xml)?.and_then(|o| o.secure_boot);
    let (secure_boot, secure_boot_mode) = secure_boot_fields(uefi, os, recorded);
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
        "secure_boot": secure_boot,
        "secure_boot_mode": secure_boot_mode,
        "tpm": has("tpm"),
        "hibernate": suspends_to_disk(root),
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
/// `target` the first CD-ROM drive is used; a shut-off VM without one gets a new drive. A `target` of the form
/// `sd<letter>` that no device uses adds a drive there on a shut-off VM, up to q35's six SATA ports.
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
    let cdrom = match plan_cdrom(name, &disks, active, target)? {
        CdromPlan::Change(i) => &disks[i],
        CdromPlan::Add(t) => {
            let dev = format!(
                "<disk type='file' device='cdrom'><driver name='qemu' type='raw'/>{source}<target dev='{t}' bus='sata'/><readonly/></disk>"
            );
            dom.attach_device_flags(&dev, sys::VIR_DOMAIN_AFFECT_CONFIG)
                .map_err(|e| anyhow!("cannot add a CD-ROM drive: {}", e.message()))?;
            return Ok(format!("added CD-ROM drive {t} to '{name}' and {what}"));
        }
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

/// A snapshot to revert to, and whether the VM was running when it was taken, so the revert starts the VM.
pub struct RevertTarget {
    pub dom: Domain,
    pub snap: DomainSnapshot,
    /// The snapshot was taken while the VM ran, so the revert boots it.
    pub running: bool,
    /// The revert leaves the VM active, running or paused, so it counts toward `max_total_memory_mib`.
    pub activates: bool,
    /// The memory of the domain the revert restores, when the snapshot records it.
    pub memory_mib: Option<u64>,
}

/// Looks up the managed VM `name` and its snapshot `snapshot` for a revert.
pub fn snapshot_target(hv: &Hypervisor, name: &str, snapshot: &str) -> Result<RevertTarget> {
    let dom = hv.manage_domain(name)?;
    let snap = DomainSnapshot::lookup_by_name(&dom, snapshot, 0)
        .map_err(|e| anyhow!("snapshot '{snapshot}' not found: {}", e.message()))?;
    let xml = snap.get_xml_desc(0)?;
    let running = snapshot_was_running(&xml)?;
    let activates = running || snapshot_state(&xml)?.as_deref() == Some("paused");
    let memory_mib = snapshot_memory_mib(&xml)?;
    Ok(RevertTarget {
        dom,
        snap,
        running,
        activates,
        memory_mib,
    })
}

/// The state a snapshot's XML says the VM was in when it was taken.
fn snapshot_state(xml: &str) -> Result<Option<String>> {
    let doc = roxmltree::Document::parse(xml)?;
    Ok(doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("state"))
        .and_then(|n| n.text())
        .map(str::to_string))
}

/// Whether a snapshot's XML says the VM was running when it was taken.
fn snapshot_was_running(xml: &str) -> Result<bool> {
    Ok(snapshot_state(xml)?.as_deref() == Some("running"))
}

/// The memory, in MiB rounded up, of the domain a snapshot restores, from its `<domain><memory>`; `None` when the
/// snapshot records no domain.
fn snapshot_memory_mib(xml: &str) -> Result<Option<u64>> {
    let doc = roxmltree::Document::parse(xml)?;
    let Some(memory) = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("domain"))
        .and_then(|d| d.children().find(|n| n.has_tag_name("memory")))
    else {
        return Ok(None);
    };
    let value: u64 = memory
        .text()
        .unwrap_or_default()
        .trim()
        .parse()
        .context("the snapshot's domain memory is not a number")?;
    let unit: u64 = match memory.attribute("unit").unwrap_or("KiB") {
        "b" | "bytes" => 1,
        "KB" => 1000,
        "k" | "KiB" => 1 << 10,
        "MB" => 1000 * 1000,
        "M" | "MiB" => 1 << 20,
        "GB" => 1000 * 1000 * 1000,
        "G" | "GiB" => 1 << 30,
        "TB" => 1000 * 1000 * 1000 * 1000,
        "T" | "TiB" => 1 << 40,
        other => bail!("the snapshot's domain memory has an unknown unit '{other}'"),
    };
    Ok(Some(value.saturating_mul(unit).div_ceil(1 << 20)))
}

/// Reverts to `target`'s snapshot: paused when it was running, so the caller can open the console before the guest
/// runs on, and as recorded otherwise.
pub fn snapshot_revert(target: &RevertTarget, snapshot: &str) -> Result<()> {
    let flags = if target.running {
        sys::VIR_DOMAIN_SNAPSHOT_REVERT_PAUSED
    } else {
        0
    };
    target
        .snap
        .revert(flags)
        .map_err(|e| anyhow!("cannot revert to '{snapshot}': {}", e.message()))?;
    Ok(())
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

pub(crate) fn check_name(name: &str, what: &str) -> Result<()> {
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

/// Checks the name of a new VM: `check_name`, and not `tpl`, which `volnames` keeps for template volumes.
pub(crate) fn check_vm_name(name: &str, what: &str) -> Result<()> {
    check_name(name, what)?;
    if name == volnames::RESERVED_VM_NAME {
        bail!("{what} cannot be '{name}', which names the template volumes");
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
    if let Some(cap) = m.max_total_memory_mib
        && mib > cap
    {
        bail!(
            "memory_mib must be at most {cap} (manage.max_total_memory_mib), or the VM could never start"
        );
    }
    Ok(())
}

/// Takes the management lock for the start of `dom`, after checking that the VMs this server created that are running,
/// paused or suspended, with `dom` added, stay within `manage.max_total_memory_mib`. The caller holds the guard until
/// libvirt has started the VM, so two starts cannot both pass the check. Returns `None`, and takes no lock, when no
/// cap is set or `dom` is not a VM this server created.
///
/// `memory_mib` replaces `dom`'s own memory, for a revert, which restores the snapshot's.
pub fn start_guard(hv: &Hypervisor, dom: &Domain, memory_mib: Option<u64>) -> Result<Option<Held>> {
    let Some(cap) = hv.config.manage.max_total_memory_mib else {
        return Ok(None);
    };
    if !is_owned(dom) {
        return Ok(None);
    }
    let guard = lock()?;
    let uuid = dom.get_uuid_string()?;
    let name = dom.get_name()?;
    let own = match memory_mib {
        Some(mib) => mib,
        None => dom.get_max_memory()?.div_ceil(1024),
    };
    let running = hv.with_conn(|conn| {
        // A VM that went away after the listing uses no memory any more; any other error refuses the start, so the
        // check never passes on a count it could not make.
        let gone = |e: &virt::error::Error| e.code() == ErrorNumber::NoDomain;
        let mut running = Vec::new();
        for d in conn.list_all_domains(sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE)? {
            let facts = d
                .get_uuid_string()
                .and_then(|u| Ok((u, d.get_name()?, d.get_max_memory()?)));
            match facts {
                Ok((u, n, kib)) => {
                    if u != uuid && is_owned(&d) {
                        running.push((n, kib.div_ceil(1024)));
                    }
                }
                Err(e) if gone(&e) => {}
                Err(e) => bail!(
                    "cannot count the memory of running VMs for manage.max_total_memory_mib: {}",
                    e.message()
                ),
            }
        }
        Ok(running)
    })?;
    check_total_memory(cap, &name, own, &running)?;
    Ok(Some(guard))
}

/// Refuses a start of `name`, using `own` MiB, that would bring the server's running VMs past `cap` MiB.
fn check_total_memory(cap: u64, name: &str, own: u64, running: &[(String, u64)]) -> Result<()> {
    let used: u64 = running.iter().map(|(_, mib)| mib).sum();
    if used + own > cap {
        let list = if running.is_empty() {
            "none".to_string()
        } else {
            running
                .iter()
                .map(|(n, mib)| format!("{n} {mib} MiB"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        bail!(
            "starting '{name}' ({own} MiB) would bring this server's running VMs to {used} + {own} MiB, over \
             manage.max_total_memory_mib = {cap}; stop one first (running: {list})"
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

/// A UEFI domain's Secure Boot mode, from its `secure-boot` and `enrolled-keys` firmware features or, when they are
/// absent, from its loader's `secure` attribute, which libvirt fills in when it picks the firmware and which the
/// explicit form (a VM that hibernates, or one from a sealed template) always carries. Secure Boot without enrolled
/// keys is setup mode. `None` when nothing says, as for a loader named by path alone.
///
/// A secure loader alone cannot tell setup from on. The mode the owner element records (`recorded`) settles that;
/// without one, the variables template's file name does (`template_without_keys`), as for a VM made before the mode
/// was recorded. The record never overrides what the definition says about Secure Boot being off.
fn secure_boot_state(
    os: roxmltree::Node<'_, '_>,
    recorded: Option<SecureBoot>,
) -> Option<SecureBoot> {
    let feature = |name: &str| {
        child(os, "firmware").and_then(|f| {
            f.children()
                .find(|n| n.has_tag_name("feature") && n.attribute("name") == Some(name))
                .and_then(|n| n.attribute("enabled"))
        })
    };
    let loader = child(os, "loader").and_then(|l| l.attribute("secure"));
    match feature("secure-boot").or(loader)? {
        "yes" if feature("enrolled-keys") == Some("no") => Some(SecureBoot::Setup),
        "yes" if feature("secure-boot").is_none() => Some(match recorded {
            Some(mode @ (SecureBoot::Setup | SecureBoot::On)) => mode,
            Some(SecureBoot::Off) | None if template_without_keys(os) => SecureBoot::Setup,
            Some(SecureBoot::Off) | None => SecureBoot::On,
        }),
        "yes" => Some(SecureBoot::On),
        "no" => Some(SecureBoot::Off),
        _ => None,
    }
}

/// Whether a domain's variables template is one without enrolled keys. The explicit form has no `enrolled-keys`
/// feature, so the template says it: Debian names the keyed one `OVMF_VARS_4M.ms.fd` and Fedora
/// `OVMF_VARS.secboot.fd`, and the template of setup and off is the plain `OVMF_VARS*.fd`. A domain without a
/// template says nothing, so it reads as on.
fn template_without_keys(os: roxmltree::Node<'_, '_>) -> bool {
    child(os, "nvram")
        .and_then(|n| n.attribute("template"))
        .and_then(|t| t.rsplit('/').next())
        .is_some_and(|file| !file.contains(".ms.") && !file.contains(".secboot."))
}

/// Whether a domain's `<pm>` enables suspend to disk.
fn suspends_to_disk(root: roxmltree::Node<'_, '_>) -> bool {
    child(root, "pm")
        .and_then(|pm| child(pm, "suspend-to-disk"))
        .and_then(|n| n.attribute("enabled"))
        == Some("yes")
}

/// The `secure_boot` boolean and the `secure_boot_mode` word `describe_vm` reports. Setup mode reads as false in the
/// boolean, which says whether the VM's keys are enrolled. A BIOS VM has no Secure Boot; for a UEFI VM, `None` means
/// the XML does not say. `recorded` is the mode the owner element holds, which `secure_boot_state` uses.
fn secure_boot_fields(
    uefi: bool,
    os: Option<roxmltree::Node<'_, '_>>,
    recorded: Option<SecureBoot>,
) -> (Option<bool>, Option<&'static str>) {
    if !uefi {
        return (Some(false), Some(SecureBoot::Off.as_str()));
    }
    match os.and_then(|os| secure_boot_state(os, recorded)) {
        Some(mode) => (Some(mode == SecureBoot::On), Some(mode.as_str())),
        None => (None, None),
    }
}

/// The path of the pool file `name` beside the VM's disk volume at `disk_path`.
fn pool_file_path(disk_path: &str, name: String) -> String {
    match Path::new(disk_path).parent() {
        Some(dir) => dir.join(name).to_string_lossy().into_owned(),
        None => name,
    }
}

/// The path of a UEFI VM's own variables file: `<vm>.vars` beside the VM's disk volume in `disk_pool`.
fn vars_path(disk_path: &str, vm: &str) -> String {
    pool_file_path(disk_path, volnames::vars_name(vm))
}

/// The path of a VM's TPM state: `<vm>.tpm` beside the VM's disk volume in `disk_pool`. The server builds it, and no
/// tool parameter can change it.
fn tpm_path(disk_path: &str, vm: &str) -> String {
    pool_file_path(disk_path, volnames::tpm_name(vm))
}

/// What `delete` may do with a disk of a VM.
#[derive(Debug, PartialEq, Eq)]
enum DiskRef<'a> {
    /// A volume of the pool, named by its pool and volume.
    Volume(&'a str),
    /// A volume of another pool, which `delete` never names.
    OtherPool,
    /// A file or device path, which libvirt places in a pool or not.
    Path(&'a str),
    /// No source.
    Empty,
}

fn disk_ref<'a>(disk: &'a DiskInfo, pool_name: &str) -> DiskRef<'a> {
    match (&disk.pool, &disk.volume, &disk.file) {
        (Some(p), Some(v), _) if p == pool_name => DiskRef::Volume(v),
        (Some(_), Some(_), _) => DiskRef::OtherPool,
        (_, _, Some(f)) => DiskRef::Path(f),
        _ => DiskRef::Empty,
    }
}

/// The refusal for a create that finds `<vm>.vars` already in the pool.
fn leftover_vars_error(uri: &str, pool: &str, vm: &str) -> anyhow::Error {
    let vars = volnames::vars_name(vm);
    anyhow!(
        "volume '{vars}' already exists in the disk pool '{pool}', so a new VM '{vm}' would start from another VM's \
         firmware variables; nothing was created. Remove it with: virsh -c {uri} vol-delete --pool {pool} {vars}"
    )
}

/// The refusal for a create that finds `<vm>.tpm` already in the pool.
fn leftover_tpm_error(uri: &str, pool: &str, vm: &str) -> anyhow::Error {
    let tpm = volnames::tpm_name(vm);
    anyhow!(
        "volume '{tpm}' already exists in the disk pool '{pool}', so a new VM '{vm}' would start from another VM's \
         TPM state; nothing was created. Remove it with: virsh -c {uri} vol-delete --pool {pool} {tpm}"
    )
}

/// A pool's volume listing, apart so a test can stand in for the pool.
trait PoolListing {
    /// Rescans the pool, so the listing shows files libvirt made outside the server's own calls.
    ///
    /// # Errors
    ///
    /// Fails when the pool refuses the scan.
    fn refresh(&self) -> Result<()>;
    /// Whether the pool holds a volume named `name`.
    ///
    /// # Errors
    ///
    /// Fails when the lookup fails for any reason but the volume's absence.
    fn has_volume(&self, name: &str) -> Result<bool>;
}

impl PoolListing for StoragePool {
    fn refresh(&self) -> Result<()> {
        let name = self.get_name().unwrap_or_default();
        StoragePool::refresh(self, 0)
            .map(drop)
            .map_err(|e| anyhow!("cannot refresh storage pool '{name}': {}", e.message()))
    }

    fn has_volume(&self, name: &str) -> Result<bool> {
        volume_listed(
            name,
            StorageVol::lookup_by_name(self, name)
                .map(drop)
                .map_err(|e| (e.code(), e.message().to_string())),
        )
    }
}

/// Reads the outcome of a volume lookup, given as libvirt's error number and message on failure: only libvirt's
/// no-such-volume error means the volume is absent, and any other failure leaves the question open.
fn volume_listed(
    name: &str,
    found: std::result::Result<(), (ErrorNumber, String)>,
) -> Result<bool> {
    match found {
        Ok(()) => Ok(true),
        Err((ErrorNumber::NoStorageVolume, _)) => Ok(false),
        Err((_, message)) => bail!("cannot look up volume '{name}': {message}"),
    }
}

/// Refuses a create while the pool lists `volume`, after a rescan whose failure fails the check: a listing that could
/// not be refreshed may be stale, and a stale listing without the leftover must not pass. libvirt reuses a state file
/// at a VM's first start, so a leftover of an earlier VM would give the new one its variables or TPM state.
fn check_no_leftover(
    pool: &impl PoolListing,
    volume: &str,
    refusal: impl FnOnce() -> anyhow::Error,
) -> Result<()> {
    pool.refresh()?;
    if pool.has_volume(volume)? {
        return Err(refusal());
    }
    Ok(())
}

/// Refuses a UEFI create while `<vm>.vars` is in the pool, after `check_no_leftover`'s rescan.
fn check_no_leftover_vars(hv: &Hypervisor, pool: &StoragePool, vm: &str) -> Result<()> {
    check_no_leftover(pool, &volnames::vars_name(vm), || {
        leftover_vars_error(&hv.config.uri, &hv.config.manage.disk_pool, vm)
    })
}

/// Refuses a create with a TPM while `<vm>.tpm` is in the pool, after `check_no_leftover`'s rescan. The state would
/// otherwise be another VM's, with its keys.
fn check_no_leftover_tpm(hv: &Hypervisor, pool: &StoragePool, vm: &str) -> Result<()> {
    check_no_leftover(pool, &volnames::tpm_name(vm), || {
        leftover_tpm_error(&hv.config.uri, &hv.config.manage.disk_pool, vm)
    })
}

/// Whether the pool lists the volume `<vm>.vars` after a rescan. libvirt's cached listing does not show a file made
/// outside its own calls, such as the one a `virsh start` creates, so the lookup never runs before the rescan.
///
/// # Errors
///
/// Fails when the rescan or the lookup fails. A caller that records the firmware then records nothing, because a
/// listing it cannot trust may hide a file made by an earlier start.
fn vars_listed(pool: &impl PoolListing, vm: &str) -> Result<bool> {
    pool.refresh()?;
    pool.has_volume(&volnames::vars_name(vm))
}

/// Rescans `disk_pool`, so that files libvirt made outside the server's own calls are listed, such as `<vm>.vars`,
/// which it creates at the VM's first start.
///
/// # Errors
///
/// Fails when `manage.disk_pool` is not set, or the pool is missing or refuses the scan.
pub fn refresh_pool(hv: &Hypervisor) -> Result<()> {
    let m = &hv.config.manage;
    if m.disk_pool.is_empty() {
        bail!("manage.disk_pool is not set");
    }
    hv.with_conn(|conn| {
        let pool = StoragePool::lookup_by_name(conn, &m.disk_pool)
            .map_err(|e| anyhow!("storage pool '{}': {}", m.disk_pool, e.message()))?;
        pool.refresh(0).map_err(|e| {
            anyhow!(
                "cannot refresh storage pool '{}': {}",
                m.disk_pool,
                e.message()
            )
        })?;
        Ok(())
    })
}

/// Before a start of `dom`, named `name`: whether it is the first start of a VM the server made with a UEFI
/// variables file in the pool, so that `record_firmware` should follow it. The pool is rescanned first, because
/// libvirt's listing does not show a `<name>.vars` that another client's start made. A VM that cannot qualify costs
/// no rescan. A failure to read the VM or the pool counts as no: nothing is recorded on a listing that cannot be
/// trusted.
pub fn firmware_record_due(hv: &Hypervisor, dom: &Domain, name: &str) -> bool {
    let due = || -> Result<bool> {
        let Some(dir) = disk_pool_dir(hv) else {
            return Ok(false);
        };
        let xml = inactive_xml(dom)?;
        let owner = owner_in_domain_xml(&xml)?;
        if !firmware::should_record(owner.as_ref(), &xml, &dir, name, false) {
            return Ok(false);
        }
        let listed =
            hv.with_conn(|conn| vars_listed(&disk_pool(conn, &hv.config.manage)?, name))?;
        Ok(firmware::should_record(
            owner.as_ref(),
            &xml,
            &dir,
            name,
            listed,
        ))
    };
    due().unwrap_or_else(|e| {
        tracing::warn!("cannot tell whether the start of '{name}' is its first: {e:#}");
        false
    })
}

/// After the first start of the VM with `uuid`, named `name`: records the firmware code file that libvirt resolved
/// and its SHA-256 in the VM's owner element. It takes the management lock, rescans the pool, reads the loader path
/// from the live definition, hashes the file, and redefines the VM from its inactive definition with the element
/// replaced. A redefinition of a running VM changes only its persistent definition, which is where a later seal reads
/// the record. Its checks run again under the lock: a concurrent delete, or a VM that no longer qualifies, records
/// nothing. The caller must not hold the lock.
///
/// # Errors
///
/// Fails when the lock is not free in time, the VM is gone or no longer qualifies, the loader or its file cannot be
/// read, or libvirt refuses the redefinition. The start has already succeeded, so the caller logs it.
pub fn record_firmware(hv: &Hypervisor, uuid: &str, name: &str) -> Result<()> {
    let _guard = lock()?;
    hv.with_conn(|conn| {
        let dom = Domain::lookup_by_uuid_string(conn, uuid)
            .map_err(|e| anyhow!("cannot find VM '{name}': {}", e.message()))?;
        if dom.get_name()? != name {
            bail!("VM '{name}' was renamed");
        }
        let pool = disk_pool(conn, &hv.config.manage)?;
        // The rescan shows the file libvirt made at this start; without it, this was not a start that made one.
        if !vars_listed(&pool, name)? {
            bail!("the pool lists no variables file for '{name}' after its start");
        }
        let dir = pool_dir(&pool)?.ok_or_else(|| anyhow!("the pool has no directory"))?;
        let inactive = inactive_xml(&dom)?;
        let owner = owner_in_domain_xml(&inactive)?;
        // The listing was checked before the start; here only the VM's own state is.
        if !firmware::should_record(owner.as_ref(), &inactive, &dir, name, false) {
            bail!("'{name}' no longer qualifies for a firmware record");
        }
        let live = dom.get_xml_desc(0).map_err(|e| {
            anyhow!(
                "cannot read the live definition of '{name}': {}",
                e.message()
            )
        })?;
        let code = firmware::loader_path(&live)?;
        let sha256 = firmware::sha256_file(Path::new(&code))?;
        let xml = with_firmware_record(&inactive, &code, &sha256)?;
        Domain::define_xml(conn, &xml)
            .map_err(|e| anyhow!("cannot redefine '{name}': {}", e.message()))?;
        Ok(())
    })
}

/// Whether a domain's `<os>` has a variables file that a reset can restore from its template: libvirt's firmware
/// autoselection, or an explicit flash loader with an `<nvram>`.
fn has_variables_file(os: roxmltree::Node<'_, '_>) -> bool {
    os.attribute("firmware") == Some("efi")
        || (child(os, "loader").and_then(|l| l.attribute("type")) == Some("pflash")
            && child(os, "nvram").is_some())
}

/// The libvirt start flags of a reset, for a VM with inactive definition `xml`.
fn reset_flags(xml: &str) -> Result<u32> {
    let doc = roxmltree::Document::parse(xml)?;
    let uefi = child(doc.root_element(), "os").is_some_and(has_variables_file);
    Ok(if uefi {
        sys::VIR_DOMAIN_START_RESET_NVRAM
    } else {
        0
    })
}

fn check_secure_boot(uefi: bool, secure_boot: SecureBoot) -> Result<()> {
    if !uefi && secure_boot != SecureBoot::On {
        bail!("secure_boot \"off\" and \"setup\" need firmware \"uefi\"");
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
pub(crate) fn upload(conn: &Connect, vol: &StorageVol, data: &[u8]) -> Result<()> {
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
    let path = match disk_ref(disk, pool_name) {
        DiskRef::Volume(v) => return StorageVol::lookup_by_name(pool, v).ok(),
        DiskRef::OtherPool | DiskRef::Empty => return None,
        DiskRef::Path(f) => f,
    };
    let vol = StorageVol::lookup_by_path(conn, path).ok()?;
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
pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

/// A disk of a new domain. Every disk refers to its file by path: a seed given as a pool-volume reference failed
/// to open on a later start (`docs/research/2026-11-10-fatfs-seed-boot.md`).
#[derive(Clone)]
enum Disk<'a> {
    /// A writable qcow2 disk on the virtio bus.
    Qcow2(&'a str),
    /// A read-only raw disk on the virtio bus, such as a NoCloud seed.
    ReadOnlyRaw(&'a str),
    /// A SATA CD-ROM, empty when `None`.
    Cdrom(Option<&'a Path>),
}

/// Everything `domain_xml` puts into a new domain.
#[derive(Clone)]
struct DomainSpec<'a> {
    name: &'a str,
    windows: bool,
    vcpus: u32,
    memory_mib: u64,
    uefi: bool,
    /// The file that keeps the VM's TPM 2.0 state, which the server made in the pool. `None` has no TPM.
    tpm: Option<&'a str>,
    /// With `uefi`, the Secure Boot features the firmware is asked for.
    secure_boot: SecureBoot,
    /// Writes `<suspend-to-disk enabled='yes'/>`.
    hibernate: bool,
    /// With `uefi`, the firmware pinned by path, which replaces `firmware='efi'` and the Secure Boot features.
    pinned: Option<&'a PinnedFirmware>,
    /// With `uefi` and autoselection, the path of the VM's own variables file in `disk_pool`, which libvirt creates
    /// at the first start. A pinned firmware carries its own.
    nvram: Option<&'a str>,
    /// The domain's `<on_crash>` action.
    on_crash: OnCrash,
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

/// Ports of q35's built-in SATA controller.
const SATA_PORTS: usize = 6;
/// Most `extra_isos` one `create_vm` takes.
const MAX_EXTRA_ISOS: usize = 4;

fn check_extra_isos(isos: &[String]) -> Result<()> {
    if isos.len() > MAX_EXTRA_ISOS {
        bail!(
            "extra_isos takes at most {MAX_EXTRA_ISOS} ISOs, and {} were given",
            isos.len()
        );
    }
    Ok(())
}

/// The disks of a new VM, in the order they are written: the system disk, the installer drive (empty without an
/// ISO), the driver ISO, then the extra ISOs. `disk_xml` gives the boot order to the first writable disk and the
/// first CD-ROM only.
fn create_disks<'a>(
    disk: &'a str,
    iso: Option<&'a Path>,
    driver_iso: Option<&'a Path>,
    extra_isos: &'a [PathBuf],
) -> Vec<Disk<'a>> {
    let mut disks = vec![Disk::Qcow2(disk), Disk::Cdrom(iso)];
    disks.extend(driver_iso.map(|p| Disk::Cdrom(Some(p))));
    disks.extend(extra_isos.iter().map(|p| Disk::Cdrom(Some(p.as_path()))));
    disks
}

/// How many of `disks` sit on the SATA bus, and so use a port of q35's built-in controller.
fn sata_devices(disks: &[DiskInfo]) -> usize {
    disks.iter().filter(|d| d.bus == "sata").count()
}

/// Refuses a VM whose SATA devices (CD-ROMs and disks on bus `sata`) would pass the controller's ports.
fn check_sata_ports(name: &str, devices: usize) -> Result<()> {
    if devices > SATA_PORTS {
        bail!(
            "VM '{name}' would have {devices} SATA devices (CD-ROM drives and SATA disks), but q35's SATA controller \
             has {SATA_PORTS} ports"
        );
    }
    Ok(())
}

/// What `set_cdrom` does with a drive.
#[derive(Debug, PartialEq, Eq)]
enum CdromPlan {
    /// Change the medium of the drive at this index of the parsed disks.
    Change(usize),
    /// Add a drive with this target to a shut-off VM.
    Add(String),
}

/// Whether `target` is `sd` and one lowercase letter.
fn is_sata_target(target: &str) -> bool {
    target
        .strip_prefix("sd")
        .is_some_and(|rest| rest.len() == 1 && rest.starts_with(|c: char| c.is_ascii_lowercase()))
}

/// Decides what `set_cdrom` does: change the medium of the named CD-ROM drive (the first one without a target), or
/// add a drive at a free `sd<letter>` target on a shut-off VM within the SATA ports.
fn plan_cdrom(
    name: &str,
    disks: &[DiskInfo],
    active: bool,
    target: Option<&str>,
) -> Result<CdromPlan> {
    if let Some(i) = disks
        .iter()
        .position(|d| d.device == "cdrom" && target.is_none_or(|t| d.target == t))
    {
        return Ok(CdromPlan::Change(i));
    }
    let target = match target {
        Some(t) if disks.iter().any(|d| d.target == t) => {
            bail!(
                "'{t}' of VM '{name}' is a disk, not a CD-ROM drive (disks are changed with attach_disk and detach_disk)"
            )
        }
        Some(t) if !is_sata_target(t) => {
            bail!(
                "VM '{name}' has no CD-ROM drive '{t}'; a new drive needs a free target of the form sd<letter>, such as sdc"
            )
        }
        Some(t) => t.to_string(),
        None if active => bail!("VM '{name}' has no CD-ROM drive; shut it down to add one"),
        None => ('a'..='z')
            .map(|c| format!("sd{c}"))
            .find(|t| !disks.iter().any(|d| &d.target == t))
            .context("no free SATA target left")?,
    };
    if active {
        bail!("VM '{name}' is running; shut it down to add a CD-ROM drive at {target}");
    }
    check_sata_ports(name, sata_devices(disks) + 1)?;
    Ok(CdromPlan::Add(target))
}

/// The UEFI firmware a VM that hibernates is pinned to: libvirt's autoselection refuses `<suspend-to-disk>`, so the
/// server reads what autoselection resolved and writes it back as an explicit loader and variables template.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinnedFirmware {
    /// The code file.
    loader: String,
    /// The loader's `secure` attribute.
    secure: bool,
    /// The variables template.
    template: String,
    /// The VM's own variables file.
    vars: String,
}

/// What a VM's first, autoselected definition resolved, which the redefinition keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedDefinition {
    firmware: PinnedFirmware,
    uuid: String,
    /// `None` for a VM without a network interface.
    mac: Option<String>,
}

impl ResolvedDefinition {
    /// Reads the resolved firmware, the UUID and the MAC address from a VM's inactive definition. Refuses a
    /// definition that lacks the loader, the variables template or the variables file, because pinning a guess
    /// would define a VM that cannot start.
    fn from_inactive_xml(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(xml).context("the definition is not XML")?;
        let root = doc.root_element();
        let text = |node: Option<roxmltree::Node<'_, '_>>| {
            node.and_then(|n| n.text())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(String::from)
        };
        let uuid = text(child(root, "uuid")).context("the definition has no uuid")?;
        let os = child(root, "os").context("the definition has no <os>")?;
        let loader_node = child(os, "loader");
        let loader = text(loader_node).context("the definition has no <loader> path")?;
        let nvram = child(os, "nvram");
        let template = nvram
            .and_then(|n| n.attribute("template"))
            .filter(|t| !t.is_empty())
            .context("the <nvram> has no template")?;
        let vars = text(nvram).context("the definition has no <nvram> variables path")?;
        let mac = child(root, "devices")
            .and_then(|d| child(d, "interface"))
            .and_then(|i| child(i, "mac"))
            .and_then(|m| m.attribute("address"))
            .map(String::from);
        Ok(Self {
            firmware: PinnedFirmware {
                loader,
                secure: loader_node.and_then(|l| l.attribute("secure")) == Some("yes"),
                template: template.to_string(),
                vars,
            },
            uuid,
            mac,
        })
    }
}

impl PinnedFirmware {
    /// The explicit `<os>` element: no `firmware` attribute and no `<firmware>` features, a read-only flash loader
    /// and a variables file with its template, all in raw format. `secure` is always written, so that
    /// `secure_boot_state` can tell Secure Boot off from a loader that says nothing.
    fn os_xml(&self) -> String {
        format!(
            "<os><type arch='x86_64' machine='q35'>hvm</type>\
             <loader readonly='yes' secure='{}' type='pflash' format='raw'>{}</loader>\
             <nvram template='{}' templateFormat='raw' format='raw'>{}</nvram></os>",
            if self.secure { "yes" } else { "no" },
            esc(&self.loader),
            esc(&self.template),
            esc(&self.vars),
        )
    }
}

/// The libvirt calls that define a new VM, so a test can fail each of them.
trait VmDefiner {
    fn define(&mut self, xml: &str) -> Result<()>;
    fn inactive_xml(&mut self) -> Result<String>;
    /// Undefines the VM, which an earlier `define` made.
    fn undefine(&mut self);
    /// Deletes the volumes the VM was to use.
    fn delete_disks(&mut self);
}

/// Defines a new VM. A UEFI VM that hibernates is defined twice, because libvirt's firmware autoselection refuses
/// `<suspend-to-disk enabled='yes'/>` (`docs/research/2026-11-24-custom-templates-build-probes.md`, probes 3 and
/// 8): first by autoselection without `<pm>`, then, under the same name, UUID and MAC address, with the firmware
/// it resolved written out as an explicit loader and variables template, plus `<pm>`. A step that fails undefines
/// what was defined and deletes the disks, and the error names the step.
fn define_new_vm(spec: &DomainSpec<'_>, definer: &mut impl VmDefiner) -> Result<()> {
    let name = spec.name;
    if let Err(e) = definer.define(&domain_xml(spec)) {
        definer.delete_disks();
        bail!("cannot define VM '{name}': {e:#}");
    }
    if spec.uefi
        && spec.hibernate
        && let Err(e) = pin_firmware(spec, definer)
    {
        definer.undefine();
        definer.delete_disks();
        return Err(e);
    }
    Ok(())
}

/// The second step of `define_new_vm` for a UEFI VM that hibernates.
fn pin_firmware(spec: &DomainSpec<'_>, definer: &mut impl VmDefiner) -> Result<()> {
    let name = spec.name;
    let xml = definer.inactive_xml().map_err(|e| {
        anyhow!("cannot read VM '{name}' after defining it, to pin its firmware: {e:#}")
    })?;
    let resolved = ResolvedDefinition::from_inactive_xml(&xml)
        .map_err(|e| anyhow!("cannot pin the firmware of VM '{name}': {e:#}"))?;
    let pinned = DomainSpec {
        pinned: Some(&resolved.firmware),
        uuid: Some(&resolved.uuid),
        mac: resolved.mac.as_deref(),
        ..spec.clone()
    };
    definer.define(&domain_xml(&pinned)).map_err(|e| {
        anyhow!("cannot redefine VM '{name}' with its firmware pinned for hibernation: {e:#}")
    })
}

/// The libvirt calls of `define_new_vm`, on the volumes the new VM was to use.
struct LibvirtDefiner<'a> {
    conn: &'a Connect,
    name: &'a str,
    volumes: Vec<&'a StorageVol>,
}

impl VmDefiner for LibvirtDefiner<'_> {
    fn define(&mut self, xml: &str) -> Result<()> {
        Domain::define_xml(self.conn, xml)
            .map(drop)
            .map_err(|e| anyhow!("{}", e.message()))
    }

    fn inactive_xml(&mut self) -> Result<String> {
        let dom =
            Domain::lookup_by_name(self.conn, self.name).map_err(|e| anyhow!("{}", e.message()))?;
        inactive_xml(&dom)
    }

    fn undefine(&mut self) {
        if let Ok(dom) = Domain::lookup_by_name(self.conn, self.name)
            && let Err(e) = dom.undefine_flags(UNDEFINE_FLAGS)
        {
            tracing::warn!(
                "cannot undefine '{}' after a failed create: {}",
                self.name,
                e.message()
            );
        }
    }

    fn delete_disks(&mut self) {
        for vol in &self.volumes {
            let _ = vol.delete(0);
        }
    }
}

/// The Secure Boot features and loader that ask libvirt's autoselection for a UEFI VM's firmware. Without features,
/// libvirt picks the host's preferred descriptor, which may not be the mode asked for. With them alone it may pick a
/// stateless ROM build such as OVMF.amdsev.fd, which keeps no boot entries, so the loader type asks for flash with
/// its own variables.
fn autoselection_features(spec: &DomainSpec<'_>) -> String {
    if !spec.uefi {
        return String::new();
    }
    let (secure, keys) = match spec.secure_boot {
        SecureBoot::Off => ("no", "no"),
        SecureBoot::On => ("yes", "yes"),
        SecureBoot::Setup => ("yes", "no"),
    };
    format!(
        "<firmware><feature enabled='{secure}' name='secure-boot'/>\
         <feature enabled='{keys}' name='enrolled-keys'/></firmware><loader type='pflash'/>"
    )
}

/// The `<nvram>` element that gives an autoselected UEFI VM its own variables file. libvirt keeps a path the
/// definition names, and at the first start creates the file from the chosen descriptor's variables template, so
/// code and variables come from one descriptor.
fn autoselection_nvram(spec: &DomainSpec<'_>) -> String {
    match spec.nvram.filter(|_| spec.uefi) {
        Some(path) => format!("<nvram format='raw'>{}</nvram>", esc(path)),
        None => String::new(),
    }
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
            // The stats period makes the balloon report the guest's own memory figures.
            "<memballoon model='virtio'><stats period='5'/></memballoon>",
            "<rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>",
        ]
        .map(String::from),
    );
    if let Some(path) = spec.tpm {
        devices.push(tpm::device_xml(path));
    }
    // A panic device turns a guest crash into libvirt's crashed event: Debian loads the pvpanic driver for `isa`, and
    // Windows reports a stop error through the Hyper-V crash registers.
    devices.push(format!(
        "<panic model='{}'/>",
        if spec.windows { "hyperv" } else { "isa" }
    ));
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
         {os}\
         <features>{features}{smm}</features>\
         <cpu mode='host-passthrough'/>\
         {clock}\
         <on_crash>{on_crash}</on_crash>\
         {pm}\
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
        on_crash = spec.on_crash.as_xml(),
        os = match spec.pinned.filter(|_| spec.uefi) {
            Some(pinned) => pinned.os_xml(),
            None => format!(
                "<os{}><type arch='x86_64' machine='q35'>hvm</type>{}{}</os>",
                if spec.uefi { " firmware='efi'" } else { "" },
                autoselection_features(spec),
                autoselection_nvram(spec),
            ),
        },
        // libvirt refuses a secure loader without SMM, and fills SMM in itself only under autoselection.
        smm = if spec.pinned.is_some_and(|p| p.secure) && spec.uefi {
            "<smm state='on'/>"
        } else {
            ""
        },
        // Autoselection refuses suspend to disk, so a UEFI VM that hibernates gets `<pm>` with the pinned firmware.
        pm = if spec.uefi && spec.hibernate && spec.pinned.is_none() {
            String::new()
        } else {
            format!(
                "<pm><suspend-to-mem enabled='no'/><suspend-to-disk enabled='{}'/></pm>",
                if spec.hibernate { "yes" } else { "no" }
            )
        },
        devices = devices.concat(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revert_to_a_paused_snapshot_activates_the_vm_and_restores_the_snapshot_s_memory() {
        let snap = |state: &str, memory: &str| {
            format!(
                "<domainsnapshot><state>{state}</state><domain>{memory}</domain></domainsnapshot>"
            )
        };
        for (state, running, active) in [
            ("running", true, true),
            ("paused", false, true),
            ("shutoff", false, false),
        ] {
            let xml = snap(state, "");
            let activates = snapshot_was_running(&xml).unwrap()
                || snapshot_state(&xml).unwrap().as_deref() == Some("paused");
            assert_eq!(snapshot_was_running(&xml).unwrap(), running, "{state}");
            assert_eq!(activates, active, "{state}");
        }
        let mib = |memory: &str| snapshot_memory_mib(&snap("running", memory)).unwrap();
        assert_eq!(mib("<memory unit='KiB'>12582912</memory>"), Some(12288));
        assert_eq!(mib("<memory>1048576</memory>"), Some(1024));
        assert_eq!(mib("<memory unit='GiB'>2</memory>"), Some(2048));
        assert_eq!(mib("<memory unit='b'>1</memory>"), Some(1));
        assert_eq!(mib(""), None);
        assert!(snapshot_memory_mib(&snap("running", "<memory unit='parsec'>1</memory>")).is_err());
    }

    #[test]
    fn the_total_memory_cap_refuses_a_start_that_would_pass_it_and_names_the_running_vms() {
        let running = vec![("t-a".to_string(), 4096), ("t-b".to_string(), 2048)];
        assert!(check_total_memory(16384, "w", 10240, &running).is_ok());
        let err = check_total_memory(16384, "w", 12288, &running)
            .unwrap_err()
            .to_string();
        assert!(err.contains("starting 'w' (12288 MiB)"), "{err}");
        assert!(err.contains("6144 + 12288 MiB"), "{err}");
        assert!(err.contains("max_total_memory_mib = 16384"), "{err}");
        assert!(err.contains("t-a 4096 MiB, t-b 2048 MiB"), "{err}");
        let alone = check_total_memory(1024, "w", 2048, &[])
            .unwrap_err()
            .to_string();
        assert!(alone.contains("running: none"), "{alone}");
    }

    #[test]
    fn a_vm_larger_than_the_total_memory_cap_is_refused_at_create_and_configure() {
        let mut m = Manage {
            max_memory_mib: 12288,
            ..Manage::default()
        };
        assert!(check_memory(&m, 12288).is_ok());
        m.max_total_memory_mib = Some(8192);
        assert!(check_memory(&m, 8192).is_ok());
        let err = check_memory(&m, 8193).unwrap_err().to_string();
        assert!(err.contains("manage.max_total_memory_mib"), "{err}");
    }

    #[test]
    fn a_revert_runs_only_a_snapshot_of_a_running_vm() {
        let snap = |state: &str| {
            format!(
                "<domainsnapshot><name>s</name><state>{state}</state><domain/></domainsnapshot>"
            )
        };
        assert!(snapshot_was_running(&snap("running")).unwrap());
        for state in ["shutoff", "paused", "disk-snapshot"] {
            assert!(!snapshot_was_running(&snap(state)).unwrap(), "{state}");
        }
        assert!(!snapshot_was_running("<domainsnapshot><name>s</name></domainsnapshot>").unwrap());
    }

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

    #[test]
    fn snapshot_and_vm_names_cannot_hold_the_volume_separator() {
        for bad in ["a@b", "@", "@@revert@s", "tpl@s1"] {
            assert!(check_name(bad, "n").is_err(), "{bad}");
            assert!(check_vm_name(bad, "n").is_err(), "{bad}");
        }
    }

    #[test]
    fn check_vm_name_refuses_tpl_only() {
        let err = check_vm_name("tpl", "VM name").unwrap_err().to_string();
        assert!(err.contains("tpl"), "{err}");
        for ok in ["tpl2", "my-tpl", "Tpl", "tpl.1", "tpl_", "t", "test-vm_1.2"] {
            assert!(check_vm_name(ok, "n").is_ok(), "{ok}");
        }
        for bad in ["", "-x", ".x", "a b", &"x".repeat(65)] {
            assert!(check_vm_name(bad, "n").is_err(), "{bad}");
        }
    }

    fn spec<'a>(disks: Vec<Disk<'a>>) -> DomainSpec<'a> {
        DomainSpec {
            name: "t",
            windows: false,
            vcpus: 2,
            memory_mib: 2048,
            uefi: false,
            tpm: None,
            secure_boot: SecureBoot::On,
            hibernate: false,
            pinned: None,
            nvram: None,
            on_crash: OnCrash::Preserve,
            disks,
            network: Some("net"),
            uuid: None,
            mac: None,
            owner: Owner::default(),
        }
    }

    /// The `enabled` value of each firmware feature in a domain's `<os>`, by name.
    fn firmware_features(xml: &str) -> Vec<(String, String)> {
        let doc = roxmltree::Document::parse(xml).unwrap();
        doc.descendants()
            .filter(|n| {
                n.has_tag_name("feature") && n.parent().is_some_and(|p| p.has_tag_name("firmware"))
            })
            .map(|n| {
                (
                    n.attribute("name").unwrap().into(),
                    n.attribute("enabled").unwrap().into(),
                )
            })
            .collect()
    }

    fn features(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, enabled)| ((*name).into(), (*enabled).into()))
            .collect()
    }

    #[test]
    fn domain_xml_writes_the_firmware_features_of_each_secure_boot_mode() {
        let uefi = |secure_boot| {
            domain_xml(&DomainSpec {
                uefi: true,
                secure_boot,
                ..spec(vec![Disk::Qcow2("/pool/t.qcow2")])
            })
        };
        for (mode, expected) in [
            (
                SecureBoot::Off,
                features(&[("secure-boot", "no"), ("enrolled-keys", "no")]),
            ),
            (
                SecureBoot::On,
                features(&[("secure-boot", "yes"), ("enrolled-keys", "yes")]),
            ),
            (
                SecureBoot::Setup,
                features(&[("secure-boot", "yes"), ("enrolled-keys", "no")]),
            ),
        ] {
            let xml = uefi(mode);
            assert_eq!(firmware_features(&xml), expected, "{mode:?}");
            let doc = roxmltree::Document::parse(&xml).unwrap();
            let os = child(doc.root_element(), "os").unwrap();
            assert_eq!(os.attribute("firmware"), Some("efi"), "{mode:?}");
            assert_eq!(
                child(os, "loader").and_then(|l| l.attribute("type")),
                Some("pflash"),
                "{mode:?}"
            );
            // What the reader says about the definition the server wrote.
            assert_eq!(secure_boot_state(os, None), Some(mode), "{mode:?}");
        }
        // BIOS has no firmware element, whatever the mode says.
        assert!(firmware_features(&domain_xml(&spec(vec![Disk::Qcow2("/p/t.qcow2")]))).is_empty());
    }

    #[test]
    fn secure_boot_is_read_only_from_what_the_xml_says() {
        let state = |os_inner: &str| {
            let xml = format!("<domain><os firmware='efi'>{os_inner}</os></domain>");
            let doc = roxmltree::Document::parse(&xml).unwrap();
            secure_boot_state(child(doc.root_element(), "os").unwrap(), None)
        };
        // What libvirt fills in after picking the secure-enrolled firmware.
        assert_eq!(
            state(
                "<firmware><feature enabled='yes' name='secure-boot'/></firmware>\
                 <loader secure='yes' type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>"
            ),
            Some(SecureBoot::On)
        );
        assert_eq!(
            state("<loader secure='no' type='pflash'>/x.fd</loader>"),
            Some(SecureBoot::Off)
        );
        // Secure Boot firmware without enrolled keys is setup mode, even when libvirt added the loader's own flag.
        assert_eq!(
            state(
                "<firmware><feature enabled='yes' name='secure-boot'/>\
                 <feature enabled='no' name='enrolled-keys'/></firmware>\
                 <loader secure='yes' type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.secboot.fd</loader>"
            ),
            Some(SecureBoot::Setup)
        );
        // Without Secure Boot, the keys do not matter.
        assert_eq!(
            state(
                "<firmware><feature enabled='no' name='secure-boot'/>\
                 <feature enabled='yes' name='enrolled-keys'/></firmware>"
            ),
            Some(SecureBoot::Off)
        );
        // A loader named by path alone, as virt-manager writes, says nothing.
        assert_eq!(
            state("<loader readonly='yes' type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>"),
            None
        );
        assert_eq!(state(""), None);
    }

    #[test]
    fn describe_keeps_the_boolean_and_adds_the_mode() {
        let fields = |uefi, os_inner: &str| {
            let xml = format!("<domain><os firmware='efi'>{os_inner}</os></domain>");
            let doc = roxmltree::Document::parse(&xml).unwrap();
            secure_boot_fields(uefi, child(doc.root_element(), "os"), None)
        };
        let feature = |secure: &str, keys: &str| {
            format!(
                "<firmware><feature enabled='{secure}' name='secure-boot'/>\
                 <feature enabled='{keys}' name='enrolled-keys'/></firmware>"
            )
        };
        assert_eq!(
            fields(true, &feature("yes", "yes")),
            (Some(true), Some("on"))
        );
        assert_eq!(
            fields(true, &feature("no", "no")),
            (Some(false), Some("off"))
        );
        // Setup mode has Secure Boot firmware, but no keys, so the boolean reads false.
        assert_eq!(
            fields(true, &feature("yes", "no")),
            (Some(false), Some("setup"))
        );
        // The XML does not say: both fields are unknown.
        assert_eq!(fields(true, ""), (None, None));
        // A BIOS VM has no Secure Boot.
        assert_eq!(fields(false, ""), (Some(false), Some("off")));
    }

    #[test]
    fn secure_boot_off_and_setup_need_uefi() {
        for mode in [SecureBoot::On, SecureBoot::Off, SecureBoot::Setup] {
            assert!(check_secure_boot(true, mode).is_ok(), "{mode:?}");
        }
        assert!(check_secure_boot(false, SecureBoot::On).is_ok());
        for mode in [SecureBoot::Off, SecureBoot::Setup] {
            let err = check_secure_boot(false, mode).unwrap_err().to_string();
            assert_eq!(
                err, "secure_boot \"off\" and \"setup\" need firmware \"uefi\"",
                "{mode:?}"
            );
        }
    }

    /// The crash action, the panic models and the balloon's stats period in a domain's XML.
    fn crash_devices(xml: &str) -> (Option<String>, Vec<String>, Option<String>) {
        let doc = roxmltree::Document::parse(xml).unwrap();
        let root = doc.root_element();
        let on_crash = child(root, "on_crash")
            .and_then(|n| n.text())
            .map(String::from);
        let panics = doc
            .descendants()
            .filter(|n| n.has_tag_name("panic"))
            .filter_map(|n| n.attribute("model").map(String::from))
            .collect();
        let period = doc
            .descendants()
            .find(|n| {
                n.has_tag_name("stats") && n.parent().is_some_and(|p| p.has_tag_name("memballoon"))
            })
            .and_then(|n| n.attribute("period").map(String::from));
        (on_crash, panics, period)
    }

    #[test]
    fn created_vms_get_a_panic_device_stats_and_the_crash_action() {
        for (windows, model) in [(false, "isa"), (true, "hyperv")] {
            for secure_boot in [SecureBoot::On, SecureBoot::Off, SecureBoot::Setup] {
                for on_crash in [OnCrash::Preserve, OnCrash::Destroy] {
                    let xml = domain_xml(&DomainSpec {
                        windows,
                        uefi: true,
                        secure_boot,
                        on_crash,
                        ..spec(vec![Disk::Qcow2("/pool/t.qcow2")])
                    });
                    assert_eq!(
                        crash_devices(&xml),
                        (
                            Some(on_crash.as_xml().into()),
                            vec![model.into()],
                            Some("5".into())
                        ),
                        "windows {windows}, secure_boot {secure_boot:?}"
                    );
                    let doc = roxmltree::Document::parse(&xml).unwrap();
                    let os = child(doc.root_element(), "os").unwrap();
                    assert_eq!(secure_boot_state(os, None), Some(secure_boot));
                }
            }
        }
    }

    #[test]
    fn a_reset_keeps_what_the_vm_has() {
        // A VM made before this server wrote crash devices: no panic device, no stats period, libvirt's own action.
        let old = format!(
            "<domain type='kvm'><name>t</name><metadata>{}</metadata><on_crash>destroy</on_crash>\
             <devices><disk type='file' device='disk'><driver name='qemu' type='qcow2'/>\
             <source file='/pool/t-g1.qcow2'/><target dev='vda' bus='virtio'/></disk>\
             <memballoon model='virtio'/></devices></domain>",
            Owner::default().element()
        );
        let (xml, _) = replace_disks(&old, &[Disk::Qcow2("/pool/t-g2.qcow2")]).unwrap();
        let xml = replace_owner(&xml, &Owner::default()).unwrap();
        assert_eq!(crash_devices(&xml), (Some("destroy".into()), vec![], None));
    }

    const POOL: &str = "/var/lib/libvirt/images/agent-vms";
    const UUID: &str = "c29b872d-e13c-4c6a-8477-154fd6b9940e";
    const MAC: &str = "52:54:00:fb:99:ec";

    /// A VM's inactive definition right after its first define by autoselection, with the `<os>` element of each
    /// Secure Boot mode as probe 8 recorded it (`docs/research/2026-11-24-custom-templates-build-probes.md`).
    fn autoselected(mode: SecureBoot) -> String {
        let (secure, keys, loader, template, smm) = match mode {
            SecureBoot::Off => (
                "no",
                "no",
                "<loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>",
                "/usr/share/OVMF/OVMF_VARS_4M.fd",
                "",
            ),
            SecureBoot::Setup => (
                "yes",
                "no",
                "<loader readonly='yes' secure='yes' type='pflash' format='raw'>\
                 /usr/share/OVMF/OVMF_CODE_4M.secboot.fd</loader>",
                "/usr/share/OVMF/OVMF_VARS_4M.fd",
                "<smm state='on'/>",
            ),
            SecureBoot::On => (
                "yes",
                "yes",
                "<loader readonly='yes' secure='yes' type='pflash' format='raw'>\
                 /usr/share/OVMF/OVMF_CODE_4M.ms.fd</loader>",
                "/usr/share/OVMF/OVMF_VARS_4M.ms.fd",
                "<smm state='on'/>",
            ),
        };
        format!(
            "<domain type='kvm'><name>t-ph</name><uuid>{UUID}</uuid>\
             <os firmware='efi'><type arch='x86_64' machine='pc-q35-10.2'>hvm</type>\
             <firmware><feature enabled='{keys}' name='enrolled-keys'/>\
             <feature enabled='{secure}' name='secure-boot'/></firmware>{loader}\
             <nvram template='{template}' templateFormat='raw' format='raw'>{POOL}/t-ph.vars</nvram>\
             <boot dev='hd'/></os><features><acpi/><apic/>{smm}</features>\
             <pm><suspend-to-mem enabled='no'/><suspend-to-disk enabled='no'/></pm>\
             <devices><interface type='network'><mac address='{MAC}'/><source network='default'/>\
             <model type='virtio'/></interface></devices></domain>"
        )
    }

    fn pinned(mode: SecureBoot) -> PinnedFirmware {
        ResolvedDefinition::from_inactive_xml(&autoselected(mode))
            .unwrap()
            .firmware
    }

    /// The `<suspend-to-mem>` and `<suspend-to-disk>` values of a domain's `<pm>`, or `None` without `<pm>`.
    fn pm(xml: &str) -> Option<(String, String)> {
        let doc = roxmltree::Document::parse(xml).unwrap();
        let pm = child(doc.root_element(), "pm")?;
        let enabled = |tag: &str| {
            child(pm, tag)
                .and_then(|n| n.attribute("enabled"))
                .unwrap_or("absent")
                .to_string()
        };
        Some((enabled("suspend-to-mem"), enabled("suspend-to-disk")))
    }

    fn pm_is(mem: &str, disk: &str) -> Option<(String, String)> {
        Some((mem.into(), disk.into()))
    }

    #[test]
    fn hibernate_sets_suspend_to_disk_and_leaves_suspend_to_memory_off() {
        let xml = |uefi, hibernate| {
            domain_xml(&DomainSpec {
                uefi,
                hibernate,
                ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
            })
        };
        assert_eq!(pm(&xml(false, true)), pm_is("no", "yes"));
        assert_eq!(pm(&xml(false, false)), pm_is("no", "no"));
        assert_eq!(pm(&xml(true, false)), pm_is("no", "no"));
    }

    #[test]
    fn a_uefi_vm_that_hibernates_is_first_defined_without_pm() {
        // Autoselection refuses `<suspend-to-disk enabled='yes'/>`, so the first definition leaves `<pm>` out.
        let xml = domain_xml(&DomainSpec {
            uefi: true,
            hibernate: true,
            ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
        });
        assert_eq!(pm(&xml), None);
        assert!(xml.contains("<os firmware='efi'>"), "{xml}");
    }

    #[test]
    fn the_explicit_os_names_the_resolved_files_of_each_mode() {
        // Probe 8: libvirt resolved these files, and accepted the explicit form with `<pm>` for each mode.
        let want = [
            (SecureBoot::Off, "OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd", None),
            (
                SecureBoot::Setup,
                "OVMF_CODE_4M.secboot.fd",
                "OVMF_VARS_4M.fd",
                Some("yes"),
            ),
            (
                SecureBoot::On,
                "OVMF_CODE_4M.ms.fd",
                "OVMF_VARS_4M.ms.fd",
                Some("yes"),
            ),
        ];
        for (mode, code, vars, secure) in want {
            let os = pinned(mode).os_xml();
            let doc = roxmltree::Document::parse(&os).unwrap();
            let os = doc.root_element();
            assert_eq!(os.tag_name().name(), "os", "{mode:?}");
            assert_eq!(os.attribute("firmware"), None, "{mode:?}");
            assert!(child(os, "firmware").is_none(), "{mode:?}");
            let loader = child(os, "loader").unwrap();
            assert_eq!(
                loader.text(),
                Some(format!("/usr/share/OVMF/{code}").as_str()),
                "{mode:?}"
            );
            assert_eq!(loader.attribute("type"), Some("pflash"));
            assert_eq!(loader.attribute("readonly"), Some("yes"));
            assert_eq!(loader.attribute("format"), Some("raw"));
            // Secure Boot off is written as `secure='no'`, so that the reader still sees a mode.
            assert_eq!(loader.attribute("secure"), Some(secure.unwrap_or("no")));
            let nvram = child(os, "nvram").unwrap();
            assert_eq!(
                nvram.attribute("template"),
                Some(format!("/usr/share/OVMF/{vars}").as_str())
            );
            assert_eq!(nvram.attribute("templateFormat"), Some("raw"));
            assert_eq!(nvram.attribute("format"), Some("raw"));
            assert_eq!(nvram.text(), Some(format!("{POOL}/t-ph.vars").as_str()));
        }
    }

    #[test]
    fn the_resolved_definition_keeps_the_uuid_and_the_mac_address() {
        let resolved =
            ResolvedDefinition::from_inactive_xml(&autoselected(SecureBoot::Off)).unwrap();
        assert_eq!(resolved.uuid, UUID);
        assert_eq!(resolved.mac.as_deref(), Some(MAC));
        let without_nic = autoselected(SecureBoot::Off)
            .replace(&format!("<mac address='{MAC}'/>"), "")
            .replace("<interface type='network'>", "<interface type='user'>");
        assert!(
            ResolvedDefinition::from_inactive_xml(&without_nic)
                .unwrap()
                .mac
                .is_none()
        );
    }

    #[test]
    fn a_definition_without_a_loader_a_template_or_variables_is_refused() {
        let off = autoselected(SecureBoot::Off);
        let loader = "<loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>";
        let template = " template='/usr/share/OVMF/OVMF_VARS_4M.fd'";
        let vars = format!("{POOL}/t-ph.vars");
        let cases = [
            ("loader", off.replace(loader, "")),
            (
                "loader",
                off.replace(">/usr/share/OVMF/OVMF_CODE_4M.fd</loader>", "></loader>"),
            ),
            ("template", off.replace(template, "")),
            ("nvram", off.replace(&vars, "")),
            ("uuid", off.replace(&format!("<uuid>{UUID}</uuid>"), "")),
        ];
        for (missing, xml) in cases {
            let err = ResolvedDefinition::from_inactive_xml(&xml)
                .unwrap_err()
                .to_string();
            assert!(err.contains(missing), "{missing}: {err}");
        }
        assert!(ResolvedDefinition::from_inactive_xml("<domain><name>x</name></domain>").is_err());
    }

    #[test]
    fn the_second_definition_is_explicit_and_carries_pm_smm_uuid_and_mac() {
        for mode in [SecureBoot::Off, SecureBoot::Setup, SecureBoot::On] {
            let resolved = ResolvedDefinition::from_inactive_xml(&autoselected(mode)).unwrap();
            let xml = domain_xml(&DomainSpec {
                uefi: true,
                secure_boot: mode,
                hibernate: true,
                pinned: Some(&resolved.firmware),
                uuid: Some(&resolved.uuid),
                mac: resolved.mac.as_deref(),
                ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
            });
            let doc = roxmltree::Document::parse(&xml).unwrap();
            let root = doc.root_element();
            let os = child(root, "os").unwrap();
            assert_eq!(os.attribute("firmware"), None, "{mode:?}");
            assert!(firmware_features(&xml).is_empty(), "{mode:?}");
            assert_eq!(pm(&xml), pm_is("no", "yes"), "{mode:?}");
            assert_eq!(
                child(root, "uuid").and_then(|n| n.text()),
                Some(UUID),
                "{mode:?}"
            );
            assert!(xml.contains(&format!("<mac address='{MAC}'/>")), "{mode:?}");
            // libvirt refuses a secure loader without SMM: "Secure boot requires SMM feature enabled".
            let smm =
                child(child(root, "features").unwrap(), "smm").and_then(|n| n.attribute("state"));
            assert_eq!(smm, (mode != SecureBoot::Off).then_some("on"), "{mode:?}");
        }
    }

    #[test]
    fn the_explicit_form_still_shows_its_secure_boot_mode() {
        for mode in [SecureBoot::Off, SecureBoot::Setup, SecureBoot::On] {
            let xml = domain_xml(&DomainSpec {
                uefi: true,
                secure_boot: mode,
                hibernate: true,
                pinned: Some(&pinned(mode)),
                ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
            });
            let doc = roxmltree::Document::parse(&xml).unwrap();
            let os = child(doc.root_element(), "os");
            assert_eq!(
                os.and_then(|o| secure_boot_state(o, None)),
                Some(mode),
                "{mode:?}"
            );
            let (boolean, word) = secure_boot_fields(true, os, None);
            assert_eq!(boolean, Some(mode == SecureBoot::On), "{mode:?}");
            assert_eq!(word, Some(mode.as_str()), "{mode:?}");
        }
    }

    #[test]
    fn setup_and_on_differ_in_the_explicit_form_by_their_variables_template() {
        let state = |loader: &str, template: &str| {
            let xml = format!(
                "<domain><os><loader secure='yes' type='pflash'>{loader}</loader>\
                 <nvram template='{template}'>/v.vars</nvram></os></domain>"
            );
            let doc = roxmltree::Document::parse(&xml).unwrap();
            secure_boot_state(child(doc.root_element(), "os").unwrap(), None)
        };
        // Debian's names and Fedora's, where the keys are in the `.ms` or `.secboot` variables.
        assert_eq!(
            state("/o/CODE_4M.secboot.fd", "/o/VARS_4M.fd"),
            Some(SecureBoot::Setup)
        );
        assert_eq!(
            state("/o/CODE_4M.ms.fd", "/o/VARS_4M.ms.fd"),
            Some(SecureBoot::On)
        );
        assert_eq!(
            state("/o/CODE.secboot.fd", "/o/VARS.secboot.fd"),
            Some(SecureBoot::On)
        );
        assert_eq!(
            state("/o/CODE.secboot.fd", "/o/VARS.fd"),
            Some(SecureBoot::Setup)
        );
        // Without a template to tell, a secure loader reads as on, as it did before this form existed.
        let xml =
            "<domain><os><loader secure='yes' type='pflash'>/o/CODE.fd</loader></os></domain>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        assert_eq!(
            secure_boot_state(child(doc.root_element(), "os").unwrap(), None),
            Some(SecureBoot::On)
        );
    }

    #[test]
    fn describe_reports_hibernate_from_the_persistent_definition() {
        let reads = |xml: &str| {
            let doc = roxmltree::Document::parse(xml).unwrap();
            suspends_to_disk(doc.root_element())
        };
        let xml = |hibernate| {
            domain_xml(&DomainSpec {
                hibernate,
                ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
            })
        };
        assert!(reads(&xml(true)));
        assert!(!reads(&xml(false)));
        // A VM without `<pm>`, as virt-manager defines one.
        assert!(!reads("<domain><name>x</name></domain>"));
    }

    /// A fake libvirt that records its calls and fails the one named.
    #[derive(Default)]
    struct FakeLibvirt {
        calls: Vec<String>,
        defined: Vec<String>,
        /// Fails the call with this number, counting `define` and `inactive_xml` together from 1.
        fail_at: Option<usize>,
        /// What `inactive_xml` returns.
        inactive: String,
    }

    impl FakeLibvirt {
        fn step(&mut self, what: &str) -> Result<()> {
            self.calls.push(what.into());
            if self.fail_at == Some(self.calls.len()) {
                bail!("{what} failed in libvirt");
            }
            Ok(())
        }
    }

    impl VmDefiner for FakeLibvirt {
        fn define(&mut self, xml: &str) -> Result<()> {
            self.step("define")?;
            self.defined.push(xml.into());
            Ok(())
        }

        fn inactive_xml(&mut self) -> Result<String> {
            self.step("inactive_xml")?;
            Ok(self.inactive.clone())
        }

        fn undefine(&mut self) {
            self.calls.push("undefine".into());
        }

        fn delete_disks(&mut self) {
            self.calls.push("delete_disks".into());
        }
    }

    fn hibernating(uefi: bool) -> DomainSpec<'static> {
        DomainSpec {
            uefi,
            hibernate: true,
            secure_boot: SecureBoot::Off,
            nvram: Some("/p/t-ph.vars"),
            ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
        }
    }

    /// A UEFI spec for `mode` whose variables sit in the pool.
    fn uefi_with_vars(mode: SecureBoot) -> DomainSpec<'static> {
        DomainSpec {
            uefi: true,
            secure_boot: mode,
            nvram: Some("/var/lib/libvirt/images/agent-vms/t.vars"),
            ..spec(vec![Disk::Qcow2(
                "/var/lib/libvirt/images/agent-vms/t.qcow2",
            )])
        }
    }

    #[test]
    fn a_uefi_vm_keeps_its_variables_in_the_pool_in_every_secure_boot_mode() {
        for mode in [SecureBoot::Off, SecureBoot::On, SecureBoot::Setup] {
            let xml = domain_xml(&uefi_with_vars(mode));
            let doc = roxmltree::Document::parse(&xml).unwrap();
            let os = child(doc.root_element(), "os").unwrap();
            let nvram: Vec<_> = os.children().filter(|n| n.has_tag_name("nvram")).collect();
            assert_eq!(nvram.len(), 1, "{mode:?}: {xml}");
            assert_eq!(
                nvram[0].text(),
                Some("/var/lib/libvirt/images/agent-vms/t.vars"),
                "{mode:?}"
            );
            assert_eq!(nvram[0].attribute("format"), Some("raw"), "{mode:?}");
            // Autoselection picks the template: naming one would make libvirt reject it, or pin the firmware.
            assert_eq!(nvram[0].attribute("template"), None, "{mode:?}");
            assert_eq!(os.attribute("firmware"), Some("efi"), "{mode:?}");
            // The variables element does not change what the mode reads as.
            assert_eq!(secure_boot_state(os, None), Some(mode), "{mode:?}");
            assert_eq!(firmware_features(&xml).len(), 2, "{mode:?}");
        }
    }

    #[test]
    fn a_bios_vm_has_no_nvram_element() {
        let bios = DomainSpec {
            nvram: Some("/p/t.vars"),
            ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
        };
        // Not even when a caller hands it a path.
        assert!(!domain_xml(&bios).contains("nvram"));
        assert!(!domain_xml(&spec(vec![Disk::Qcow2("/p/t.qcow2")])).contains("nvram"));
        // A UEFI spec without a path, as nothing builds today, leaves libvirt its own default.
        let none = DomainSpec {
            nvram: None,
            ..uefi_with_vars(SecureBoot::On)
        };
        assert!(!domain_xml(&none).contains("nvram"));
    }

    #[test]
    fn the_variables_file_sits_beside_the_disk_volume() {
        assert_eq!(
            vars_path("/var/lib/libvirt/images/agent-vms/t-x.qcow2", "t-x"),
            "/var/lib/libvirt/images/agent-vms/t-x.vars"
        );
        // The pool is wherever the disk is, and a VM's name carries no path.
        assert_eq!(vars_path("/srv/pool/t.qcow2", "t"), "/srv/pool/t.vars");
        assert_eq!(vars_path("t.qcow2", "t"), "t.vars");
    }

    #[test]
    fn the_leftover_refusal_names_the_volume_and_the_command_that_removes_it() {
        let err = leftover_vars_error("qemu:///system", "agent-vms", "t-lv").to_string();
        assert!(err.contains("t-lv.vars"), "{err}");
        assert!(
            err.contains("virsh -c qemu:///system vol-delete --pool agent-vms t-lv.vars"),
            "{err}"
        );
        assert!(err.contains("nothing was created"), "{err}");
    }

    /// A pool that answers from a fixed listing and records the calls made to it.
    struct FakePool {
        refresh: std::result::Result<(), &'static str>,
        volumes: Vec<&'static str>,
        /// The error a lookup of a volume fails with, in place of the listing.
        lookup_error: Option<&'static str>,
        calls: std::cell::RefCell<Vec<&'static str>>,
    }

    impl PoolListing for FakePool {
        fn refresh(&self) -> Result<()> {
            self.calls.borrow_mut().push("refresh");
            self.refresh.map_err(|m| anyhow!("{m}"))
        }

        fn has_volume(&self, name: &str) -> Result<bool> {
            self.calls.borrow_mut().push("has_volume");
            if let Some(m) = self.lookup_error {
                return Err(anyhow!("{m}"));
            }
            Ok(self.volumes.contains(&name))
        }
    }

    fn fake_pool(
        refresh: std::result::Result<(), &'static str>,
        volumes: &[&'static str],
    ) -> FakePool {
        FakePool {
            refresh,
            volumes: volumes.to_vec(),
            lookup_error: None,
            calls: Default::default(),
        }
    }

    /// A pool whose cached listing lacks a file until the pool is rescanned, as libvirt's does for a file another
    /// client made.
    struct CachedPool {
        on_disk: bool,
        scanned: std::cell::Cell<bool>,
        refresh_fails: bool,
    }

    impl PoolListing for CachedPool {
        fn refresh(&self) -> Result<()> {
            if self.refresh_fails {
                bail!("pool is busy");
            }
            self.scanned.set(true);
            Ok(())
        }

        fn has_volume(&self, _name: &str) -> Result<bool> {
            Ok(self.on_disk && self.scanned.get())
        }
    }

    #[test]
    fn a_vars_file_made_by_another_client_shows_in_the_listing_only_after_the_rescan() {
        let pool = |on_disk, refresh_fails| CachedPool {
            on_disk,
            scanned: false.into(),
            refresh_fails,
        };
        assert!(vars_listed(&pool(true, false), "t-x").unwrap());
        assert!(!vars_listed(&pool(false, false), "t-x").unwrap());
        // A rescan that fails gives no answer, so no record follows from a stale listing.
        assert!(vars_listed(&pool(false, true), "t-x").is_err());
    }

    fn refusal() -> anyhow::Error {
        anyhow!("leftover refusal")
    }

    #[test]
    fn a_failed_pool_refresh_fails_the_leftover_check_instead_of_reading_a_stale_listing() {
        // The stale listing has no leftover, and the check must still not pass.
        let pool = fake_pool(Err("pool is busy"), &[]);
        let err = check_no_leftover(&pool, "t-lv.vars", refusal).unwrap_err();
        assert!(format!("{err:#}").contains("pool is busy"), "{err:#}");
        assert!(!format!("{err:#}").contains("leftover refusal"), "{err:#}");
        // The listing is not read after the refresh failed.
        assert_eq!(*pool.calls.borrow(), ["refresh"]);
    }

    #[test]
    fn the_leftover_check_refreshes_the_pool_before_it_reads_the_listing() {
        let pool = fake_pool(Ok(()), &[]);
        check_no_leftover(&pool, "t-lv.vars", refusal).unwrap();
        assert_eq!(*pool.calls.borrow(), ["refresh", "has_volume"]);
    }

    #[test]
    fn the_leftover_check_refuses_a_volume_the_refreshed_pool_lists() {
        let pool = fake_pool(Ok(()), &["t-lv.vars", "t-other.vars"]);
        let err = check_no_leftover(&pool, "t-lv.vars", refusal).unwrap_err();
        assert_eq!(err.to_string(), "leftover refusal");
        check_no_leftover(&pool, "t-lv.tpm", refusal).unwrap();
    }

    #[test]
    fn a_failed_volume_lookup_fails_the_leftover_check_instead_of_reading_as_absent() {
        // A pool that cannot answer must not pass the check as if the leftover were not there.
        let mut pool = fake_pool(Ok(()), &[]);
        pool.lookup_error = Some("pool is gone");
        let err = check_no_leftover(&pool, "t-lv.tpm", refusal).unwrap_err();
        assert!(format!("{err:#}").contains("pool is gone"), "{err:#}");
        assert!(!format!("{err:#}").contains("leftover refusal"), "{err:#}");
    }

    #[test]
    fn only_libvirts_no_such_volume_error_means_the_volume_is_absent() {
        let found = || Ok::<(), (ErrorNumber, String)>(());
        assert!(volume_listed("v", found()).unwrap());
        let missing = Err((ErrorNumber::NoStorageVolume, "no volume".to_string()));
        assert!(!volume_listed("v", missing).unwrap());
        for code in [
            ErrorNumber::NoStoragePool,
            ErrorNumber::OperationInvalid,
            ErrorNumber::InternalError,
        ] {
            let err = volume_listed("v.tpm", Err((code, "it broke".to_string())))
                .unwrap_err()
                .to_string();
            assert!(err.contains("v.tpm") && err.contains("it broke"), "{err}");
        }
    }

    #[test]
    fn a_reset_restores_variables_from_either_uefi_form() {
        let flags = |os: &str| reset_flags(&format!("<domain><os{os}</os></domain>")).unwrap();
        let reset = sys::VIR_DOMAIN_START_RESET_NVRAM;
        // Autoselection, as every VM from the server's create tools is defined.
        assert_eq!(flags(" firmware='efi'><type>hvm</type>"), reset);
        // An explicit flash loader with variables: a hibernating VM, or one from a sealed template.
        assert_eq!(
            flags(
                "><loader type='pflash' readonly='yes'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>\
                 <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd'>/p/t.vars</nvram>"
            ),
            reset
        );
        // BIOS, a ROM firmware and a flash loader without variables have nothing to restore.
        assert_eq!(flags("><type>hvm</type>"), 0);
        assert_eq!(
            flags("><loader type='rom'>/usr/share/ovmf/OVMF.amdsev.fd</loader>"),
            0
        );
        assert_eq!(
            flags("><loader type='pflash'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>"),
            0
        );
        assert_eq!(
            flags("><loader type='rom'>/f.fd</loader><nvram>/p/t.vars</nvram>"),
            0
        );
    }

    #[test]
    fn delete_names_only_the_disks_of_the_pool() {
        // The server's own UEFI VM: one disk, and the variables, which libvirt deletes with the VM and `delete`
        // never looks up as a disk.
        let xml = domain_xml(&uefi_with_vars(SecureBoot::On));
        let disks = parse_disks(&xml).unwrap();
        assert_eq!(disks.len(), 1);
        assert!(disks.iter().all(|d| !d.source().ends_with(".vars")));
        // A volume of another pool is never named, whatever its volume name; a path is left to libvirt's lookup
        // of the pool that holds it, which `pool_volume` compares with the configured pool.
        let by_ref = |pool: &str| {
            let xml = format!(
                "<domain><devices><disk type='volume' device='disk'><source pool='{pool}' volume='x.vars'/>\
                 <target dev='vda'/></disk></devices></domain>"
            );
            parse_disks(&xml).unwrap()
        };
        let own = by_ref("agent-vms");
        assert_eq!(disk_ref(&own[0], "agent-vms"), DiskRef::Volume("x.vars"));
        let other = by_ref("elsewhere");
        assert_eq!(disk_ref(&other[0], "agent-vms"), DiskRef::OtherPool);
        let path = disk_at("/etc/passwd");
        assert_eq!(disk_ref(&path, "agent-vms"), DiskRef::Path("/etc/passwd"));
        let none = DiskInfo {
            file: None,
            ..disk_at("")
        };
        assert_eq!(disk_ref(&none, "agent-vms"), DiskRef::Empty);
    }

    #[test]
    fn a_bios_vm_that_hibernates_is_defined_once_with_pm() {
        let mut libvirt = FakeLibvirt::default();
        define_new_vm(&hibernating(false), &mut libvirt).unwrap();
        assert_eq!(libvirt.calls, ["define"]);
        assert_eq!(pm(&libvirt.defined[0]), pm_is("no", "yes"));
    }

    #[test]
    fn a_uefi_vm_without_hibernate_is_defined_once_by_autoselection() {
        let mut libvirt = FakeLibvirt::default();
        let spec = DomainSpec {
            hibernate: false,
            ..hibernating(true)
        };
        define_new_vm(&spec, &mut libvirt).unwrap();
        assert_eq!(libvirt.calls, ["define"]);
        assert!(libvirt.defined[0].contains("firmware='efi'"));
        assert_eq!(pm(&libvirt.defined[0]), pm_is("no", "no"));
    }

    #[test]
    fn a_uefi_vm_that_hibernates_is_defined_twice_and_the_second_keeps_its_identity() {
        let mut libvirt = FakeLibvirt {
            inactive: autoselected(SecureBoot::Off),
            ..Default::default()
        };
        define_new_vm(&hibernating(true), &mut libvirt).unwrap();
        assert_eq!(libvirt.calls, ["define", "inactive_xml", "define"]);
        let (first, second) = (&libvirt.defined[0], &libvirt.defined[1]);
        assert_eq!(pm(first), None);
        assert!(first.contains("firmware='efi'"));
        // The first definition asks for a variables file in the pool, and the second keeps the path libvirt
        // resolved from it, which is that file.
        assert!(
            first.contains("<nvram format='raw'>/p/t-ph.vars</nvram>"),
            "{first}"
        );
        assert!(
            second.contains(&format!("format='raw'>{POOL}/t-ph.vars</nvram>")),
            "{second}"
        );
        assert_eq!(second.matches("<nvram").count(), 1, "{second}");
        assert_eq!(pm(second), pm_is("no", "yes"));
        assert!(!second.contains("firmware"), "{second}");
        assert!(second.contains(&format!("<uuid>{UUID}</uuid>")));
        assert!(second.contains(&format!("<mac address='{MAC}'/>")));
        assert!(second.contains("OVMF_CODE_4M.fd"));
    }

    #[test]
    fn each_failed_step_discards_the_vm_and_names_the_step() {
        // (call that fails, text the error names, calls made)
        let cases: [(usize, &str, &[&str]); 3] = [
            // A define that failed made nothing to undefine.
            (1, "cannot define VM 't'", &["define", "delete_disks"]),
            (
                2,
                "read VM 't' after defining it",
                &["define", "inactive_xml", "undefine", "delete_disks"],
            ),
            (
                3,
                "redefine VM 't'",
                &[
                    "define",
                    "inactive_xml",
                    "define",
                    "undefine",
                    "delete_disks",
                ],
            ),
        ];
        for (fail_at, names, calls) in cases {
            let mut libvirt = FakeLibvirt {
                inactive: autoselected(SecureBoot::Off),
                fail_at: Some(fail_at),
                ..Default::default()
            };
            let err = define_new_vm(&hibernating(true), &mut libvirt).unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains(names), "{err}");
            assert!(err.contains("failed in libvirt"), "{err}");
            assert_eq!(libvirt.calls, calls, "{err}");
        }
    }

    #[test]
    fn a_definition_that_resolved_no_loader_is_refused_and_discarded() {
        let mut libvirt = FakeLibvirt {
            inactive: autoselected(SecureBoot::Off)
                .replace(">/usr/share/OVMF/OVMF_CODE_4M.fd</loader>", "></loader>"),
            ..Default::default()
        };
        let err = format!(
            "{:#}",
            define_new_vm(&hibernating(true), &mut libvirt).unwrap_err()
        );
        assert!(
            err.contains("pin the firmware of VM 't'") && err.contains("loader"),
            "{err}"
        );
        assert_eq!(
            libvirt.calls,
            ["define", "inactive_xml", "undefine", "delete_disks"]
        );
    }

    fn extra_isos(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| format!("/isos/{n}.iso")).collect()
    }

    /// The disks of a definition with the given (device, target, bus) triples.
    fn disks_of(devices: &[(&str, &str, &str)]) -> Vec<DiskInfo> {
        let body: String = devices
            .iter()
            .map(|(device, target, bus)| {
                format!("<disk type='file' device='{device}'><target dev='{target}' bus='{bus}'/></disk>")
            })
            .collect();
        parse_disks(&format!("<domain><devices>{body}</devices></domain>")).unwrap()
    }

    #[test]
    fn five_extra_isos_are_refused_and_four_are_not() {
        assert!(check_extra_isos(&extra_isos(&["a", "b", "c", "d"])).is_ok());
        assert!(check_extra_isos(&[]).is_ok());
        let err = check_extra_isos(&extra_isos(&["a", "b", "c", "d", "e"]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("extra_isos") && err.contains("at most 4") && err.contains('5'),
            "{err}"
        );
    }

    #[test]
    fn create_writes_installer_driver_and_extras_in_order_and_only_the_first_boots() {
        let extras = [
            PathBuf::from("/isos/x1.iso"),
            PathBuf::from("/isos/x2.iso"),
            PathBuf::from("/isos/x3.iso"),
        ];
        let xml = domain_xml(&spec(create_disks(
            "/pool/t.qcow2",
            Some(Path::new("/isos/install.iso")),
            Some(Path::new("/isos/drivers.iso")),
            &extras,
        )));
        let disks = parse_disks(&xml).unwrap();
        let layout: Vec<_> = disks
            .iter()
            .map(|d| {
                (
                    d.target.as_str(),
                    d.bus.as_str(),
                    d.device.as_str(),
                    d.file.as_deref().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            layout,
            [
                ("vda", "virtio", "disk", "/pool/t.qcow2"),
                ("sda", "sata", "cdrom", "/isos/install.iso"),
                ("sdb", "sata", "cdrom", "/isos/drivers.iso"),
                ("sdc", "sata", "cdrom", "/isos/x1.iso"),
                ("sdd", "sata", "cdrom", "/isos/x2.iso"),
                ("sde", "sata", "cdrom", "/isos/x3.iso"),
            ]
        );
        // Only the system disk (1) and the installer drive (2) have a boot order; every extra drive is read-only.
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let boots: Vec<_> = doc
            .descendants()
            .filter(|n| n.has_tag_name("disk"))
            .map(|d| {
                let target = child(d, "target").unwrap().attribute("dev").unwrap();
                let order = child(d, "boot").and_then(|b| b.attribute("order"));
                (target, order, child(d, "readonly").is_some())
            })
            .collect();
        assert_eq!(
            boots,
            [
                ("vda", Some("1"), false),
                ("sda", Some("2"), true),
                ("sdb", None, true),
                ("sdc", None, true),
                ("sdd", None, true),
                ("sde", None, true),
            ]
        );
    }

    #[test]
    fn create_without_a_driver_iso_puts_the_extras_right_after_the_installer() {
        let extras = [PathBuf::from("/isos/x1.iso")];
        let xml = domain_xml(&spec(create_disks("/pool/t.qcow2", None, None, &extras)));
        let targets: Vec<_> = parse_disks(&xml)
            .unwrap()
            .into_iter()
            .map(|d| (d.target, d.file))
            .collect();
        assert_eq!(
            targets,
            [
                ("vda".to_string(), Some("/pool/t.qcow2".to_string())),
                ("sda".to_string(), None),
                ("sdb".to_string(), Some("/isos/x1.iso".to_string())),
            ]
        );
    }

    #[test]
    fn sata_devices_counts_cdroms_and_sata_disks_but_not_virtio_ones() {
        let extras = [
            PathBuf::from("/isos/x1.iso"),
            PathBuf::from("/isos/x2.iso"),
            PathBuf::from("/isos/x3.iso"),
        ];
        let xml = domain_xml(&spec(create_disks(
            "/pool/t.qcow2",
            None,
            Some(Path::new("/isos/drivers.iso")),
            &extras,
        )));
        // The installer drive, the driver ISO and three extras; the virtio system disk uses no port.
        assert_eq!(sata_devices(&parse_disks(&xml).unwrap()), 5);
        let mixed = disks_of(&[
            ("disk", "vda", "virtio"),
            ("disk", "sda", "sata"),
            ("cdrom", "sdb", "sata"),
            ("disk", "sdc", "usb"),
            ("cdrom", "hda", "ide"),
        ]);
        assert_eq!(sata_devices(&mixed), 2);
    }

    #[test]
    fn six_sata_devices_fit_and_a_seventh_is_refused_by_name() {
        assert!(check_sata_ports("t-x", 6).is_ok());
        let err = check_sata_ports("t-x", 7).unwrap_err().to_string();
        assert!(
            err.contains("t-x") && err.contains('7') && err.contains("SATA") && err.contains('6'),
            "{err}"
        );
    }

    #[test]
    fn set_cdrom_changes_the_named_or_first_cdrom_drive_even_when_running() {
        let disks = disks_of(&[
            ("disk", "vda", "virtio"),
            ("cdrom", "sda", "sata"),
            ("cdrom", "sdb", "sata"),
        ]);
        assert_eq!(
            plan_cdrom("t-x", &disks, true, None).unwrap(),
            CdromPlan::Change(1)
        );
        assert_eq!(
            plan_cdrom("t-x", &disks, true, Some("sdb")).unwrap(),
            CdromPlan::Change(2)
        );
        assert_eq!(
            plan_cdrom("t-x", &disks, false, Some("sda")).unwrap(),
            CdromPlan::Change(1)
        );
    }

    #[test]
    fn set_cdrom_adds_a_drive_at_a_free_sd_target_on_a_shut_off_vm() {
        let disks = disks_of(&[("disk", "vda", "virtio"), ("cdrom", "sda", "sata")]);
        assert_eq!(
            plan_cdrom("t-x", &disks, false, Some("sdc")).unwrap(),
            CdromPlan::Add("sdc".into())
        );
        // Without a CD-ROM drive, no target means the first free one.
        let none = disks_of(&[("disk", "vda", "virtio"), ("disk", "vdb", "virtio")]);
        assert_eq!(
            plan_cdrom("t-x", &none, false, None).unwrap(),
            CdromPlan::Add("sda".into())
        );
        assert_eq!(
            plan_cdrom("t-x", &none, false, Some("sdc")).unwrap(),
            CdromPlan::Add("sdc".into())
        );
    }

    #[test]
    fn set_cdrom_refuses_a_used_target_a_non_sd_name_and_a_running_vm() {
        let disks = disks_of(&[
            ("disk", "vda", "virtio"),
            ("disk", "sda", "sata"),
            ("cdrom", "sdb", "sata"),
        ]);
        // A used target that holds a disk is no CD-ROM drive.
        for used in ["vda", "sda"] {
            let err = plan_cdrom("t-x", &disks, false, Some(used))
                .unwrap_err()
                .to_string();
            assert!(err.contains(used) && err.contains("not a CD-ROM"), "{err}");
        }
        // Only sd<letter> names a new drive.
        for bad in [
            "hdc", "vdc", "sd", "sdc1", "sdAB", "SDC", "sdC", "sd1", "", "sdc'",
        ] {
            let err = plan_cdrom("t-x", &disks, false, Some(bad))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("sd<letter>") || err.contains("sda to sdz"),
                "{bad}: {err}"
            );
        }
        // A running VM keeps refusing to gain a drive, with or without a target.
        for target in [Some("sdc"), None] {
            let none = disks_of(&[("disk", "vda", "virtio")]);
            let err = plan_cdrom("t-x", &none, true, target)
                .unwrap_err()
                .to_string();
            assert!(err.contains("shut it down"), "{err}");
        }
    }

    #[test]
    fn set_cdrom_refuses_a_seventh_sata_device_and_adds_a_sixth() {
        let five: Vec<_> = ["sda", "sdb", "sdc", "sdd", "sde"]
            .iter()
            .map(|t| ("cdrom", *t, "sata"))
            .collect();
        let five = disks_of(&five);
        assert_eq!(
            plan_cdrom("t-x", &five, false, Some("sdf")).unwrap(),
            CdromPlan::Add("sdf".into())
        );
        let six: Vec<_> = ["sda", "sdb", "sdc", "sdd", "sde", "sdf"]
            .iter()
            .map(|t| ("cdrom", *t, "sata"))
            .collect();
        let six = disks_of(&six);
        let err = plan_cdrom("t-x", &six, false, Some("sdg"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("7") && err.contains("SATA"), "{err}");
        // A SATA disk takes a port too.
        let mixed = disks_of(&[
            ("disk", "sda", "sata"),
            ("cdrom", "sdb", "sata"),
            ("cdrom", "sdc", "sata"),
            ("cdrom", "sdd", "sata"),
            ("cdrom", "sde", "sata"),
            ("cdrom", "sdf", "sata"),
        ]);
        assert!(plan_cdrom("t-x", &mixed, false, Some("sdg")).is_err());
        // Changing media in a full VM is no new device.
        assert_eq!(
            plan_cdrom("t-x", &six, false, Some("sdf")).unwrap(),
            CdromPlan::Change(5)
        );
    }

    #[test]
    fn domain_xml_parses_and_escapes() {
        let xml = domain_xml(&DomainSpec {
            windows: true,
            uefi: true,
            tpm: Some("/pool/t.tpm"),
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
    fn a_tpm_is_a_file_source_in_the_pool_and_only_when_asked_for() {
        let tpm_source = |spec: &DomainSpec<'_>| {
            let xml = domain_xml(spec);
            let doc = roxmltree::Document::parse(&xml).unwrap();
            let tpms: Vec<_> = doc
                .descendants()
                .filter(|n| n.has_tag_name("tpm"))
                .collect();
            assert!(tpms.len() <= 1, "{xml}");
            tpms.first().map(|tpm| {
                let source = doc
                    .descendants()
                    .find(|n| n.has_tag_name("source") && n.parent() == child(*tpm, "backend"))
                    .unwrap_or_else(|| panic!("no file source: {xml}"));
                (
                    tpm.attribute("model").map(String::from),
                    source.attribute("type").map(String::from),
                    source.attribute("path").map(String::from),
                )
            })
        };
        let path = "/var/lib/libvirt/images/agent-vms/t.tpm";
        let disks = || vec![Disk::Qcow2("/var/lib/libvirt/images/agent-vms/t.qcow2")];
        for uefi in [false, true] {
            let with = DomainSpec {
                uefi,
                tpm: Some(path),
                ..spec(disks())
            };
            assert_eq!(
                tpm_source(&with),
                Some((
                    Some("tpm-crb".into()),
                    Some("file".into()),
                    Some(path.into())
                )),
                "uefi {uefi}"
            );
            let without = DomainSpec {
                uefi,
                tpm: None,
                ..spec(disks())
            };
            assert_eq!(tpm_source(&without), None, "uefi {uefi}");
        }
        // The hibernation redefinition keeps the TPM beside the pinned firmware.
        let pinned_spec = DomainSpec {
            uefi: true,
            hibernate: true,
            tpm: Some(path),
            ..spec(disks())
        };
        assert!(domain_xml(&pinned_spec).contains(&format!("path='{path}'")));
    }

    #[test]
    fn the_tpm_state_sits_beside_the_disk_volume_in_the_pool() {
        assert_eq!(
            tpm_path("/var/lib/libvirt/images/agent-vms/t-x.qcow2", "t-x"),
            "/var/lib/libvirt/images/agent-vms/t-x.tpm"
        );
        assert_eq!(tpm_path("/srv/pool/t.qcow2", "t"), "/srv/pool/t.tpm");
        // A name that `check_vm_name` accepts holds no separator, so the path stays in the disk's directory.
        for name in ["a", "a.b", "a_b-c", "..", "x.tpm"] {
            if check_vm_name(name, "n").is_err() {
                continue;
            }
            let path = tpm_path("/srv/pool/d.qcow2", name);
            assert_eq!(
                Path::new(&path).parent(),
                Some(Path::new("/srv/pool")),
                "{name}: {path}"
            );
            assert_eq!(
                Path::new(&path).file_name().unwrap().to_str(),
                Some(format!("{name}.tpm").as_str())
            );
        }
    }

    #[test]
    fn the_tpm_leftover_refusal_names_the_volume_and_the_command_that_removes_it() {
        let err = leftover_tpm_error("qemu:///system", "agent-vms", "t-lv").to_string();
        assert!(
            err.contains("volume 't-lv.tpm' already exists in the disk pool 'agent-vms'"),
            "{err}"
        );
        assert!(
            err.contains("virsh -c qemu:///system vol-delete --pool agent-vms t-lv.tpm"),
            "{err}"
        );
        assert!(err.contains("nothing was created"), "{err}");
        assert!(err.contains("TPM state"), "{err}");
    }

    #[test]
    fn template_domain_xml() {
        let owner = Owner {
            template: Some("debian13".into()),
            generation: Some(2),
            expires: Some(1_794_300_000),
            ..Owner::default()
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
            secure_boot: Some(SecureBoot::Setup),
            firmware_code: Some("/o/it's \"CODE\" <4M>&.fd".into()),
            firmware_sha256: Some("ab".repeat(32)),
        };
        assert_eq!(Owner::from_element(&owner.element()).unwrap(), owner);
        assert_eq!(
            Owner::from_element(&Owner::default().element()).unwrap(),
            Owner::default()
        );
        let garbled =
            format!("<o xmlns='{OWNER_NS}' generation='x' expires='-1' secure_boot='maybe'/>");
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
            ..Owner::default()
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
        let err = expiry_report("t", "u-1", &outcomes, &[])
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
        assert!(expiry_report("t", "u-1", &outcomes, &[]).is_err());
    }

    #[test]
    fn expiry_report_names_deleted_and_outside_disks() {
        let outcomes = [
            ("/pool/t-g1.qcow2".to_string(), DiskDeletion::Deleted),
            ("/home/u/extra.img".to_string(), DiskDeletion::NotInPool),
        ];
        let msg = expiry_report("t", "u-1", &outcomes, &[]).unwrap();
        assert!(msg.starts_with("expired VM 't' (u-1) deleted; deleted disk /pool/t-g1.qcow2"));
        assert!(msg.contains("disk /home/u/extra.img is not a volume in manage.disk_pool"));
        assert!(msg.ends_with("so it was not deleted"));
    }

    #[test]
    fn expiry_report_names_tpm_state_kept_outside_the_pool() {
        let outside = ["/home/u/other.tpm".to_string()];
        let msg = expiry_report("t", "u-1", &[], &outside).unwrap();
        assert!(msg.contains("/home/u/other.tpm"), "{msg}");
        assert!(msg.contains("TPM state"), "{msg}");
        assert!(msg.contains("outside manage.disk_pool"), "{msg}");
        let none = expiry_report("t", "u-1", &[], &[]).unwrap();
        assert!(!none.contains("TPM"), "{none}");
    }

    #[test]
    fn state_outside_the_pool_is_kept_by_the_undefine_flags() {
        let delete = sys::VIR_DOMAIN_UNDEFINE_TPM;
        let keep = sys::VIR_DOMAIN_UNDEFINE_KEEP_TPM;
        // Nothing outside the pool: today's flags, which delete the pool file.
        assert_eq!(undefine_flags_for(&[]), UNDEFINE_FLAGS);
        assert_ne!(UNDEFINE_FLAGS & delete, 0);
        assert_eq!(UNDEFINE_FLAGS & keep, 0);
        // Outside the pool: libvirt keeps the state, and every other cleanup stays.
        let flags = undefine_flags_for(&["/home/u/x.tpm".to_string()]);
        assert_ne!(flags & keep, 0);
        assert_eq!(flags & delete, 0);
        assert_eq!(flags & !keep, UNDEFINE_FLAGS & !delete);
    }

    #[test]
    fn a_kept_tpm_note_lists_every_path_and_is_empty_without_any() {
        assert_eq!(tpm_kept_note(&[]), "");
        let note = tpm_kept_note(&["/a/x.tpm".to_string(), "/b/y.tpm".to_string()]);
        assert!(note.starts_with("; "), "{note}");
        assert!(
            note.contains("TPM state outside manage.disk_pool"),
            "{note}"
        );
        assert!(note.contains("/a/x.tpm, /b/y.tpm"), "{note}");
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
                ..Owner::default()
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
            ..Owner::default()
        };
        let new = replace_owner(&template_vm_xml(), &next).unwrap();
        assert_eq!(owner_in_domain_xml(&new).unwrap(), Some(next.clone()));
        // A VM without our owner element never gains one.
        let plain = "<domain><name>x</name><metadata><o:owner xmlns:o='http://example.com/o'/></metadata></domain>";
        assert!(replace_owner(plain, &next).is_err());
        assert!(replace_owner("<domain><name>x</name></domain>", &next).is_err());
    }

    #[test]
    fn a_missing_firmware_attribute_reads_as_absent() {
        let only_code = format!("<o xmlns='{OWNER_NS}' firmware_code='/o/CODE.fd'/>");
        let owner = Owner::from_element(&only_code).unwrap();
        assert_eq!(owner.firmware_code.as_deref(), Some("/o/CODE.fd"));
        assert_eq!(owner.firmware_sha256, None);
        assert!(owner.has_firmware_record());
        assert!(!Owner::default().has_firmware_record());
        // An owner element from before this change reads with no record and no mode.
        let old = format!("<o xmlns='{OWNER_NS}' template='debian13' generation='3'/>");
        let owner = Owner::from_element(&old).unwrap();
        assert_eq!((owner.secure_boot, owner.firmware_code), (None, None));
    }

    #[test]
    fn a_new_vm_records_the_secure_boot_mode_it_was_asked_for_when_it_is_uefi() {
        for mode in [SecureBoot::Off, SecureBoot::Setup, SecureBoot::On] {
            let owner = Owner::for_new_vm(true, mode);
            assert_eq!(owner.secure_boot, Some(mode), "{mode:?}");
            let xml = domain_xml(&DomainSpec {
                uefi: true,
                secure_boot: mode,
                owner,
                ..spec(vec![Disk::Qcow2("/p/t.qcow2")])
            });
            let recorded = owner_in_domain_xml(&xml).unwrap().unwrap();
            assert_eq!(recorded.secure_boot, Some(mode), "{mode:?}");
            // The firmware record waits for the first start.
            assert!(!recorded.has_firmware_record());
        }
        assert_eq!(Owner::for_new_vm(false, SecureBoot::On), Owner::default());
    }

    #[test]
    fn a_recorded_mode_settles_what_a_pinned_loader_cannot_tell() {
        let state = |loader_secure: &str, template: &str, recorded| {
            let xml = format!(
                "<domain><os><loader secure='{loader_secure}' type='pflash'>/o/CODE.fd</loader>\
                 <nvram template='{template}'>/v.vars</nvram></os></domain>"
            );
            let doc = roxmltree::Document::parse(&xml).unwrap();
            secure_boot_state(child(doc.root_element(), "os").unwrap(), recorded)
        };
        // The file name still answers for a VM made before the mode was recorded.
        assert_eq!(state("yes", "/o/VARS.fd", None), Some(SecureBoot::Setup));
        assert_eq!(state("yes", "/o/VARS.ms.fd", None), Some(SecureBoot::On));
        // The record wins over a template whose name says otherwise, in both directions.
        assert_eq!(
            state("yes", "/o/custom-vars.bin", Some(SecureBoot::Setup)),
            Some(SecureBoot::Setup)
        );
        assert_eq!(
            state("yes", "/o/VARS.fd", Some(SecureBoot::On)),
            Some(SecureBoot::On)
        );
        assert_eq!(
            state("yes", "/o/VARS.ms.fd", Some(SecureBoot::Setup)),
            Some(SecureBoot::Setup)
        );
        // A loader that says Secure Boot is off stays off: the record only separates setup from on.
        assert_eq!(
            state("no", "/o/VARS.fd", Some(SecureBoot::On)),
            Some(SecureBoot::Off)
        );
        // `describe_vm` reports the recorded mode through both of its fields.
        let xml = "<domain><os><loader secure='yes' type='pflash'>/o/C.fd</loader>\
                   <nvram template='/o/custom.bin'>/v.vars</nvram></os></domain>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        let os = child(doc.root_element(), "os");
        assert_eq!(
            secure_boot_fields(true, os, Some(SecureBoot::Setup)),
            (Some(false), Some("setup"))
        );
    }

    #[test]
    fn recording_the_firmware_keeps_every_other_owner_attribute_and_the_rest_of_the_definition() {
        let before = Owner {
            template: Some("debian13".into()),
            generation: Some(2),
            expires: Some(99),
            secure_boot: Some(SecureBoot::Setup),
            ..Owner::default()
        };
        let xml = replace_owner(&template_vm_xml(), &before).unwrap();
        let new = with_firmware_record(&xml, "/o/CODE.fd", &"cd".repeat(32)).unwrap();
        let after = owner_in_domain_xml(&new).unwrap().unwrap();
        assert_eq!(
            after,
            Owner {
                firmware_code: Some("/o/CODE.fd".into()),
                firmware_sha256: Some("cd".repeat(32)),
                ..before
            }
        );
        let old_owner = xml.find("<vmm:owner").unwrap();
        assert_eq!(&xml[..old_owner], &new[..old_owner]);
    }

    #[test]
    fn the_firmware_record_never_gives_a_vm_an_owner_element() {
        // The guardrail: only the create tools set the owner element, so a record written to a definition without
        // one fails, and never claims the VM.
        let record = |xml: &str| with_firmware_record(xml, "/o/CODE.fd", &"ab".repeat(32));
        assert!(record("<domain><name>x</name></domain>").is_err());
        assert!(record("<domain><name>x</name><metadata/></domain>").is_err());
        let foreign = "<domain><name>x</name><metadata>\
                       <o:owner xmlns:o='http://example.com/o'/></metadata></domain>";
        let err = record(foreign).unwrap_err().to_string();
        assert!(err.contains("no owner element"), "{err}");
    }

    #[test]
    fn a_firmware_record_is_written_once() {
        let owner = Owner {
            firmware_code: Some("/o/CODE.fd".into()),
            firmware_sha256: Some("ab".repeat(32)),
            ..Owner::default()
        };
        let xml = replace_owner(&template_vm_xml(), &owner).unwrap();
        assert!(with_firmware_record(&xml, "/o/OTHER.fd", &"cd".repeat(32)).is_err());
    }

    #[test]
    fn new_uuids_differ() {
        let (a, b) = (new_uuid(), new_uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }
}
