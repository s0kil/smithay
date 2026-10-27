//! Opt-in, bounded GPU diagnostics. These are command timeline intervals, not
//! isolated PCIe transfer times or GPU busy-cycle measurements.

use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{Dmabuf, SyncPoint, WeakDmabuf, ffi, gpu_timing_fence};
use crate::backend::allocator::Buffer;

const CAPACITY: usize = 64;
const MAX_SAMPLES: u64 = 100_000;
const MAX_CAPTURE: Duration = Duration::from_secs(120);
static NEXT_CONTEXT: AtomicU64 = AtomicU64::new(1);

#[cfg(all(feature = "wayland_frontend", feature = "backend_drm"))]
pub(crate) use explicit_acquire::register_explicit_acquire;

/// Bounded handoff from the Wayland commit thread to GLES output threads.
/// Presence is recorded while configured but unarmed; only an armed capture
/// exports fences. Therefore a known explicit acquire can never fall back to
/// an unrelated implicit reservation fence after an arming transition.
#[cfg(all(feature = "wayland_frontend", feature = "backend_drm"))]
mod explicit_acquire {
    use super::*;
    use crate::wayland::drm_syncobj::DrmSyncPoint;
    use std::sync::{Mutex, OnceLock};
    use wayland_server::backend::ObjectId;

    const MAX_OWNERS: usize = 256;
    static REGISTRY: OnceLock<Option<Mutex<Registry>>> = OnceLock::new();

