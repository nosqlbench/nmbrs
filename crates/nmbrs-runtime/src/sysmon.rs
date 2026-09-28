// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Session-level system-performance sampler.
//!
//! Enabled per session with `sysmon=<categories>` — `sysmon=all`,
//! `sysmon=any`, or a comma list of `cpu,io,ram,rambw,storage`. `all` and
//! `any` differ in exactly one way: `all` ABORTS when a subsystem is
//! unavailable, `any` enables what the host supports and NAMES what it
//! skipped — an explicit opt-in to best-effort, so the skip is announced
//! rather than silent. Reads `/proc` (and statvfs)
//! on a fixed interval (default 5 s, `sysmon-interval=<seconds>`) and
//! publishes host-side utilization as session-scoped gauges plus a live
//! observer callback for display surfaces: measurement of the machine
//! under the workload, not of the workload.
//!
//! The categories:
//!
//! - **cpu** — `/proc/stat`. Three separate measures: the MEAN utilization
//!   from the aggregate `cpu ` line, the MAXIMUM single-core saturation
//!   from the `cpuN` lines (with the core named), and the QUARTILES of the
//!   per-core distribution (`p25`/`p50`/`p75`). A pinned compaction thread
//!   can saturate one core while the mean reads 3% — both facts matter and
//!   neither substitutes for the other.
//!
//!   The quartiles exist because neither the mean nor the max answers "is
//!   there headroom". The max says 100% for a single hot thread on an idle
//!   box; the mean says 3% for a box where every core is at 3% and for a
//!   box with one pegged core, without distinguishing them. `p50` low with
//!   `max_core` pegged is one hot thread with headroom to spare; `p50` high
//!   is genuine saturation. Any adaptive guard that throttles on CPU wants
//!   the quartile, not the max. The per-core vector is already built to
//!   find the max, so the shape costs one sort per window.
//! - **io** — `/proc/diskstats`, every line. Utilization per device is
//!   `Δio_ticks / Δwall` (io_ticks is the 10th stat field: milliseconds the
//!   device had I/O in flight — the same quantity `iostat -x %util`
//!   reports). Only the HIGHEST device utilization is recorded, with the
//!   device named. On NVMe this saturates as an idle-detector rather than a
//!   capacity meter (dozens of requests run in parallel), which is exactly
//!   what a "is the disk the bottleneck" glance wants.
//! - **ram** — `/proc/meminfo`. Two separate measures:
//!   `(MemTotal − MemAvailable) / MemTotal` (committed: what is actually
//!   claimed and not readily reclaimable — the kernel's own estimate) and
//!   `(MemTotal − MemFree) / MemTotal` (everything, page cache included).
//!   On a database host the second sits near 100% by design; the pair reads
//!   as "how much is spoken for" vs "how much is touched".
//! - **rambw** — memory bandwidth via resctrl MBM
//!   (`/sys/fs/resctrl/mon_data`). REQUIRED-EXPLICIT when enabled: if the
//!   resctrl interface is not mounted (or `sysmon-membw-gbps=` is not set to
//!   provide the peak reference), the session ABORTS with instructions —
//!   never a silent skip. `sysmon=all` includes it, so `all` on a host
//!   without resctrl aborts; a host without it runs
//!   `sysmon=cpu,io,ram,storage`.
//! - **storage** — filesystem SPACE utilization: statvfs over every
//!   `/dev/`-backed mount in `/proc/mounts` (deduplicated by source device),
//!   `1 − available/total` per mount, only the highest recorded, mount
//!   point named. Space is the disk measure `io` cannot see: a device can
//!   be I/O-idle and one write from full.
//!
//! Counters are cumulative, so every rate-like utilization here is a
//! pairwise delta over the sample window; the published gauge is the latest
//! window's value and any windowing beyond that belongs to MetricsQL at
//! query time.

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use nmbrs_metrics::component::Component;
use nmbrs_metrics::instruments::gauge::ValueGauge;

/// Which categories a `sysmon=` setting enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Categories {
    pub cpu: bool,
    pub io: bool,
    pub ram: bool,
    pub rambw: bool,
    pub storage: bool,
}

impl Categories {
    pub const ALL: Categories = Categories {
        cpu: true,
        io: true,
        ram: true,
        rambw: true,
        storage: true,
    };

    pub fn any(&self) -> bool {
        self.cpu || self.io || self.ram || self.rambw || self.storage
    }
}

/// What a `sysmon=` value asked for: a fixed category set, or "whatever
/// this host supports".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Named categories (including `all`): every one is REQUIRED, and an
    /// unavailable subsystem aborts the session with instructions.
    Cats(Categories),
    /// `sysmon=any`: enable every available subsystem, skip-and-announce
    /// the rest. The stated opt-in is what makes the skip acceptable.
    Any,
}

/// Parse a `sysmon=` value: `all`, `any`, or a comma list of category
/// names. Unknown names are errors that NAME the valid set — a typo
/// silently monitoring nothing would be the failure mode this surface
/// exists to avoid.
pub fn parse_selection(value: &str) -> Result<Selection, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("any") {
        return Ok(Selection::Any);
    }
    if trimmed
        .to_ascii_lowercase()
        .split(',')
        .any(|t| t.trim() == "any")
    {
        return Err("sysmon: `any` stands alone (it means \"every available \
             subsystem\") — combining it with named categories is \
             ambiguous. Use `sysmon=any`, or name the categories."
            .to_string());
    }
    parse_categories(trimmed).map(Selection::Cats)
}

