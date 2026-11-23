//! `vm_stats`: one snapshot of a running VM's resource use, from two samples of libvirt's domain statistics, and the
//! guest's filesystems from the guest agent.

use std::collections::HashMap;
use std::ffi::CStr;
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Map, Value, json};
use virt::domain::Domain;
use virt::sys;

use crate::agent::Agent;

/// The time between the two samples.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// The statistics groups each sample asks for.
const GROUPS: u32 = sys::VIR_DOMAIN_STATS_STATE
    | sys::VIR_DOMAIN_STATS_CPU_TOTAL
    | sys::VIR_DOMAIN_STATS_BALLOON
    | sys::VIR_DOMAIN_STATS_VCPU
    | sys::VIR_DOMAIN_STATS_INTERFACE
    | sys::VIR_DOMAIN_STATS_BLOCK;

/// What the reply says about the filesystem figures it holds.
const FS_CLAIM: &str =
    "The filesystem figures come from the guest agent: they are the guest's own claim.";
const FS_READ_ONLY: &str = "read-only mode disables guest-agent queries";

/// One typed parameter's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Int(i64),
    Uint(u64),
    Double(f64),
    Text(String),
}

/// One reading of libvirt's statistics for a VM, by parameter name, such as `cpu.time` or `block.0.rd.bytes`.
#[derive(Debug, Clone)]
pub struct Sample {
    pub taken: Instant,
    pub params: HashMap<String, Param>,
}

impl Sample {
    fn uint(&self, key: &str) -> Option<u64> {
        match self.params.get(key)? {
            Param::Uint(v) => Some(*v),
            Param::Int(v) => u64::try_from(*v).ok(),
            Param::Double(_) | Param::Text(_) => None,
        }
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.params.get(key)? {
            Param::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The names of the indexed devices of one group (`block` or `net`), by index.
    fn devices(&self, group: &str) -> Vec<(usize, String)> {
        let count = self.uint(&format!("{group}.count")).unwrap_or(0);
        (0..usize::try_from(count).unwrap_or(0))
            .filter_map(|i| Some((i, self.text(&format!("{group}.{i}.name"))?.to_owned())))
            .collect()
    }

    /// The index of the device named `name` in `group`.
    fn index_of(&self, group: &str, name: &str) -> Option<usize> {
        self.devices(group)
            .into_iter()
            .find_map(|(i, n)| (n == name).then_some(i))
    }
}

/// The VM a report reads.
pub trait StatsVm {
    /// libvirt's state number, such as `VIR_DOMAIN_RUNNING`.
    fn state(&self) -> Result<u32>;
    fn sample(&self) -> Result<Sample>;
    /// The VM's live XML, read for its balloon device.
    fn xml(&self) -> Result<String>;
}

impl StatsVm for Domain {
    fn state(&self) -> Result<u32> {
        Ok(self.get_state()?.0)
    }

    fn sample(&self) -> Result<Sample> {
        sample(self)
    }

    fn xml(&self) -> Result<String> {
        Ok(self.get_xml_desc(0)?)
    }
}

/// Owns the record list `virDomainListGetStats` returns, and frees it once.
struct Records(*mut sys::virDomainStatsRecordPtr);

impl Drop for Records {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from virDomainListGetStats, which allocated the list for this caller, and only
            // this owner frees it.
            unsafe { sys::virDomainStatsRecordListFree(self.0) };
        }
    }
}