    enum Acquire {
        Implicit,
        Unrecorded,
        Recorded(Arc<OwnedFd>),
        Failed(String),
    }
    struct Entry {
        owner: ObjectId,
        buffer: WeakDmabuf,
        sequence: u64,
        acquire: Acquire,
    }
    struct Registry {
        directory: PathBuf,
        last_poll: Instant,
        armed: bool,
        generation: u64,
        sequence: u64,
        overflowed: bool,
        entries: VecDeque<Entry>,
    }
    impl Registry {
        fn refresh(&mut self) {
            self.entries.retain(|entry| !entry.buffer.is_gone());
            if self.last_poll.elapsed() < Duration::from_secs(1) {
                return;
            }
            self.last_poll = Instant::now();
            let armed = self.directory.join("enable").is_file();
            self.set_armed(armed);
        }
        fn set_armed(&mut self, armed: bool) {
            if self.armed != armed {
                self.armed = armed;
                self.generation += 1;
                for entry in &mut self.entries {
                    if !matches!(entry.acquire, Acquire::Implicit) {
                        entry.acquire = Acquire::Unrecorded;
                    }
                }
            }
        }
        fn record(&mut self, owner: ObjectId, buffer: Option<WeakDmabuf>, acquire: Acquire) {
            self.entries.retain(|entry| entry.owner != owner);
            let Some(buffer) = buffer else {
                return;
            };
            self.sequence += 1;
            while self.entries.len() >= MAX_OWNERS {
                self.overflowed = true;
                self.entries.pop_front();
            }
            self.entries.push_back(Entry {
                owner,
                buffer,
                sequence: self.sequence,
                acquire,
            });
        }
    }
    fn registry() -> Option<&'static Mutex<Registry>> {
        REGISTRY
            .get_or_init(|| {
                let directory = PathBuf::from(std::env::var_os("SMITHAY_GPU_TIMING")?);
                let metadata = fs::symlink_metadata(&directory).ok()?;
                if !directory.is_absolute()
                    || !metadata.is_dir()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.mode() & 0o077 != 0
                {
                    return None;
                }
                let now = Instant::now();
                Some(Mutex::new(Registry {
                    directory,
                    last_poll: now.checked_sub(Duration::from_secs(2)).unwrap_or(now),
                    armed: false,
                    generation: 0,
                    sequence: 0,
                    overflowed: false,
                    entries: VecDeque::new(),
                }))
            })
            .as_ref()
    }

    /// Record each committed owner/buffer association, including implicit
    /// commits and detach. Object identity prevents conflating reused protocol
    /// numbers. Multiple owners of one buffer are conservatively ambiguous.
    pub(crate) fn register_explicit_acquire(
        owner: ObjectId,
        buffer: Option<&Dmabuf>,
        acquire: Option<&DrmSyncPoint>,
    ) {
        let Some(registry) = registry() else {
            return;
        };
        let mut registry = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.refresh();
        let acquire = match acquire {
            None => Acquire::Implicit,
            Some(_) if !registry.armed || registry.overflowed => Acquire::Unrecorded,
            Some(point) => match point.export_sync_file() {
                Ok(fd) => Acquire::Recorded(Arc::new(fd)),
                Err(error) => Acquire::Failed(error.to_string()),
            },
        };
        registry.record(owner, buffer.map(Dmabuf::weak), acquire);
    }

    pub(super) fn lookup(buffer: &WeakDmabuf) -> Option<(String, String, Result<Vec<OwnedFd>, String>)> {
        let mut registry = registry()?
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.refresh();
        resolve(&registry, buffer)
    }

    fn resolve(
        registry: &Registry,
        buffer: &WeakDmabuf,
    ) -> Option<(String, String, Result<Vec<OwnedFd>, String>)> {
        if registry.overflowed {
            return Some((
                "registry_capacity_exceeded".into(),
                "null".into(),
                Err("producer ownership registry exceeded its bounded capacity".into()),
            ));
        }
        let entries = registry
            .entries
            .iter()
            .filter(|entry| &entry.buffer == buffer)
            .collect::<Vec<_>>();
        if entries.len() > 1 {
            return Some((
                "ambiguous_committed_owners".into(),
                format!(
                    "{{\"owners\":{},\"generation\":{}}}",
                    entries.len(),
                    registry.generation
                ),
                Err("buffer has multiple committed surface owners".into()),
            ));
        }
        let entry = *entries.first()?;
        let provenance = format!(
            "{{\"owner\":{},\"registration_sequence\":{},\"capture_generation\":{}}}",
            quote(&format!("{:?}", entry.owner)),
            entry.sequence,
            registry.generation
        );
        match &entry.acquire {
            Acquire::Implicit => Some((
                "wayland_implicit".into(),
                provenance,
                entry
                    .buffer
                    .upgrade()
                    .ok_or_else(|| "source buffer released".into())
                    .and_then(|buffer| {
                        gpu_timing_fence::export_dmabuf_read_fences(&buffer)
                            .map_err(|error| error.to_string())
                    }),
            )),
            Acquire::Unrecorded => Some((
                "wayland_explicit_unrecorded".into(),
                provenance,
                Err("explicit acquire was not recorded in this capture generation".into()),
            )),
            Acquire::Failed(error) => Some((
                "wayland_explicit_export_failed".into(),
                provenance,
                Err(error.clone()),
            )),
            Acquire::Recorded(fd) => Some((
                "wayland_explicit".into(),
                provenance,
                fd.try_clone()
                    .map(|fd| vec![fd])
                    .map_err(|error| error.to_string()),
            )),
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        fn fixture() -> (Registry, WeakDmabuf) {
            use crate::backend::allocator::{Fourcc, Modifier, dmabuf::DmabufFlags};
            let mut builder =
                Dmabuf::builder((1, 1), Fourcc::Argb8888, Modifier::Linear, DmabufFlags::empty());
            let fd: OwnedFd = fs::File::open("/dev/null").unwrap().into();
            builder.add_plane(fd, 0, 4);
            let buffer = builder.build().unwrap().weak();
            (
                Registry {
                    directory: PathBuf::new(),
                    last_poll: Instant::now(),
                    armed: false,
                    generation: 0,
                    sequence: 0,
                    overflowed: false,
                    entries: VecDeque::new(),
                },
                buffer,
            )
        }
        #[test]
        fn replacement_and_detach_clear_the_old_explicit_acquire() {
            let (mut registry, buffer) = fixture();
            registry.record(
                ObjectId::null(),
                Some(buffer.clone()),
                Acquire::Failed("export failed".into()),
            );
            registry.record(ObjectId::null(), Some(buffer), Acquire::Implicit);
            assert_eq!(registry.entries.len(), 1);
            assert!(matches!(registry.entries[0].acquire, Acquire::Implicit));
            registry.record(ObjectId::null(), None, Acquire::Implicit);
            assert!(registry.entries.is_empty());
        }
        #[test]
        fn capture_transition_invalidates_previously_recorded_explicit_fences() {
            let (mut registry, buffer) = fixture();
            registry.record(
                ObjectId::null(),
                Some(buffer.clone()),
                Acquire::Failed("prior generation".into()),
            );
            registry.set_armed(true);
            assert_eq!(registry.generation, 1);
            assert!(matches!(registry.entries[0].acquire, Acquire::Unrecorded));
            let selected = resolve(&registry, &buffer).unwrap();
            assert_eq!(selected.0, "wayland_explicit_unrecorded");
            assert!(selected.2.is_err());
        }
        #[test]
        fn explicit_export_error_never_falls_back_to_implicit() {
            let (mut registry, buffer) = fixture();
            registry.record(
                ObjectId::null(),
                Some(buffer.clone()),
                Acquire::Failed("expected failure".into()),
            );
            let selected = resolve(&registry, &buffer).unwrap();
            assert_eq!(selected.0, "wayland_explicit_export_failed");
            assert_eq!(selected.2.unwrap_err(), "expected failure");
        }
        #[test]
        fn ambiguous_or_overflowed_ownership_is_unknown() {
            let (mut registry, buffer) = fixture();
            registry.record(ObjectId::null(), Some(buffer.clone()), Acquire::Implicit);
            registry.entries.push_back(Entry {
                owner: ObjectId::null(),
                buffer: buffer.clone(),
                sequence: 2,
                acquire: Acquire::Unrecorded,
            });
            assert_eq!(
                resolve(&registry, &buffer).unwrap().0,
                "ambiguous_committed_owners"
            );
            registry.overflowed = true;
            assert_eq!(
                resolve(&registry, &buffer).unwrap().0,
                "registry_capacity_exceeded"
            );
        }
    }
}

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn clocks() -> (u64, u64) {
    let monotonic = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let mono_ns = (monotonic.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(monotonic.tv_nsec as u64);
    let unix_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    (mono_ns, unix_ns)
}

/// Captured only when diagnostics are configured. Node is an import hint, not
/// proof of the producer GPU; plane inode/exporter metadata permits correlation.
#[derive(Debug)]
pub(super) struct DmabufTiming {
    json: String,
    buffer: WeakDmabuf,
}

impl DmabufTiming {
    pub(super) fn new(buffer: &Dmabuf) -> Arc<Self> {
        let planes = buffer
            .handles()
            .map(|fd| {
                let stat = rustix::fs::fstat(fd).ok();
                let info =
                    fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).unwrap_or_default();
                let field = |key: &str| {
                    info.lines().find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| *name == key)
                            .map(|(_, v)| v.trim())
                    })
                };
                format!(
                    "{{\"inode\":{},\"device\":{},\"exporter\":{},\"size\":{}}}",
                    stat.as_ref().map(|s| s.st_ino).unwrap_or_default(),
                    stat.as_ref().map(|s| s.st_dev).unwrap_or_default(),
                    quote(field("exp_name").unwrap_or("unknown")),
                    field("size")
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        #[cfg(feature = "backend_drm")]
        let node_hint = quote(&format!("{:?}", buffer.node()));
        #[cfg(not(feature = "backend_drm"))]
        let node_hint = "null".to_owned();
        Arc::new(Self {
            buffer: buffer.weak(),
            json: format!(
                "{{\"width\":{},\"height\":{},\"format\":{},\"modifier\":{},\"assigned_node_hint\":{},\"planes\":[{}]}}",
                buffer.size().w,
                buffer.size().h,
                quote(&format!("{:?}", buffer.format().code)),
                quote(&format!("{:?}", buffer.format().modifier)),
                node_hint,
                planes
            ),
        })
    }
}

