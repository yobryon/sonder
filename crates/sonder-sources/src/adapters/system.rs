//! `system.metrics` — CPU, memory, network and process counts of the host via `sysinfo`.

use std::time::Duration;

use sonder_core::{SourceSpec, SourceState};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

struct SystemSource {
    interval: Duration,
    processes: bool,
}

/// Pure derivation of the readings and events from two consecutive snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Snapshot {
    pub cpu: f64,
    pub mem: f64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub load1: f64,
    pub process_count: f64,
}

/// Events to emit between two snapshots.
pub fn detect_events(prev: Option<&Snapshot>, cur: &Snapshot) -> Vec<(&'static str, f64)> {
    let mut out = Vec::new();
    if let Some(p) = prev {
        if cur.process_count > p.process_count {
            out.push(("proc_spawn", clamp01((cur.process_count - p.process_count) / 5.0)));
        }
        if cur.cpu - p.cpu > 0.3 {
            out.push(("cpu_spike", clamp01(cur.cpu)));
        }
    }
    out
}

impl Source for SystemSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            em.status(SourceState::Connecting, "sampling host").await;
            let mut sys = System::new_with_specifics(
                RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage()).with_memory(MemoryRefreshKind::nothing().with_ram()),
            );
            let mut nets = Networks::new_with_refreshed_list();
            let mut prev: Option<Snapshot> = None;
            let mut prev_wall = std::time::Instant::now();
            // First CPU sample is meaningless; warm up briefly.
            if !em.sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250))).await {
                return;
            }
            loop {
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                let process_count = if self.processes {
                    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
                    sys.processes().len() as f64
                } else {
                    0.0
                };
                nets.refresh(true);
                let (mut rx, mut tx) = (0u64, 0u64);
                for (_, data) in nets.iter() {
                    rx = rx.saturating_add(data.total_received());
                    tx = tx.saturating_add(data.total_transmitted());
                }
                let total_mem = sys.total_memory().max(1) as f64;
                let cur = Snapshot {
                    cpu: clamp01(f64::from(sys.global_cpu_usage()) / 100.0),
                    mem: clamp01(sys.used_memory() as f64 / total_mem),
                    rx_total: rx,
                    tx_total: tx,
                    load1: System::load_average().one,
                    process_count,
                };
                let now_wall = std::time::Instant::now();
                let dt = now_wall.duration_since(prev_wall).as_secs_f64().max(1e-3);
                prev_wall = now_wall;
                let (net_rx, net_tx) = match prev {
                    Some(p) => (
                        cur.rx_total.saturating_sub(p.rx_total) as f64 / dt,
                        cur.tx_total.saturating_sub(p.tx_total) as f64 / dt,
                    ),
                    None => (0.0, 0.0),
                };
                let mut vals = vec![
                    ("cpu", cur.cpu),
                    ("mem", cur.mem),
                    ("net_rx", net_rx),
                    ("net_tx", net_tx),
                    ("load1", cur.load1),
                ];
                if self.processes {
                    vals.push(("process_count", cur.process_count));
                }
                em.reading(vals).await;
                for (name, intensity) in detect_events(prev.as_ref(), &cur) {
                    let attrs = Attrs::new().num("cpu", cur.cpu).num("process_count", cur.process_count);
                    em.event(name, intensity, attrs.into_map()).await;
                }
                prev = Some(cur);
                em.status(SourceState::Live, "live").await;
                if !em.sleep(self.interval).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "system.metrics",
        summary: "Host CPU/memory/network/process metrics via sysinfo",
        params: vec![
            ParamInfo { name: "interval", summary: "Seconds between readings", default: Some("1") },
            ParamInfo { name: "processes", summary: "Count processes (slightly more expensive)", default: Some("true") },
        ],
        channels: vec!["cpu", "mem", "net_rx", "net_tx", "load1", "process_count"],
        events: vec!["proc_spawn", "cpu_spike"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(SystemSource {
        interval: util::param_secs(spec, "interval", 1.0, 0.2)?,
        processes: util::param_bool(spec, "processes", true)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_from_snapshots() {
        let a = Snapshot { cpu: 0.1, process_count: 100.0, ..Default::default() };
        let b = Snapshot { cpu: 0.5, process_count: 102.0, ..Default::default() };
        assert!(detect_events(None, &b).is_empty());
        let evs = detect_events(Some(&a), &b);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].0, "proc_spawn");
        assert_eq!(evs[1].0, "cpu_spike");
        assert!(detect_events(Some(&b), &a).is_empty());
    }
}
