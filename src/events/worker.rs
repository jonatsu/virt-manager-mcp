//! The event worker, which turns queued libvirt events into log records for visible VMs, and the listener that
//! registers the callbacks on each connection and accounts for what a lost connection may have hidden.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use virt::domain::Domain;

use super::ffi::{Batch, Live, QUEUE_CAPACITY, Queue, Registration};
use super::{Events, Raw, Source};
use crate::manage::now_epoch;
use crate::server::State;

/// How long the worker waits for events before it retries a pending reconnect or registration.
const POLL: Duration = Duration::from_secs(1);

/// What the worker needs from the rest of the server.
pub trait Host {
    /// Whether config lets the server see the VM with this UUID, `false` for a VM that is already gone, or `None`
    /// when that cannot be told now, while libvirt is unreachable. The worker tries such an event again with the next
    /// batch.
    fn visible(&self, uuid: &str) -> Option<bool>;
    /// Drops what the server keeps for a VM besides its log, such as its console sessions.
    fn forget(&self, uuid: &str);
    /// Replaces the connection if it is no longer alive.
    fn reconnect(&self) -> Result<()>;
    /// Registers the callbacks again after a reconnect failed to.
    fn register_again(&self) -> Result<()>;
}

/// A VM as a connection reports it after a reconnect.
#[derive(Debug, Clone)]
pub struct VmState {
    pub uuid: String,
    pub name: String,
    pub running: bool,
}

/// One libvirt connection, as the listener uses it.
pub trait Session {
    fn register(&self, queue: &Arc<Queue>) -> Result<Registration>;
    fn deregister(&self, registration: Registration);
    /// The visible VMs and whether each is running; a VM that cannot be read is left out.
    fn visible_vms(&self) -> Result<Vec<VmState>>;
}

/// The listener's state, kept under one lock.
#[derive(Default)]
struct Registered {
    registration: Option<Registration>,
    /// When the close callback reported the connection gone, until a reconnect uses it.
    lost_at: Option<u64>,
    /// When the last reconnect ran; a close reported no later than this belongs to the connection it replaced.
    reconnected_at: u64,
    /// Since when no callbacks are registered, after a reconnect whose registration failed.
    unregistered_since: Option<u64>,
}

/// Keeps the callbacks registered on the current connection.
pub struct Listener {
    queue: Arc<Queue>,
    events: Arc<Events>,
    state: Mutex<Registered>,
    /// The UUIDs of the events the worker has taken from the queue and not yet handled. A reconnect can run in the
    /// middle of a batch, and these events still speak for their VMs, as queued ones do.
    in_flight: Mutex<Vec<String>>,
}

impl Listener {
    pub fn new(queue: Arc<Queue>, events: Arc<Events>) -> Self {
        Self {
            queue,
            events,
            state: Mutex::new(Registered::default()),
            in_flight: Mutex::new(Vec::new()),
        }
    }

    fn in_flight(&self) -> MutexGuard<'_, Vec<String>> {
        // A list of names that no panic can leave half-written.
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn queue(&self) -> &Arc<Queue> {
        &self.queue
    }

    pub fn events(&self) -> &Arc<Events> {
        &self.events
    }

    fn state(&self) -> MutexGuard<'_, Registered> {
        // Plain values that no panic can leave half-written.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the callbacks on the first connection.
    ///
    /// # Errors
    ///
    /// Fails when libvirt refuses a registration.
    pub fn start(&self, session: &dyn Session) -> Result<()> {
        self.state().registration = Some(session.register(&self.queue)?);
        Ok(())
    }

    /// Notes when the close callback reported the connection gone. The earliest report since the last reconnect
    /// counts, and a report from before it is about the connection that reconnect replaced.
    pub fn note_lost(&self, at: u64) {
        let mut state = self.state();
        if at > state.reconnected_at {
            state.lost_at.get_or_insert(at);
        }
    }

    /// Whether a reconnect left the connection without callbacks.
    pub fn needs_register(&self) -> bool {
        self.state().unregistered_since.is_some()
    }