struct Pending {
    queries: [u32; 4],
    sequence: u64,
    frame: u64,
    scope: &'static str,
    source: Arc<DmabufTiming>,
    submitted: (u64, u64),
    ended: (u64, u64),
    wait_marked: bool,
    draw_marked: bool,
    complete: bool,
    created: Instant,
    producer_fences: Result<Vec<OwnedFd>, String>,
    producer_fence_source: String,
    producer_provenance: String,
    completion_fence: Option<OwnedFd>,
    completion_attached: bool,
}

struct State {
    free: Vec<[u32; 4]>,
    queries: Vec<u32>,
    pending: VecDeque<Pending>,
    last_poll: Instant,
    enabled: bool,
    marker_present: bool,
    armed_at: Instant,
    capture_count: u64,
    sequence: u64,
    frame: u64,
    rejected: u64,
    dropped: u64,
    failed: bool,
}

pub(super) struct GpuTiming {
    directory: PathBuf,
    context: u64,
    bits: i32,
    sender: SyncSender<String>,
    state: RefCell<State>,
}

impl GpuTiming {
    pub(super) fn new(gl: &ffi::Gles2, extensions: &[String]) -> Option<Self> {
        let directory = PathBuf::from(std::env::var_os("SMITHAY_GPU_TIMING")?);
        let metadata = fs::symlink_metadata(&directory).ok()?;
        if !directory.is_absolute()
            || !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            tracing::warn!("GPU timing requires an existing private, owned, absolute non-symlink directory");
            return None;
        }
        if !extensions.iter().any(|s| s == "GL_EXT_disjoint_timer_query")
            || !gl.GenQueriesEXT.is_loaded()
            || !gl.DeleteQueriesEXT.is_loaded()
            || !gl.QueryCounterEXT.is_loaded()
            || !gl.GetQueryivEXT.is_loaded()
            || !gl.GetQueryObjectuivEXT.is_loaded()
            || !gl.GetQueryObjectui64vEXT.is_loaded()
        {
            tracing::warn!("GPU timing unavailable: EXT_disjoint_timer_query entry points missing");
            return None;
        }
        let mut bits = 0;
        unsafe { gl.GetQueryivEXT(ffi::TIMESTAMP_EXT, ffi::QUERY_COUNTER_BITS_EXT, &mut bits) };
        if !(1..=64).contains(&bits) {
            tracing::warn!(bits, "GPU timestamp counter unsupported");
            return None;
        }
        let context = NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("gpu-timing-{}-{context}.jsonl", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .ok()?;
        let (sender, receiver) = mpsc::sync_channel::<String>(1024);
        std::thread::Builder::new()
            .name("gpu-timing-log".into())
            .spawn(move || {
                for line in receiver {
                    if writeln!(file, "{line}").is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        let gl_string = |name| unsafe {
            let p = gl.GetString(name);
            if p.is_null() {
                "unknown".to_owned()
            } else {
                std::ffi::CStr::from_ptr(p.cast()).to_string_lossy().into_owned()
            }
        };
        let _ = sender.try_send(format!(
            "{{\"event\":\"context\",\"version\":1,\"pid\":{},\"context\":{},\"vendor\":{},\"renderer\":{},\"counter_bits\":{},\"metric\":\"GPU command timeline elapsed; not isolated PCIe transfer\"}}",
            std::process::id(), context, quote(&gl_string(ffi::VENDOR)), quote(&gl_string(ffi::RENDERER)), bits));
        let now = Instant::now();
        Some(Self {
            directory,
            context,
            bits,
            sender,
            state: RefCell::new(State {
                free: Vec::new(),
                queries: Vec::new(),
                pending: VecDeque::new(),
                last_poll: now.checked_sub(Duration::from_secs(2)).unwrap_or(now),
                enabled: false,
                marker_present: false,
                armed_at: now,
                capture_count: 0,
                sequence: 0,
                frame: 0,
                rejected: 0,
                dropped: 0,
                failed: false,
            }),
        })
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.state.borrow().pending.is_empty()
    }

    fn send(&self, state: &mut State, line: String) {
        match self.sender.try_send(line) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => state.dropped += 1,
            Err(mpsc::TrySendError::Disconnected(_)) => {
                state.dropped += 1;
                state.failed = true;
                state.enabled = false;
            }
        }
    }

    pub(super) fn begin_frame(&self) -> u64 {
        let mut state = self.state.borrow_mut();
        state.frame += 1;
        state.frame
    }

    /// Called with this renderer's context current. Never waits for results.
    pub(super) fn collect(&self, gl: &ffi::Gles2) {
        let mut s = self.state.borrow_mut();
        if s.last_poll.elapsed() >= Duration::from_secs(1) {
            s.last_poll = Instant::now();
            let present = self.directory.join("enable").is_file();
            if present != s.marker_present {
                s.marker_present = present;
                s.enabled = present && !s.failed;
                s.capture_count = 0;
                s.armed_at = Instant::now();
                let (mono, unix) = clocks();
                let rejected = s.rejected;
                let dropped = s.dropped;
                self.send(&mut s, format!("{{\"event\":\"control\",\"context\":{},\"enabled\":{},\"monotonic_ns\":{},\"unix_ns\":{},\"rejected\":{},\"dropped\":{}}}", self.context, present, mono, unix, rejected, dropped));
            }
        }
        if s.enabled && (s.armed_at.elapsed() >= MAX_CAPTURE || s.capture_count >= MAX_SAMPLES) {
            s.enabled = false;
            self.send(
                &mut s,
                format!("{{\"event\":\"limit\",\"context\":{}}}", self.context),
            );
        }
        if s.pending.is_empty() {
            return;
        }
        // A disjoint invalidates every outstanding sample, even one whose end
        // query already became available before this poll.
        let mut disjoint = 0;
        unsafe { gl.GetIntegerv(ffi::GPU_DISJOINT_EXT, &mut disjoint) };
        if disjoint != 0 {
            reject_pending(&mut s);
            self.send(
                &mut s,
                format!("{{\"event\":\"disjoint\",\"context\":{}}}", self.context),
            );
            return;
        }
        while let Some(p) = s.pending.front() {
            let mut available = true;
            for query in p.queries {
                let mut ready = 0;
                unsafe { gl.GetQueryObjectuivEXT(query, ffi::QUERY_RESULT_AVAILABLE, &mut ready) };
                available &= ready != 0;
            }
            let fence_data = fence_data(p);
            if !available || fence_data.is_none() {
                if p.created.elapsed() > Duration::from_secs(10) {
                    s.failed = true;
                    s.enabled = false;
                    s.rejected += s.pending.len() as u64;
                    s.pending.clear();
                    self.send(
                        &mut s,
                        format!("{{\"event\":\"query_timeout\",\"context\":{}}}", self.context),
                    );
                }
                break;
            }
            // Recheck immediately before accepting results: disjoint is sticky
            // until read, and can occur during the availability checks above.
            unsafe { gl.GetIntegerv(ffi::GPU_DISJOINT_EXT, &mut disjoint) };
            if disjoint != 0 {
                reject_pending(&mut s);
                self.send(
                    &mut s,
                    format!("{{\"event\":\"disjoint\",\"context\":{}}}", self.context),
                );
                break;
            }
            let p = s.pending.pop_front().unwrap();
            let mut ts = [0_u64; 4];
            for (query, time) in p.queries.into_iter().zip(ts.iter_mut()) {
                unsafe { gl.GetQueryObjectui64vEXT(query, ffi::QUERY_RESULT, time) };
            }
            let valid = valid_timestamps(ts, self.bits) && p.complete;
            if !valid {
                s.rejected += 1;
            }
            let wait = if p.wait_marked && valid {
                (ts[1] - ts[0]).to_string()
            } else {
                "null".into()
            };
            let draw = if p.draw_marked && valid {
                (ts[3] - ts[2]).to_string()
            } else {
                "null".into()
            };
            let collected = clocks();
            self.send(&mut s, format!(
                "{{\"event\":\"sample\",\"context\":{},\"sequence\":{},\"frame\":{},\"scope\":{},\"source\":{},\"producer_fence_source\":{},\"producer_provenance\":{},\"submitted_monotonic_ns\":{},\"submitted_unix_ns\":{},\"ended_monotonic_ns\":{},\"ended_unix_ns\":{},\"collected_monotonic_ns\":{},\"collected_unix_ns\":{},\"gpu_timestamps_ns\":[{},{},{},{}],\"gpu_total_ns\":{},\"gpu_texture_upload_wait_interval_ns\":{},\"gpu_draw_interval_ns\":{},\"fences\":{},\"valid\":{}}}",
                self.context, p.sequence, p.frame, quote(p.scope), p.source.json,
                quote(&p.producer_fence_source), p.producer_provenance,
                p.submitted.0, p.submitted.1, p.ended.0, p.ended.1, collected.0, collected.1,
                ts[0], ts[1], ts[2], ts[3], if valid { (ts[3]-ts[0]).to_string() } else { "null".into() }, wait, draw, fence_data.unwrap(), valid));
            s.free.push(p.queries);
        }
    }

    pub(super) fn begin<'a>(
        &'a self,
        gl: &'a ffi::Gles2,
        scope: &'static str,
        source: &Arc<DmabufTiming>,
        frame: u64,
    ) -> Option<TimingScope<'a>> {
        let mut s = self.state.borrow_mut();
        if !s.enabled || s.failed || s.capture_count >= MAX_SAMPLES || s.armed_at.elapsed() >= MAX_CAPTURE {
            return None;
        }
        if s.queries.is_empty() {
            let mut disjoint = 0;
            unsafe { gl.GetIntegerv(ffi::GPU_DISJOINT_EXT, &mut disjoint) };
            s.queries.resize(CAPACITY * 4, 0);
            unsafe { gl.GenQueriesEXT(s.queries.len() as i32, s.queries.as_mut_ptr()) };
            if s.queries.contains(&0) {
                s.failed = true;
                s.enabled = false;
                return None;
            }
            s.free = s
                .queries
                .chunks_exact(4)
                .map(|q| [q[0], q[1], q[2], q[3]])
                .collect();
        }
        let Some(queries) = s.free.pop() else {
            s.dropped += 1;
            return None;
        };
        s.sequence += 1;
        s.capture_count += 1;
        let (producer_fence_source, producer_provenance, producer_fences) = if scope == "dmabuf_draw" {
            select_producer_fences(source)
        } else {
            ("not_applicable".into(), "null".into(), Ok(Vec::new()))
        };
        let p = Pending {
            queries,
            sequence: s.sequence,
            frame,
            scope,
            source: source.clone(),
            submitted: clocks(),
            ended: (0, 0),
            wait_marked: false,
            draw_marked: false,
            complete: false,
            created: Instant::now(),
            producer_fences,
            producer_fence_source,
            producer_provenance,
            completion_fence: None,
            completion_attached: scope != "dmabuf_draw",
        };
        unsafe { gl.QueryCounterEXT(queries[0], ffi::TIMESTAMP_EXT) };
        Some(TimingScope {
            owner: self,
            gl,
            pending: Some(p),
        })
    }

