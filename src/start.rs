//! The one way the server boots a VM: start it paused, open its console, then resume, so the console holds the guest's
//! output from its first byte. `start_vm`, `create_vm_from_template` and `reset_vm` all boot through it.

use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use virt::domain::Domain;
use virt::sys;

use crate::console::{
    ConsoleOpener, Consoles, LibvirtOpener, LogStatus, OpenRequest, OpenResult, VmId,
};
use crate::consolelog::LogTarget;
use crate::events::{Cause, Events, Kind};
use crate::hypervisor::{self, Hypervisor};
use crate::manage::{self, RevertTarget, now_epoch};
use crate::server::State;

/// What a start does to a VM in a given state.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Boot,
    Resume,
    Wake,
    AlreadyRunning,
}

/// Chooses what `start_vm` does from libvirt's state and reason, read in one call. A crashed VM, or one paused because
/// it crashed, is refused: a resume would let a panicked guest run on. A VM that the `destroy` crash action left shut
/// off boots normally.
///
/// # Errors
///
/// Fails for a crashed VM in either state.
pub fn decide(name: &str, state: u32, reason: i32) -> Result<Action> {
    let paused_crashed = state == sys::VIR_DOMAIN_PAUSED
        && u32::try_from(reason) == Ok(sys::VIR_DOMAIN_PAUSED_CRASHED);
    if state == sys::VIR_DOMAIN_CRASHED || paused_crashed {
        bail!("VM '{name}' has crashed; stop it with force: true, then start it");
    }
    Ok(match state {
        sys::VIR_DOMAIN_SHUTOFF => Action::Boot,
        sys::VIR_DOMAIN_PAUSED => Action::Resume,
        sys::VIR_DOMAIN_PMSUSPENDED => Action::Wake,
        _ => Action::AlreadyRunning,
    })
}

/// The VMs whose start is under way, by UUID.
#[derive(Default)]
pub struct Starting(Mutex<HashSet<String>>);

/// Holds a VM's place in `Starting` until dropped.
pub struct StartGuard<'a> {
    starting: &'a Starting,
    uuid: String,
}

impl Starting {
    /// Claims the VM for one start.
    ///
    /// # Errors
    ///
    /// Fails while another start of the same VM runs.
    pub fn claim(&self, uuid: &str, name: &str) -> Result<StartGuard<'_>> {
        if !self.0.lock().unwrap().insert(uuid.to_string()) {
            bail!("VM '{name}' is starting; wait for that start to finish, then try again");
        }
        Ok(StartGuard {
            starting: self,
            uuid: uuid.to_string(),
        })
    }
}

impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        self.starting.0.lock().unwrap().remove(&self.uuid);
    }
}

/// The libvirt calls of a boot, apart so a test can stand in for them.
pub trait Boot {
    /// Starts the domain paused, with libvirt's start `flags` added.
    fn start_paused(&self, flags: u32) -> Result<()>;
    /// The running domain's ID.
    fn domain_id(&self) -> Option<u32>;
    fn resume(&self) -> Result<()>;
}

/// What a boot needs besides the libvirt calls.
pub struct BootRequest<'a> {
    pub vm: VmId<'a>,
    /// libvirt flags for the paused start, such as resetting a UEFI VM's firmware variables.
    pub flags: u32,
    /// The tool whose start this is, which the event records name as their cause.
    pub tool: &'static str,
    /// The kinds of event the start produces, which the records then attribute to `tool`.
    pub kinds: &'static [Kind],
    pub buffer_bytes: usize,
    /// Where the captured console's output also goes, if anywhere.
    pub log: Option<&'a LogTarget>,
}

/// The kinds of event a paused boot and its resume produce.
const BOOT_KINDS: &[Kind] = &[Kind::Started, Kind::Paused, Kind::Resumed];

/// The kinds of event a revert to a running snapshot produces: the old run's shutdown and stop when the VM was running,
/// and the paused start and the resume.
const REVERT_KINDS: &[Kind] = &[
    Kind::Shutdown,
    Kind::Stopped,
    Kind::Started,
    Kind::Paused,
    Kind::Resumed,
];

