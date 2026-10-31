//! Real-time data: container resource statistics (polled) and the engine's event stream.
//! Two plain threads publish an immutable `Arc<LiveState>` (pointer swap under a mutex, no lock held
//! across I/O), following the rules in docs/ARCHITECTURE.md. Dropping `Live` stops and joins both.
use super::api::{Engine, Event};
use crate::linux::{push, Stop};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HIST: usize = 150;
const EVENTS_KEPT: usize = 2000;
const POLL: Duration = Duration::from_secs(2);
/// Stats are only polled while a Docker page was shown within this window.
const IDLE_AFTER: Duration = Duration::from_secs(6);

#[derive(Clone, Default, Debug)]
pub struct CStats {
    pub name: String,
    pub cpu_pct: f32,
    pub mem: u64,
    pub mem_limit: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub blk_read: u64,
    pub blk_write: u64,
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
    pub blk_read_bps: f64,
    pub blk_write_bps: f64,
    pub pids: u64,
}

#[derive(Clone, Default)]
pub struct Hist {
    pub cpu: VecDeque<f32>,
    pub mem: VecDeque<f32>,
    pub net_rx: VecDeque<f32>,
    pub net_tx: VecDeque<f32>,
    pub blk_r: VecDeque<f32>,
    pub blk_w: VecDeque<f32>,
}

#[derive(Clone, Default)]
pub struct LiveState {
    pub stats: HashMap<String, CStats>,
    pub hist: HashMap<String, Hist>,
    pub total_cpu: VecDeque<f32>,
    pub total_mem: VecDeque<f32>,
    pub events: VecDeque<Event>,
    pub events_error: Option<String>,
    pub stats_error: Option<String>,
}

/// Raw counters of one sample, used to derive rates and CPU %.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Counters {
    pub cpu_total: u64,
    pub system: u64,
    pub online: u64,
    pub mem: u64,
    pub mem_limit: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub blk_r: u64,
    pub blk_w: u64,
    pub pids: u64,
}

fn u(v: &Value, path: &[&str]) -> u64 {
    path.iter().try_fold(v, |v, k| v.get(*k)).and_then(Value::as_u64).unwrap_or(0)
}

/// Extract counters from a `/containers/{id}/stats` document (cgroup v1 and v2).
pub fn counters(v: &Value) -> Counters {
    let mut c = Counters {
        cpu_total: u(v, &["cpu_stats", "cpu_usage", "total_usage"]),
        system: u(v, &["cpu_stats", "system_cpu_usage"]),
        online: u(v, &["cpu_stats", "online_cpus"]),
        mem_limit: u(v, &["memory_stats", "limit"]),
        pids: u(v, &["pids_stats", "current"]),
        ..Default::default()
    };
    if c.online == 0 {
        c.online = v.pointer("/cpu_stats/cpu_usage/percpu_usage").and_then(Value::as_array).map(|a| a.len() as u64).unwrap_or(1).max(1);
    }
    // what `docker stats` shows: usage minus reclaimable page cache
    let usage = u(v, &["memory_stats", "usage"]);
    let cache = v.pointer("/memory_stats/stats/inactive_file").or_else(|| v.pointer("/memory_stats/stats/total_inactive_file")).and_then(Value::as_u64).unwrap_or(0);
    c.mem = usage.saturating_sub(cache);
    if let Some(n) = v.get("networks").and_then(Value::as_object) {
        for i in n.values() {
            c.net_rx += u(i, &["rx_bytes"]);
            c.net_tx += u(i, &["tx_bytes"]);
        }
    }
    if let Some(a) = v.pointer("/blkio_stats/io_service_bytes_recursive").and_then(Value::as_array) {
        for e in a {
            let n = e.get("value").and_then(Value::as_u64).unwrap_or(0);
            match e.get("op").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
                Some("read") => c.blk_r += n,
                Some("write") => c.blk_w += n,
                _ => {}
            }
        }
    }
    c
}

/// CPU % between two samples, as `docker stats` computes it (100 % = one core).
pub fn cpu_pct(prev: &Counters, cur: &Counters) -> f32 {
    let dc = cur.cpu_total.saturating_sub(prev.cpu_total) as f64;
    let ds = cur.system.saturating_sub(prev.system) as f64;
    if ds <= 0.0 || dc <= 0.0 { 0.0 } else { (dc / ds * cur.online as f64 * 100.0) as f32 }
}