    /// Duplicate the existing frame fence; this creates no GPU commands.
    pub(super) fn frame_finished(&self, frame: u64, completion: &SyncPoint) {
        let mut s = self.state.borrow_mut();
        if !s
            .pending
            .iter()
            .any(|p| p.frame == frame && p.scope == "dmabuf_draw")
        {
            return;
        }
        let exported = completion.export();
        for p in &mut s.pending {
            if p.frame == frame && p.scope == "dmabuf_draw" {
                p.completion_attached = true;
                p.completion_fence = exported.as_ref().and_then(|fd| fd.try_clone().ok());
            }
        }
    }

    pub(super) fn cleanup(&self, gl: Option<&ffi::Gles2>) {
        let mut s = self.state.borrow_mut();
        if let Some(gl) = gl {
            if !s.queries.is_empty() {
                unsafe { gl.DeleteQueriesEXT(s.queries.len() as i32, s.queries.as_ptr()) };
            }
        }
        let unresolved = s.pending.len();
        let rejected = s.rejected;
        let dropped = s.dropped;
        self.send(
            &mut s,
            format!(
                "{{\"event\":\"cleanup\",\"context\":{},\"unresolved\":{},\"rejected\":{},\"dropped\":{}}}",
                self.context, unresolved, rejected, dropped
            ),
        );
        s.pending.clear();
        s.free.clear();
        s.queries.clear();
        s.enabled = false;
    }
}

