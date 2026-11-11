//! Copies that may outlast a tool call.
//!
//! Every copy tool runs its copy as a transfer on a thread of its own and waits for it up to the call's limit. A
//! transfer that finishes in time answers the call; one that does not keeps running, and `wait_for`'s
//! `transfer_done` condition returns its result later. The table lives in the server process, so a transfer ends
//! with it. Several clients may share one HTTP-transport server, so IDs are random.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

/// How many finished transfers keep their results.
const KEEP_FINISHED: usize = 64;

/// How a finished transfer ended: the reply its copy tool gives, or why it failed.
pub type Outcome = std::result::Result<String, String>;

/// A transfer's state as a caller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running { bytes: u64 },
    Finished(Outcome),
}

struct Entry {
    vm: String,
    uuid: String,
    bytes: Arc<AtomicU64>,
    outcome: Option<Outcome>,
}

#[derive(Default)]
struct Table {
    entries: HashMap<String, Entry>,
    /// Finished IDs, oldest first.
    finished: VecDeque<String>,
}

/// The transfers of this server process.
#[derive(Clone, Default)]
pub struct Transfers {
    table: Arc<(Mutex<Table>, Condvar)>,
}

/// The VM a transfer belongs to.
pub struct Owner {
    pub vm: String,
    pub uuid: String,
}

impl Transfers {
    /// Starts `work` on its own thread as a transfer of `owner`, refusing it while `max_running` transfers run, and
    /// returns its ID. `work` counts the bytes it moves through the counter it is given.
    pub fn start(
        &self,
        owner: Owner,
        max_running: usize,
        work: impl FnOnce(&AtomicU64) -> Result<String> + Send + 'static,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let bytes = Arc::new(AtomicU64::new(0));
        {
            let (lock, _) = &*self.table;
            let mut table = lock
                .lock()
                .expect("no thread panics while holding this lock");
            let running = table
                .entries
                .values()
                .filter(|e| e.outcome.is_none())
                .count();
            if running >= max_running {
                bail!(
                    "{running} transfers are running, the most this server allows (limits.max_transfers); wait \
                     for one with wait_for's transfer_done condition"
                );
            }
            table.entries.insert(
                id.clone(),
                Entry {
                    vm: owner.vm,
                    uuid: owner.uuid,
                    bytes: bytes.clone(),
                    outcome: None,
                },
            );
        }
        let table = self.table.clone();
        let thread_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("transfer-{}", &id[..8]))
            .spawn(move || {
                // A panic would otherwise leave the transfer running for good, holding a slot under the limit.
                let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    work(&bytes)
                })) {
                    Ok(result) => result.map_err(|e| format!("{e:#}")),
                    Err(_) => Err("the transfer failed with an internal error (a panic)".into()),
                };
                finish(&table, &thread_id, outcome);
            });
        if let Err(e) = spawned {
            finish(
                &self.table,
                &id,
                Err(format!("could not start the transfer: {e}")),
            );
        }
        Ok(id)
    }

    /// The transfer `id` of the VM named `vm`, refusing an unknown ID and a transfer of another VM.
    #[cfg(test)]
    pub fn status(&self, id: &str, vm: &str) -> Result<Status> {
        let (lock, _) = &*self.table;
        let table = lock
            .lock()
            .expect("no thread panics while holding this lock");
        status_of(&table, id, vm)
    }

    /// Waits up to `wait` for transfer `id` of `vm` to finish, checking `cancelled` as it goes, and returns its
    /// status then. `on_tick` hears the time waited about once a second.
    pub fn wait(
        &self,
        id: &str,
        vm: &str,
        wait: Duration,
        cancelled: &dyn Fn() -> bool,
        on_tick: &dyn Fn(Duration),
    ) -> Result<Status> {
        let started = Instant::now();
        let (lock, finished) = &*self.table;
        let mut table = lock
            .lock()
            .expect("no thread panics while holding this lock");
        loop {
            let status = status_of(&table, id, vm)?;
            let waited = started.elapsed();
            if matches!(status, Status::Finished(_)) || waited >= wait || cancelled() {
                return Ok(status);
            }
            let slice = (wait - waited).min(Duration::from_secs(1));
            table = finished
                .wait_timeout(table, slice)
                .expect("no thread panics while holding this lock")
                .0;
            on_tick(started.elapsed());
        }
    }
}

fn status_of(table: &Table, id: &str, vm: &str) -> Result<Status> {
    let Some(entry) = table.entries.get(id) else {
        bail!(
            "unknown transfer {id}: it never existed, or it ran in an earlier server process, whose transfers ended \
             with it, or its result was dropped after {KEEP_FINISHED} newer transfers finished"
        );
    };
    if entry.vm != vm && entry.uuid != vm {
        bail!("transfer {id} belongs to another VM");
    }
    Ok(match &entry.outcome {
        None => Status::Running {
            bytes: entry.bytes.load(Ordering::Relaxed),
        },
        Some(outcome) => Status::Finished(outcome.clone()),
    })
}

