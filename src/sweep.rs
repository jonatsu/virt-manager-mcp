//! Expiry: deleting VMs made from a template once their lifetime has passed.
//!
//! Every server process runs one sweep thread, at start and then once a minute, and `virtmanager-mcp sweep` runs a
//! single sweep for a timer. Two processes may sweep the same VM; the one that finds it gone does nothing.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::config::Config;
use crate::manage::{self, Owner};
use crate::server::State;

/// Time between two sweeps of a running server.
const INTERVAL: Duration = Duration::from_secs(60);

/// Why the sweep may not run with `config`, or `None` when it may. It runs only when VMs can be made from templates
/// at all: `manage.create` on, a template configured, and not read-only.
pub fn refusal(config: &Config) -> Option<&'static str> {
    if config.read_only {
        Some("the server is read-only")
    } else if !config.manage.create {
        Some("manage.create is off")
    } else if config.manage.templates.is_empty() {
        Some("no template is configured")
    } else {
        None
    }
}

/// The UUIDs of the VMs whose expiry has passed at `now`.
fn select(candidates: &[(String, Owner)], now: u64) -> Vec<&str> {
    candidates
        .iter()
        .filter(|(_, owner)| manage::is_expired(owner, now))
        .map(|(uuid, _)| uuid.as_str())
        .collect()
}

/// Deletes every expired VM once and returns how many it deleted. Each deletion and each failure is logged.
///
/// # Errors
///
/// Fails when the configuration does not allow a sweep or the VMs cannot be listed.
pub fn sweep_once(state: &State) -> Result<usize> {
    if let Some(why) = refusal(&state.hv.config) {
        bail!("not sweeping: {why}");
    }
    let candidates = manage::owned_with_expiry(&state.hv)?;
    let now = manage::now_epoch();
    let mut deleted = 0;
    for uuid in select(&candidates, now) {
        match manage::expire(&state.hv, uuid, now) {
            Ok(Some(msg)) => {
                deleted += 1;
                tracing::info!("{msg}");
            }
            Ok(None) => {}
            Err(e) => tracing::error!("expiry of VM {uuid} failed: {e:#}"),
        }
    }
    Ok(deleted)
}

/// Starts the sweep thread of a serving process, when the configuration allows a sweep.
pub fn spawn(state: Arc<State>) -> Result<()> {
    if let Some(why) = refusal(&state.hv.config) {
        tracing::debug!("expiry sweep off: {why}");
        return Ok(());
    }
    std::thread::Builder::new()
        .name("expiry-sweep".into())
        .spawn(move || {
            loop {
                if let Err(e) = sweep_once(&state) {
                    tracing::error!("expiry sweep: {e:#}");
                }
                std::thread::sleep(INTERVAL);
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Template;

    fn owner(expires: Option<u64>) -> Owner {
        Owner {
            template: Some("debian13".into()),
            generation: Some(1),
            expires,
        }
    }

    #[test]
    fn selects_only_expired_vms() {
        let candidates = vec![
            ("past".to_string(), owner(Some(100))),
            ("now".to_string(), owner(Some(200))),
            ("future".to_string(), owner(Some(201))),
            ("never".to_string(), owner(None)),
        ];
        assert_eq!(select(&candidates, 200), ["past", "now"]);
        assert!(select(&candidates, 99).is_empty());
    }

    #[test]
    fn a_changed_expiry_is_rechecked() {
        // expire() re-reads the owner just before acting; a VM recreated with a later expiry is left alone.
        assert!(manage::is_expired(&owner(Some(100)), 150));
        assert!(!manage::is_expired(&owner(Some(1000)), 150));
        assert!(!manage::is_expired(&owner(None), u64::MAX));
    }

    #[test]
    fn sweep_needs_templates_create_and_write_access() {
        let mut config = Config::default();
        assert!(refusal(&config).is_some());
        config.manage.create = true;
        assert_eq!(refusal(&config), Some("no template is configured"));
        config.manage.templates.insert(
            "debian13".into(),
            toml::from_str::<Template>("volume = 'tpl.qcow2'").unwrap(),
        );
        assert_eq!(refusal(&config), None);
        config.read_only = true;
        assert_eq!(refusal(&config), Some("the server is read-only"));
    }
}