/// Reads libvirt's statistics for `dom` alone. The `virt` crate's `get_all_domain_stats` has no accessors and leaks
/// its records, so this reads them through `virt::sys` and copies each parameter before freeing the list.
fn sample(dom: &Domain) -> Result<Sample> {
    let mut doms = [dom.as_ptr(), ptr::null_mut()];
    let mut list: *mut sys::virDomainStatsRecordPtr = ptr::null_mut();
    // SAFETY: `doms` is a NULL-terminated array holding a domain pointer that `dom` keeps referenced for the call, and
    // libvirt writes a list it allocates to `list`, which `Records` frees.
    let n = unsafe { sys::virDomainListGetStats(doms.as_mut_ptr(), GROUPS, &mut list, 0) };
    let taken = Instant::now();
    let records = Records(list);
    if n < 0 {
        return Err(virt::error::Error::last_error().into());
    }
    let mut params = HashMap::new();
    if n > 0 && !records.0.is_null() {
        // SAFETY: the list holds `n` record pointers followed by NULL, so its first entry is readable.
        let record = unsafe { *records.0 };
        if !record.is_null() {
            // SAFETY: `record` points to a record in the list, which lives until `records` drops.
            let record = unsafe { &*record };
            let count = usize::try_from(record.nparams).unwrap_or(0);
            if count > 0 && !record.params.is_null() {
                // SAFETY: `params` points to `nparams` typed parameters owned by the record.
                let items = unsafe { std::slice::from_raw_parts(record.params, count) };
                for item in items {
                    if let Some((name, value)) = read_param(item) {
                        params.insert(name, value);
                    }
                }
            }
        }
    }
    Ok(Sample { taken, params })
}

/// Copies one typed parameter, or `None` for a name that is not text or a type the reply does not use.
fn read_param(item: &sys::virTypedParameter) -> Option<(String, Param)> {
    let field: Vec<u8> = item.field.iter().map(|&c| c as u8).collect();
    let name = CStr::from_bytes_until_nul(&field)
        .ok()?
        .to_str()
        .ok()?
        .to_owned();
    let type_ = u32::try_from(item.type_).ok()?;
    // SAFETY: each arm reads the union member that `type_` names, as libvirt's typed-parameter contract requires.
    let value = unsafe {
        match type_ {
            sys::VIR_TYPED_PARAM_INT => Param::Int(i64::from(item.value.i)),
            sys::VIR_TYPED_PARAM_UINT => Param::Uint(u64::from(item.value.ui)),
            sys::VIR_TYPED_PARAM_LLONG => Param::Int(item.value.l),
            sys::VIR_TYPED_PARAM_ULLONG => Param::Uint(item.value.ul),
            sys::VIR_TYPED_PARAM_DOUBLE => Param::Double(item.value.d),
            sys::VIR_TYPED_PARAM_BOOLEAN => Param::Uint(u64::from(item.value.b != 0)),
            sys::VIR_TYPED_PARAM_STRING if !item.value.s.is_null() => {
                Param::Text(CStr::from_ptr(item.value.s).to_string_lossy().into_owned())
            }
            _ => return None,
        }
    };
    Some((name, value))
}

/// The per-second rate of a counter between two samples, or `None` when either reading is missing or the counter went
/// down, as it does when the VM restarts between them.
fn rate(first: Option<u64>, second: Option<u64>, elapsed: Duration) -> Option<u64> {
    let delta = second?.checked_sub(first?)?;
    let secs = elapsed.as_secs_f64();
    (secs > 0.0).then(|| (delta as f64 / secs).round() as u64)
}

/// The time the vCPUs ran, in nanoseconds, summed over `vcpu.<n>.time`. `cpu.time` would also count QEMU's own
/// threads, which put a busy VM above 100 %.
fn vcpu_time(sample: &Sample) -> Option<u64> {
    let times: Vec<u64> = (0..sample.uint("vcpu.maximum")?)
        .filter_map(|n| sample.uint(&format!("vcpu.{n}.time")))
        .collect();
    if times.is_empty() {
        return None;
    }
    times.into_iter().try_fold(0_u64, u64::checked_add)
}