fn reject_pending(state: &mut State) {
    while let Some(pending) = state.pending.pop_front() {
        state.free.push(pending.queries);
        state.rejected += 1;
    }
}

fn select_producer_fences(source: &DmabufTiming) -> (String, String, Result<Vec<OwnedFd>, String>) {
    #[cfg(all(feature = "wayland_frontend", feature = "backend_drm"))]
    if let Some(explicit) = explicit_acquire::lookup(&source.buffer) {
        return explicit;
    }
    (
        "implicit_unregistered".into(),
        "null".into(),
        source
            .buffer
            .upgrade()
            .ok_or_else(|| "source buffer released".into())
            .and_then(|buffer| {
                gpu_timing_fence::export_dmabuf_read_fences(&buffer).map_err(|e| e.to_string())
            }),
    )
}

fn valid_timestamps(ts: [u64; 4], bits: i32) -> bool {
    (1..=64).contains(&bits)
        && ts[0] != 0
        && ts.windows(2).all(|pair| pair[0] <= pair[1])
        && ts[3].saturating_sub(ts[0]) <= 10_000_000_000
        && (bits == 64 || ts.iter().all(|t| *t < (1_u64 << bits)))
}

/// None means a real fence has not signaled yet. Unsupported/error cases are
/// emitted explicitly rather than manufacturing a zero latency.
fn fence_data(p: &Pending) -> Option<String> {
    if p.scope != "dmabuf_draw" || !p.complete {
        return Some("null".into());
    }
    if !p.completion_attached {
        return None;
    }
    let (Ok(producers), Some(completion)) = (&p.producer_fences, &p.completion_fence) else {
        let reason = p
            .producer_fences
            .as_ref()
            .err()
            .map(String::as_str)
            .unwrap_or("native completion fence unavailable");
        return Some(format!(
            "{{\"status\":\"unavailable\",\"reason\":{}}}",
            quote(reason)
        ));
    };
    let mut source_ready = Some(0_u64);
    let mut sources = Vec::new();
    for fd in producers {
        match gpu_timing_fence::inspect_sync_file(fd.as_fd()) {
            Ok(snapshot) => {
                if snapshot.status == 0 {
                    return None;
                }
                source_ready = source_ready
                    .zip(snapshot.completed_timestamp_ns)
                    .map(|(a, b)| a.max(b));
                sources.push(snapshot_json(&snapshot));
            }
            Err(error) => {
                source_ready = None;
                sources.push(format!("{{\"error\":{}}}", quote(&error.to_string())));
            }
        }
    }
    if producers.is_empty() {
        source_ready = None;
    }
    let (target_ready, target) = match gpu_timing_fence::inspect_sync_file(completion.as_fd()) {
        Ok(snapshot) => {
            if snapshot.status == 0 {
                return None;
            }
            (snapshot.completed_timestamp_ns, snapshot_json(&snapshot))
        }
        Err(error) => (None, format!("{{\"error\":{}}}", quote(&error.to_string()))),
    };
    let elapsed = source_ready
        .zip(target_ready)
        .and_then(|(source, target)| target.checked_sub(source));
    Some(format!(
        "{{\"status\":{},\"producer_ready_monotonic_ns\":{},\"frame_complete_monotonic_ns\":{},\"producer_ready_to_frame_complete_ns\":{},\"producer\":[{}],\"completion\":{}}}",
        quote(if elapsed.is_some() {
            "timestamped"
        } else {
            "unknown_timestamp"
        }),
        source_ready
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into()),
        target_ready
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into()),
        elapsed.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
        sources.join(","),
        target
    ))
}