/// Boots a VM: starts it paused, opens its console in libvirt's safe mode, replacing this server's own session, and
/// resumes it. A console that cannot be opened is reported in the reply, and the VM still runs. Returns what the reply
/// says about the console, such as "; console captured from boot (generation 7)".
///
/// # Errors
///
/// Fails when the VM does not start, or stays paused because the resume failed; the message says which.
pub fn boot(
    consoles: &Consoles,
    events: &Events,
    target: &dyn Boot,
    opener: &dyn ConsoleOpener,
    req: BootRequest,
) -> Result<String> {
    let name = req.vm.name;
    let expectation = events.expect(req.vm.uuid, Cause::Tool(req.tool), req.kinds);
    // The window closes after the resume, whose events are the boot's too.
    let result = boot_inner(consoles, target, opener, &req).and_then(|console| {
        target.resume().map_err(|e| {
            anyhow!(
                "VM '{name}' is paused{console}, because it could not be resumed ({e:#}); use start_vm to resume \
                 it"
            )
        })?;
        Ok(console)
    });
    events.finish(expectation, now_epoch());
    result
}

/// Starts the VM paused and opens its console; returns what the reply says about the console.
fn boot_inner(
    consoles: &Consoles,
    target: &dyn Boot,
    opener: &dyn ConsoleOpener,
    req: &BootRequest,
) -> Result<String> {
    target.start_paused(req.flags)?;
    let opened = consoles.open(
        opener,
        OpenRequest {
            vm: req.vm,
            domain_id: target.domain_id(),
            device: None,
            // The capture never takes the console over from another client.
            force: false,
            replace: true,
            buffer_bytes: req.buffer_bytes,
            log: req.log,
        },
    );
    Ok(match opened {
        Ok(OpenResult::Opened { generation, log }) => {
            format!(
                "; console captured from boot (generation {generation}{})",
                LogStatus::describe(log.as_ref())
            )
        }
        Ok(OpenResult::Attached { generation, .. }) => {
            format!("; console captured (generation {generation})")
        }
        Ok(OpenResult::NoDevice(_)) => "; it has no console device to capture".into(),
        Ok(OpenResult::Failed(reason)) => format!("; its console could not be captured: {reason}"),
        Err(e) => format!("; its console could not be captured: {e:#}"),
    })
}

/// The libvirt calls of a boot on a real domain.
struct LibvirtBoot<'a> {
    hv: &'a Hypervisor,
    dom: &'a Domain,
    name: &'a str,
}

impl Boot for LibvirtBoot<'_> {
    fn start_paused(&self, flags: u32) -> Result<()> {
        let _memory = manage::start_guard(self.hv, self.dom, None)?;
        self.dom
            .create_with_flags(flags | sys::VIR_DOMAIN_START_PAUSED)
            .map_err(|e| anyhow!("cannot start '{}': {}", self.name, e.message()))?;
        Ok(())
    }

    fn domain_id(&self) -> Option<u32> {
        self.dom.get_id()
    }

    fn resume(&self) -> Result<()> {
        self.dom.resume().map_err(|e| anyhow!("{}", e.message()))?;
        Ok(())
    }
}

/// The libvirt calls of a revert to a running snapshot, which starts the VM as a boot does.
struct LibvirtRevert<'a> {
    hv: &'a Hypervisor,
    target: &'a RevertTarget,
    uuid: &'a str,
    snapshot: &'a str,
}

impl Boot for LibvirtRevert<'_> {
    fn start_paused(&self, _flags: u32) -> Result<()> {
        let _memory = manage::start_guard(self.hv, &self.target.dom, self.target.memory_mib)?;
        manage::snapshot_revert(self.target, self.snapshot)
    }

    fn domain_id(&self) -> Option<u32> {
        // A revert gives the VM a new domain ID that the handle from before it does not show, so look it up again.
        self.hv
            .with_conn(|conn| Ok(Domain::lookup_by_uuid_string(conn, self.uuid)?.get_id()))
            .ok()
            .flatten()
    }

    fn resume(&self) -> Result<()> {
        self.target
            .dom
            .resume()
            .map_err(|e| anyhow!("{}", e.message()))?;
        Ok(())
    }
}

impl State {
    /// Boots the shut-off VM `dom`, named `name`, for `tool`, with libvirt start `flags`. Refused while another start
    /// of the VM runs.
    ///
    /// # Errors
    ///
    /// Fails as `boot` does, or while the VM is already starting.
    pub fn boot(&self, dom: &Domain, name: &str, flags: u32, tool: &'static str) -> Result<String> {
        let uuid = dom.get_uuid_string()?;
        let _guard = self.starting.claim(&uuid, name)?;
        self.boot_claimed(dom, &uuid, name, flags, tool)
    }

