//! libvirt's event callbacks. Each callback copies plain data into a bounded queue and returns: it never calls back
//! into the connection, and a panic inside it stops at the callback instead of unwinding into libvirt's event loop,
//! which would abort the process.

use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use virt::connect::Connect;
use virt::domain::Domain;
use virt::sys;

use super::worker::{Session, VmState};
use super::{Raw, Source};
use crate::manage::now_epoch;

/// How many events wait for the worker before new ones are dropped and a gap is recorded instead.
pub const QUEUE_CAPACITY: usize = 1024;

/// What the worker takes from the queue in one go.
#[derive(Debug, Default)]
pub struct Batch {
    pub events: Vec<Raw>,
    /// When the queue first dropped an event since the last batch: events from then until now may be missing.
    pub dropped_since: Option<u64>,
    /// When the connection reported itself closed, if it did since the last batch.
    pub closed_at: Option<u64>,
}

#[derive(Default)]
struct Inner {
    events: VecDeque<Raw>,
    dropped_since: Option<u64>,
    closed_at: Option<u64>,
}

/// The bounded queue between libvirt's event thread and the worker.
#[derive(Default)]
pub struct Queue {
    inner: Mutex<Inner>,
    ready: Condvar,
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Every state of the queue is valid, so a panic elsewhere while holding it leaves nothing to repair.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds an event, or marks the overflow when the queue is full.
    pub fn push(&self, raw: Raw) {
        let mut inner = self.lock();
        if inner.events.len() >= QUEUE_CAPACITY {
            inner.dropped_since.get_or_insert(raw.time);
        } else {
            tracing::debug!(vm = %raw.name, uuid = %raw.uuid, source = ?raw.source, "libvirt event");
            inner.events.push_back(raw);
        }
        drop(inner);
        self.ready.notify_one();
    }

    /// Marks the connection closed at `at`, unless an earlier close is still unread.
    pub fn close(&self, at: u64) {
        self.lock().closed_at.get_or_insert(at);
        self.ready.notify_one();
    }

    /// The UUIDs of the VMs with events still queued.
    pub fn pending(&self) -> HashSet<String> {
        self.lock().events.iter().map(|r| r.uuid.clone()).collect()
    }

    /// Takes everything queued, waiting up to `timeout` for something to arrive.
    pub fn take(&self, timeout: Duration) -> Batch {
        let inner = self.lock();
        let (mut inner, _) = self
            .ready
            .wait_timeout_while(inner, timeout, |i| {
                i.events.is_empty() && i.dropped_since.is_none() && i.closed_at.is_none()
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Batch {
            events: inner.events.drain(..).collect(),
            dropped_since: inner.dropped_since.take(),
            closed_at: inner.closed_at.take(),
        }
    }
}

/// The callbacks registered on one connection, to remove them before the connection is replaced.
#[derive(Debug)]
pub struct Registration {
    ids: Vec<c_int>,
    close: bool,
}

impl Registration {
    /// A registration with nothing registered, for a test's fake connection.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            ids: Vec::new(),
            close: false,
        }
    }
}

/// A live libvirt connection as the listener uses it, with the server's visibility rule.
pub struct Live<'a> {
    pub conn: &'a Connect,
    pub visible: &'a dyn Fn(&Domain) -> bool,
}

impl Session for Live<'_> {
    fn register(&self, queue: &Arc<Queue>) -> Result<Registration> {
        register(self.conn, queue)
    }

    fn deregister(&self, registration: Registration) {
        deregister(self.conn, registration);
    }

    fn visible_vms(&self) -> Result<Vec<VmState>> {
        let mut vms = Vec::new();
        for dom in self.conn.list_all_domains(0)? {
            if !(self.visible)(&dom) {
                continue;
            }
            // A VM undefined since the listing cannot be read; the others still count.
            if let (Ok(uuid), Ok(name), Ok(running)) =
                (dom.get_uuid_string(), dom.get_name(), dom.is_active())
            {
                vms.push(VmState {
                    uuid,
                    name,
                    running,
                });
            }
        }
        Ok(vms)
    }
}

/// Runs a callback body, stopping a panic there. libvirt's event thread is C code, and unwinding into it aborts.
fn guarded(what: &str, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        tracing::error!("the {what} callback panicked; its event is lost");
    }
}

/// The queue an `opaque` pointer passed to libvirt stands for.
///
/// # Safety
///
/// `opaque` must come from `opaque_for` and not yet have been passed to `free_opaque`.
unsafe fn queue<'a>(opaque: *mut c_void) -> &'a Queue {
    // SAFETY: the caller guarantees `opaque` is a live `Box<Arc<Queue>>`.
    unsafe { &*(opaque as *const Arc<Queue>) }
}

