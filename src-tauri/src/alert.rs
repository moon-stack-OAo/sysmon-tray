use crate::monitor::Metrics;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_COOLDOWN_SECS: u64 = 60;
const SAMPLE_CONTINUITY_MAX_INTERVAL: Duration = Duration::from_secs(5);
/// 百分比类（CPU / 内存 / 磁盘）解除线相对触发线的回差
const HYSTERESIS_PERCENT: f32 = 3.0;
/// 温度解除线相对触发线的回差（°C）
const HYSTERESIS_TEMP: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    Cpu,
    Memory,
    Temperature,
    Disk,
    Gpu,
}

fn default_disk_percent() -> f32 {
    90.0
}

fn default_gpu_percent() -> f32 {
    90.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertThresholds {
    /// CPU 占用阈值（%）
    pub cpu_percent: f32,
    /// 内存占用阈值（%）
    pub memory_percent: f32,
    /// CPU 温度阈值（°C）；温度为 None 时跳过
    pub cpu_temp_celsius: f32,
    /// 磁盘占用阈值（%）；任一盘达到即告警
    #[serde(default = "default_disk_percent")]
    pub disk_percent: f32,
    /// GPU 占用阈值（%）；无读数时跳过
    #[serde(default = "default_gpu_percent")]
    pub gpu_percent: f32,
    /// 同类告警冷却秒数
    pub cooldown_secs: u64,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            cpu_percent: 90.0,
            memory_percent: 90.0,
            cpu_temp_celsius: 85.0,
            disk_percent: default_disk_percent(),
            gpu_percent: default_gpu_percent(),
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        }
    }
}