/// CPU use over the interval as a percentage of the VM's vCPUs, rounded to one decimal. Sampling jitter can put a
/// fully busy VM a little over 100 %, so the figure stops there.
fn cpu_percent(first: &Sample, second: &Sample, elapsed: Duration) -> Option<f64> {
    let busy = vcpu_time(second)?.checked_sub(vcpu_time(first)?)?;
    let vcpus = second.uint("vcpu.current")?;
    let total = elapsed.as_nanos() as f64 * vcpus as f64;
    (total > 0.0).then(|| ((busy as f64 / total * 1000.0).round() / 10.0).min(100.0))
}

fn kib(sample: &Sample, key: &str) -> Option<u64> {
    sample.uint(key).map(|v| v.saturating_mul(1024))
}

/// Why the guest's own memory figures are missing, read from the VM's XML.
fn memory_missing_reason(xml: &str) -> String {
    let doc = match roxmltree::Document::parse(xml) {
        Ok(doc) => doc,
        Err(e) => return format!("the VM's XML could not be read: {e}"),
    };
    let balloon = doc
        .descendants()
        .find(|n| n.has_tag_name("memballoon"))
        .filter(|n| n.attribute("model") != Some("none"));
    let Some(balloon) = balloon else {
        return "the VM has no memory balloon device, so nothing reports the guest's memory".into();
    };
    let period = balloon
        .children()
        .find(|n| n.has_tag_name("stats"))
        .and_then(|n| n.attribute("period"))
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    if period == 0 {
        return "the memory balloon has no stats period, so it reports no guest memory figures"
            .into();
    }
    "no balloon driver reports in the guest; a Windows guest needs the virtio balloon driver and its service".into()
}

fn memory(first: &Sample, second: &Sample, xml: impl FnOnce() -> Result<String>) -> Value {
    let mut out = Map::new();
    for (name, key) in [
        ("configured_bytes", "balloon.maximum"),
        ("current_bytes", "balloon.current"),
        ("host_resident_bytes", "balloon.rss"),
    ] {
        out.insert(name.into(), json!(kib(second, key)));
    }
    let guest: Vec<(&str, u64)> = [
        ("available_bytes", "balloon.available"),
        ("usable_bytes", "balloon.usable"),
        ("unused_bytes", "balloon.unused"),
        ("disk_caches_bytes", "balloon.disk_caches"),
    ]
    .into_iter()
    .filter_map(|(name, key)| Some((name, kib(second, key).or_else(|| kib(first, key))?)))
    .collect();
    if guest.is_empty() {
        let reason = match xml() {
            Ok(xml) => memory_missing_reason(&xml),
            Err(e) => format!("the VM's XML could not be read: {e:#}"),
        };
        out.insert("guest_missing".into(), json!(reason));
    } else {
        let guest: Map<String, Value> = guest
            .into_iter()
            .map(|(name, v)| (name.to_owned(), json!(v)))
            .collect();
        out.insert("guest".into(), Value::Object(guest));
    }
    Value::Object(out)
}

fn disks(first: &Sample, second: &Sample, elapsed: Duration) -> Vec<Value> {
    second
        .devices("block")
        .into_iter()
        .map(|(i, name)| {
            let before = first.index_of("block", &name);
            let counter = |key: &str| {
                rate(
                    before.and_then(|b| first.uint(&format!("block.{b}.{key}"))),
                    second.uint(&format!("block.{i}.{key}")),
                    elapsed,
                )
            };
            json!({
                "target": name,
                "read_bytes_per_sec": counter("rd.bytes"),
                "write_bytes_per_sec": counter("wr.bytes"),
                "capacity_bytes": second.uint(&format!("block.{i}.capacity")),
                "allocation_bytes": second.uint(&format!("block.{i}.allocation")),
            })
        })
        .collect()
}

