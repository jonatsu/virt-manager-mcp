//! libvirt connection handling and access control.

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::sys;

use crate::config::Config;
use crate::manage;

/// Starts libvirt's default event loop on a background thread. Must run before
/// the connection is opened: the remote driver only delivers stream data
/// (serial console) and keepalives when an event loop is registered.
pub fn start_event_loop() -> Result<()> {
    virt::event::event_register_default_impl().context("registering libvirt event loop")?;
    std::thread::Builder::new()
        .name("libvirt-events".into())
        .spawn(|| {
            loop {
                if let Err(e) = virt::event::event_run_default_impl() {
                    tracing::error!("libvirt event loop: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        })?;
    Ok(())
}

pub struct Hypervisor {
    pub config: Config,
    conn: RwLock<Connect>,
}

impl Hypervisor {
    pub fn connect(config: Config) -> Result<Self> {
        let conn = open(&config.uri)?;
        Ok(Self {
            config,
            conn: RwLock::new(conn),
        })
    }

    /// Runs `f` with a live connection, reconnecting once if libvirtd went away.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connect) -> Result<T>) -> Result<T> {
        {
            let conn = self.conn.read().unwrap();
            if conn.is_alive().unwrap_or(false) {
                return f(&conn);
            }
        }
        let mut conn = self.conn.write().unwrap();
        if !conn.is_alive().unwrap_or(false) {
            tracing::warn!(
                "libvirt connection lost, reconnecting to {}",
                self.config.uri
            );
            let _ = conn.close();
            *conn = open(&self.config.uri)?;
        }
        f(&conn)
    }

    /// The target path of every storage pool on the connection that has one, by pool name, resolved where it exists.
    /// Pull directories may not overlap any of them, so a guest cannot replace a VM's disk.
    pub fn pool_targets(&self) -> Result<Vec<(String, PathBuf)>> {
        self.with_conn(|conn| {
            let mut targets = Vec::new();
            for pool in conn.list_all_storage_pools(0)? {
                let name = pool.get_name()?;
                let xml = pool.get_xml_desc(0)?;
                if let Some(path) = pool_target(&xml)? {
                    targets.push((name, resolve_existing(&path)));
                }
            }
            Ok(targets)
        })
    }

    /// Looks up a VM by name, enforcing the allow-list. VMs created by this server
    /// count as allowed while VM management is enabled.
    pub fn lookup(&self, name: &str) -> Result<Domain> {
        let listed = self.config.is_allowed(name);
        if !listed && !self.config.manage.enabled() {
            bail!("VM '{name}' is not in allowed_vms");
        }
        let dom = self.with_conn(|conn| {
            Domain::lookup_by_name(conn, name).map_err(|e| {
                if listed {
                    anyhow!("VM '{name}' not found: {}", e.message())
                } else {
                    anyhow!("VM '{name}' is not in allowed_vms and was not created by this server")
                }
            })
        })?;
        if !listed && !manage::is_owned(&dom) {
            bail!("VM '{name}' is not in allowed_vms and was not created by this server");
        }
        Ok(dom)
    }

    /// Looks up a VM by name, enforcing the allow-list and that it is running.
    pub fn domain(&self, name: &str) -> Result<Domain> {
        let dom = self.lookup(name)?;
        if !dom.is_active()? {
            if self.can_power(&dom) {
                bail!("VM '{name}' is not running (start it with start_vm)");
            }
            bail!("VM '{name}' is not running (this server may not start it)");
        }
        Ok(dom)
    }

    /// All VMs (running or not) that are in the allow-list or were created by this server.
    pub fn allowed(&self) -> Result<Vec<Domain>> {
        let manage = self.config.manage.enabled();
        self.with_conn(|conn| {
            let doms = conn.list_all_domains(0)?;
            Ok(doms
                .into_iter()
                .filter(|d| {
                    d.get_name().is_ok_and(|n| self.config.is_allowed(&n))
                        || (manage && manage::is_owned(d))
                })
                .collect())
        })
    }

    /// Whether the AI may start and stop `dom`: listed in power_vms, or created by this server.
    pub fn can_power(&self, dom: &Domain) -> bool {
        let name = dom.get_name().unwrap_or_default();
        self.config.can_power(&name) || (self.config.manage.enabled() && manage::is_owned(dom))
    }

    /// Looks up a VM the AI may power on/off.
    fn power_domain(&self, name: &str) -> Result<Domain> {
        let dom = self.lookup(name)?;
        if !self.can_power(&dom) {
            bail!("VM '{name}' is not in power_vms");
        }
        Ok(dom)
    }

    /// Whether the enabled management operations may act on `dom`: created by this
    /// server, or listed in manage.manage_vms.
    pub fn can_manage(&self, dom: &Domain) -> bool {
        let name = dom.get_name().unwrap_or_default();
        self.config.manage.enabled() && (self.config.in_manage_vms(&name) || manage::is_owned(dom))
    }

    /// Looks up a VM the management tools may act on.
    pub fn manage_domain(&self, name: &str) -> Result<Domain> {
        let dom = self.lookup(name)?;
        if !self.can_manage(&dom) {
            bail!("VM '{name}' was not created by this server and is not in manage.manage_vms");
        }
        Ok(dom)
    }

    /// Boots, resumes or wakes a VM. Returns a short description of what happened.
    pub fn start(&self, name: &str) -> Result<String> {
        let dom = self.power_domain(name)?;
        if !dom.is_active()? {
            dom.create()
                .map_err(|e| anyhow!("cannot start '{name}': {}", e.message()))?;
            return Ok("started".into());
        }
        Ok(match dom.get_state()?.0 {
            sys::VIR_DOMAIN_PAUSED => {
                dom.resume()
                    .map_err(|e| anyhow!("cannot resume '{name}': {}", e.message()))?;
                "resumed from pause".into()
            }
            sys::VIR_DOMAIN_PMSUSPENDED => {
                dom.pm_wakeup(0)
                    .map_err(|e| anyhow!("cannot wake '{name}': {}", e.message()))?;
                "woken from suspend".into()
            }
            sys::VIR_DOMAIN_CRASHED => {
                bail!("VM '{name}' has crashed; stop it with force: true, then start it")
            }
            _ => "already running".into(),
        })
    }

    /// Requests a guest shutdown (ACPI or guest agent, whichever libvirt picks), or
    /// powers the VM off immediately with `force`. Waits up to `wait` for it to stop.
    /// Returns whether the VM is off.
    pub fn stop(&self, name: &str, force: bool, wait: Duration) -> Result<bool> {
        let dom = self.power_domain(name)?;
        if !dom.is_active()? {
            return Ok(true);
        }
        if force {
            dom.destroy()
                .map_err(|e| anyhow!("cannot power off '{name}': {}", e.message()))?;
            return Ok(true);
        }
        dom.shutdown()
            .map_err(|e| anyhow!("cannot shut down '{name}': {}", e.message()))?;
        let start = Instant::now();
        while dom.is_active()? {
            if start.elapsed() >= wait {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(true)
    }
}

/// libvirt's state name for a VM, e.g. "running" or "shutoff".
pub fn state_name(dom: &Domain) -> &'static str {
    match dom.get_state().map(|s| s.0) {
        Ok(sys::VIR_DOMAIN_RUNNING) => "running",
        Ok(sys::VIR_DOMAIN_BLOCKED) => "blocked",
        Ok(sys::VIR_DOMAIN_PAUSED) => "paused",
        Ok(sys::VIR_DOMAIN_SHUTDOWN) => "shutting down",
        Ok(sys::VIR_DOMAIN_SHUTOFF) => "shutoff",
        Ok(sys::VIR_DOMAIN_CRASHED) => "crashed",
        Ok(sys::VIR_DOMAIN_PMSUSPENDED) => "suspended",
        _ => "unknown",
    }
}

fn open(uri: &str) -> Result<Connect> {
    Connect::open(Some(uri)).map_err(|e| {
        anyhow!(
            "cannot connect to {uri}: {} (for qemu:///system your user usually needs to be in the 'libvirt' group)",
            e.message()
        )
    })
}

/// `path` with its longest existing ancestor resolved, so a target that does not exist yet still compares by its
/// real location when a parent directory is a symlink.
fn resolve_existing(path: &std::path::Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(real) = cur.canonicalize() {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The `<target><path>` of a storage pool's XML, if it has one.
fn pool_target(xml: &str) -> Result<Option<PathBuf>> {
    let doc = roxmltree::Document::parse(xml).context("parsing storage pool XML")?;
    Ok(doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("target"))
        .and_then(|t| t.children().find(|n| n.has_tag_name("path")))
        .and_then(|p| p.text())
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| p.is_absolute()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_target_reads_the_target_path_only() {
        let dir = r#"<pool type="dir"><name>agent-vms</name><source/>
            <target><path>/var/lib/libvirt/images/agent-vms</path></target></pool>"#;
        assert_eq!(
            pool_target(dir).unwrap(),
            Some(PathBuf::from("/var/lib/libvirt/images/agent-vms"))
        );
        let no_target =
            r#"<pool type="rbd"><name>ceph</name><source><name>rbd</name></source></pool>"#;
        assert_eq!(pool_target(no_target).unwrap(), None);
        let source_path = r#"<pool type="logical"><name>vg</name>
            <source><device path="/dev/sdb"/></source><target><path>/dev/vg</path></target></pool>"#;
        assert_eq!(
            pool_target(source_path).unwrap(),
            Some(PathBuf::from("/dev/vg"))
        );
    }

    #[test]
    fn a_missing_pool_target_resolves_through_its_existing_ancestor() {
        let root =
            std::env::temp_dir().join(format!("virtmanager-mcp-pool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).unwrap();
        let root = root.canonicalize().unwrap();
        let real = root.join("real");
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            resolve_existing(&link.join("pool/sub")),
            real.join("pool/sub")
        );
        assert_eq!(resolve_existing(&link), real);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