    /// Moves the callbacks from `old` to `new` and records what the lost connection may have hidden: a `gap` in every
    /// log from when the connection was lost, and a `started` or `stopped` with detail `during_gap` for each visible
    /// VM whose state differs from its last record. `alive_at` is when the server last saw the old connection alive,
    /// used when the close callback never reported the loss.
    pub fn reconnected(&self, old: &dyn Session, new: &dyn Session, alive_at: u64, now: u64) {
        let from = {
            let mut state = self.state();
            if let Some(registration) = state.registration.take() {
                old.deregister(registration);
            }
            // A registration that failed earlier ends here when this one succeeds, so the worker does not register a
            // second set; its time without callbacks joins this gap.
            let mut unregistered = None;
            match new.register(&self.queue) {
                Ok(registration) => {
                    state.registration = Some(registration);
                    unregistered = state.unregistered_since.take();
                }
                Err(e) => {
                    tracing::error!("registering libvirt event callbacks after a reconnect: {e:#}");
                    state.unregistered_since.get_or_insert(now);
                }
            }
            state.reconnected_at = now;
            let lost = state.lost_at.take().unwrap_or(alive_at);
            unregistered.map_or(lost, |since| since.min(lost))
        };
        self.events.record_gap(from, now);
        self.compare(new, now);
    }

    /// Registers on `session` after a failed attempt, and records a gap for the time without callbacks.
    ///
    /// # Errors
    ///
    /// Fails when libvirt refuses the registration again.
    pub fn register_again(&self, session: &dyn Session, now: u64) -> Result<()> {
        let from = {
            let mut state = self.state();
            let Some(from) = state.unregistered_since else {
                return Ok(());
            };
            state.registration = Some(session.register(&self.queue)?);
            state.unregistered_since = None;
            from
        };
        self.events.record_gap(from, now);
        self.compare(session, now);
        Ok(())
    }

    /// Records `during_gap` for each visible VM whose state differs from its last record. A VM with events still
    /// queued, or in the worker's batch and not yet handled, is left to them, so a stop just before the loss is
    /// recorded once, by its own event.
    fn compare(&self, session: &dyn Session, now: u64) {
        let vms = match session.visible_vms() {
            Ok(vms) => vms,
            Err(e) => {
                tracing::warn!("listing VMs after a reconnect: {e:#}");
                return;
            }
        };
        let mut pending = self.queue.pending();
        pending.extend(self.in_flight().iter().cloned());
        for vm in vms {
            if !pending.contains(&vm.uuid)
                && self
                    .events
                    .last_running(&vm.uuid)
                    .is_some_and(|was| was != vm.running)
            {
                self.events
                    .record_during_gap(&vm.uuid, &vm.name, vm.running, now);
            }
        }
    }
}

/// Takes events from the queue and records those of visible VMs.
pub struct Worker<H> {
    listener: Arc<Listener>,
    host: H,
    /// Visibility by UUID, for the name it was checked under.
    cache: HashMap<String, (String, bool)>,
    reconnect_pending: bool,
    /// Events whose VM could not be looked up yet, oldest first, tried again with the next batch. Past
    /// `QUEUE_CAPACITY`, the oldest are dropped for a gap.
    unplaced: Vec<Raw>,
}

impl<H: Host> Worker<H> {
    pub fn new(listener: Arc<Listener>, host: H) -> Self {
        Self {
            listener,
            host,
            cache: HashMap::new(),
            reconnect_pending: false,
            unplaced: Vec::new(),
        }
    }

    /// Whether the event's VM is visible, or `None` when that cannot be told now.
    fn visible(&mut self, raw: &Raw) -> Option<bool> {
        if let Some((name, visible)) = self.cache.get(&raw.uuid)
            && *name == raw.name
        {
            return Some(*visible);
        }
        // Only a definite answer is cached, so a VM is not hidden for good by a lookup during a reconnect.
        let visible = self.host.visible(&raw.uuid)?;
        self.cache
            .insert(raw.uuid.clone(), (raw.name.clone(), visible));
        Some(visible)
    }

