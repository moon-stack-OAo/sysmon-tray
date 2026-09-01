use crate::history::MetricsHistory;
use crate::temperature::{ChainedTemperatureProvider, TemperatureProvider};
use crate::temperature_lhm::{LhmRuntimeConfig, TempSourceTracker};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Disks, Networks, System};

/// 多窗口同时拉取时，短于此间隔则返回缓存，避免网速差分被打乱
pub const METRICS_CACHE_MIN_INTERVAL: Duration = Duration::from_millis(700);

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
    pub disks: Vec<DiskStat>,
    pub cpu_temp_celsius: Option<f32>,
    pub sampled_at_ms: u128,
}

pub struct MonitorState {
    system: System,
    disks: Disks,
    networks: Networks,
    temperature: ChainedTemperatureProvider,
    history: MetricsHistory,
    last_sample_at: Instant,
    last_net_down: u64,
    last_net_up: u64,
    /// 最近一次真实采样结果（供短时缓存）
    last_metrics: Option<Metrics>,
}

impl MonitorState {
    pub fn with_history_capacity(
        capacity: usize,
        lhm_runtime: Arc<LhmRuntimeConfig>,
        temp_tracker: Arc<TempSourceTracker>,
    ) -> Self {
        let mut system = System::new_all();
        system.refresh_all();

        let disks = Disks::new_with_refreshed_list();
        let networks = Networks::new_with_refreshed_list();

        let (net_down, net_up) = sum_network(&networks);

        // 第一次 CPU 采样通常不准，先刷新一次再睡最小间隔
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        system.refresh_cpu_usage();

        Self {
            system,
            disks,
            networks,
            temperature: ChainedTemperatureProvider::platform_default(lhm_runtime, temp_tracker),
            history: MetricsHistory::new(capacity),
            last_sample_at: Instant::now(),
            last_net_down: net_down,
            last_net_up: net_up,
            last_metrics: None,
        }
    }

    pub fn set_history_capacity(&mut self, capacity: usize) {
        self.history.set_capacity(capacity);
    }

    /// 距上次真实采样不足 `min_interval` 时返回缓存，避免双窗口打乱网速差分。
    pub fn sample_cached(&mut self, min_interval: Duration) -> Metrics {
        if let Some(ref cached) = self.last_metrics {
            if self.last_sample_at.elapsed() < min_interval {
                return cached.clone();
            }
        }
        self.sample()
    }

    pub fn sample(&mut self) -> Metrics {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        self.networks.refresh(true);

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_at).as_secs_f64().max(0.001);

        let (net_down, net_up) = sum_network(&self.networks);
        let down_delta = net_down.saturating_sub(self.last_net_down);
        let up_delta = net_up.saturating_sub(self.last_net_up);

        self.last_net_down = net_down;
        self.last_net_up = net_up;
        self.last_sample_at = now;

        let memory_total = self.system.total_memory();
        let memory_used = self.system.used_memory();
        let memory_percent = if memory_total == 0 {
            0.0
        } else {
            (memory_used as f64 / memory_total as f64 * 100.0) as f32
        };

        let disks = self
            .disks
            .iter()
            .map(|disk| {
                let total = disk.total_space();
                let available = disk.available_space();
                let used_percent = if total == 0 {
                    0.0
                } else {
                    ((total - available) as f64 / total as f64 * 100.0) as f32
                };

                DiskStat {
                    name: disk.name().to_string_lossy().to_string(),
                    mount_point: disk.mount_point().to_string_lossy().to_string(),
                    total_bytes: total,
                    available_bytes: available,
                    used_percent,
                }
            })
            .collect();

        let metrics = Metrics {
            cpu_percent: self.system.global_cpu_usage(),
            memory_used_bytes: memory_used,
            memory_total_bytes: memory_total,
            memory_percent,
            swap_used_bytes: self.system.used_swap(),
            swap_total_bytes: self.system.total_swap(),
            net_down_bps: (down_delta as f64 / elapsed) as u64,
            net_up_bps: (up_delta as f64 / elapsed) as u64,
            disks,
            cpu_temp_celsius: self.temperature.read_cpu_temp_celsius(),
            sampled_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        };

        self.history.push_from_metrics(&metrics);
        self.last_metrics = Some(metrics.clone());
        metrics
    }

    pub fn history_snapshot(&self) -> Vec<crate::history::HistoryPoint> {
        self.history.snapshot()
    }
}

fn sum_network(networks: &Networks) -> (u64, u64) {
    networks.iter().fold((0u64, 0u64), |(down, up), (_, data)| {
        (
            down.saturating_add(data.total_received()),
            up.saturating_add(data.total_transmitted()),
        )
    })
}

pub type SharedMonitor = Mutex<MonitorState>;