/// Parse a fixed category list (`all` or comma names). See
/// [`parse_selection`] for the `any` form.
pub fn parse_categories(value: &str) -> Result<Categories, String> {
    if value.trim().eq_ignore_ascii_case("all") {
        return Ok(Categories::ALL);
    }
    let mut cats = Categories::default();
    for token in value.split(',') {
        match token.trim().to_ascii_lowercase().as_str() {
            "cpu" => cats.cpu = true,
            "io" => cats.io = true,
            "ram" => cats.ram = true,
            "rambw" => cats.rambw = true,
            "storage" => cats.storage = true,
            "" => {}
            other => {
                return Err(format!(
                    "sysmon: unknown category '{other}'. Valid: all, or a \
                     comma list of cpu, io, ram, rambw, storage"
                ));
            }
        }
    }
    if !cats.any() {
        return Err(
            "sysmon: no categories enabled. Use `sysmon=all` or a comma \
             list of cpu, io, ram, rambw, storage"
                .to_string(),
        );
    }
    Ok(cats)
}

/// Per-category CPU readings: the mean, the hottest core, and the
/// SHAPE of the per-core distribution.
///
/// The hottest core alone cannot answer "is there headroom": one
/// pegged core on an otherwise idle box reads identically to a box
/// that is genuinely out of CPU. Measured 2026-08-21 on a live
/// `stcs_adaptive` run — `max_core` p90 = 1.000 while `mean` sat at
/// 0.183 — so any guard keyed on the max would throttle at 18% host
/// utilization. The quartiles separate the two cases: `p50` low with
/// `max_core` pegged is a single hot thread (headroom remains); `p50`
/// high is real saturation.
///
/// `max_core` is kept, not replaced — it remains the right signal for
/// spotting a single-threaded bottleneck, which is a different
/// question from headroom.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CpuReading {
    pub mean: f64,
    pub max_core: f64,
    pub top_core: usize,
    /// Per-core utilization quartiles across all online cores.
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    /// How many cores the quartiles were computed over — the sample
    /// size behind the shape, so a reader can tell a 2-core box's
    /// "quartiles" from a 96-core box's.
    pub cores: usize,
}

/// One completed sample window. Each field is `Some` exactly when its
/// category was enabled — a disabled category is absent, not zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SysmonSample {
    pub cpu: Option<CpuReading>,
    /// (device, utilization) — the busiest device this window.
    pub io: Option<(String, f64)>,
    /// (committed, everything-including-page-cache).
    pub ram: Option<(f64, f64)>,
    /// Memory-bandwidth utilization against the configured peak.
    pub rambw: Option<f64>,
    /// (mount point, space utilization) — the fullest filesystem.
    pub storage: Option<(String, f64)>,
}

/// Sampler configuration, resolved by the runner from session params.
#[derive(Debug, Clone)]
pub struct SysmonConfig {
    pub cats: Categories,
    /// Sample window. Default 5 s (`sysmon-interval=<seconds>`).
    pub interval: Duration,
    /// Peak memory bandwidth in bytes/s — the reference that turns rambw
    /// bytes/s into a utilization. Required when `rambw` is enabled.
    pub membw_peak_bytes_per_s: Option<f64>,
}

/// The rambw prerequisites, checked BEFORE the session starts so an
/// unsupported host aborts with instructions instead of silently monitoring
/// less than was asked for.
pub fn check_rambw_requirements(config: &SysmonConfig) -> Result<(), String> {
    if !config.cats.rambw {
        return Ok(());
    }
    if read_membw_bytes().is_none() {
        return Err("\
sysmon: rambw was requested, but the kernel resctrl interface is not \
available at /sys/fs/resctrl/mon_data.

To enable memory-bandwidth monitoring:
  1. The CPU must support bandwidth monitoring (Intel RDT / AMD QoS) —
     check for the `cqm_mbm_total` flag:  grep -m1 cqm_mbm_total /proc/cpuinfo
  2. The kernel must be built with CONFIG_X86_CPU_RESCTRL (standard on
     mainstream distro kernels).
  3. Mount the interface:  sudo mount -t resctrl resctrl /sys/fs/resctrl

If this host cannot support it (most VMs cannot), run without the rambw
category:  sysmon=cpu,io,ram,storage  — or use  sysmon=any  to enable
every subsystem this host supports."
            .to_string());
    }
    if config.membw_peak_bytes_per_s.is_none() {
        return Err("\
sysmon: rambw needs a peak-bandwidth reference to turn bytes/s into a \
utilization. Set it with  sysmon-membw-gbps=<peak>  (the host's rated \
memory bandwidth in GB/s), or run without the rambw category:  \
sysmon=cpu,io,ram,storage  — or use  sysmon=any"
            .to_string());
    }
    Ok(())
}

/// Resolve `sysmon=any` against THIS host: every category the host
/// supports, plus one human-readable line per category that had to be
/// skipped and why. cpu/io/ram/storage are /proc-backed and always
/// available on Linux; rambw carries real prerequisites.
///
/// The skip lines exist because `any` is best-effort, not silent-effort:
/// the runner logs each one, so a session that monitored four of five
/// categories says so.
pub fn resolve_any(config: &SysmonConfig) -> (Categories, Vec<String>) {
    let mut cats = Categories::ALL;
    let mut skipped = Vec::new();
    let rambw_probe = SysmonConfig {
        cats,
        ..config.clone()
    };
    if let Err(reason) = check_rambw_requirements(&rambw_probe) {
        cats.rambw = false;
        // First line of the full instructions — the one that names the
        // missing prerequisite. `sysmon=rambw` gets the complete text.
        let first = reason.lines().next().unwrap_or("unavailable").to_string();
        skipped.push(format!("{first} (run `sysmon=rambw` for the enable steps)"));
    }
    (cats, skipped)
}