fn interfaces(first: &Sample, second: &Sample, elapsed: Duration) -> Vec<Value> {
    second
        .devices("net")
        .into_iter()
        .map(|(i, name)| {
            let before = first.index_of("net", &name);
            let counter = |key: &str| {
                rate(
                    before.and_then(|b| first.uint(&format!("net.{b}.{key}"))),
                    second.uint(&format!("net.{i}.{key}")),
                    elapsed,
                )
            };
            json!({
                "device": name,
                "received_bytes_per_sec": counter("rx.bytes"),
                "sent_bytes_per_sec": counter("tx.bytes"),
                "received_drops": second.uint(&format!("net.{i}.rx.drop")),
                "sent_drops": second.uint(&format!("net.{i}.tx.drop")),
            })
        })
        .collect()
}

/// The guest's filesystems from `guest-get-fsinfo`, or why there are none.
fn filesystems(agent: &Agent<'_>) -> std::result::Result<Vec<Value>, String> {
    let info = agent
        .fsinfo()
        .map_err(|e| format!("the guest agent did not answer: {e:#}"))?;
    let list = info
        .as_array()
        .ok_or("the guest agent's filesystem list is not a list")?;
    Ok(list
        .iter()
        .map(|fs| {
            json!({
                "mountpoint": fs.get("mountpoint"),
                "type": fs.get("type"),
                "total_bytes": fs.get("total-bytes"),
                "used_bytes": fs.get("used-bytes"),
            })
        })
        .collect())
}