impl AlertThresholds {
    /// 校验阈值范围；失败返回可读错误信息
    pub fn validate(&self) -> Result<(), String> {
        if !(1.0..=100.0).contains(&self.cpu_percent) {
            return Err("CPU 阈值须在 1–100".to_string());
        }
        if !(1.0..=100.0).contains(&self.memory_percent) {
            return Err("内存阈值须在 1–100".to_string());
        }
        if !(40.0..=120.0).contains(&self.cpu_temp_celsius) {
            return Err("温度阈值须在 40–120".to_string());
        }
        if !(1.0..=100.0).contains(&self.disk_percent) {
            return Err("磁盘阈值须在 1–100".to_string());
        }
        if !(1.0..=100.0).contains(&self.gpu_percent) {
            return Err("GPU 阈值须在 1–100".to_string());
        }
        if !(5..=3600).contains(&self.cooldown_secs) {
            return Err("冷却秒数须在 5–3600".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertStatus {
    pub cpu: bool,
    pub memory: bool,
    pub temperature: bool,
    pub disk: bool,
    pub gpu: bool,
    pub active: bool,
    pub messages: Vec<String>,
    pub thresholds: AlertThresholds,
}

impl AlertStatus {
    pub fn tooltip_text(&self) -> String {
        if !self.active {
            return "系统监测".to_string();
        }
        format!("⚠ 告警：{}", self.messages.join(" · "))
    }
}

struct AlertObservation {
    exceeded_since: Option<Instant>,
    notified: bool,
}

pub struct AlertEngine {
    thresholds: AlertThresholds,
    last_fired: HashMap<AlertKind, Instant>,
    observations: HashMap<AlertKind, AlertObservation>,
    last_sample: Option<(u128, Instant)>,
}

impl AlertEngine {
    pub fn with_thresholds(thresholds: AlertThresholds) -> Self {
        Self {
            thresholds,
            last_fired: HashMap::new(),
            observations: HashMap::new(),
            last_sample: None,
        }
    }

    pub fn interrupt_sampling(&mut self) {
        self.observations.clear();
        self.last_sample = None;
    }

    pub fn thresholds(&self) -> AlertThresholds {
        self.thresholds.clone()
    }

    pub fn set_thresholds(&mut self, thresholds: AlertThresholds) {
        if self.thresholds != thresholds {
            self.observations.clear();
            self.last_sample = None;
        }
        self.thresholds = thresholds;
    }

    /// 根据当前指标评估告警。不写 last_fired；返回 (状态, 持续超限且已过冷却的待发送通知)。
    /// 通知是否真正发出由调用方决定，成功后再 `mark_fired`。
    pub fn evaluate(
        &mut self,
        metrics: &Metrics,
        sustain: Duration,
    ) -> (AlertStatus, Vec<(AlertKind, String)>) {
        self.evaluate_at(metrics, sustain, Instant::now())
    }

    fn evaluate_at(
        &mut self,
        metrics: &Metrics,
        sustain: Duration,
        now: Instant,
    ) -> (AlertStatus, Vec<(AlertKind, String)>) {
        let observed_at = self.observe_sample(metrics.sampled_at_ms, now);
        let t = self.thresholds.clone();
        let mut messages = Vec::new();
        let mut newly_fired = Vec::new();

        let cpu = self.observe(
            AlertKind::Cpu,
            metrics.cpu_percent,
            t.cpu_percent,
            HYSTERESIS_PERCENT,
            observed_at,
        );
        if cpu {
            messages.push(format!("CPU {:.0}%", metrics.cpu_percent));
            if self.should_fire(AlertKind::Cpu, sustain, observed_at, now) {
                newly_fired.push((
                    AlertKind::Cpu,
                    format!(
                        "CPU 占用过高：{:.1}%（阈值 {:.0}%）",
                        metrics.cpu_percent, t.cpu_percent
                    ),
                ));
            }
        }

        let memory = self.observe(
            AlertKind::Memory,
            metrics.memory_percent,
            t.memory_percent,
            HYSTERESIS_PERCENT,
            observed_at,
        );
        if memory {
            messages.push(format!("内存 {:.0}%", metrics.memory_percent));
            if self.should_fire(AlertKind::Memory, sustain, observed_at, now) {
                newly_fired.push((
                    AlertKind::Memory,
                    format!(
                        "内存占用过高：{:.1}%（阈值 {:.0}%）",
                        metrics.memory_percent, t.memory_percent
                    ),
                ));
            }
        }

        let temperature = match metrics.cpu_temp_celsius {
            Some(temp) => {
                let active = self.observe(
                    AlertKind::Temperature,
                    temp,
                    t.cpu_temp_celsius,
                    HYSTERESIS_TEMP,
                    observed_at,
                );
                if active {
                    messages.push(format!("温度 {:.0}°C", temp));
                    if self.should_fire(AlertKind::Temperature, sustain, observed_at, now) {
                        newly_fired.push((
                            AlertKind::Temperature,
                            format!(
                                "CPU 温度过高：{:.1}°C（阈值 {:.0}°C）",
                                temp, t.cpu_temp_celsius
                            ),
                        ));
                    }
                }
                active
            }
            None => {
                self.clear_latch(AlertKind::Temperature);
                false
            }
        };

        let mut disk_peak = 0.0_f32;
        let mut disk_notify = Vec::new();
        for d in &metrics.disks {
            if !d.used_percent.is_finite() {
                continue;
            }
            if d.used_percent > disk_peak {
                disk_peak = d.used_percent;
            }
            if d.used_percent < t.disk_percent {
                continue;
            }
            let mount = if d.mount_point.is_empty() {
                d.name.as_str()
            } else {
                d.mount_point.as_str()
            };
            disk_notify.push(format!(
                "磁盘 {mount} 占用过高：{:.1}%（阈值 {:.0}%）",
                d.used_percent, t.disk_percent
            ));
        }
        let disk = self.observe(
            AlertKind::Disk,
            disk_peak,
            t.disk_percent,
            HYSTERESIS_PERCENT,
            observed_at,
        );
        if disk {
            let release = (t.disk_percent - HYSTERESIS_PERCENT).max(0.0);
            for d in &metrics.disks {
                if !d.used_percent.is_finite() || d.used_percent < release {
                    continue;
                }
                let mount = if d.mount_point.is_empty() {
                    d.name.as_str()
                } else {
                    d.mount_point.as_str()
                };
                messages.push(format!("{mount} {:.0}%", d.used_percent));
            }
            if self.should_fire(AlertKind::Disk, sustain, observed_at, now) {
                for msg in disk_notify {
                    newly_fired.push((AlertKind::Disk, msg));
                }
            }
        }

        let gpu_load = metrics.gpu.as_ref().and_then(|g| g.load_percent);
        let gpu = match gpu_load {
            Some(load) => {
                let active = self.observe(
                    AlertKind::Gpu,
                    load,
                    t.gpu_percent,
                    HYSTERESIS_PERCENT,
                    observed_at,
                );
                if active {
                    messages.push(format!("GPU {:.0}%", load));
                    if self.should_fire(AlertKind::Gpu, sustain, observed_at, now) {
                        newly_fired.push((
                            AlertKind::Gpu,
                            format!("GPU 占用过高：{:.1}%（阈值 {:.0}%）", load, t.gpu_percent),
                        ));
                    }
                }
                active
            }
            None => {
                self.clear_latch(AlertKind::Gpu);
                false
            }
        };

        let active = cpu || memory || temperature || disk || gpu;
        (
            AlertStatus {
                cpu,
                memory,
                temperature,
                disk,
                gpu,
                active,
                messages,
                thresholds: t,
            },
            newly_fired,
        )
    }

    /// 仅在通知实际 show 成功后记账，避免评估阶段或通知关闭时吃掉冷却。
    pub fn mark_fired(&mut self, kinds: &[AlertKind]) {
        self.mark_fired_at(kinds, Instant::now());
    }

    fn mark_fired_at(&mut self, kinds: &[AlertKind], now: Instant) {
        for kind in kinds {
            self.last_fired.insert(*kind, now);
            if let Some(observation) = self.observations.get_mut(kind) {
                observation.notified = true;
            }
        }
    }

    fn clear_latch(&mut self, kind: AlertKind) {
        self.observations.remove(&kind);
    }

    fn observe_sample(&mut self, sampled_at_ms: u128, now: Instant) -> Instant {
        if let Some((previous_ms, previous_at)) = self.last_sample {
            let interrupted = now.saturating_duration_since(previous_at)
                > SAMPLE_CONTINUITY_MAX_INTERVAL
                || sampled_at_ms < previous_ms
                || sampled_at_ms.saturating_sub(previous_ms)
                    > SAMPLE_CONTINUITY_MAX_INTERVAL.as_millis();
            if interrupted {
                self.observations.clear();
            } else if sampled_at_ms == previous_ms {
                return previous_at;
            }
        }
        self.last_sample = Some((sampled_at_ms, now));
        now
    }

    fn observe(
        &mut self,
        kind: AlertKind,
        value: f32,
        threshold: f32,
        hysteresis: f32,
        observed_at: Instant,
    ) -> bool {
        let release = (threshold - hysteresis).max(0.0);
        if !value.is_finite() || value < release {
            self.clear_latch(kind);
            return false;
        }
        if value >= threshold {
            self.observations
                .entry(kind)
                .or_insert(AlertObservation {
                    exceeded_since: None,
                    notified: false,
                })
                .exceeded_since
                .get_or_insert(observed_at);
        } else if let Some(observation) = self.observations.get_mut(&kind) {
            observation.exceeded_since = None;
        }
        self.observations.contains_key(&kind)
    }

    fn should_fire(
        &self,
        kind: AlertKind,
        sustain: Duration,
        observed_at: Instant,
        now: Instant,
    ) -> bool {
        let Some(observation) = self.observations.get(&kind) else {
            return false;
        };
        if observation.notified
            || observation
                .exceeded_since
                .is_none_or(|since| observed_at.saturating_duration_since(since) < sustain)
        {
            return false;
        }
        let cooldown = Duration::from_secs(self.thresholds.cooldown_secs.max(1));
        match self.last_fired.get(&kind) {
            Some(last) if now.saturating_duration_since(*last) < cooldown => false,
            _ => true,
        }
    }
}

pub type SharedAlerts = Mutex<AlertEngine>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::DiskStat;
    use crate::temperature_lhm::GpuMetrics;

    const SUSTAIN: Duration = Duration::from_secs(5);

    fn metrics(second: u64) -> Metrics {
        Metrics {
            cpu_percent: 95.0,
            memory_used_bytes: 0,
            memory_total_bytes: 0,
            memory_percent: 0.0,
            swap_used_bytes: 0,
            swap_total_bytes: 0,
            net_down_bps: 0,
            net_up_bps: 0,
            net_interface: None,
            net_link: "down".to_string(),
            disks: Vec::new(),
            cpu_temp_celsius: None,
            gpu: None,
            fans: Vec::new(),
            sampled_at_ms: 1_800_000_000_000 + u128::from(second) * 1000,
        }
    }

    fn tick(
        engine: &mut AlertEngine,
        start: Instant,
        second: u64,
        value: &Metrics,
    ) -> (AlertStatus, Vec<(AlertKind, String)>) {
        engine.evaluate_at(value, SUSTAIN, start + Duration::from_secs(second))
    }

    fn gpu(load: f32) -> GpuMetrics {
        GpuMetrics {
            name: "test".to_string(),
            load_percent: Some(load),
            memory_used_bytes: None,
            memory_total_bytes: None,
            memory_percent: None,
            temp_celsius: None,
        }
    }

    #[test]
    fn sustained_high_load_fires_after_five_seconds_and_success_deduplicates() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        for second in 0..5 {
            let (status, pending) = tick(&mut engine, start, second, &metrics(second));
            assert!(status.cpu);
            assert!(pending.is_empty());
        }
        let (_, pending) = tick(&mut engine, start, 5, &metrics(5));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, AlertKind::Cpu);
        engine.mark_fired_at(&[AlertKind::Cpu], start + Duration::from_secs(5));
        for second in 6..70 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
    }

    #[test]
    fn short_spike_and_dip_within_hysteresis_restart_sustain() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        for second in 0..4 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        let mut dip = metrics(4);
        dip.cpu_percent = 89.0;
        let (status, pending) = tick(&mut engine, start, 4, &dip);
        assert!(status.cpu);
        assert!(pending.is_empty());
        for second in 5..10 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 10, &metrics(10)).1.len(), 1);
        let mut recovered = metrics(11);
        recovered.cpu_percent = 86.0;
        let (status, pending) = tick(&mut engine, start, 11, &recovered);
        assert!(!status.cpu);
        assert!(pending.is_empty());
    }

    #[test]
    fn failed_or_suppressed_notifications_remain_pending_until_success() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        for second in 0..5 {
            tick(&mut engine, start, second, &metrics(second));
        }
        for second in 5..12 {
            assert_eq!(
                tick(&mut engine, start, second, &metrics(second)).1.len(),
                1
            );
        }
        engine.mark_fired_at(&[AlertKind::Cpu], start + Duration::from_secs(11));
        assert!(tick(&mut engine, start, 12, &metrics(12)).1.is_empty());
    }

    #[test]
    fn recovered_episode_waits_for_cooldown_then_notifies_without_another_edge() {
        let start = Instant::now();
        let mut thresholds = AlertThresholds::default();
        thresholds.cooldown_secs = 10;
        let mut engine = AlertEngine::with_thresholds(thresholds);
        for second in 0..=5 {
            tick(&mut engine, start, second, &metrics(second));
        }
        engine.mark_fired_at(&[AlertKind::Cpu], start + Duration::from_secs(5));
        let mut recovered = metrics(6);
        recovered.cpu_percent = 0.0;
        tick(&mut engine, start, 6, &recovered);
        for second in 7..15 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 15, &metrics(15)).1.len(), 1);
    }

    #[test]
    fn cached_frame_does_not_advance_sustain() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        let cached = metrics(0);
        tick(&mut engine, start, 0, &cached);
        assert!(tick(&mut engine, start, 5, &cached).1.is_empty());
        assert!(tick(&mut engine, start, 6, &metrics(6)).1.is_empty());
        for second in 7..11 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 11, &metrics(11)).1.len(), 1);
    }

    #[test]
    fn interrupted_sampling_and_clock_changes_restart_sustain() {
        for (observed_second, sample_second) in [(20, 20), (5, 20), (5, 1)] {
            let start = Instant::now();
            let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
            for second in 0..5 {
                tick(&mut engine, start, second, &metrics(second));
            }
            for offset in 0..5 {
                let sample = metrics(sample_second + offset);
                assert!(tick(&mut engine, start, observed_second + offset, &sample)
                    .1
                    .is_empty());
            }
            assert_eq!(
                tick(
                    &mut engine,
                    start,
                    observed_second + 5,
                    &metrics(sample_second + 5),
                )
                .1
                .len(),
                1
            );
        }
    }

    #[test]
    fn missing_temperature_and_gpu_clear_their_pending_episodes() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        for second in 0..5 {
            let mut value = metrics(second);
            value.cpu_percent = 0.0;
            value.cpu_temp_celsius = Some(95.0);
            value.gpu = Some(gpu(95.0));
            assert!(tick(&mut engine, start, second, &value).1.is_empty());
        }
        let mut missing = metrics(5);
        missing.cpu_percent = 0.0;
        let (status, pending) = tick(&mut engine, start, 5, &missing);
        assert!(!status.temperature && !status.gpu);
        assert!(pending.is_empty());
        for second in 6..=11 {
            let mut value = metrics(second);
            value.cpu_percent = 0.0;
            value.cpu_temp_celsius = Some(95.0);
            value.gpu = Some(gpu(95.0));
            let (_, pending) = tick(&mut engine, start, second, &value);
            assert_eq!(pending.len(), if second == 11 { 2 } else { 0 });
        }
    }

    #[test]
    fn non_finite_values_clear_state_and_never_generate_invalid_messages() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        let mut value = metrics(0);
        value.memory_percent = 95.0;
        value.cpu_temp_celsius = Some(95.0);
        value.gpu = Some(gpu(95.0));
        tick(&mut engine, start, 0, &value);
        value.sampled_at_ms += 1000;
        value.cpu_percent = f32::NAN;
        value.memory_percent = f32::INFINITY;
        value.cpu_temp_celsius = Some(f32::NAN);
        value.gpu = Some(gpu(f32::INFINITY));
        value.disks = vec![DiskStat {
            name: "bad".to_string(),
            mount_point: "bad".to_string(),
            total_bytes: 0,
            available_bytes: 0,
            used_percent: f32::NAN,
        }];
        let (status, pending) = tick(&mut engine, start, 1, &value);
        assert!(!status.active);
        assert!(pending.is_empty());
        assert!(status.messages.is_empty());
        for second in 2..7 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 7, &metrics(7)).1.len(), 1);
    }

    #[test]
    fn changed_thresholds_restart_sustain_and_preserve_successful_cooldown() {
        let start = Instant::now();
        let mut thresholds = AlertThresholds::default();
        thresholds.cooldown_secs = 10;
        let mut engine = AlertEngine::with_thresholds(thresholds.clone());
        for second in 0..4 {
            tick(&mut engine, start, second, &metrics(second));
        }
        thresholds.cpu_percent = 94.0;
        engine.set_thresholds(thresholds.clone());
        for second in 4..9 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 9, &metrics(9)).1.len(), 1);
        engine.mark_fired_at(&[AlertKind::Cpu], start + Duration::from_secs(9));
        thresholds.cpu_percent = 93.0;
        engine.set_thresholds(thresholds);
        for second in 10..19 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 19, &metrics(19)).1.len(), 1);
    }

    #[test]
    fn zero_sustain_notifies_immediately_but_only_success_marks_episode() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        assert_eq!(
            engine
                .evaluate_at(&metrics(0), Duration::ZERO, start)
                .1
                .len(),
            1
        );
        assert_eq!(
            engine
                .evaluate_at(&metrics(0), Duration::ZERO, start)
                .1
                .len(),
            1
        );
        engine.mark_fired_at(&[AlertKind::Cpu], start);
        assert!(engine
            .evaluate_at(&metrics(0), Duration::ZERO, start)
            .1
            .is_empty());
    }

    #[test]
    fn threshold_change_does_not_reuse_a_cached_frames_old_observation_time() {
        let start = Instant::now();
        let mut thresholds = AlertThresholds::default();
        let mut engine = AlertEngine::with_thresholds(thresholds.clone());
        tick(&mut engine, start, 0, &metrics(0));
        thresholds.cpu_percent = 94.0;
        engine.set_thresholds(thresholds);
        assert!(tick(&mut engine, start, 4, &metrics(0)).1.is_empty());
        for second in 5..9 {
            assert!(tick(&mut engine, start, second, &metrics(second))
                .1
                .is_empty());
        }
        assert_eq!(tick(&mut engine, start, 9, &metrics(9)).1.len(), 1);
    }

    #[test]
    fn every_metric_sustains_independently_and_invalid_disks_are_ignored() {
        let start = Instant::now();
        let mut engine = AlertEngine::with_thresholds(AlertThresholds::default());
        for second in 0..=5 {
            let mut value = metrics(second);
            value.memory_percent = 95.0;
            value.cpu_temp_celsius = Some(95.0);
            value.gpu = Some(gpu(95.0));
            value.disks = [f32::NAN, 95.0]
                .into_iter()
                .enumerate()
                .map(|(index, used_percent)| DiskStat {
                    name: index.to_string(),
                    mount_point: index.to_string(),
                    total_bytes: 0,
                    available_bytes: 0,
                    used_percent,
                })
                .collect();
            let (status, pending) = tick(&mut engine, start, second, &value);
            assert!(status.cpu && status.memory && status.temperature && status.disk && status.gpu);
            assert!(status.messages.iter().all(|text| !text.contains("NaN")));
            assert_eq!(pending.len(), if second == 5 { 5 } else { 0 });
        }
    }
}