pub struct Live {
    stop: Arc<Stop>,
    state: Arc<Mutex<Arc<LiveState>>>,
    touched: Arc<AtomicU64>,
    epoch: Instant,
    /// Incremented for every event that changes containers/images/volumes/networks.
    pub generation: Arc<AtomicU64>,
    cancel: Arc<Mutex<Option<super::http::Canceller>>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Live {
    pub fn start(engine: Engine, wake: impl Fn() + Send + Sync + 'static) -> Live {
        let wake = Arc::new(wake);
        let mut l = Live { stop: Arc::new(Stop::default()), state: Arc::default(), touched: Arc::default(), epoch: Instant::now(), generation: Arc::default(), cancel: Arc::default(), threads: vec![] };
        let (stop, state, touched, epoch, e2, w2) = (l.stop.clone(), l.state.clone(), l.touched.clone(), l.epoch, engine.clone(), wake.clone());
        l.threads.push(std::thread::Builder::new().name("docker-stats".into()).spawn(move || stats_loop(e2, stop, state, touched, epoch, w2)).expect("spawn"));
        let (stop, state, generation, cancel) = (l.stop.clone(), l.state.clone(), l.generation.clone(), l.cancel.clone());
        l.threads.push(std::thread::Builder::new().name("docker-events".into()).spawn(move || events_loop(engine, stop, state, generation, cancel, wake)).expect("spawn"));
        l
    }

    pub fn snapshot(&self) -> Arc<LiveState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Called while a Docker page is visible; stats polling pauses otherwise.
    pub fn touch(&self) {
        self.touched.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    pub fn clear_events(&self) {
        update(&self.state, |s| s.events.clear());
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.trigger();
        if let Some(c) = self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
            c.cancel();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn update(state: &Mutex<Arc<LiveState>>, f: impl FnOnce(&mut LiveState)) {
    let mut g = state.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = (**g).clone();
    f(&mut s);
    *g = Arc::new(s);
}

fn stats_loop(engine: Engine, stop: Arc<Stop>, state: Arc<Mutex<Arc<LiveState>>>, touched: Arc<AtomicU64>, epoch: Instant, wake: Arc<dyn Fn() + Send + Sync>) {
    let mut prev: HashMap<String, (Counters, Instant)> = HashMap::new();
    loop {
        let idle = epoch.elapsed().as_millis() as u64 > touched.load(Ordering::Relaxed) + IDLE_AFTER.as_millis() as u64;
        if !idle {
            // all I/O happens here, without the state lock
            let res = engine.containers().map(|list| {
                list.into_iter().filter(|c| c.state == "running").filter_map(|c| engine.stats_once(&c.id).ok().map(|v| (c.id.clone(), c.name(), counters(&v), Instant::now()))).collect::<Vec<_>>()
            });
            match res {
                Ok(samples) => {
                    let mut out = HashMap::new();
                    for (id, name, cur, t) in samples {
                        let mut s = CStats { name, mem: cur.mem, mem_limit: cur.mem_limit, net_rx: cur.net_rx, net_tx: cur.net_tx, blk_read: cur.blk_r, blk_write: cur.blk_w, pids: cur.pids, ..Default::default() };
                        if let Some((p, pt)) = prev.get(&id) {
                            let dt = t.duration_since(*pt).as_secs_f64().max(0.001);
                            s.cpu_pct = cpu_pct(p, &cur);
                            s.net_rx_bps = cur.net_rx.saturating_sub(p.net_rx) as f64 / dt;
                            s.net_tx_bps = cur.net_tx.saturating_sub(p.net_tx) as f64 / dt;
                            s.blk_read_bps = cur.blk_r.saturating_sub(p.blk_r) as f64 / dt;
                            s.blk_write_bps = cur.blk_w.saturating_sub(p.blk_w) as f64 / dt;
                        }
                        prev.insert(id.clone(), (cur, t));
                        out.insert(id, s);
                    }
                    prev.retain(|k, _| out.contains_key(k));
                    update(&state, |st| {
                        st.stats_error = None;
                        st.hist.retain(|k, _| out.contains_key(k));
                        for (id, s) in &out {
                            let h = st.hist.entry(id.clone()).or_default();
                            push(&mut h.cpu, s.cpu_pct, HIST);
                            push(&mut h.mem, s.mem as f32, HIST);
                            push(&mut h.net_rx, s.net_rx_bps as f32, HIST);
                            push(&mut h.net_tx, s.net_tx_bps as f32, HIST);
                            push(&mut h.blk_r, s.blk_read_bps as f32, HIST);
                            push(&mut h.blk_w, s.blk_write_bps as f32, HIST);
                        }
                        push(&mut st.total_cpu, out.values().map(|s| s.cpu_pct).sum(), HIST);
                        push(&mut st.total_mem, out.values().map(|s| s.mem as f32).sum(), HIST);
                        st.stats = out;
                    });
                }
                Err(e) => update(&state, |st| st.stats_error = Some(e.to_string())),
            }
            wake();
        }
        if stop.sleep(POLL) {
            return;
        }
    }
}

fn events_loop(engine: Engine, stop: Arc<Stop>, state: Arc<Mutex<Arc<LiveState>>>, generation: Arc<AtomicU64>, cancel: Arc<Mutex<Option<super::http::Canceller>>>, wake: Arc<dyn Fn() + Send + Sync>) {
    // first connection replays the last hour from the engine's buffer; reconnects continue from the last event
    let mut since = crate::linux::now_secs().saturating_sub(3600);
    let mut last_nano = 0i64;
    let mut backoff = Duration::from_secs(1);
    while !stop.is_set() {
        match engine.events(since) {
            Ok(mut s) => {
                if let Ok(c) = s.canceller() {
                    *cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
                }
                if stop.is_set() {
                    return;
                }
                update(&state, |st| st.events_error = None);
                backoff = Duration::from_secs(1);
                let mut line = String::new();
                loop {
                    line.clear();
                    match s.reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let Ok(ev) = serde_json::from_str::<Event>(line.trim()) else { continue };
                    // a reconnect re-requests the last second; skip what was already seen
                    if ev.time_nano != 0 && ev.time_nano <= last_nano {
                        continue;
                    }
                    last_nano = ev.time_nano;
                    since = since.max(ev.time.max(0) as u64);
                    if ev.changes_inventory() {
                        generation.fetch_add(1, Ordering::Relaxed);
                    }
                    update(&state, |st| {
                        st.events.push_back(ev);
                        while st.events.len() > EVENTS_KEPT {
                            st.events.pop_front();
                        }
                    });
                    wake();
                }
                if !stop.is_set() {
                    update(&state, |st| st.events_error = Some("event stream closed; reconnecting".into()));
                }
            }
            Err(e) => update(&state, |st| st.events_error = Some(e.to_string())),
        }
        wake();
        if stop.sleep(backoff) {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