fn snapshot_json(snapshot: &gpu_timing_fence::FenceSnapshot) -> String {
    let details = snapshot
        .fences
        .iter()
        .take(8)
        .map(|f| {
            format!(
                "{{\"driver\":{},\"object\":{},\"status\":{},\"timestamp_ns\":{}}}",
                quote(&f.driver_name),
                quote(&f.obj_name),
                f.status,
                f.timestamp_ns
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"status\":{},\"fence_count\":{},\"details_truncated\":{},\"details\":[{}]}}",
        snapshot.status,
        snapshot.fences.len(),
        snapshot.fences.len() > 8,
        details
    )
}

pub(super) struct TimingScope<'a> {
    owner: &'a GpuTiming,
    gl: &'a ffi::Gles2,
    pending: Option<Pending>,
}

impl TimingScope<'_> {
    pub(super) fn wait_done(&mut self) {
        let p = self.pending.as_mut().unwrap();
        unsafe { self.gl.QueryCounterEXT(p.queries[1], ffi::TIMESTAMP_EXT) };
        p.wait_marked = true;
    }
    pub(super) fn draw_begin(&mut self) {
        let p = self.pending.as_mut().unwrap();
        unsafe { self.gl.QueryCounterEXT(p.queries[2], ffi::TIMESTAMP_EXT) };
        p.draw_marked = true;
    }
    pub(super) fn complete(mut self) {
        self.pending.as_mut().unwrap().complete = true;
    }
}