fn opaque_for(queue: &Arc<Queue>) -> *mut c_void {
    Box::into_raw(Box::new(queue.clone())) as *mut c_void
}

/// libvirt calls this once for each registration, after the last callback that uses `opaque`.
unsafe extern "C" fn free_opaque(opaque: *mut c_void) {
    guarded("free", || {
        // SAFETY: libvirt passes back the pointer `opaque_for` made, once.
        drop(unsafe { Box::from_raw(opaque as *mut Arc<Queue>) });
    });
}

/// The UUID and name of the domain an event is about, read from the domain object libvirt passes in.
///
/// # Safety
///
/// `dom` must be the domain pointer libvirt passed to the running callback.
unsafe fn identity(dom: sys::virDomainPtr) -> Option<(String, String)> {
    let mut buf: [c_char; sys::VIR_UUID_STRING_BUFLEN as usize] =
        [0; sys::VIR_UUID_STRING_BUFLEN as usize];
    // SAFETY: `dom` is valid for the callback, and `buf` holds the string and its terminator.
    if unsafe { sys::virDomainGetUUIDString(dom, buf.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: libvirt wrote a terminated string into `buf`.
    let uuid = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: the name belongs to `dom`, which outlives this call; it is copied before returning.
    let name = unsafe { sys::virDomainGetName(dom) };
    if name.is_null() {
        return None;
    }
    // SAFETY: checked non-null; libvirt keeps it terminated.
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    Some((uuid, name))
}

/// Queues one event.
///
/// # Safety
///
/// The arguments are the ones libvirt passed to the running callback.
unsafe fn deliver(dom: sys::virDomainPtr, opaque: *mut c_void, source: Source) {
    // SAFETY: passed through from libvirt, per the caller.
    let Some((uuid, name)) = (unsafe { identity(dom) }) else {
        tracing::warn!("a libvirt event arrived for a domain without a UUID or name; dropped");
        return;
    };
    // SAFETY: libvirt passes back the `opaque` it was registered with, before freeing it.
    unsafe { queue(opaque) }.push(Raw {
        uuid,
        name,
        source,
        time: now_epoch(),
    });
}

unsafe extern "C" fn on_lifecycle(
    _conn: sys::virConnectPtr,
    dom: sys::virDomainPtr,
    event: c_int,
    detail: c_int,
    opaque: *mut c_void,
) -> c_int {
    // SAFETY: the arguments come from libvirt.
    guarded("lifecycle", || unsafe {
        deliver(dom, opaque, Source::Lifecycle { event, detail })
    });
    0
}

unsafe extern "C" fn on_reboot(
    _conn: sys::virConnectPtr,
    dom: sys::virDomainPtr,
    opaque: *mut c_void,
) {
    // SAFETY: the arguments come from libvirt.
    guarded("reboot", || unsafe { deliver(dom, opaque, Source::Reboot) });
}

unsafe extern "C" fn on_watchdog(
    _conn: sys::virConnectPtr,
    dom: sys::virDomainPtr,
    action: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: the arguments come from libvirt.
    guarded("watchdog", || unsafe {
        deliver(dom, opaque, Source::Watchdog { action })
    });
}

unsafe extern "C" fn on_agent(
    _conn: sys::virConnectPtr,
    dom: sys::virDomainPtr,
    state: c_int,
    reason: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: the arguments come from libvirt.
    guarded("agent lifecycle", || unsafe {
        deliver(dom, opaque, Source::Agent { state, reason })
    });
}

unsafe extern "C" fn on_close(_conn: sys::virConnectPtr, reason: c_int, opaque: *mut c_void) {
    guarded("close", || {
        tracing::warn!("the libvirt connection closed (reason {reason})");
        // SAFETY: libvirt passes back the `opaque` it was registered with, before freeing it.
        unsafe { queue(opaque) }.close(now_epoch());
    });
}

/// Registers the four event callbacks and the close callback on `conn`, all feeding `queue`.
///
/// # Errors
///
/// Fails when libvirt refuses a registration; those made before it are removed again.
pub fn register(conn: &Connect, queue: &Arc<Queue>) -> Result<Registration> {
    let ptr = conn.as_ptr();
    // libvirt calls each callback through the generic type and casts it back by event ID, so each function is cast
    // to the generic type here, as the C API documents.
    // SAFETY: each function's real signature is the one libvirt uses for its event ID.
    let callbacks: [(u32, sys::virConnectDomainEventGenericCallback); 4] = unsafe {
        [
            (
                sys::VIR_DOMAIN_EVENT_ID_LIFECYCLE,
                std::mem::transmute::<
                    unsafe extern "C" fn(
                        sys::virConnectPtr,
                        sys::virDomainPtr,
                        c_int,
                        c_int,
                        *mut c_void,
                    ) -> c_int,
                    sys::virConnectDomainEventGenericCallback,
                >(on_lifecycle),
            ),
            (sys::VIR_DOMAIN_EVENT_ID_REBOOT, Some(on_reboot)),
            (
                sys::VIR_DOMAIN_EVENT_ID_WATCHDOG,
                std::mem::transmute::<
                    unsafe extern "C" fn(sys::virConnectPtr, sys::virDomainPtr, c_int, *mut c_void),
                    sys::virConnectDomainEventGenericCallback,
                >(on_watchdog),
            ),
            (
                sys::VIR_DOMAIN_EVENT_ID_AGENT_LIFECYCLE,
                std::mem::transmute::<
                    unsafe extern "C" fn(
                        sys::virConnectPtr,
                        sys::virDomainPtr,
                        c_int,
                        c_int,
                        *mut c_void,
                    ),
                    sys::virConnectDomainEventGenericCallback,
                >(on_agent),
            ),
        ]
    };
    let mut registration = Registration {
        ids: Vec::new(),
        close: false,
    };
    for (event, callback) in callbacks {
        let opaque = opaque_for(queue);
        // SAFETY: `ptr` is a live connection. libvirt frees `opaque` through `free_opaque` once the registration ends;
        // a failed registration may not free it, which leaks one queue reference rather than risking a double free.
        let id = unsafe {
            sys::virConnectDomainEventRegisterAny(
                ptr,
                std::ptr::null_mut(),
                event as c_int,
                callback,
                opaque,
                Some(free_opaque),
            )
        };
        if id < 0 {
            // Read the error first: the deregistration's own calls reset it.
            let error = virt::error::Error::last_error();
            deregister(conn, registration);
            bail!("registering libvirt event {event}: {error}");
        }
        registration.ids.push(id);
    }
    let opaque = opaque_for(queue);
    // SAFETY: as above.
    let rc = unsafe {
        sys::virConnectRegisterCloseCallback(ptr, Some(on_close), opaque, Some(free_opaque))
    };
    if rc < 0 {
        let error = virt::error::Error::last_error();
        deregister(conn, registration);
        bail!("registering the libvirt close callback: {error}");
    }
    registration.close = true;
    Ok(registration)
}

/// Removes the callbacks of `registration` from `conn`. Best effort: a connection that is already gone has dropped
/// them itself.
pub fn deregister(conn: &Connect, registration: Registration) {
    let ptr = conn.as_ptr();
    for id in registration.ids {
        // SAFETY: `ptr` is the connection `id` was registered on.
        if unsafe { sys::virConnectDomainEventDeregisterAny(ptr, id) } < 0 {
            tracing::debug!("removing libvirt event callback {id} failed");
        }
    }
    if registration.close {
        // SAFETY: as above.
        if unsafe { sys::virConnectUnregisterCloseCallback(ptr, Some(on_close)) } < 0 {
            tracing::debug!("removing the libvirt close callback failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(time: u64) -> Raw {
        Raw {
            uuid: "u".into(),
            name: "t-a".into(),
            source: Source::Reboot,
            time,
        }
    }

    #[test]
    fn the_wrapper_returns_from_a_body_that_panics() {
        let mut after = false;
        guarded("test", || panic!("a callback body panicked"));
        guarded("test", || after = true);
        assert!(after);
    }

    #[test]
    fn a_full_queue_drops_new_events_and_marks_when_it_started() {
        let q = Queue::default();
        for t in 0..QUEUE_CAPACITY as u64 {
            q.push(raw(t));
        }
        q.push(raw(5000));
        q.push(raw(5001));
        let batch = q.take(Duration::ZERO);
        assert_eq!(batch.events.len(), QUEUE_CAPACITY);
        assert_eq!(
            batch.events.last().map(|r| r.time),
            Some(QUEUE_CAPACITY as u64 - 1)
        );
        assert_eq!(batch.dropped_since, Some(5000));
        assert_eq!(batch.closed_at, None);
        let next = q.take(Duration::ZERO);
        assert!(next.events.is_empty() && next.dropped_since.is_none());
    }

    #[test]
    fn take_returns_events_and_the_close_mark_once() {
        let q = Queue::default();
        q.push(raw(1));
        q.close(7);
        q.close(9);
        let batch = q.take(Duration::from_secs(5));
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.closed_at, Some(7));
        assert_eq!(q.take(Duration::ZERO).closed_at, None);
    }

    #[test]
    fn the_free_callback_releases_the_queue_reference() {
        let q = Arc::new(Queue::default());
        let opaque = opaque_for(&q);
        assert_eq!(Arc::strong_count(&q), 2);
        // SAFETY: `opaque` came from `opaque_for` and is freed once.
        unsafe { free_opaque(opaque) };
        assert_eq!(Arc::strong_count(&q), 1);
    }
}
