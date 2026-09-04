use crate::history::MetricsHistory;
use crate::temperature::{ChainedTemperatureProvider, TemperatureProvider};
use crate::temperature_lhm::{FanMetrics, GpuMetrics, SharedLhmExtras};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use sysinfo::{Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};

/// 多窗口同时拉取时，短于此间隔则返回缓存，避免网速差分被打乱
pub const METRICS_CACHE_MIN_INTERVAL: Duration = Duration::from_millis(700);

/// 距上次真实采样超过该间隔（长时间无请求、休眠恢复）时差分失真，本帧网速置 0
pub const NET_DIFF_MAX_INTERVAL: Duration = Duration::from_secs(5);

/// 进程 Top 列表独立降频，避免每秒全量 refresh 进程
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

pub struct MonitorState {
    system: System,
    disks: Disks,
    networks: Networks,
    history: MetricsHistory,
    last_sample_at: Instant,
    last_net_down: u64,
    last_net_up: u64,
    /// 最近一次真实采样结果（供短时缓存）
    last_metrics: Option<Metrics>,
    last_process_top_at: Option<Instant>,
    last_process_top: Option<ProcessTopSnapshot>,
}

impl MonitorState {
    pub fn with_history_capacity(capacity: usize) -> Self {
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
            history: MetricsHistory::new(capacity),
            last_sample_at: Instant::now(),
            last_net_down: net_down,
            last_net_up: net_up,
            last_metrics: None,
            last_process_top_at: None,
            last_process_top: None,
        }
    }

    pub fn set_history_capacity(&mut self, capacity: usize) {
        self.history.set_capacity(capacity);
    }

    /// 进程 Top：与 metrics 缓存独立，默认关闭时不 refresh；开启后按 PROCESS_TOP_MIN_INTERVAL 降频。
    /// 同名进程合并 CPU/内存，展示 count；CPU 需至少两轮 refresh 后才有意义。
    pub fn sample_process_top_cached(&mut self, enabled: bool) -> ProcessTopSnapshot {
        if !enabled {
            let empty = ProcessTopSnapshot {
                enabled: false,
                by_cpu: Vec::new(),
                by_memory: Vec::new(),
                sampled_at_ms: 0,
            };
            self.last_process_top = None;
            self.last_process_top_at = None;
            return empty;
        }

        if let (Some(at), Some(cached)) = (self.last_process_top_at, self.last_process_top.as_ref())
        {
            if at.elapsed() < PROCESS_TOP_MIN_INTERVAL {
                return cached.clone();
            }
        }

        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );

        let mut merged: HashMap<String, (f32, u64, u32)> = HashMap::new();
        for (_, proc) in self.system.processes() {
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
            .map(|(name, (cpu_percent, memory_bytes, count))| ProcessTopEntry {
                name,
                cpu_percent,
                memory_bytes,
                count,
            })
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
        let by_memory = entries;

        let snapshot = ProcessTopSnapshot {
            enabled: true,
            by_cpu,
            by_memory,
            sampled_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        };
        self.last_process_top_at = Some(Instant::now());
        self.last_process_top = Some(snapshot.clone());
        snapshot
    }

    /// 锁内仅完成 sysinfo 采样与网速差分；温度 IO 由 sample_cached_shared 在锁外回填。
    /// 温度先沿用上次读数占位入缓存，避免温度 IO 期间并发请求触发第二路采样打乱网速差分。
    fn sample_system_only(&mut self) -> Metrics {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        self.networks.refresh(true);

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_at);

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

        // 间隔过长时差分不代表当前网速，置 0 比沿用陈旧旧值更不易误导，下轮即恢复正常差分
        let (net_down_bps, net_up_bps) = if elapsed > NET_DIFF_MAX_INTERVAL {
            (0, 0)
        } else {
            let secs = elapsed.as_secs_f64().max(0.001);
            (
                (down_delta as f64 / secs) as u64,
                (up_delta as f64 / secs) as u64,
            )
        };

        let last_temp = self.last_metrics.as_ref().and_then(|m| m.cpu_temp_celsius);
        let last_gpu = self.last_metrics.as_ref().and_then(|m| m.gpu.clone());
        let last_fans = self
            .last_metrics
            .as_ref()
            .map(|m| m.fans.clone())
            .unwrap_or_default();

        let metrics = Metrics {
            cpu_percent: self.system.global_cpu_usage(),
            memory_used_bytes: memory_used,
            memory_total_bytes: memory_total,
            memory_percent,
            swap_used_bytes: self.system.used_swap(),
            swap_total_bytes: self.system.total_swap(),
            net_down_bps,
            net_up_bps,
            disks,
            cpu_temp_celsius: last_temp,
            gpu: last_gpu,
            fans: last_fans,
            sampled_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        };

        self.last_metrics = Some(metrics.clone());
        metrics
    }

    /// 温度读数到位后写入历史并回填缓存；缓存已被更新采样覆盖时跳过，避免历史乱序
    fn complete_sample(&mut self, metrics: Metrics) {
        let is_current = self
            .last_metrics
            .as_ref()
            .is_some_and(|last| last.sampled_at_ms == metrics.sampled_at_ms);
        if is_current {
            self.history.push_from_metrics(&metrics);
            self.last_metrics = Some(metrics);
        }
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
pub type SharedTemperature = Mutex<ChainedTemperatureProvider>;

/// 主面板与叠加层共用的采样入口：距上次真实采样不足 `min_interval` 返回缓存。
/// 真实采样时锁内完成 sysinfo 采样与网速差分，释放锁后再读温度（LHM/WMI IO
/// 最长可达数百毫秒），避免阻塞 get_metrics_history 等共享读者。
/// GPU / 风扇随 LHM 同一次请求写入 lhm_extras，在此回填到 Metrics。
pub fn sample_cached_shared(
    monitor: &SharedMonitor,
    temperature: &SharedTemperature,
    lhm_extras: &SharedLhmExtras,
    min_interval: Duration,
) -> Result<Metrics, String> {
    let (mut metrics, needs_temp) = {
        let mut state = monitor.lock().map_err(|e| e.to_string())?;
        match state.last_metrics {
            Some(ref cached) if state.last_sample_at.elapsed() < min_interval => {
                (cached.clone(), false)
            }
            _ => (state.sample_system_only(), true),
        }
    };

    if needs_temp {
        let temp = {
            let mut provider = temperature.lock().map_err(|e| e.to_string())?;
            provider.read_cpu_temp_celsius()
        };
        metrics.cpu_temp_celsius = temp;
        let extras = lhm_extras.lock().map_err(|e| e.to_string())?.clone();
        metrics.gpu = extras.gpu;
        metrics.fans = extras.fans;

        let mut state = monitor.lock().map_err(|e| e.to_string())?;
        state.complete_sample(metrics.clone());
    }

    Ok(metrics)
}