impl Drop for TimingScope<'_> {
    fn drop(&mut self) {
        let mut p = self.pending.take().unwrap();
        unsafe {
            if !p.wait_marked {
                self.gl.QueryCounterEXT(p.queries[1], ffi::TIMESTAMP_EXT);
            }
            if !p.draw_marked {
                self.gl.QueryCounterEXT(p.queries[2], ffi::TIMESTAMP_EXT);
            }
            self.gl.QueryCounterEXT(p.queries[3], ffi::TIMESTAMP_EXT);
        }
        p.ended = clocks();
        self.owner.state.borrow_mut().pending.push_back(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_wrap_and_invalid_counter_width() {
        assert!(valid_timestamps([1, 2, 2, 3], 64));
        assert!(!valid_timestamps([100, 101, 2, 3], 64));
        assert!(!valid_timestamps([1, 2, 3, 256], 8));
        assert!(!valid_timestamps([1, 2, 3, 4], 0));
        assert!(!valid_timestamps([0, 0, 0, 0], 64));
        assert!(!valid_timestamps([1, 2, 3, 10_000_000_002], 64));
    }
    #[test]
    fn json_quote_escapes_control_and_delimiters() {
        assert_eq!(quote("a\"\\\n\u{1}"), "\"a\\\"\\\\\\n\\u0001\"");
    }
    fn fixture() -> (GpuTiming, mpsc::Receiver<String>, Arc<DmabufTiming>) {
        use crate::backend::allocator::{Fourcc, Modifier, dmabuf::DmabufFlags};
        let mut builder = Dmabuf::builder((1, 1), Fourcc::Argb8888, Modifier::Linear, DmabufFlags::empty());
        let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        builder.add_plane(fd, 0, 4);
        let buffer = builder.build().unwrap();
        let metadata = DmabufTiming::new(&buffer);
        let (sender, receiver) = mpsc::sync_channel(4);
        let now = Instant::now();
        (
            GpuTiming {
                directory: PathBuf::new(),
                context: 1,
                bits: 64,
                sender,
                state: RefCell::new(State {
                    free: Vec::new(),
                    queries: Vec::new(),
                    pending: VecDeque::new(),
                    last_poll: now,
                    enabled: false,
                    marker_present: false,
                    armed_at: now,
                    capture_count: 0,
                    sequence: 0,
                    frame: 0,
                    rejected: 0,
                    dropped: 0,
                    failed: false,
                }),
            },
            receiver,
            metadata,
        )
    }
    fn pending(frame: u64, source: Arc<DmabufTiming>) -> Pending {
        Pending {
            queries: [1, 2, 3, 4],
            sequence: frame,
            frame,
            scope: "dmabuf_draw",
            source,
            submitted: (1, 1),
            ended: (2, 2),
            wait_marked: true,
            draw_marked: true,
            complete: true,
            created: Instant::now(),
            producer_fences: Err("fixture".into()),
            producer_fence_source: "fixture".into(),
            producer_provenance: "null".into(),
            completion_fence: None,
            completion_attached: false,
        }
    }
    #[test]
    fn nested_frames_pair_only_their_own_completion() {
        let (timing, _receiver, source) = fixture();
        let outer = timing.begin_frame();
        timing
            .state
            .borrow_mut()
            .pending
            .push_back(pending(outer, source.clone()));
        let inner = timing.begin_frame();
        timing
            .state
            .borrow_mut()
            .pending
            .push_back(pending(inner, source.clone()));
        timing.frame_finished(inner, &SyncPoint::signaled());
        assert!(!timing.state.borrow().pending[0].completion_attached);
        assert!(timing.state.borrow().pending[1].completion_attached);
        // The outer frame resumes after the inner frame ends.
        timing
            .state
            .borrow_mut()
            .pending
            .push_back(pending(outer, source));
        timing.frame_finished(outer, &SyncPoint::signaled());
        assert!(
            timing
                .state
                .borrow()
                .pending
                .iter()
                .all(|p| p.completion_attached)
        );
        assert_ne!(outer, inner);
    }
    #[test]
    fn disjoint_rejects_every_pending_sample_and_releases_slots() {
        let (timing, _receiver, source) = fixture();
        let mut state = timing.state.borrow_mut();
        state.pending.push_back(pending(1, source.clone()));
        state.pending.push_back(pending(2, source));
        reject_pending(&mut state);
        assert_eq!(state.rejected, 2);
        assert!(state.pending.is_empty());
        assert_eq!(state.free.len(), 2);
    }
    #[test]
    fn disconnected_writer_disables_collection() {
        let (timing, receiver, _) = fixture();
        drop(receiver);
        let mut state = timing.state.borrow_mut();
        state.enabled = true;
        timing.send(&mut state, "fixture".into());
        assert!(state.failed);
        assert!(!state.enabled);
        assert_eq!(state.dropped, 1);
    }
}