/// The statistics block of a reply from two samples. A counter that went down between them, as on a restart, gives
/// `null` for its rate.
pub fn figures(
    first: &Sample,
    second: &Sample,
    xml: impl FnOnce() -> Result<String>,
) -> Map<String, Value> {
    let elapsed = second.taken.saturating_duration_since(first.taken);
    let mut out = Map::new();
    out.insert("state".into(), json!("running"));
    out.insert(
        "interval_ms".into(),
        json!(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
    );
    out.insert(
        "cpu".into(),
        json!({
            "percent": cpu_percent(first, second, elapsed),
            "vcpus": second.uint("vcpu.current"),
        }),
    );
    out.insert("memory".into(), memory(first, second, xml));
    out.insert("disks".into(), Value::Array(disks(first, second, elapsed)));
    out.insert(
        "interfaces".into(),
        Value::Array(interfaces(first, second, elapsed)),
    );
    out
}

/// A reply for a VM that is not running: its state alone.
fn not_running(state: u32) -> Value {
    json!({
        "state": crate::hypervisor::state_label(state),
        "note": "vm_stats reports figures only for a running VM.",
    })
}

/// Samples `vm` twice, `interval` apart, and adds the guest's filesystems from `agent` unless `read_only`.
pub fn report(
    vm: &dyn StatsVm,
    agent: &Agent<'_>,
    read_only: bool,
    interval: Duration,
) -> Result<Value> {
    let state = vm.state()?;
    if state != sys::VIR_DOMAIN_RUNNING {
        return Ok(not_running(state));
    }
    let first = vm.sample()?;
    std::thread::sleep(interval);
    let second = match vm.sample() {
        Ok(second) => second,
        Err(e) => {
            // The VM may have stopped between the two samples.
            let state = vm.state()?;
            if state != sys::VIR_DOMAIN_RUNNING {
                return Ok(not_running(state));
            }
            return Err(e);
        }
    };
    if let Some(state) = second.uint("state.state")
        && state != u64::from(sys::VIR_DOMAIN_RUNNING)
    {
        return Ok(not_running(
            u32::try_from(state).unwrap_or(sys::VIR_DOMAIN_NOSTATE),
        ));
    }
    let mut out = figures(&first, &second, || vm.xml());
    let fs = if read_only {
        Err(FS_READ_ONLY.to_owned())
    } else {
        filesystems(agent)
    };
    match fs {
        Ok(list) => {
            out.insert("filesystems".into(), Value::Array(list));
            out.insert("filesystems_note".into(), json!(FS_CLAIM));
        }
        Err(why) => {
            out.insert("filesystems".into(), json!([]));
            out.insert("filesystems_missing".into(), json!(why));
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Channel;
    use std::cell::RefCell;

    fn sample(at: Instant, params: &[(&str, Param)]) -> Sample {
        Sample {
            taken: at,
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        }
    }

    fn u(v: u64) -> Param {
        Param::Uint(v)
    }

    fn t(s: &str) -> Param {
        Param::Text(s.into())
    }

    const XML_PERIOD: &str = "<domain><devices><memballoon model='virtio'><stats period='5'/></memballoon>\
        </devices></domain>";

    fn base(at: Instant, cpu_ns: u64, rd: u64, rx: u64) -> Sample {
        sample(
            at,
            &[
                ("state.state", u(1)),
                // QEMU's own threads count in cpu.time and must not count as guest use.
                ("cpu.time", u(cpu_ns.saturating_mul(3))),
                ("vcpu.current", u(2)),
                ("vcpu.maximum", u(2)),
                ("vcpu.0.time", u(cpu_ns)),
                ("vcpu.1.time", u(0)),
                ("balloon.maximum", u(1_048_576)),
                ("balloon.current", u(1_048_576)),
                ("balloon.rss", u(524_288)),
                ("block.count", u(1)),
                ("block.0.name", t("vda")),
                ("block.0.rd.bytes", u(rd)),
                ("block.0.wr.bytes", u(1000)),
                ("block.0.capacity", u(10_737_418_240)),
                ("block.0.allocation", u(2_000_000_000)),
                ("net.count", u(1)),
                ("net.0.name", t("vnet3")),
                ("net.0.rx.bytes", u(rx)),
                ("net.0.tx.bytes", u(500)),
                ("net.0.rx.drop", u(0)),
                ("net.0.tx.drop", u(2)),
            ],
        )
    }

    #[test]
    fn rates_come_from_two_fixed_samples() {
        let at = Instant::now();
        let first = base(at, 1_000_000_000, 4096, 100);
        // One second later: one vCPU's worth of a two-vCPU VM, 8 KiB read, 1000 bytes received.
        let second = base(
            at + Duration::from_secs(1),
            2_000_000_000,
            4096 + 8192,
            1100,
        );
        let out = figures(&first, &second, || Ok(XML_PERIOD.into()));
        assert_eq!(out["interval_ms"], 1000);
        assert_eq!(out["cpu"], json!({"percent": 50.0, "vcpus": 2}));
        assert_eq!(
            out["disks"],
            json!([{
                "target": "vda",
                "read_bytes_per_sec": 8192,
                "write_bytes_per_sec": 0,
                "capacity_bytes": 10_737_418_240_u64,
                "allocation_bytes": 2_000_000_000,
            }])
        );
        assert_eq!(
            out["interfaces"],
            json!([{
                "device": "vnet3",
                "received_bytes_per_sec": 1000,
                "sent_bytes_per_sec": 0,
                "received_drops": 0,
                "sent_drops": 2,
            }])
        );
        assert_eq!(out["memory"]["configured_bytes"], 1_073_741_824_u64);
        assert_eq!(out["memory"]["host_resident_bytes"], 536_870_912_u64);
    }

    #[test]
    fn cpu_use_stops_at_100_percent() {
        let at = Instant::now();
        // 2.1 s of vCPU time over 1 s on two vCPUs: jitter, not 105 %.
        let first = base(at, 0, 0, 0);
        let second = base(at + Duration::from_secs(1), 2_100_000_000, 0, 0);
        let out = figures(&first, &second, || Ok(XML_PERIOD.into()));
        assert_eq!(out["cpu"]["percent"], 100.0);
    }

    #[test]
    fn a_counter_that_resets_gives_no_rate() {
        let at = Instant::now();
        let first = base(at, 9_000_000_000, 1_000_000, 5000);
        let second = base(at + Duration::from_secs(1), 1_000_000, 10, 5);
        let out = figures(&first, &second, || Ok(XML_PERIOD.into()));
        assert_eq!(out["cpu"]["percent"], Value::Null);
        assert_eq!(out["disks"][0]["read_bytes_per_sec"], Value::Null);
        assert_eq!(out["interfaces"][0]["received_bytes_per_sec"], Value::Null);
    }

    #[test]
    fn devices_are_matched_by_name_not_index() {
        let at = Instant::now();
        let first = sample(
            at,
            &[
                ("block.count", u(2)),
                ("block.0.name", t("sda")),
                ("block.0.rd.bytes", u(0)),
                ("block.1.name", t("vda")),
                ("block.1.rd.bytes", u(1000)),
            ],
        );
        let second = sample(
            at + Duration::from_secs(2),
            &[
                ("block.count", u(1)),
                ("block.0.name", t("vda")),
                ("block.0.rd.bytes", u(3000)),
            ],
        );
        let out = figures(&first, &second, || Ok(XML_PERIOD.into()));
        assert_eq!(out["disks"][0]["target"], "vda");
        assert_eq!(out["disks"][0]["read_bytes_per_sec"], 1000);
    }

    #[test]
    fn guest_memory_figures_appear_when_the_balloon_reports_them() {
        let at = Instant::now();
        let mut second = base(at + Duration::from_secs(1), 1, 1, 1);
        second.params.insert("balloon.available".into(), u(1000));
        second.params.insert("balloon.unused".into(), u(10));
        let out = figures(&base(at, 0, 0, 0), &second, || panic!("no XML needed"));
        assert_eq!(
            out["memory"]["guest"],
            json!({"available_bytes": 1_024_000, "unused_bytes": 10_240})
        );
        assert!(out["memory"].get("guest_missing").is_none());
    }

    #[test]
    fn missing_memory_figures_name_one_of_three_causes() {
        let none = "<domain><devices><memballoon model='none'/></devices></domain>";
        let absent = "<domain><devices/></domain>";
        let no_period = "<domain><devices><memballoon model='virtio'/></devices></domain>";
        let zero = "<domain><devices><memballoon model='virtio'><stats period='0'/></memballoon></devices></domain>";
        assert!(memory_missing_reason(none).contains("no memory balloon device"));
        assert!(memory_missing_reason(absent).contains("no memory balloon device"));
        assert!(memory_missing_reason(no_period).contains("no stats period"));
        assert!(memory_missing_reason(zero).contains("no stats period"));
        assert!(memory_missing_reason(XML_PERIOD).contains("virtio balloon driver"));
        let at = Instant::now();
        let out = figures(&base(at, 0, 0, 0), &base(at, 0, 0, 0), || Ok(none.into()));
        assert!(
            out["memory"]["guest_missing"]
                .as_str()
                .unwrap()
                .contains("no memory balloon device")
        );
    }

    /// A VM that hands out scripted samples and states.
    struct FakeVm {
        states: RefCell<Vec<u32>>,
        samples: RefCell<Vec<Result<Sample>>>,
    }

    impl FakeVm {
        fn running(samples: Vec<Result<Sample>>) -> Self {
            Self {
                states: RefCell::new(vec![sys::VIR_DOMAIN_RUNNING]),
                samples: RefCell::new(samples),
            }
        }
    }

    impl StatsVm for FakeVm {
        fn state(&self) -> Result<u32> {
            let mut states = self.states.borrow_mut();
            Ok(if states.len() > 1 {
                states.remove(0)
            } else {
                states[0]
            })
        }

        fn sample(&self) -> Result<Sample> {
            self.samples.borrow_mut().remove(0)
        }

        fn xml(&self) -> Result<String> {
            Ok(XML_PERIOD.into())
        }
    }

    /// A guest agent that fails the test on any call.
    struct NoCalls;

    impl Channel for NoCalls {
        fn command(&self, request: &str, _: i32) -> std::result::Result<String, String> {
            panic!("read-only mode sent a guest-agent query: {request}");
        }
    }

    /// A guest agent that answers `guest-get-fsinfo`.
    struct FsGuest(&'static str);

    impl Channel for FsGuest {
        fn command(&self, request: &str, _: i32) -> std::result::Result<String, String> {
            assert!(request.contains("guest-get-fsinfo"), "{request}");
            Ok(self.0.into())
        }
    }

    fn two_samples() -> Vec<Result<Sample>> {
        let at = Instant::now();
        vec![
            Ok(base(at, 0, 0, 0)),
            Ok(base(at + Duration::from_secs(1), 1, 1, 1)),
        ]
    }

    #[test]
    fn read_only_mode_makes_no_agent_call() {
        let vm = FakeVm::running(two_samples());
        let out = report(&vm, &Agent::new(&NoCalls, 5), true, Duration::ZERO).unwrap();
        assert_eq!(out["filesystems"], json!([]));
        assert_eq!(out["filesystems_missing"], FS_READ_ONLY);
        assert_eq!(out["state"], "running");
    }

    #[test]
    fn filesystems_come_from_the_agent_and_are_marked_as_its_claim() {
        let vm = FakeVm::running(two_samples());
        let guest = FsGuest(
            r#"{"return":[{"name":"vda1","mountpoint":"/","type":"ext4","total-bytes":1000,"used-bytes":400,"disk":[]}]}"#,
        );
        let out = report(&vm, &Agent::new(&guest, 5), false, Duration::ZERO).unwrap();
        assert_eq!(
            out["filesystems"],
            json!([{"mountpoint": "/", "type": "ext4", "total_bytes": 1000, "used_bytes": 400}])
        );
        assert_eq!(out["filesystems_note"], FS_CLAIM);
    }

    #[test]
    fn a_silent_agent_leaves_the_rest_and_says_why() {
        struct Silent;
        impl Channel for Silent {
            fn command(&self, _: &str, _: i32) -> std::result::Result<String, String> {
                Err("Guest agent is not responding".into())
            }
        }
        let vm = FakeVm::running(two_samples());
        let out = report(&vm, &Agent::new(&Silent, 5), false, Duration::ZERO).unwrap();
        assert!(
            out["filesystems_missing"]
                .as_str()
                .unwrap()
                .contains("not responding")
        );
        assert_eq!(out["cpu"]["vcpus"], 2);
    }

    #[test]
    fn a_stopped_vm_replies_with_its_state() {
        let vm = FakeVm {
            states: RefCell::new(vec![sys::VIR_DOMAIN_SHUTOFF]),
            samples: RefCell::new(Vec::new()),
        };
        let out = report(&vm, &Agent::new(&NoCalls, 5), false, Duration::ZERO).unwrap();
        assert_eq!(out["state"], "shutoff");
        assert!(out.get("cpu").is_none());
    }

    #[test]
    fn a_second_sample_of_a_stopped_vm_gives_its_state() {
        let at = Instant::now();
        let mut stopped = base(at + Duration::from_secs(1), 0, 0, 0);
        stopped.params.insert("state.state".into(), u(5));
        let vm = FakeVm::running(vec![Ok(base(at, 0, 0, 0)), Ok(stopped)]);
        let out = report(&vm, &Agent::new(&NoCalls, 5), false, Duration::ZERO).unwrap();
        assert_eq!(out["state"], "shutoff");
    }

    #[test]
    fn a_vm_that_stops_between_samples_replies_with_its_state() {
        let at = Instant::now();
        let vm = FakeVm {
            states: RefCell::new(vec![sys::VIR_DOMAIN_RUNNING, sys::VIR_DOMAIN_SHUTOFF]),
            samples: RefCell::new(vec![
                Ok(base(at, 0, 0, 0)),
                Err(anyhow::anyhow!("domain is not running")),
            ]),
        };
        let out = report(&vm, &Agent::new(&NoCalls, 5), false, Duration::ZERO).unwrap();
        assert_eq!(out["state"], "shutoff");
    }
}
