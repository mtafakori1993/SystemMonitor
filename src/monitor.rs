//! Background sampler thread: collects all metrics and publishes immutable snapshots.
//!
//! The sampler works on a private `Snapshot` (no lock held while reading /proc, /sys),
//! then publishes a clone as `Arc<Snapshot>`; readers only hold the lock for a pointer copy.
//! The thread is stopped and joined when the `Sampler` handle is dropped.
use crate::{cpu, memory, network, processes, storage};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default, Clone)]
pub struct Snapshot {
    pub cpu: cpu::Cpu,
    pub mem: memory::Mem,
    pub net: network::Net,
    pub storage: storage::Storage,
    pub procs: processes::Procs,
    pub hostname: String,
    pub kernel: String,
    pub uptime: u64,
    pub cap: usize,
    pub ready: bool,
}

pub struct Config {
    pub interval_ms: AtomicU64,
    pub history: AtomicUsize,
}

pub type Shared = Arc<Mutex<Arc<Snapshot>>>;

/// Owns the sampler thread. Dropping it cancels the thread (wakes it from its sleep) and joins it.
pub struct Sampler {
    pub shared: Shared,
    stop: Arc<crate::linux::Stop>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.trigger();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// `wake` is called after every publish (the GUI passes `ctx.request_repaint`; the CLI passes a no-op).
pub fn start(wake: impl Fn() + Send + 'static, cfg: Arc<Config>) -> Sampler {
    let shared: Shared = Arc::new(Mutex::new(Arc::new(Snapshot::default())));
    let out = shared.clone();
    let stop = Arc::new(crate::linux::Stop::default());
    let st = stop.clone();
    let handle = std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let (mut cs, mut ns, mut ss, mut ps) = (cpu::CpuSampler::default(), network::NetSampler::default(), storage::StorageSampler::default(), processes::ProcSampler::default());
            let mut work = Snapshot::default();
            let mut tick = 0u64;
            while !st.is_set() {
                let cap = cfg.history.load(Ordering::Relaxed).clamp(30, 900);
                let t0 = std::time::Instant::now();
                work.cap = cap;
                cs.sample(&mut work.cpu, cap);
                memory::sample(&mut work.mem, cap);
                ns.sample(&mut work.net, cap);
                ss.sample(&mut work.storage, cap);
                if tick % 2 == 0 {
                    ps.sample(&mut work.procs);
                }
                if tick % 10 == 0 {
                    work.hostname = crate::linux::read_trim("/proc/sys/kernel/hostname").unwrap_or_default();
                    work.kernel = crate::linux::read_trim("/proc/sys/kernel/osrelease").unwrap_or_default();
                }
                work.uptime = crate::linux::read_trim("/proc/uptime").and_then(|s| s.split('.').next()?.parse().ok()).unwrap_or(0);
                work.ready = true;
                *shared.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(work.clone());
                wake();
                tick += 1;
                let iv = Duration::from_millis(cfg.interval_ms.load(Ordering::Relaxed).clamp(250, 10_000));
                if st.sleep(iv.saturating_sub(t0.elapsed())) {
                    break;
                }
            }
        })
        .expect("spawn sampler");
    Sampler { shared: out, stop, handle: Some(handle) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the handle must wake the sampler out of its (long) sleep and join it promptly.
    #[test]
    fn sampler_stops_promptly_and_publishes() {
        let cfg = Arc::new(Config { interval_ms: 10_000.into(), history: 30.into() });
        let s = start(|| {}, cfg);
        let t = std::time::Instant::now();
        while !s.snapshot().ready && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(s.snapshot().ready, "first snapshot published");
        let t = std::time::Instant::now();
        drop(s);
        assert!(t.elapsed() < Duration::from_secs(2), "joined in {:?} despite a 10 s interval", t.elapsed());
    }

    #[test]
    fn stop_sleep_is_interruptible() {
        let st = Arc::new(crate::linux::Stop::default());
        let s2 = st.clone();
        let h = std::thread::spawn(move || s2.sleep(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(50));
        st.trigger();
        assert!(h.join().unwrap());
    }
}