    /// Boots a VM whose `Starting` claim the caller holds, such as `reset_vm`, which holds it from before it stops
    /// the VM. Returns the reply's text, starting "started".
    pub fn boot_claimed(
        &self,
        dom: &Domain,
        uuid: &str,
        name: &str,
        flags: u32,
        tool: &'static str,
    ) -> Result<String> {
        let console = boot(
            &self.consoles,
            &self.events,
            &LibvirtBoot {
                hv: &self.hv,
                dom,
                name,
            },
            &LibvirtOpener { hv: &self.hv, dom },
            BootRequest {
                vm: VmId { uuid, name },
                flags,
                tool,
                kinds: BOOT_KINDS,
                buffer_bytes: self.hv.config.limits.console_buffer_bytes,
                log: self.hv.config.console_log().as_ref(),
            },
        )?;
        // libvirt creates a UEFI VM's variables file at its first start, and the pool lists it only after a scan.
        if !self.hv.config.manage.disk_pool.is_empty()
            && let Err(e) = manage::refresh_pool(&self.hv)
        {
            tracing::warn!("cannot refresh the disk pool after starting '{name}': {e:#}");
        }
        Ok(format!("started{console}"))
    }

    /// `snapshot_revert`: reverts the managed VM `name` to `snapshot`. The session before the revert moves to the
    /// previous run. A revert to a running snapshot runs paused, opens the console as a boot does, and resumes, so the
    /// capture holds the guest from the snapshot on; a revert to a shut-off snapshot opens nothing. Refused while a
    /// start of the VM runs.
    ///
    /// # Errors
    ///
    /// Fails for a VM the management tools may not touch, a missing snapshot, a VM that is starting, or a libvirt
    /// refusal.
    pub fn revert(&self, name: &str, snapshot: &str) -> Result<String> {
        let target = manage::snapshot_target(&self.hv, name, snapshot)?;
        let uuid = target.dom.get_uuid_string()?;
        let _guard = self.starting.claim(&uuid, name)?;
        // A revert replaces the guest's state under the console, so the session ends here, running or not.
        let closed = if self.consoles.close(&uuid, "closed by snapshot_revert") {
            "; the console session before it moved to the previous run (console_read run: \"previous\")"
        } else {
            ""
        };
        let done = format!("reverted '{name}' to snapshot '{snapshot}'");
        if !target.running {
            let id = self.events.expect(
                &uuid,
                Cause::Tool("snapshot_revert"),
                &[Kind::Shutdown, Kind::Stopped],
            );
            // A revert to a paused snapshot leaves the VM active, so it passes the memory cap like a start.
            let reverted = if target.activates {
                manage::start_guard(&self.hv, &target.dom, target.memory_mib)
                    .and_then(|_memory| manage::snapshot_revert(&target, snapshot))
            } else {
                manage::snapshot_revert(&target, snapshot)
            };
            self.events.finish(id, now_epoch());
            reverted?;
            return Ok(format!(
                "{done}; the VM is now {}{closed}",
                hypervisor::state_name(&target.dom)
            ));
        }
        let console = boot(
            &self.consoles,
            &self.events,
            &LibvirtRevert {
                hv: &self.hv,
                target: &target,
                uuid: &uuid,
                snapshot,
            },
            &LibvirtOpener {
                hv: &self.hv,
                dom: &target.dom,
            },
            BootRequest {
                vm: VmId { uuid: &uuid, name },
                flags: 0,
                tool: "snapshot_revert",
                kinds: REVERT_KINDS,
                buffer_bytes: self.hv.config.limits.console_buffer_bytes,
                log: self.hv.config.console_log().as_ref(),
            },
        )?;
        let console = console.replace("captured from boot", "captured from the snapshot on");
        Ok(format!("{done}; the VM is now running{console}{closed}"))
    }