fn finish(table: &Arc<(Mutex<Table>, Condvar)>, id: &str, outcome: Outcome) {
    let (lock, finished) = &**table;
    let mut table = lock
        .lock()
        .expect("no thread panics while holding this lock");
    if let Some(entry) = table.entries.get_mut(id) {
        entry.outcome = Some(outcome);
    }
    table.finished.push_back(id.to_string());
    while table.finished.len() > KEEP_FINISHED {
        if let Some(old) = table.finished.pop_front() {
            table.entries.remove(&old);
        }
    }
    finished.notify_all();
}

/// The result of a `wait_for` whose only condition is `transfer_done`. It reads the transfer table alone, never the
/// VM, so a transfer cut off by a stopped, expired or deleted VM still reports how it ended.
pub fn transfer_done(
    transfers: &Transfers,
    vm: &str,
    id: &str,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
    on_tick: &dyn Fn(Duration),
) -> Result<Value> {
    let started = Instant::now();
    let status = transfers.wait(id, vm, timeout, cancelled, on_tick)?;
    let waited_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(match status {
        Status::Finished(outcome) => {
            let mut v = json!({
                "result": "matched", "index": 0, "kind": "transfer_done", "transfer": id, "waited_ms": waited_ms,
            });
            match outcome {
                Ok(reply) => v["text"] = json!(reply),
                Err(error) => v["error"] = json!(error),
            }
            v
        }
        Status::Running { bytes } => json!({
            "result": "timed_out", "transfer": id, "bytes": bytes, "waited_ms": waited_ms,
        }),
    })
}

/// The reply of a copy tool whose transfer is still running.
pub fn running_reply(id: &str, bytes: u64) -> Value {
    json!({
        "transfer": id,
        "state": "running",
        "bytes": bytes,
        "note": "the copy continues in the server; wait for it with wait_for and a transfer_done condition naming \
                 this transfer",
    })
}