// ---------------------------------------------------------------------------
// Pure parsers + delta math. Everything below reads STRINGS so the arithmetic
// is testable on fixtures; only the spawn loop touches the filesystem.
// ---------------------------------------------------------------------------

/// Per-device cumulative `io_ticks` (ms with I/O in flight) from
/// `/proc/diskstats`. Field layout per the kernel's Documentation/iostats:
/// `major minor name <17 stat fields>`; io_ticks is stat field 10, i.e.
/// whitespace token 12.
pub fn parse_diskstats(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|line| {
            let t: Vec<&str> = line.split_whitespace().collect();
            let name = t.get(2)?;
            let io_ticks: u64 = t.get(12)?.parse().ok()?;
            Some((name.to_string(), io_ticks))
        })
        .collect()
}

/// The highest per-device utilization between two diskstats snapshots taken
/// `dt_ms` apart. Devices present in only one snapshot are skipped (hotplug
/// between samples); an empty intersection yields `None`.
pub fn max_disk_util(
    prev: &[(String, u64)],
    cur: &[(String, u64)],
    dt_ms: f64,
) -> Option<(String, f64)> {
    if dt_ms <= 0.0 {
        return None;
    }
    let mut best: Option<(String, f64)> = None;
    for (name, cur_ticks) in cur {
        let Some((_, prev_ticks)) = prev.iter().find(|(n, _)| n == name) else {
            continue;
        };
        let util = (cur_ticks.saturating_sub(*prev_ticks) as f64 / dt_ms).clamp(0.0, 1.0);
        if best.as_ref().is_none_or(|(_, b)| util > *b) {
            best = Some((name.clone(), util));
        }
    }
    best
}

/// Cumulative jiffy counts for one `cpu` line: (busy, total).
///
/// total = user+nice+system+idle+iowait+irq+softirq+steal (the first eight
/// fields; guest time is already accounted inside user/nice). busy = total −
/// idle − iowait: iowait is idle-with-an-excuse, and counting it as busy
/// would read an I/O-bound stall as CPU saturation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTicks {
    pub busy: u64,
    pub total: u64,
}

/// Aggregate + per-core cumulative ticks from `/proc/stat`.
pub fn parse_proc_stat(text: &str) -> Option<(CpuTicks, Vec<CpuTicks>)> {
    let mut aggregate: Option<CpuTicks> = None;
    let mut cores: Vec<(usize, CpuTicks)> = Vec::new();
    for line in text.lines() {
        let mut t = line.split_whitespace();
        let Some(name) = t.next() else { continue };
        if !name.starts_with("cpu") {
            continue;
        }
        let fields: Vec<u64> = t.filter_map(|f| f.parse().ok()).collect();
        if fields.len() < 8 {
            continue;
        }
        let total: u64 = fields[..8].iter().sum();
        let idle = fields[3] + fields[4];
        let ticks = CpuTicks {
            busy: total - idle,
            total,
        };
        if name == "cpu" {
            aggregate = Some(ticks);
        } else if let Ok(n) = name[3..].parse::<usize>() {
            cores.push((n, ticks));
        }
    }
    cores.sort_by_key(|(n, _)| *n);
    aggregate.map(|a| (a, cores.into_iter().map(|(_, t)| t).collect()))
}

/// Utilization between two tick snapshots: Δbusy / Δtotal.
pub fn cpu_util(prev: CpuTicks, cur: CpuTicks) -> f64 {
    let dt = cur.total.saturating_sub(prev.total);
    if dt == 0 {
        return 0.0;
    }
    (cur.busy.saturating_sub(prev.busy) as f64 / dt as f64).clamp(0.0, 1.0)
}