    /// `start_vm`: boots a shut-off VM, resumes a paused one, or wakes a suspended one. Returns what happened.
    ///
    /// # Errors
    ///
    /// Fails for a VM the server may not power, a crashed VM, a VM that is starting, or a libvirt refusal.
    pub fn start_vm(&self, name: &str) -> Result<String> {
        let dom = self.hv.power_domain(name)?;
        let uuid = dom.get_uuid_string()?;
        // Held for the whole start, so a second start cannot resume the guest before its console opens.
        let _guard = self.starting.claim(&uuid, name)?;
        let (state, reason) = dom.get_state()?;
        match decide(name, state, reason)? {
            Action::Boot => self.boot_claimed(&dom, &uuid, name, 0, "start_vm"),
            Action::Resume => {
                let id = self
                    .events
                    .expect(&uuid, Cause::Tool("start_vm"), &[Kind::Resumed]);
                let resumed = dom.resume();
                self.events.finish(id, now_epoch());
                resumed.map_err(|e| anyhow!("cannot resume '{name}': {}", e.message()))?;
                Ok("resumed from pause".into())
            }
            Action::Wake => {
                let id = self
                    .events
                    .expect(&uuid, Cause::Tool("start_vm"), &[Kind::Started]);
                let woken = dom.pm_wakeup(0);
                self.events.finish(id, now_epoch());
                woken.map_err(|e| anyhow!("cannot wake '{name}': {}", e.message()))?;
                Ok("woken from suspend".into())
            }
            Action::AlreadyRunning => Ok("already running".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::console::{ConsoleStream, OpenError, Recv, Run, Sent};

    #[test]
    fn the_start_decision_for_each_state() {
        let d = |state, reason: u32| decide("t-a", state, reason as i32);
        assert_eq!(d(sys::VIR_DOMAIN_SHUTOFF, 0).unwrap(), Action::Boot);
        assert_eq!(
            d(sys::VIR_DOMAIN_PAUSED, sys::VIR_DOMAIN_PAUSED_USER).unwrap(),
            Action::Resume
        );
        assert_eq!(d(sys::VIR_DOMAIN_PMSUSPENDED, 0).unwrap(), Action::Wake);
        assert_eq!(
            d(sys::VIR_DOMAIN_RUNNING, 0).unwrap(),
            Action::AlreadyRunning
        );
        // Under the destroy crash action the VM is shut off with reason crashed, and starts normally.
        assert_eq!(
            d(sys::VIR_DOMAIN_SHUTOFF, sys::VIR_DOMAIN_SHUTOFF_CRASHED).unwrap(),
            Action::Boot
        );
        for (state, reason) in [
            (sys::VIR_DOMAIN_CRASHED, sys::VIR_DOMAIN_CRASHED_PANICKED),
            (sys::VIR_DOMAIN_PAUSED, sys::VIR_DOMAIN_PAUSED_CRASHED),
        ] {
            let refused = d(state, reason).unwrap_err().to_string();
            assert!(refused.contains("has crashed"), "{refused}");
        }
    }

    #[test]
    fn the_guard_refuses_a_concurrent_start_and_frees_the_vm_after() {
        let starting = Starting::default();
        let guard = starting.claim("u", "t-a").unwrap();
        let refused = starting.claim("u", "t-a").err().unwrap().to_string();
        assert!(refused.contains("is starting"), "{refused}");
        starting.claim("other", "t-b").unwrap();
        drop(guard);
        starting.claim("u", "t-a").unwrap();
    }

    #[derive(Default)]
    struct FakeBoot {
        calls: RefCell<Vec<String>>,
        fail_start: bool,
        fail_resume: bool,
    }

    impl Boot for FakeBoot {
        fn start_paused(&self, flags: u32) -> Result<()> {
            self.calls.borrow_mut().push(format!("start {flags}"));
            if self.fail_start {
                bail!("no space left");
            }
            Ok(())
        }
        fn domain_id(&self) -> Option<u32> {
            Some(7)
        }
        fn resume(&self) -> Result<()> {
            self.calls.borrow_mut().push("resume".into());
            if self.fail_resume {
                bail!("resume refused");
            }
            Ok(())
        }
    }

    struct SilentStream;

    impl ConsoleStream for SilentStream {
        fn recv(&self, _buf: &mut [u8]) -> Recv {
            Recv::Again
        }
        fn send(&self, data: &[u8]) -> Sent {
            Sent::Bytes(data.len())
        }
        fn abort(&self) {}
    }

    /// Records each open's force flag, and answers with `result`.
    struct FakeOpener {
        forces: RefCell<Vec<bool>>,
        result: Cell<Option<OpenError>>,
    }

    impl FakeOpener {
        fn new(result: Option<OpenError>) -> Self {
            Self {
                forces: RefCell::default(),
                result: Cell::new(result),
            }
        }
    }

    impl ConsoleOpener for FakeOpener {
        fn open(
            &self,
            _device: Option<&str>,
            force: bool,
        ) -> std::result::Result<Box<dyn ConsoleStream>, OpenError> {
            self.forces.borrow_mut().push(force);
            match self.result.take() {
                Some(e) => Err(e),
                None => Ok(Box::new(SilentStream)),
            }
        }
    }

    fn request(flags: u32) -> BootRequest<'static> {
        BootRequest {
            vm: VmId {
                uuid: "u",
                name: "t-a",
            },
            flags,
            tool: "start_vm",
            kinds: BOOT_KINDS,
            buffer_bytes: 1 << 16,
            log: None,
        }
    }

    #[test]
    fn a_boot_starts_paused_opens_one_safe_mode_console_and_resumes() {
        let (consoles, events) = (Consoles::default(), Events::new(16));
        let (target, opener) = (FakeBoot::default(), FakeOpener::new(None));
        let reply = boot(&consoles, &events, &target, &opener, request(4)).unwrap();
        assert!(reply.starts_with("; console captured from boot"), "{reply}");
        assert_eq!(*target.calls.borrow(), ["start 4", "resume"]);
        assert_eq!(*opener.forces.borrow(), [false]);
        // A second boot replaces the first capture without forcing.
        boot(&consoles, &events, &target, &opener, request(0)).unwrap();
        assert_eq!(*opener.forces.borrow(), [false, false]);
        let read = consoles
            .read(
                VmId {
                    uuid: "u",
                    name: "t-a",
                },
                Run::Current,
                Some(0),
                None,
                1024,
                std::time::Duration::ZERO,
            )
            .unwrap();
        assert_eq!(read.next, 0);
    }

    #[test]
    fn the_reply_says_what_became_of_the_console() {
        for (error, says) in [
            (
                OpenError::NoDevice("cannot find character device".into()),
                "no console device to capture",
            ),
            (
                OpenError::Failed("in use".into()),
                "console could not be captured: in use",
            ),
        ] {
            let (consoles, events) = (Consoles::default(), Events::new(16));
            let target = FakeBoot::default();
            let reply = boot(
                &consoles,
                &events,
                &target,
                &FakeOpener::new(Some(error)),
                request(0),
            )
            .unwrap();
            assert!(reply.contains(says), "{reply}");
            // The VM runs even without its console.
            assert_eq!(target.calls.borrow().last().unwrap(), "resume");
        }
    }

    #[test]
    fn a_failed_start_opens_nothing_and_a_failed_resume_says_the_vm_is_paused() {
        let (consoles, events) = (Consoles::default(), Events::new(16));
        let opener = FakeOpener::new(None);
        let target = FakeBoot {
            fail_start: true,
            ..FakeBoot::default()
        };
        let e = boot(&consoles, &events, &target, &opener, request(0)).unwrap_err();
        assert!(e.to_string().contains("no space left"), "{e}");
        assert!(opener.forces.borrow().is_empty());

        let target = FakeBoot {
            fail_resume: true,
            ..FakeBoot::default()
        };
        let e = boot(&consoles, &events, &target, &opener, request(0))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("is paused") && e.contains("console captured"),
            "{e}"
        );
    }

    #[test]
    fn a_boots_events_are_the_tools_and_raise_no_notice() {
        let (consoles, events) = (Consoles::default(), Events::new(16));
        boot(
            &consoles,
            &events,
            &FakeBoot::default(),
            &FakeOpener::new(None),
            request(0),
        )
        .unwrap();
        for (event, detail) in [
            (
                sys::VIR_DOMAIN_EVENT_STARTED,
                sys::VIR_DOMAIN_EVENT_STARTED_BOOTED,
            ),
            (
                sys::VIR_DOMAIN_EVENT_SUSPENDED,
                sys::VIR_DOMAIN_EVENT_SUSPENDED_PAUSED,
            ),
            (
                sys::VIR_DOMAIN_EVENT_RESUMED,
                sys::VIR_DOMAIN_EVENT_RESUMED_UNPAUSED,
            ),
        ] {
            events.append(crate::events::Raw {
                uuid: "u".into(),
                name: "t-a".into(),
                source: crate::events::Source::Lifecycle {
                    event: event as i32,
                    detail: detail as i32,
                },
                time: now_epoch(),
            });
        }
        let read = events.read("u", 0, None);
        assert_eq!(read.records.len(), 3);
        assert!(
            read.records
                .iter()
                .all(|r| r.cause == Cause::Tool("start_vm") && !r.has_notice())
        );
    }
}