/// Counts the bytes written through it.
pub struct CountingWriter<'a, W> {
    pub inner: W,
    pub count: &'a AtomicU64,
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Counts the bytes read through it.
pub struct CountingReader<'a, R> {
    pub inner: R,
    pub count: &'a AtomicU64,
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn owner(vm: &str) -> Owner {
        Owner {
            vm: vm.into(),
            uuid: format!("uuid-of-{vm}"),
        }
    }

    fn never_cancelled() -> bool {
        false
    }

    #[test]
    fn a_short_transfer_answers_within_the_wait_and_a_long_one_later() {
        let transfers = Transfers::default();
        let quick = transfers
            .start(owner("vm"), 4, |_| Ok("copied 3 bytes".into()))
            .unwrap();
        let status = transfers
            .wait(
                &quick,
                "vm",
                Duration::from_secs(5),
                &never_cancelled,
                &|_| {},
            )
            .unwrap();
        assert_eq!(status, Status::Finished(Ok("copied 3 bytes".into())));

        let (go, gate) = mpsc::channel::<()>();
        let slow = transfers
            .start(owner("vm"), 4, move |bytes| {
                bytes.fetch_add(10, Ordering::Relaxed);
                gate.recv().unwrap();
                Ok("copied 10 bytes".into())
            })
            .unwrap();
        let status = transfers
            .wait(
                &slow,
                "vm",
                Duration::from_millis(50),
                &never_cancelled,
                &|_| {},
            )
            .unwrap();
        assert!(matches!(status, Status::Running { .. }), "{status:?}");
        go.send(()).unwrap();
        let status = transfers
            .wait(
                &slow,
                "vm",
                Duration::from_secs(5),
                &never_cancelled,
                &|_| {},
            )
            .unwrap();
        assert_eq!(status, Status::Finished(Ok("copied 10 bytes".into())));
    }

    #[test]
    fn a_failed_transfer_reports_why_whatever_happened_to_its_vm() {
        let transfers = Transfers::default();
        let id = transfers
            .start(owner("vm"), 4, |_| bail!("VM 'vm' is not running"))
            .unwrap();
        let status = transfers
            .wait(&id, "vm", Duration::from_secs(5), &never_cancelled, &|_| {})
            .unwrap();
        assert_eq!(
            status,
            Status::Finished(Err("VM 'vm' is not running".into()))
        );
    }

    #[test]
    fn a_panicking_transfer_finishes_and_frees_its_slot() {
        let transfers = Transfers::default();
        let id = transfers
            .start(owner("vm"), 1, |_| panic!("scripted panic"))
            .unwrap();
        let status = transfers
            .wait(&id, "vm", Duration::from_secs(5), &never_cancelled, &|_| {})
            .unwrap();
        assert!(
            matches!(&status, Status::Finished(Err(e)) if e.contains("panic")),
            "{status:?}"
        );
        transfers
            .start(owner("vm"), 1, |_| Ok("next".into()))
            .unwrap();
    }

    #[test]
    fn another_vms_transfer_and_an_unknown_id_are_refused() {
        let transfers = Transfers::default();
        let id = transfers
            .start(owner("vm"), 4, |_| Ok(String::new()))
            .unwrap();
        let err = transfers.status(&id, "other").unwrap_err().to_string();
        assert!(err.contains("another VM"), "{err}");
        transfers.status(&id, "uuid-of-vm").unwrap();
        let err = transfers.status("nope", "vm").unwrap_err().to_string();
        assert!(err.contains("unknown transfer"), "{err}");
    }

    #[test]
    fn a_transfer_beyond_the_limit_is_refused() {
        let transfers = Transfers::default();
        let (go, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let mut ids = Vec::new();
        for _ in 0..4 {
            let gate = gate.clone();
            ids.push(
                transfers
                    .start(owner("vm"), 4, move |_| {
                        gate.lock().unwrap().recv().unwrap();
                        Ok(String::new())
                    })
                    .unwrap(),
            );
        }
        let err = transfers
            .start(owner("vm"), 4, |_| Ok(String::new()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_transfers"), "{err}");
        for _ in 0..4 {
            go.send(()).unwrap();
        }
        for id in &ids {
            transfers
                .wait(id, "vm", Duration::from_secs(5), &never_cancelled, &|_| {})
                .unwrap();
        }
        transfers
            .start(owner("vm"), 4, |_| Ok(String::new()))
            .unwrap();
    }

    #[test]
    fn only_the_newest_finished_results_are_kept() {
        let transfers = Transfers::default();
        let first = transfers
            .start(owner("vm"), 4, |_| Ok(String::new()))
            .unwrap();
        transfers
            .wait(
                &first,
                "vm",
                Duration::from_secs(5),
                &never_cancelled,
                &|_| {},
            )
            .unwrap();
        for _ in 0..KEEP_FINISHED {
            let id = transfers
                .start(owner("vm"), 4, |_| Ok(String::new()))
                .unwrap();
            transfers
                .wait(&id, "vm", Duration::from_secs(5), &never_cancelled, &|_| {})
                .unwrap();
        }
        assert!(transfers.status(&first, "vm").is_err());
    }

    #[test]
    fn transfer_done_reports_a_result_a_failure_or_a_timeout() {
        let transfers = Transfers::default();
        let ok = transfers
            .start(owner("vm"), 4, |_| Ok("copied 5 bytes".into()))
            .unwrap();
        let v = transfer_done(
            &transfers,
            "vm",
            &ok,
            Duration::from_secs(5),
            &never_cancelled,
            &|_| {},
        )
        .unwrap();
        assert_eq!(
            (v["result"].as_str(), v["text"].as_str()),
            (Some("matched"), Some("copied 5 bytes"))
        );

        // The VM stopped mid-copy: the table still knows how the transfer ended, without looking the VM up.
        let cut = transfers
            .start(owner("gone"), 4, |_| bail!("guest agent is not connected"))
            .unwrap();
        let v = transfer_done(
            &transfers,
            "gone",
            &cut,
            Duration::from_secs(5),
            &never_cancelled,
            &|_| {},
        )
        .unwrap();
        assert_eq!(v["error"].as_str(), Some("guest agent is not connected"));

        let (_go, gate) = mpsc::channel::<()>();
        let slow = transfers
            .start(owner("vm"), 4, move |bytes| {
                bytes.store(7, Ordering::Relaxed);
                let _ = gate.recv();
                Ok(String::new())
            })
            .unwrap();
        let v = transfer_done(
            &transfers,
            "vm",
            &slow,
            Duration::from_millis(100),
            &never_cancelled,
            &|_| {},
        )
        .unwrap();
        assert_eq!(
            (v["result"].as_str(), v["bytes"].as_u64()),
            (Some("timed_out"), Some(7))
        );
        assert!(
            transfer_done(
                &transfers,
                "other",
                &slow,
                Duration::ZERO,
                &never_cancelled,
                &|_| {}
            )
            .is_err()
        );
    }

    #[test]
    fn a_cancelled_wait_returns_at_once() {
        let transfers = Transfers::default();
        let (_go, gate) = mpsc::channel::<()>();
        let id = transfers
            .start(owner("vm"), 4, move |_| {
                let _ = gate.recv();
                Ok(String::new())
            })
            .unwrap();
        let started = Instant::now();
        let status = transfers
            .wait(&id, "vm", Duration::from_secs(30), &|| true, &|_| {})
            .unwrap();
        assert!(matches!(status, Status::Running { .. }));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