/// The most saturated core between two snapshots. Core lists of different
/// lengths (offline/online between samples) compare over the shared prefix.
pub fn max_core_util(prev: &[CpuTicks], cur: &[CpuTicks]) -> Option<(usize, f64)> {
    prev.iter()
        .zip(cur.iter())
        .enumerate()
        .map(|(i, (p, c))| (i, cpu_util(*p, *c)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Per-core utilization quartiles between two snapshots: `(p25, p50,
/// p75, cores)`. Same shared-prefix rule as [`max_core_util`].
///
/// The per-core vector is already built to find the max; the quartiles
/// are a sort of that same vector, so the shape costs one sort per
/// sample window and no extra reads of `/proc/stat`.
///
/// Nearest-rank on the sorted ascending vector (no interpolation): the
/// quantile of a core count is a real core's utilization, which is the
/// honest reading when the population is small — an interpolated "p75"
/// across 4 cores would name a utilization no core actually had.
pub fn core_util_quartiles(prev: &[CpuTicks], cur: &[CpuTicks]) -> Option<(f64, f64, f64, usize)> {
    let mut utils: Vec<f64> = prev
        .iter()
        .zip(cur.iter())
        .map(|(p, c)| cpu_util(*p, *c))
        .collect();
    if utils.is_empty() {
        return None;
    }
    utils.sort_by(f64::total_cmp);
    let n = utils.len();
    let at = |q: f64| -> f64 {
        // Nearest-rank, clamped: ceil(q*n) - 1 over 1-based ranks.
        let rank = (q * n as f64).ceil().max(1.0) as usize;
        utils[rank.min(n) - 1]
    };
    Some((at(0.25), at(0.50), at(0.75), n))
}

/// The three `/proc/meminfo` fields the two utilization measures need,
/// in kB as the kernel reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kb: u64,
    pub free_kb: u64,
    pub available_kb: u64,
}

pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut total = None;
    let mut free = None;
    let mut available = None;
    for line in text.lines() {
        let mut t = line.split_whitespace();
        match t.next() {
            Some("MemTotal:") => total = t.next()?.parse().ok(),
            Some("MemFree:") => free = t.next()?.parse().ok(),
            Some("MemAvailable:") => available = t.next()?.parse().ok(),
            _ => {}
        }
    }
    Some(MemInfo {
        total_kb: total?,
        free_kb: free?,
        available_kb: available?,
    })
}

/// The two memory measures: (committed, everything-including-page-cache).
pub fn mem_utils(m: MemInfo) -> (f64, f64) {
    if m.total_kb == 0 {
        return (0.0, 0.0);
    }
    let committed = (m.total_kb.saturating_sub(m.available_kb)) as f64 / m.total_kb as f64;
    let cached = (m.total_kb.saturating_sub(m.free_kb)) as f64 / m.total_kb as f64;
    (committed.clamp(0.0, 1.0), cached.clamp(0.0, 1.0))
}

/// WRITABLE `/dev/`-backed mount points from `/proc/mounts`, deduplicated by
/// source device (bind mounts and btrfs subvolumes re-list one device many
/// times; space is a per-DEVICE fact).
///
/// Read-only mounts are excluded, and it matters: snap images are
/// `/dev/loop*` squashfs mounts that are 100% full BY CONSTRUCTION, so one
/// installed snap would pin the storage item at bright-orange forever.
/// Verified on this host — `/dev/loop0 /snap/... ro,...` at 100% while the
/// fullest writable filesystem sat at 40%. A read-only filesystem cannot
/// fill up, so its fullness is not a utilization.
pub fn parse_dev_mounts(text: &str) -> Vec<String> {
    let mut seen_sources: Vec<&str> = Vec::new();
    let mut mounts = Vec::new();
    for line in text.lines() {
        let mut t = line.split_whitespace();
        let (Some(source), Some(mount), _fstype, Some(options)) =
            (t.next(), t.next(), t.next(), t.next())
        else {
            continue;
        };
        if !source.starts_with("/dev/") || seen_sources.contains(&source) {
            continue;
        }
        let read_only = options.split(',').any(|o| o == "ro");
        if read_only {
            continue;
        }
        seen_sources.push(source);
        // /proc/mounts octal-escapes spaces in mount points (\040).
        mounts.push(mount.replace("\\040", " "));
    }
    mounts
}

/// Space utilization of one filesystem: `1 − available/total`, matching what
/// `df` calls Use%. `None` on statvfs failure or a zero-block pseudo-fs.
/// Unix-only, like its caller [`max_storage_util`] (which walks
/// `/proc/mounts` and therefore never yields a mount elsewhere).
#[cfg(unix)]
fn statvfs_util(mount: &str) -> Option<f64> {
    let c_mount = std::ffi::CString::new(mount).ok()?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_mount.as_ptr(), &mut vfs) } != 0 {
        return None;
    }
    if vfs.f_blocks == 0 {
        return None;
    }
    Some((1.0 - vfs.f_bavail as f64 / vfs.f_blocks as f64).clamp(0.0, 1.0))
}