    /// Handles one batch at `now`, after the events whose VM could not be looked up before.
    pub fn handle(&mut self, batch: Batch, now: u64) {
        let events = self.listener.events().clone();
        let mut all = std::mem::take(&mut self.unplaced);
        all.extend(batch.events);
        *self.listener.in_flight() = all.iter().map(|r| r.uuid.clone()).collect();
        // After a lookup that could not be answered, the pass makes no more: libvirt is unreachable, and each lookup
        // would try to reconnect again.
        let mut lookups_stopped = false;
        for raw in all {
            let uuid = raw.uuid.clone();
            let cached = self
                .cache
                .get(&raw.uuid)
                .is_some_and(|(name, _)| *name == raw.name);
            // A VM's events keep their order: one behind a waiting event of its VM waits too.
            if !is_undefined(raw.source)
                && ((lookups_stopped && !cached) || self.unplaced.iter().any(|r| r.uuid == uuid))
            {
                self.unplaced.push(raw);
                continue;
            }
            if is_undefined(raw.source) {
                // The VM is gone, so a lookup cannot say whether it was visible; dropping is harmless either way.
                self.cache.remove(&raw.uuid);
                self.host.forget(&raw.uuid);
                events.drop_vm(&raw.uuid);
            } else {
                match self.visible(&raw) {
                    Some(true) => {
                        let (name, source) = (raw.name.clone(), raw.source);
                        if let Some(seq) = events.append(raw) {
                            tracing::debug!(vm = %name, ?source, seq, "recorded");
                        }
                    }
                    Some(false) => {}
                    // Kept for the next batch, in order, and still in flight, so a reconnect leaves the VM to it.
                    None => {
                        lookups_stopped = true;
                        self.unplaced.push(raw);
                        continue;
                    }
                }
            }
            let mut in_flight = self.listener.in_flight();
            if let Some(at) = in_flight.iter().position(|u| *u == uuid) {
                in_flight.remove(at);
            }
        }
        if self.unplaced.len() > QUEUE_CAPACITY {
            let over = self.unplaced.len() - QUEUE_CAPACITY;
            let lost: Vec<Raw> = self.unplaced.drain(..over).collect();
            let from = lost.iter().map(|r| r.time).min().unwrap_or(now);
            tracing::warn!(
                "libvirt stayed unreachable for too many events' lookups; recording a gap"
            );
            events.record_gap(from, now);
            let mut in_flight = self.listener.in_flight();
            for raw in &lost {
                if let Some(at) = in_flight.iter().position(|u| *u == raw.uuid) {
                    in_flight.remove(at);
                }
            }
        }
        if let Some(from) = batch.dropped_since {
            tracing::warn!("the libvirt event queue overflowed; recording a gap");
            events.record_gap(from, now);
        }
        if let Some(at) = batch.closed_at {
            self.listener.note_lost(at);
            self.reconnect_pending = true;
        }
        if self.reconnect_pending {
            match self.host.reconnect() {
                Ok(()) => self.reconnect_pending = false,
                Err(e) => tracing::warn!("reconnecting to libvirt after it closed: {e:#}"),
            }
        }
        if self.listener.needs_register()
            && let Err(e) = self.host.register_again()
        {
            tracing::warn!("registering libvirt event callbacks again: {e:#}");
        }
    }

    /// Runs on its own thread for the life of the server. A panic while handling a batch loses that batch only.
    pub fn run(mut self) {
        loop {
            let batch = self.listener.queue().take(POLL);
            if catch_unwind(AssertUnwindSafe(|| self.handle(batch, now_epoch()))).is_err() {
                tracing::error!("the event worker panicked; the events of one batch are lost");
            }
        }
    }
}

/// The worker's view of the running server.
struct ServerHost {
    state: Arc<State>,
    listener: Arc<Listener>,
}

impl Host for ServerHost {
    fn visible(&self, uuid: &str) -> Option<bool> {
        let hv = &self.state.hv;
        // A VM that no longer exists has no log to add to. Any other failure, such as libvirt going away during the
        // lookup, leaves the answer open, so nothing is cached and the event is tried again.
        let dom = hv
            .with_conn(|conn| match Domain::lookup_by_uuid_string(conn, uuid) {
                Ok(dom) => Ok(Some(dom)),
                Err(e) if matches!(e.code(), virt::error::ErrorNumber::NoDomain) => Ok(None),
                Err(e) => Err(e.into()),
            })
            .ok()?;
        Some(dom.is_some_and(|dom| hv.is_visible(&dom)))
    }

    fn forget(&self, uuid: &str) {
        self.state.consoles.forget(uuid);
    }

    fn reconnect(&self) -> Result<()> {
        self.state.hv.with_conn(|_| Ok(()))
    }

    fn register_again(&self) -> Result<()> {
        let hv = &self.state.hv;
        hv.with_conn(|conn| {
            self.listener.register_again(
                &Live {
                    conn,
                    visible: &|dom| hv.is_visible(dom),
                },
                now_epoch(),
            )
        })
    }
}

