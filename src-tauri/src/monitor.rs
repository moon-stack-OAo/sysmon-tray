use crate::history::MetricsHistory;
use crate::network::{InterfaceCounter, NetworkInterface, NetworkSampler, NetworkSelection};
use crate::temperature::{ChainedTemperatureProvider, TemperatureProvider};
use crate::temperature_lhm::{FanMetrics, GpuMetrics, SharedLhmExtras};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{DiskRefreshKind, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
pub const TEMPERATURE_INTERVAL: Duration = Duration::from_secs(2);
pub const DISK_CAPACITY_INTERVAL: Duration = Duration::from_secs(15);
pub const DISK_DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

pub const PROCESS_TOP_MIN_INTERVAL: Duration = Duration::from_secs(4);

pub const PROCESS_TOP_N: usize = 5;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskStat {
    pub name: String,
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub memory_percent: f32,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub net_down_bps: u64,
    pub net_up_bps: u64,
    /// 与本帧速率对应的接口名；汇总模式明确标记为全部接口
    pub net_interface: Option<String>,
    /// `up` | `down`；指定接口不存在时为 down，空闲不代表断网
    pub net_link: String,
    pub disks: Vec<DiskStat>,
    pub cpu_temp_celsius: Option<f32>,
    /// 仅 LHM 可用；无可靠低成本回退时为 None
    pub gpu: Option<GpuMetrics>,
    /// 仅 LHM 可用；无数据时为空列表
    pub fans: Vec<FanMetrics>,
    pub sampled_at_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTopEntry {
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTopSnapshot {
    pub enabled: bool,
    pub by_cpu: Vec<ProcessTopEntry>,
    pub by_memory: Vec<ProcessTopEntry>,
    pub sampled_at_ms: u128,
}

#[derive(Default)]
pub struct RefreshSchedule {
    temperature: Option<Instant>,
    disk_capacity: Option<Instant>,
    disk_discovery: Option<Instant>,
}

impl RefreshSchedule {
    fn due(last: &mut Option<Instant>, now: Instant, interval: Duration) -> bool {
        if last.is_none_or(|at| now.saturating_duration_since(at) >= interval) {
            *last = Some(now);
            true
        } else {
            false
        }
    }
}

pub struct MonitorState {
    system: System,
    disks: Disks,
    networks: Networks,
    history: MetricsHistory,
    last_sample_at: Option<Instant>,
    network_sampler: NetworkSampler,
    interfaces: Vec<NetworkInterface>,
    schedule: RefreshSchedule,
    disk_stats: Vec<DiskStat>,
    last_metrics: Option<Metrics>,
    process_system: System,
    last_process_top_at: Option<Instant>,
    last_process_top: Option<ProcessTopSnapshot>,
    process_refresh_in_progress: Arc<AtomicBool>,
}

impl MonitorState {
    pub fn with_history_capacity(capacity: usize) -> Self {
        Self {
            system: System::new(),
            disks: Disks::new(),
            networks: Networks::new(),
            history: MetricsHistory::new(capacity),
            last_sample_at: None,
            network_sampler: NetworkSampler::default(),
            interfaces: Vec::new(),
            schedule: RefreshSchedule::default(),
            disk_stats: Vec::new(),
            last_metrics: None,
            process_system: System::new(),
            last_process_top_at: None,
            last_process_top: None,
            process_refresh_in_progress: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn prime_cpu(&mut self) {
        self.system.refresh_cpu_usage();
    }

    pub fn history_batch(&self, cursor: Option<u64>) -> crate::history::HistoryBatch {
        self.history.batch(cursor)
    }

    pub fn set_history_capacity(&mut self, capacity: usize) {
        self.history.set_capacity(capacity);
    }

    pub fn network_interfaces(&self) -> Vec<NetworkInterface> {
        self.interfaces.clone()
    }

    fn process_top_cache_hit(&self, enabled: bool) -> Option<ProcessTopSnapshot> {
        if !enabled {
            return Some(ProcessTopSnapshot {
                enabled: false,
                by_cpu: Vec::new(),
                by_memory: Vec::new(),
                sampled_at_ms: 0,
            });
        }
        if let (Some(at), Some(cached)) = (self.last_process_top_at, self.last_process_top.as_ref())
        {
            if at.elapsed() < PROCESS_TOP_MIN_INTERVAL {
                return Some(cached.clone());
            }
        }
        None
    }

    fn clear_process_top_cache(&mut self) {
        self.last_process_top = None;
        self.last_process_top_at = None;
    }

    fn store_process_top(&mut self, snapshot: ProcessTopSnapshot) {
        self.last_process_top_at = Some(Instant::now());
        self.last_process_top = Some(snapshot);
    }

    fn sample_system_only(&mut self, selection: &NetworkSelection) -> (Metrics, bool) {
        let now = Instant::now();
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        let discover = RefreshSchedule::due(
            &mut self.schedule.disk_discovery,
            now,
            DISK_DISCOVERY_INTERVAL,
        );
        let capacity = RefreshSchedule::due(
            &mut self.schedule.disk_capacity,
            now,
            DISK_CAPACITY_INTERVAL,
        );
        if discover {
            self.disks
                .refresh_specifics(true, DiskRefreshKind::nothing().with_kind().with_storage());
        } else if capacity {
            for disk in self.disks.list_mut() {
                disk.refresh_specifics(DiskRefreshKind::nothing().with_storage());
            }
        }
        if discover || capacity {
            self.disk_stats = self
                .disks
                .iter()
                .map(|disk| {
                    let total = disk.total_space();
                    let available = disk.available_space();
                    DiskStat {
                        name: disk.name().to_string_lossy().to_string(),
                        mount_point: disk.mount_point().to_string_lossy().to_string(),
                        total_bytes: total,
                        available_bytes: available,
                        used_percent: if total == 0 {
                            0.0
                        } else {
                            (total.saturating_sub(available) as f64 / total as f64 * 100.0) as f32
                        },
                    }
                })
                .collect();
            self.disk_stats.sort_by_key(|disk| {
                (
                    disk.mount_point.trim().is_empty(),
                    disk.mount_point.to_ascii_lowercase(),
                    disk.name.to_ascii_lowercase(),
                )
            });
        }
        self.networks.refresh(true);
        let counters: Vec<_> = self
            .networks
            .iter()
            .map(|(name, data)| InterfaceCounter {
                name: name.clone(),
                received: data.total_received(),
                transmitted: data.total_transmitted(),
                has_address: !data.ip_networks().is_empty(),
            })
            .collect();
        self.interfaces = NetworkSampler::interfaces(&counters);
        let network_now = Instant::now();
        let elapsed = self
            .last_sample_at
            .map(|at| network_now.saturating_duration_since(at))
            .unwrap_or_default();
        let network = self.network_sampler.sample(&counters, selection, elapsed);
        self.last_sample_at = Some(network_now);
        let memory_total = self.system.total_memory();
        let memory_used = self.system.used_memory();
        let needs_temp =
            RefreshSchedule::due(&mut self.schedule.temperature, now, TEMPERATURE_INTERVAL);
        let metrics = Metrics {
            cpu_percent: self.system.global_cpu_usage(),
            memory_used_bytes: memory_used,
            memory_total_bytes: memory_total,
            memory_percent: if memory_total == 0 {
                0.0
            } else {
                (memory_used as f64 / memory_total as f64 * 100.0) as f32
            },
            swap_used_bytes: self.system.used_swap(),
            swap_total_bytes: self.system.total_swap(),
            net_down_bps: network.down_bps,
            net_up_bps: network.up_bps,
            net_interface: network.interface,
            net_link: if network.available { "up" } else { "down" }.to_string(),
            disks: self.disk_stats.clone(),
            cpu_temp_celsius: self.last_metrics.as_ref().and_then(|m| m.cpu_temp_celsius),
            gpu: self.last_metrics.as_ref().and_then(|m| m.gpu.clone()),
            fans: self
                .last_metrics
                .as_ref()
                .map(|m| m.fans.clone())
                .unwrap_or_default(),
            sampled_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        };
        (metrics, needs_temp)
    }

    fn complete_sample(&mut self, metrics: Metrics) {
        self.history.push_from_metrics(&metrics);
        self.last_metrics = Some(metrics);
    }

    pub fn history_snapshot(&self) -> Vec<crate::history::HistoryPoint> {
        self.history.snapshot()
    }
}

pub type SharedMonitor = Mutex<MonitorState>;
pub type SharedTemperature = Mutex<ChainedTemperatureProvider>;

fn empty_process_top() -> ProcessTopSnapshot {
    ProcessTopSnapshot {
        enabled: true,
        by_cpu: Vec::new(),
        by_memory: Vec::new(),
        sampled_at_ms: 0,
    }
}

fn build_process_top(system: &System) -> ProcessTopSnapshot {
    let mut merged: HashMap<String, (f32, u64, u32)> = HashMap::new();
    for (_, proc) in system.processes() {
        let name = proc.name().to_string_lossy().to_string();
        if name.is_empty() {
            continue;
        }
        let entry = merged.entry(name).or_insert((0.0, 0, 0));
        entry.0 += proc.cpu_usage();
        entry.1 = entry.1.saturating_add(proc.memory());
        entry.2 = entry.2.saturating_add(1);
    }

    let mut entries: Vec<ProcessTopEntry> = merged
        .into_iter()
        .map(
            |(name, (cpu_percent, memory_bytes, count))| ProcessTopEntry {
                name,
                cpu_percent,
                memory_bytes,
                count,
            },
        )
        .collect();

    let mut by_cpu = entries.clone();
    by_cpu.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    by_cpu.truncate(PROCESS_TOP_N);

    entries.sort_by(|a, b| {
        b.memory_bytes
            .cmp(&a.memory_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    entries.truncate(PROCESS_TOP_N);

    ProcessTopSnapshot {
        enabled: true,
        by_cpu,
        by_memory: entries,
        sampled_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    }
}

struct ProcessRefreshFlag(Arc<AtomicBool>);

impl Drop for ProcessRefreshFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 进程列表刷新离开 SharedMonitor：短锁 swap 出独立的 process_system，锁外 refresh，再短锁写回。
/// 刷新进行中的并发调用只返回上一份缓存（没有则返回空列表），不阻塞 get_metrics。
pub fn sample_process_top_shared(
    monitor: &SharedMonitor,
    enabled: bool,
) -> Result<ProcessTopSnapshot, String> {
    let (mut process_system, _flag) = {
        let mut state = monitor.lock().map_err(|e| e.to_string())?;
        if let Some(cached) = state.process_top_cache_hit(enabled) {
            if !enabled {
                state.clear_process_top_cache();
            }
            return Ok(cached);
        }
        if state
            .process_refresh_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(state
                .last_process_top
                .clone()
                .unwrap_or_else(empty_process_top));
        }
        let flag = ProcessRefreshFlag(Arc::clone(&state.process_refresh_in_progress));
        let taken = std::mem::replace(&mut state.process_system, System::new());
        (taken, flag)
    };

    process_system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let snapshot = build_process_top(&process_system);

    {
        let mut state = monitor.lock().map_err(|e| e.to_string())?;
        state.process_system = process_system;
        state.store_process_top(snapshot.clone());
    }

    Ok(snapshot)
}

pub fn sample_cached_shared(
    monitor: &SharedMonitor,
    temperature: &SharedTemperature,
    lhm_extras: &SharedLhmExtras,
    selection: &NetworkSelection,
) -> Result<Metrics, String> {
    let (mut metrics, needs_temp) = monitor
        .lock()
        .map_err(|e| e.to_string())?
        .sample_system_only(selection);
    if needs_temp {
        let mut provider = temperature.lock().map_err(|e| e.to_string())?;
        metrics.cpu_temp_celsius = provider.read_cpu_temp_celsius();
        let extras = lhm_extras.lock().map_err(|e| e.to_string())?.clone();
        metrics.gpu = extras.gpu;
        metrics.fans = extras.fans;
    }
    monitor
        .lock()
        .map_err(|e| e.to_string())?
        .complete_sample(metrics.clone());
    Ok(metrics)
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    #[test]
    fn slow_collectors_are_independent_and_do_not_catch_up_after_suspend() {
        let start = Instant::now();
        let mut schedule = RefreshSchedule::default();
        let mut temperatures = 0;
        let mut disks = 0;
        let mut discoveries = 0;
        for second in 0..30 {
            let now = start + Duration::from_secs(second);
            temperatures += usize::from(RefreshSchedule::due(
                &mut schedule.temperature,
                now,
                TEMPERATURE_INTERVAL,
            ));
            disks += usize::from(RefreshSchedule::due(
                &mut schedule.disk_capacity,
                now,
                DISK_CAPACITY_INTERVAL,
            ));
            discoveries += usize::from(RefreshSchedule::due(
                &mut schedule.disk_discovery,
                now,
                DISK_DISCOVERY_INTERVAL,
            ));
        }
        assert_eq!((temperatures, disks, discoveries), (15, 2, 1));
        let resumed = start + Duration::from_secs(300);
        assert!(RefreshSchedule::due(
            &mut schedule.temperature,
            resumed,
            TEMPERATURE_INTERVAL
        ));
        assert!(!RefreshSchedule::due(
            &mut schedule.temperature,
            resumed,
            TEMPERATURE_INTERVAL
        ));
    }
}