/// The fullest `/dev/`-backed filesystem right now: (mount point, util).
fn max_storage_util() -> Option<(String, f64)> {
    #[cfg(unix)]
    {
        let mounts = std::fs::read_to_string("/proc/mounts")
            .map(|t| parse_dev_mounts(&t))
            .unwrap_or_default();
        mounts
            .into_iter()
            .filter_map(|m| statvfs_util(&m).map(|u| (m, u)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }
    // No `/proc/mounts` off Unix — storage-utilization sampling
    // is simply unavailable there.
    #[cfg(not(unix))]
    {
        None
    }
}

/// Sum of resctrl MBM total-bytes counters across mon_data groups, when the
/// resctrl filesystem is mounted with monitoring. `None` when unavailable.
fn read_membw_bytes() -> Option<u64> {
    let root = std::path::Path::new("/sys/fs/resctrl/mon_data");
    let entries = std::fs::read_dir(root).ok()?;
    let mut sum: u64 = 0;
    let mut seen = false;
    for e in entries.flatten() {
        let f = e.path().join("mbm_total_bytes");
        if let Ok(text) = std::fs::read_to_string(&f)
            && let Ok(v) = text.trim().parse::<u64>()
        {
            sum += v;
            seen = true;
        }
    }
    seen.then_some(sum)
}

// ---------------------------------------------------------------------------
// The sampler task.
// ---------------------------------------------------------------------------

/// A gauge family whose series are one-per-SUBJECT — the device, core, or
/// mount the measurement is about. Each distinct subject value materialises
/// a dimensional cell under the session component
/// ([`nmbrs_metrics::cells::resolve_under`]) and registers the family there
/// once; after that a sample is a hash lookup and a `set`.
///
/// This is what "submitted to metrics with the appropriate dimensional
/// labels, keyed by the session component" means concretely:
/// `sysmon_io_util{session="…",device="nvme1n1"}` — the subject is a label,
/// the cell refines the session's identity, and a sweep that shifts between
/// devices yields one honestly-labeled series per device rather than one
/// anonymous series whose subject silently changes.
struct SubjectGauge {
    family: &'static str,
    /// The dimension this family's subject occupies (`device`, `core`,
    /// `mount`).
    label_key: &'static str,
    parent: Arc<RwLock<Component>>,
    /// Instruments already materialised, by subject value.
    instances: std::collections::HashMap<String, Arc<ValueGauge>>,
}

impl SubjectGauge {
    fn new(family: &'static str, label_key: &'static str, parent: Arc<RwLock<Component>>) -> Self {
        Self {
            family,
            label_key,
            parent,
            instances: Default::default(),
        }
    }

    fn set(&mut self, subject: &str, value: f64) {
        if let Some(g) = self.instances.get(subject) {
            g.set(value);
            return;
        }
        let coord = nmbrs_metrics::labels::Labels::of(self.label_key, subject);
        let cell = nmbrs_metrics::cells::resolve_under(&self.parent, &coord);
        let labels = {
            let guard = cell.read().unwrap_or_else(|e| e.into_inner());
            guard.effective_labels().clone()
        };
        let g = Arc::new(ValueGauge::new(labels.with("family", self.family)));
        let registered = cell
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .register_instrument(
                self.family,
                nmbrs_metrics::component::InstrumentRef::Gauge(g.clone()),
            );
        if let Err(e) = registered {
            // One cell, one family: a second registration here is a
            // programming error worth hearing about once, not per sample.
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "sysmon: {family} cell {subject}: {e}",
                family = self.family
            );
        }
        g.set(value);
        self.instances.insert(subject.to_string(), g);
    }
}

struct Gauges {
    /// Host-scalar measures — no subject, so they live on the session root
    /// itself and carry exactly its labels.
    cpu_mean: Option<Arc<ValueGauge>>,
    /// Per-core distribution shape. Plain (not subject-dimensioned)
    /// gauges: they describe the HOST, the same dimensional cell as
    /// `cpu_mean`. Fanning them out per core — as `cpu_core_max` does
    /// via its `core` label — would scatter one number across as many
    /// instances as the box has cores and make aggregation a join.
    cpu_core_p25: Option<Arc<ValueGauge>>,
    cpu_core_p50: Option<Arc<ValueGauge>>,
    cpu_core_p75: Option<Arc<ValueGauge>>,
    ram_committed: Option<Arc<ValueGauge>>,
    ram_cached: Option<Arc<ValueGauge>>,
    rambw: Option<Arc<ValueGauge>>,
    /// Subject-dimensioned measures — one cell per device / core / mount.
    io: Option<SubjectGauge>,
    cpu_core_max: Option<SubjectGauge>,
    storage: Option<SubjectGauge>,
}

/// Register gauges for the ENABLED categories on the session component.
/// Direct registration on the session root — no child component, no new
/// labels: the samples describe the whole host, which is exactly the
/// session's dimensional cell.
fn register_gauges(component: &Arc<RwLock<Component>>, cats: Categories) -> Result<Gauges, String> {
    let mut guard = component.write().unwrap_or_else(|e| e.into_inner());
    let labels = guard.effective_labels().clone();
    let mut mk = |family: &str| -> Result<Option<Arc<ValueGauge>>, String> {
        let g = Arc::new(ValueGauge::new(labels.with("family", family)));
        guard.register_instrument(
            family,
            nmbrs_metrics::component::InstrumentRef::Gauge(g.clone()),
        )?;
        Ok(Some(g))
    };
    let mut gauges = Gauges {
        cpu_mean: None,
        cpu_core_p25: None,
        cpu_core_p50: None,
        cpu_core_p75: None,
        ram_committed: None,
        ram_cached: None,
        rambw: None,
        io: None,
        cpu_core_max: None,
        storage: None,
    };
    if cats.cpu {
        gauges.cpu_mean = mk("sysmon_cpu_util")?;
        gauges.cpu_core_p25 = mk("sysmon_cpu_core_p25")?;
        gauges.cpu_core_p50 = mk("sysmon_cpu_core_p50")?;
        gauges.cpu_core_p75 = mk("sysmon_cpu_core_p75")?;
    }
    if cats.ram {
        gauges.ram_committed = mk("sysmon_ram_util")?;
        gauges.ram_cached = mk("sysmon_ram_util_cached")?;
    }
    if cats.rambw {
        gauges.rambw = mk("sysmon_rambw_util")?;
    }
    drop(guard);
    // Subject-dimensioned families register per cell at first sight of each
    // subject, NOT here — registering on the root as well would claim the
    // family for the un-refined identity and collide with the first cell.
    if cats.io {
        gauges.io = Some(SubjectGauge::new(
            "sysmon_io_util",
            "device",
            component.clone(),
        ));
    }
    if cats.cpu {
        gauges.cpu_core_max = Some(SubjectGauge::new(
            "sysmon_cpu_core_max",
            "core",
            component.clone(),
        ));
    }
    if cats.storage {
        gauges.storage = Some(SubjectGauge::new(
            "sysmon_storage_util",
            "mount",
            component.clone(),
        ));
    }
    Ok(gauges)
}

/// Spawn the sampler. `check_rambw_requirements` must have passed first —
/// the runner aborts the session on its Err rather than calling this.
/// Runs until session shutdown; publishes each window to the session gauges
/// and to `observer.sysmon_update`.
pub fn spawn(
    config: SysmonConfig,
    component: Arc<RwLock<Component>>,
    observer: Arc<dyn crate::observer::RunObserver>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    check_rambw_requirements(&config)?;
    let mut gauges = register_gauges(&component, config.cats)?;
    let cats = config.cats;

    let mut shutdown = crate::session_signals::subscribe_shutdown();
    Ok(tokio::spawn(async move {
        let mut prev_disks = cats
            .io
            .then(|| {
                std::fs::read_to_string("/proc/diskstats")
                    .map(|t| parse_diskstats(&t))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let mut prev_cpu = if cats.cpu {
            std::fs::read_to_string("/proc/stat")
                .ok()
                .and_then(|t| parse_proc_stat(&t))
        } else {
            None
        };
        let mut prev_membw = if cats.rambw { read_membw_bytes() } else { None };
        let mut prev_at = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(config.interval) => {}
                _ = shutdown.changed() => break,
            }
            let now = std::time::Instant::now();
            let dt = now.duration_since(prev_at);
            let dt_ms = dt.as_secs_f64() * 1000.0;
            prev_at = now;
            let mut sample = SysmonSample::default();

            if cats.io {
                let cur = std::fs::read_to_string("/proc/diskstats")
                    .map(|t| parse_diskstats(&t))
                    .unwrap_or_default();
                sample.io = max_disk_util(&prev_disks, &cur, dt_ms);
                prev_disks = cur;
            }
            if cats.cpu {
                let cur = std::fs::read_to_string("/proc/stat")
                    .ok()
                    .and_then(|t| parse_proc_stat(&t));
                if let (Some((pa, pc)), Some((ca, cc))) = (&prev_cpu, &cur) {
                    let (top_core, max_core) = max_core_util(pc, cc).unwrap_or((0, 0.0));
                    let (p25, p50, p75, cores) =
                        core_util_quartiles(pc, cc).unwrap_or((0.0, 0.0, 0.0, 0));
                    sample.cpu = Some(CpuReading {
                        mean: cpu_util(*pa, *ca),
                        max_core,
                        top_core,
                        p25,
                        p50,
                        p75,
                        cores,
                    });
                }
                prev_cpu = cur;
            }
            if cats.ram {
                sample.ram = std::fs::read_to_string("/proc/meminfo")
                    .ok()
                    .and_then(|t| parse_meminfo(&t))
                    .map(mem_utils);
            }
            if cats.rambw
                && let Some(peak) = config.membw_peak_bytes_per_s
            {
                let cur = read_membw_bytes();
                if let (Some(p), Some(c)) = (prev_membw, cur) {
                    sample.rambw = Some(
                        ((c.saturating_sub(p)) as f64 / dt.as_secs_f64().max(1e-9) / peak)
                            .clamp(0.0, 1.0),
                    );
                }
                prev_membw = cur;
            }
            if cats.storage {
                sample.storage = max_storage_util();
            }

            if let (Some(g), Some((dev, u))) = (&mut gauges.io, &sample.io) {
                g.set(dev, *u);
            }
            if let (Some(g), Some(c)) = (&gauges.cpu_mean, &sample.cpu) {
                g.set(c.mean);
            }
            if let (Some(g), Some(c)) = (&gauges.cpu_core_p25, &sample.cpu) {
                g.set(c.p25);
            }
            if let (Some(g), Some(c)) = (&gauges.cpu_core_p50, &sample.cpu) {
                g.set(c.p50);
            }
            if let (Some(g), Some(c)) = (&gauges.cpu_core_p75, &sample.cpu) {
                g.set(c.p75);
            }
            if let (Some(g), Some(c)) = (&mut gauges.cpu_core_max, &sample.cpu) {
                g.set(&c.top_core.to_string(), c.max_core);
            }
            if let (Some(g), Some((committed, _))) = (&gauges.ram_committed, &sample.ram) {
                g.set(*committed);
            }
            if let (Some(g), Some((_, cached))) = (&gauges.ram_cached, &sample.ram) {
                g.set(*cached);
            }
            if let (Some(g), Some(u)) = (&gauges.rambw, sample.rambw) {
                g.set(u);
            }
            if let (Some(g), Some((mount, u))) = (&mut gauges.storage, &sample.storage) {
                g.set(mount, *u);
            }

            observer.sysmon_update(&sample);
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `all` and the full comma list mean the same thing — the user's words.
    #[test]
    fn all_equals_the_full_category_list() {
        assert_eq!(parse_categories("all").unwrap(), Categories::ALL);
        assert_eq!(
            parse_categories("cpu,io,ram,rambw,storage").unwrap(),
            Categories::ALL
        );
    }

    #[test]
    fn category_subsets_parse_and_typos_are_named_errors() {
        let c = parse_categories("cpu, io").unwrap();
        assert!(c.cpu && c.io && !c.ram && !c.rambw && !c.storage);
        let err = parse_categories("cpu,ramb").unwrap_err();
        assert!(
            err.contains("ramb") && err.contains("rambw"),
            "the error names the typo and the valid set: {err}"
        );
        assert!(parse_categories("").is_err(), "empty enables nothing");
    }

    /// rambw enabled on a host without resctrl is an ABORT with instructions,
    /// not a skip. (This box has no resctrl, so this exercises the real
    /// probe.)
    #[test]
    fn rambw_without_resctrl_aborts_with_instructions() {
        let config = SysmonConfig {
            cats: parse_categories("rambw").unwrap(),
            interval: Duration::from_secs(5),
            membw_peak_bytes_per_s: Some(100e9),
        };
        if std::path::Path::new("/sys/fs/resctrl/mon_data").exists() {
            // Host actually has resctrl — the gate passes instead; nothing
            // to assert about instructions here.
            return;
        }
        let err = check_rambw_requirements(&config).unwrap_err();
        assert!(
            err.contains("mount -t resctrl"),
            "the abort must tell the user HOW to enable it: {err}"
        );
        assert!(
            err.contains("sysmon=cpu,io,ram,storage"),
            "…and how to run without it: {err}"
        );
    }

    /// rambw with resctrl but no peak reference is equally an abort — a
    /// bytes/s figure with no denominator is not a utilization.
    #[test]
    fn rambw_without_a_peak_reference_aborts() {
        let config = SysmonConfig {
            cats: parse_categories("rambw").unwrap(),
            interval: Duration::from_secs(5),
            membw_peak_bytes_per_s: None,
        };
        let err = check_rambw_requirements(&config).unwrap_err();
        assert!(
            err.contains("sysmon-membw-gbps") || err.contains("resctrl"),
            "must name the missing prerequisite: {err}"
        );
    }

    /// Disabled rambw asks nothing of the host.
    #[test]
    fn no_rambw_no_requirements() {
        let config = SysmonConfig {
            cats: parse_categories("cpu,io,ram,storage").unwrap(),
            interval: Duration::from_secs(5),
            membw_peak_bytes_per_s: None,
        };
        assert!(check_rambw_requirements(&config).is_ok());
    }

    /// `any` stands alone; combined with names it is ambiguous and refused.
    #[test]
    fn any_parses_alone_and_refuses_combination() {
        assert_eq!(parse_selection("any").unwrap(), Selection::Any);
        assert_eq!(parse_selection("ANY").unwrap(), Selection::Any);
        assert!(parse_selection("any,cpu").is_err());
        assert!(matches!(parse_selection("all").unwrap(),
            Selection::Cats(c) if c == Categories::ALL));
    }

    /// On a host without resctrl, `any` yields everything but rambw and
    /// SAYS SO; the skip line points at the full instructions. (This box
    /// has no resctrl; a host that has it passes the gate instead.)
    #[test]
    fn any_downgrades_rambw_with_an_announced_reason() {
        let config = SysmonConfig {
            cats: Categories::ALL,
            interval: Duration::from_secs(5),
            membw_peak_bytes_per_s: None,
        };
        let (cats, skipped) = resolve_any(&config);
        assert!(cats.cpu && cats.io && cats.ram && cats.storage);
        if std::path::Path::new("/sys/fs/resctrl/mon_data").exists() {
            return; // host genuinely supports it; nothing to skip here
        }
        assert!(!cats.rambw, "unavailable rambw is disabled under `any`");
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].contains("sysmon=rambw"),
            "the skip points at the full enable steps: {}",
            skipped[0]
        );
    }

    /// A subject-dimensioned gauge materialises ONE cell per subject under
    /// the session component, each series carrying the subject as a label —
    /// and re-setting an existing subject attaches no twin.
    #[test]
    fn subject_gauges_dimension_by_cell_under_the_session_component() {
        let session = Arc::new(RwLock::new(nmbrs_metrics::component::Component::new(
            nmbrs_metrics::labels::Labels::of("session", "s1"),
            std::collections::HashMap::new(),
        )));
        let mut g = SubjectGauge::new("sysmon_io_util", "device", session.clone());
        g.set("nvme1n1", 0.97);
        g.set("nvme2n1", 0.40);
        g.set("nvme1n1", 0.98);

        let guard = session.read().unwrap();
        assert_eq!(
            guard.child_count(),
            2,
            "one cell per device, re-sets attach no twin"
        );
        drop(guard);
        assert_eq!(g.instances.len(), 2);
        // The series carries session + device + family — the session's
        // identity refined by the subject, never replaced.
        let labels = g.instances["nvme1n1"].labels().to_prometheus();
        for owned in ["session=", "device=\"nvme1n1\"", "family="] {
            assert!(labels.contains(owned), "{owned} missing from {labels}");
        }
    }

    /// Real lines from this host's /proc/diskstats — io_ticks is token 12.
    #[test]
    fn diskstats_reads_io_ticks_from_the_tenth_stat_field() {
        let text = "\
 259       0 nvme0n1 1083809 77036 79071293 1310486 4905083 3299234 633830668 26973654 0 3344862 28284141 0 0 0 0 0 0
 259       5 nvme1n1 16478852962 684755 139660303464 4041025697 191545776 3994164 30038357911 3695409309 0 588447592 3442404703 29888 5913 38808742648 936992 0 0";
        let parsed = parse_diskstats(text);
        assert_eq!(
            parsed,
            vec![
                ("nvme0n1".to_string(), 3_344_862),
                ("nvme1n1".to_string(), 588_447_592),
            ]
        );
    }

    /// Highest utilization wins and is named; a device busy 1000ms of a
    /// 2000ms window is 50%.
    #[test]
    fn disk_util_is_the_max_across_devices() {
        let prev = vec![("a".to_string(), 1000_u64), ("b".to_string(), 5000)];
        let cur = vec![("a".to_string(), 1400), ("b".to_string(), 6000)];
        let (name, util) = max_disk_util(&prev, &cur, 2000.0).unwrap();
        assert_eq!(name, "b");
        assert!((util - 0.5).abs() < 1e-9);
    }

    /// Mean and max-core are separate measures: one saturated core among
    /// idle ones must not disappear into the mean.
    #[test]
    fn one_hot_core_shows_in_max_not_mean() {
        let stat_t0 = "\
cpu  1000 0 0 8000 0 0 0 0 0 0
cpu0 1000 0 0 0 0 0 0 0 0 0
cpu1 0 0 0 4000 0 0 0 0 0 0
cpu2 0 0 0 4000 0 0 0 0 0 0";
        let stat_t1 = "\
cpu  2000 0 0 16000 0 0 0 0 0 0
cpu0 2000 0 0 0 0 0 0 0 0 0
cpu1 0 0 0 8000 0 0 0 0 0 0
cpu2 0 0 0 8000 0 0 0 0 0 0";
        let (a0, c0) = parse_proc_stat(stat_t0).unwrap();
        let (a1, c1) = parse_proc_stat(stat_t1).unwrap();
        let mean = cpu_util(a0, a1);
        let (core, max) = max_core_util(&c0, &c1).unwrap();
        assert!((mean - 1000.0 / 9000.0).abs() < 1e-9, "mean {mean}");
        assert_eq!(core, 0);
        assert!((max - 1.0).abs() < 1e-9, "core0 fully busy, got {max}");

        // The quartiles are what separate THIS box — one pegged core, two
        // idle — from a box where every core is genuinely busy. Both read
        // max_core = 1.0; only the median tells them apart.
        let (p25, p50, p75, cores) = core_util_quartiles(&c0, &c1).unwrap();
        assert_eq!(cores, 3);
        assert!((p25 - 0.0).abs() < 1e-9, "p25 {p25}");
        assert!((p50 - 0.0).abs() < 1e-9, "median core is IDLE, got {p50}");
        assert!((p75 - 1.0).abs() < 1e-9, "p75 {p75}");
    }

    /// The saturation case the quartiles must NOT confuse with one hot
    /// core: every core busy reads max_core = 1.0 just like a single
    /// pegged core, but the median moves with it.
    #[test]
    fn quartiles_separate_broad_saturation_from_one_hot_core() {
        let stat_t0 = "\
cpu  0 0 0 12000 0 0 0 0 0 0
cpu0 0 0 0 4000 0 0 0 0 0 0
cpu1 0 0 0 4000 0 0 0 0 0 0
cpu2 0 0 0 4000 0 0 0 0 0 0";
        let stat_t1 = "\
cpu  12000 0 0 12000 0 0 0 0 0 0
cpu0 4000 0 0 4000 0 0 0 0 0 0
cpu1 4000 0 0 4000 0 0 0 0 0 0
cpu2 4000 0 0 4000 0 0 0 0 0 0";
        let (_, c0) = parse_proc_stat(stat_t0).unwrap();
        let (_, c1) = parse_proc_stat(stat_t1).unwrap();
        let (_, max) = max_core_util(&c0, &c1).unwrap();
        let (p25, p50, p75, cores) = core_util_quartiles(&c0, &c1).unwrap();
        assert_eq!(cores, 3);
        // EXACTLY the max the one-hot-core case reports — 1.0 there, 1.0
        // here. On the max alone the two boxes are indistinguishable.
        assert!((max - 1.0).abs() < 1e-9, "max {max}");
        // The median is what tells them apart: 0.0 when one core is hot
        // and the rest idle, 1.0 when the whole box is saturated.
        assert!((p25 - 1.0).abs() < 1e-9, "p25 {p25}");
        assert!((p50 - 1.0).abs() < 1e-9, "p50 {p50}");
        assert!((p75 - 1.0).abs() < 1e-9, "p75 {p75}");
    }

    /// Nearest-rank, not interpolation: every quartile of a small core
    /// count must be a utilization some real core actually had.
    #[test]
    fn quartiles_are_nearest_rank_over_real_cores() {
        let single = vec![CpuTicks {
            busy: 0,
            total: 1000,
        }];
        let single_end = vec![CpuTicks {
            busy: 250,
            total: 2000,
        }];
        let (p25, p50, p75, cores) = core_util_quartiles(&single, &single_end).unwrap();
        assert_eq!(cores, 1);
        // One core: every quantile is that core.
        assert!((p25 - 0.25).abs() < 1e-9);
        assert!((p50 - 0.25).abs() < 1e-9);
        assert!((p75 - 0.25).abs() < 1e-9);
        // No cores at all is None, not a fabricated zero.
        assert!(core_util_quartiles(&[], &[]).is_none());
    }

    /// iowait is idle-with-an-excuse: an I/O-bound stall must not read as
    /// CPU saturation.
    #[test]
    fn iowait_does_not_count_as_busy() {
        let t0 = parse_proc_stat("cpu 0 0 0 0 0 0 0 0 0 0").unwrap().0;
        let t1 = parse_proc_stat("cpu 0 0 0 500 500 0 0 0 0 0").unwrap().0;
        assert_eq!(cpu_util(t0, t1), 0.0);
    }

    /// The two memory measures diverge exactly by reclaimable cache.
    #[test]
    fn committed_and_cached_measures_are_distinct() {
        let m =
            parse_meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 600 kB\n").unwrap();
        let (committed, cached) = mem_utils(m);
        assert!((committed - 0.4).abs() < 1e-9, "claimed = 1 - avail/total");
        assert!((cached - 0.9).abs() < 1e-9, "touched = 1 - free/total");
    }

    /// A window with no elapsed time or no shared devices yields nothing
    /// rather than a fabricated zero.
    #[test]
    fn degenerate_windows_yield_none() {
        let d = vec![("a".to_string(), 5_u64)];
        assert!(max_disk_util(&d, &d, 0.0).is_none());
        assert!(max_disk_util(&[], &d, 1000.0).is_none());
    }

    /// Only WRITABLE /dev/-backed mounts count for storage, deduplicated by
    /// source. Pseudo-filesystems are not disks; a read-only squashfs snap
    /// image is 100% full by construction and would pin the item forever.
    #[test]
    fn dev_mounts_are_filtered_and_deduplicated() {
        let mounts = "\
proc /proc proc rw 0 0
/dev/nvme0n1p1 / ext4 rw 0 0
tmpfs /tmp tmpfs rw 0 0
/dev/loop0 /snap/core22/2045 squashfs ro,nodev 0 0
/dev/nvme1n1 /mnt/nvme xfs rw 0 0
/dev/nvme1n1 /mnt/alias xfs rw 0 0
/dev/mapper/vg-data /data\\040dir ext4 rw 0 0";
        assert_eq!(
            parse_dev_mounts(mounts),
            vec![
                "/".to_string(),
                "/mnt/nvme".to_string(),
                "/data dir".to_string(),
            ]
        );
    }
}