/// Registers the event callbacks on the server's connection, has every reconnect move them, and starts the worker
/// that records events into `events`.
///
/// # Errors
///
/// Fails when libvirt refuses a registration or the worker thread cannot start.
pub fn spawn(state: Arc<State>, events: Arc<Events>) -> Result<()> {
    let listener = Arc::new(Listener::new(Arc::new(Queue::default()), events));
    state.hv.with_conn(|conn| {
        listener.start(&Live {
            conn,
            visible: &|dom| state.hv.is_visible(dom),
        })
    })?;
    let hook_listener = listener.clone();
    state
        .hv
        .set_reconnect_hook(Box::new(move |hv, old, new, alive_at| {
            let visible = |dom: &Domain| hv.is_visible(dom);
            hook_listener.reconnected(
                &Live {
                    conn: old,
                    visible: &visible,
                },
                &Live {
                    conn: new,
                    visible: &visible,
                },
                alive_at,
                now_epoch(),
            );
        }));
    let worker = Worker::new(listener.clone(), ServerHost { state, listener });
    std::thread::Builder::new()
        .name("event-worker".into())
        .spawn(move || worker.run())
        .context("starting the event worker")?;
    Ok(())
}

fn is_undefined(source: Source) -> bool {
    matches!(source, Source::Lifecycle { event, .. }
        if u32::try_from(event) == Ok(virt::sys::VIR_DOMAIN_EVENT_UNDEFINED))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use anyhow::bail;
    use virt::sys;

    use super::*;
    use crate::events::Kind;

    #[derive(Default)]
    struct FakeHost {
        hidden: RefCell<Vec<&'static str>>,
        unknown: RefCell<Vec<&'static str>>,
        lookups: Cell<usize>,
        forgotten: RefCell<Vec<String>>,
        reconnects: Cell<usize>,
        fail_reconnects: Cell<usize>,
        registers: Cell<usize>,
        /// Runs once inside the next lookup, as a reconnect through `with_conn` would.
        on_lookup: RefCell<Option<Box<dyn FnOnce()>>>,
        /// How many of the next lookups cannot be answered, whatever the VM.
        unanswered: Cell<usize>,
    }

    impl Host for &FakeHost {
        fn visible(&self, uuid: &str) -> Option<bool> {
            self.lookups.set(self.lookups.get() + 1);
            if self.unanswered.get() > 0 {
                self.unanswered.set(self.unanswered.get() - 1);
                return None;
            }
            if let Some(hook) = self.on_lookup.borrow_mut().take() {
                hook();
            }
            if self.unknown.borrow().contains(&uuid) {
                return None;
            }
            Some(!self.hidden.borrow().contains(&uuid))
        }
        fn forget(&self, uuid: &str) {
            self.forgotten.borrow_mut().push(uuid.into());
        }
        fn reconnect(&self) -> Result<()> {
            self.reconnects.set(self.reconnects.get() + 1);
            if self.fail_reconnects.get() > 0 {
                self.fail_reconnects.set(self.fail_reconnects.get() - 1);
                bail!("libvirtd is not back yet");
            }
            Ok(())
        }
        fn register_again(&self) -> Result<()> {
            self.registers.set(self.registers.get() + 1);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeSession {
        registered: Cell<usize>,
        deregistered: Cell<usize>,
        fail_register: Cell<bool>,
        vms: Vec<VmState>,
    }

    impl Session for FakeSession {
        fn register(&self, _queue: &Arc<Queue>) -> Result<Registration> {
            self.registered.set(self.registered.get() + 1);
            if self.fail_register.get() {
                bail!("registration refused");
            }
            Ok(Registration::empty())
        }
        fn deregister(&self, _registration: Registration) {
            self.deregistered.set(self.deregistered.get() + 1);
        }
        fn visible_vms(&self) -> Result<Vec<VmState>> {
            Ok(self.vms.clone())
        }
    }

    fn lifecycle(uuid: &str, name: &str, event: u32, detail: u32, time: u64) -> Raw {
        Raw {
            uuid: uuid.into(),
            name: name.into(),
            source: Source::Lifecycle {
                event: event as i32,
                detail: detail as i32,
            },
            time,
        }
    }

    fn started(uuid: &str, name: &str, time: u64) -> Raw {
        lifecycle(
            uuid,
            name,
            sys::VIR_DOMAIN_EVENT_STARTED,
            sys::VIR_DOMAIN_EVENT_STARTED_BOOTED,
            time,
        )
    }

    fn batch(events: Vec<Raw>) -> Batch {
        Batch {
            events,
            ..Batch::default()
        }
    }

    fn listener() -> Arc<Listener> {
        Arc::new(Listener::new(
            Arc::new(Queue::default()),
            Arc::new(Events::new(16)),
        ))
    }

    fn kinds(events: &Events, uuid: &str) -> Vec<(Kind, String)> {
        events
            .read(uuid, 0, None)
            .records
            .into_iter()
            .map(|r| (r.kind, r.detail))
            .collect()
    }

    #[test]
    fn only_visible_vms_get_records_and_visibility_is_cached_by_name() {
        let host = FakeHost::default();
        host.hidden.borrow_mut().push("b");
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(
            batch(vec![
                started("a", "t-a", 1),
                started("b", "users-own", 1),
                started("a", "t-a", 2),
                started("b", "users-own", 2),
            ]),
            3,
        );
        assert_eq!(kinds(l.events(), "a").len(), 2);
        assert!(kinds(l.events(), "b").is_empty());
        assert_eq!(host.lookups.get(), 2);
        // A renamed VM is checked again, here after config came to allow it.
        host.hidden.borrow_mut().clear();
        w.handle(batch(vec![started("b", "t-b", 4)]), 5);
        assert_eq!(host.lookups.get(), 3);
        assert_eq!(kinds(l.events(), "b").len(), 1);
    }

    #[test]
    fn a_visibility_that_cannot_be_told_is_not_cached() {
        let host = FakeHost::default();
        host.unknown.borrow_mut().push("a");
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(batch(vec![started("a", "t-a", 1)]), 1);
        assert!(kinds(l.events(), "a").is_empty());
        // The VM is still in flight, so a reconnect meanwhile leaves it to its own event.
        assert_eq!(*l.in_flight(), ["a"]);
        host.unknown.borrow_mut().clear();
        w.handle(Batch::default(), 2);
        // Kept, tried again with the next batch, and recorded at its own time.
        let records = l.events().read("a", 0, None).records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].time, 1);
        assert_eq!(host.lookups.get(), 2);
        assert!(l.in_flight().is_empty());
    }

    #[test]
    fn a_vm_whose_event_waits_keeps_its_events_in_order_and_the_pass_stops_looking_up() {
        let host = FakeHost::default();
        host.unanswered.set(1);
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        let stopped = lifecycle(
            "a",
            "t-a",
            sys::VIR_DOMAIN_EVENT_STOPPED,
            sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED,
            2,
        );
        // libvirt could answer again by the time a's second event or b's event came up; they wait all the same.
        w.handle(
            batch(vec![
                started("a", "t-a", 1),
                stopped,
                started("b", "t-b", 3),
            ]),
            3,
        );
        assert_eq!(host.lookups.get(), 1);
        assert!(kinds(l.events(), "a").is_empty());
        w.handle(Batch::default(), 4);
        let a: Vec<Kind> = kinds(l.events(), "a").into_iter().map(|(k, _)| k).collect();
        assert_eq!(a, [Kind::Started, Kind::Stopped]);
        assert_eq!(kinds(l.events(), "b").len(), 1);
        assert!(l.in_flight().is_empty());
    }

    #[test]
    fn events_that_cannot_be_placed_for_too_long_give_way_to_a_gap() {
        let host = FakeHost::default();
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(batch(vec![started("b", "t-b", 1)]), 1);
        host.unknown.borrow_mut().push("a");
        let many = (0..=QUEUE_CAPACITY as u64)
            .map(|t| started("a", "t-a", 10 + t))
            .collect();
        w.handle(batch(many), 2000);
        let last = l.events().read("b", 0, None).records.pop().unwrap();
        assert_eq!(last.gap, Some((10, 2000)));
        assert_eq!(l.in_flight().len(), QUEUE_CAPACITY);
    }

    #[test]
    fn a_reconnect_in_the_middle_of_a_batch_leaves_its_events_to_record_themselves() {
        let host = FakeHost::default();
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(batch(vec![started("a", "t-a", 1)]), 1);
        // The lookup of b's VM reconnects, and the new connection already shows a shut off.
        let hook_listener = l.clone();
        *host.on_lookup.borrow_mut() = Some(Box::new(move || {
            let new = FakeSession {
                vms: vec![VmState {
                    uuid: "a".into(),
                    name: "t-a".into(),
                    running: false,
                }],
                ..FakeSession::default()
            };
            hook_listener.reconnected(&FakeSession::default(), &new, 2, 3);
        }));
        w.handle(
            batch(vec![
                started("b", "t-b", 2),
                lifecycle(
                    "a",
                    "t-a",
                    sys::VIR_DOMAIN_EVENT_STOPPED,
                    sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED,
                    2,
                ),
            ]),
            3,
        );
        let stops: Vec<_> = kinds(l.events(), "a")
            .into_iter()
            .filter(|(kind, _)| *kind == Kind::Stopped)
            .collect();
        assert_eq!(stops, [(Kind::Stopped, "destroyed".to_string())]);
    }

    #[test]
    fn a_close_reported_before_the_last_reconnect_does_not_date_the_next_gap() {
        let l = listener();
        l.events().record_during_gap("a", "t-a", true, 1);
        let session = FakeSession::default();
        l.reconnected(&session, &session, 3, 10);
        // The old connection's close callback, or a worker that saw it late.
        l.note_lost(9);
        l.reconnected(&session, &session, 20, 30);
        let gap = l.events().read("a", 0, None).records.pop().unwrap();
        assert_eq!(gap.gap, Some((20, 30)));
    }

    #[test]
    fn a_failed_registration_is_retried_and_its_time_without_callbacks_is_a_gap() {
        let l = listener();
        l.events().record_during_gap("a", "t-a", true, 1);
        let new = FakeSession::default();
        new.fail_register.set(true);
        l.reconnected(&FakeSession::default(), &new, 3, 10);
        assert!(l.needs_register());
        let host = FakeHost::default();
        Worker::new(l.clone(), &host).handle(Batch::default(), 11);
        assert_eq!(host.registers.get(), 1);
        assert!(l.register_again(&new, 12).is_err());
        new.fail_register.set(false);
        l.register_again(&new, 15).unwrap();
        assert!(!l.needs_register());
        let gap = l.events().read("a", 0, None).records.pop().unwrap();
        assert_eq!(gap.gap, Some((10, 15)));
    }

    #[test]
    fn a_good_reconnect_after_a_failed_registration_registers_once() {
        let l = listener();
        l.events().record_during_gap("a", "t-a", true, 1);
        let failing = FakeSession::default();
        failing.fail_register.set(true);
        l.reconnected(&FakeSession::default(), &failing, 3, 10);
        assert!(l.needs_register());
        // register_again reconnects first when the connection died, and that reconnect registers.
        let good = FakeSession::default();
        l.reconnected(&failing, &good, 12, 14);
        assert!(!l.needs_register());
        l.register_again(&good, 15).unwrap();
        assert_eq!(good.registered.get(), 1);
        // The time without callbacks since 10 is one gap with the reconnect's, not a second one.
        let gaps: Vec<_> = l
            .events()
            .read("a", 0, None)
            .records
            .into_iter()
            .filter_map(|r| r.gap)
            .collect();
        assert_eq!(gaps.last(), Some(&(10, 14)));
    }

    #[test]
    fn a_vm_with_events_still_queued_gets_no_during_gap_record() {
        let l = listener();
        let host = FakeHost::default();
        Worker::new(l.clone(), &host).handle(batch(vec![started("a", "t-a", 1)]), 1);
        l.queue().push(lifecycle(
            "a",
            "t-a",
            sys::VIR_DOMAIN_EVENT_STOPPED,
            sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED,
            2,
        ));
        let new = FakeSession {
            vms: vec![VmState {
                uuid: "a".into(),
                name: "t-a".into(),
                running: false,
            }],
            ..FakeSession::default()
        };
        l.reconnected(&FakeSession::default(), &new, 3, 10);
        let records = kinds(l.events(), "a");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].0, Kind::Gap);
    }

    #[test]
    fn an_overflow_records_a_gap_after_the_events_it_kept() {
        let host = FakeHost::default();
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(
            Batch {
                events: vec![started("a", "t-a", 1)],
                dropped_since: Some(2),
                closed_at: None,
            },
            9,
        );
        let read = l.events().read("a", 0, None);
        assert_eq!(read.records.len(), 2);
        assert_eq!(read.records[1].kind, Kind::Gap);
        assert_eq!(read.records[1].gap, Some((2, 9)));
        assert_eq!(host.reconnects.get(), 0);
    }

    #[test]
    fn undefined_drops_the_vms_log_and_other_diagnostics_without_a_lookup() {
        let host = FakeHost::default();
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(batch(vec![started("a", "t-a", 1)]), 1);
        w.handle(
            batch(vec![lifecycle(
                "a",
                "t-a",
                sys::VIR_DOMAIN_EVENT_UNDEFINED,
                sys::VIR_DOMAIN_EVENT_UNDEFINED_REMOVED,
                2,
            )]),
            2,
        );
        assert!(kinds(l.events(), "a").is_empty());
        assert_eq!(*host.forgotten.borrow(), vec!["a".to_string()]);
        assert_eq!(host.lookups.get(), 1);
    }

    #[test]
    fn a_close_reconnects_and_retries_until_it_succeeds() {
        let host = FakeHost::default();
        host.fail_reconnects.set(2);
        let l = listener();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(
            Batch {
                closed_at: Some(5),
                ..Batch::default()
            },
            5,
        );
        w.handle(Batch::default(), 6);
        w.handle(Batch::default(), 7);
        w.handle(Batch::default(), 8);
        assert_eq!(host.reconnects.get(), 3);
    }

    fn reconnect_case(lost_at: Option<u64>) -> (Arc<Listener>, FakeSession, FakeSession) {
        let l = listener();
        let old = FakeSession::default();
        l.start(&old).unwrap();
        let host = FakeHost::default();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(
            batch(vec![
                started("up", "t-up", 1),
                started("down", "t-down", 1),
                started("same", "t-same", 1),
            ]),
            1,
        );
        if let Some(at) = lost_at {
            w.handle(
                Batch {
                    closed_at: Some(at),
                    ..Batch::default()
                },
                at,
            );
        }
        let new = FakeSession {
            vms: vec![
                VmState {
                    uuid: "up".into(),
                    name: "t-up".into(),
                    running: true,
                },
                VmState {
                    uuid: "down".into(),
                    name: "t-down".into(),
                    running: false,
                },
                VmState {
                    uuid: "same".into(),
                    name: "t-same".into(),
                    running: true,
                },
                VmState {
                    uuid: "unlogged".into(),
                    name: "t-new".into(),
                    running: false,
                },
            ],
            ..FakeSession::default()
        };
        // The real hook runs inside the hypervisor's reconnect, whichever path started it.
        l.reconnected(&old, &new, 3, 10);
        (l, old, new)
    }

    #[test]
    fn a_reconnect_from_a_tool_call_registers_again_and_records_the_gap_from_last_alive() {
        let (l, old, new) = reconnect_case(None);
        assert_eq!((old.registered.get(), old.deregistered.get()), (1, 1));
        assert_eq!(new.registered.get(), 1);
        let down = l.events().read("down", 0, None).records;
        assert_eq!(down[1].gap, Some((3, 10)));
        assert_eq!(
            (down[2].kind, down[2].detail.as_str()),
            (Kind::Stopped, "during_gap")
        );
        assert_eq!(kinds(l.events(), "up").len(), 2);
        assert_eq!(kinds(l.events(), "same").len(), 2);
        assert!(kinds(l.events(), "unlogged").is_empty());
    }

    #[test]
    fn a_reconnect_from_the_close_callback_records_the_gap_from_the_close() {
        let (l, _old, new) = reconnect_case(Some(6));
        assert_eq!(new.registered.get(), 1);
        let down = l.events().read("down", 0, None).records;
        assert_eq!(down[1].gap, Some((6, 10)));
        assert_eq!(down[2].detail, "during_gap");
    }

    #[test]
    fn a_vm_that_stopped_and_came_back_reads_started_during_gap() {
        let l = listener();
        let host = FakeHost::default();
        let mut w = Worker::new(l.clone(), &host);
        w.handle(
            batch(vec![
                started("a", "t-a", 1),
                lifecycle(
                    "a",
                    "t-a",
                    sys::VIR_DOMAIN_EVENT_STOPPED,
                    sys::VIR_DOMAIN_EVENT_STOPPED_DESTROYED,
                    2,
                ),
            ]),
            2,
        );
        let new = FakeSession {
            vms: vec![VmState {
                uuid: "a".into(),
                name: "t-a".into(),
                running: true,
            }],
            ..FakeSession::default()
        };
        l.reconnected(&FakeSession::default(), &new, 3, 10);
        let last = l.events().read("a", 0, None).records.pop().unwrap();
        assert_eq!(
            (last.kind, last.detail.as_str()),
            (Kind::Started, "during_gap")
        );
    }
}
