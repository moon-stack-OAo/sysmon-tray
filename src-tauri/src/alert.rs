use crate::monitor::Metrics;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_COOLDOWN_SECS: u64 = 60;
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
}

fn default_disk_percent() -> f32 {
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

pub struct AlertEngine {
    thresholds: AlertThresholds,
    last_fired: HashMap<AlertKind, Instant>,
    /// 已越过触发线；降到解除线以下才清除。latched 期间不再 fire，避免贴线抖动连发。
    latched: HashMap<AlertKind, bool>,
}

impl AlertEngine {
    pub fn with_thresholds(thresholds: AlertThresholds) -> Self {
        Self {
            thresholds,
            last_fired: HashMap::new(),
            latched: HashMap::new(),
        }
    }

    pub fn thresholds(&self) -> AlertThresholds {
        self.thresholds.clone()
    }

    pub fn set_thresholds(&mut self, thresholds: AlertThresholds) {
        self.thresholds = thresholds;
    }

    /// 根据当前指标评估告警。不写 last_fired；返回 (状态, 本次新触发且已过冷却的通知)。
    /// 通知是否真正发出由调用方决定，成功后再 `mark_fired`。
    pub fn evaluate(
        &mut self,
        metrics: &Metrics,
    ) -> (AlertStatus, Vec<(AlertKind, String)>) {
        let t = self.thresholds.clone();
        let mut messages = Vec::new();
        let mut newly_fired = Vec::new();

        let cpu_rising = self.rising_edge(AlertKind::Cpu, metrics.cpu_percent, t.cpu_percent);
        let cpu = self.observe(
            AlertKind::Cpu,
            metrics.cpu_percent,
            t.cpu_percent,
            HYSTERESIS_PERCENT,
        );
        if cpu {
            messages.push(format!("CPU {:.0}%", metrics.cpu_percent));
            if cpu_rising && self.should_fire(AlertKind::Cpu) {
                newly_fired.push((
                    AlertKind::Cpu,
                    format!(
                        "CPU 占用过高：{:.1}%（阈值 {:.0}%）",
                        metrics.cpu_percent, t.cpu_percent
                    ),
                ));
            }
        }

        let memory_rising =
            self.rising_edge(AlertKind::Memory, metrics.memory_percent, t.memory_percent);
        let memory = self.observe(
            AlertKind::Memory,
            metrics.memory_percent,
            t.memory_percent,
            HYSTERESIS_PERCENT,
        );
        if memory {
            messages.push(format!("内存 {:.0}%", metrics.memory_percent));
            if memory_rising && self.should_fire(AlertKind::Memory) {
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
                let rising = self.rising_edge(AlertKind::Temperature, temp, t.cpu_temp_celsius);
                let active = self.observe(
                    AlertKind::Temperature,
                    temp,
                    t.cpu_temp_celsius,
                    HYSTERESIS_TEMP,
                );
                if active {
                    messages.push(format!("温度 {:.0}°C", temp));
                    if rising && self.should_fire(AlertKind::Temperature) {
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
        let disk_rising = self.rising_edge(AlertKind::Disk, disk_peak, t.disk_percent);
        let disk = self.observe(
            AlertKind::Disk,
            disk_peak,
            t.disk_percent,
            HYSTERESIS_PERCENT,
        );
        if disk {
            let release = (t.disk_percent - HYSTERESIS_PERCENT).max(0.0);
            for d in &metrics.disks {
                if d.used_percent < release {
                    continue;
                }
                let mount = if d.mount_point.is_empty() {
                    d.name.as_str()
                } else {
                    d.mount_point.as_str()
                };
                messages.push(format!("{mount} {:.0}%", d.used_percent));
            }
            if disk_rising && self.should_fire(AlertKind::Disk) {
                for msg in disk_notify {
                    newly_fired.push((AlertKind::Disk, msg));
                }
            }
        }

        let active = cpu || memory || temperature || disk;
        (
            AlertStatus {
                cpu,
                memory,
                temperature,
                disk,
                active,
                messages,
                thresholds: t,
            },
            newly_fired,
        )
    }

    /// 仅在通知实际 show 成功后记账，避免评估阶段或通知关闭时吃掉冷却。
    pub fn mark_fired(&mut self, kinds: &[AlertKind]) {
        let now = Instant::now();
        for kind in kinds {
            self.last_fired.insert(*kind, now);
        }
    }

    fn is_latched(&self, kind: AlertKind) -> bool {
        self.latched.get(&kind).copied().unwrap_or(false)
    }

    fn clear_latch(&mut self, kind: AlertKind) {
        self.latched.insert(kind, false);
    }

    /// 本帧从未 latched 跨过触发线。须在 observe 更新 latch 之前调用。
    fn rising_edge(&self, kind: AlertKind, value: f32, threshold: f32) -> bool {
        !self.is_latched(kind) && value >= threshold
    }

    /// 未 latched 用触发线；已 latched 只有降到解除线以下才清除。
    fn observe(&mut self, kind: AlertKind, value: f32, threshold: f32, hysteresis: f32) -> bool {
        let release = (threshold - hysteresis).max(0.0);
        if self.is_latched(kind) {
            if value < release {
                self.clear_latch(kind);
                false
            } else {
                true
            }
        } else if value >= threshold {
            self.latched.insert(kind, true);
            true
        } else {
            false
        }
    }

    /// 冷却只限制同一次恢复后的重复触发。不修改 last_fired；latched 期间由 rising_edge 挡住。
    fn should_fire(&self, kind: AlertKind) -> bool {
        let cooldown = Duration::from_secs(self.thresholds.cooldown_secs.max(1));
        match self.last_fired.get(&kind) {
            Some(last) if last.elapsed() < cooldown => false,
            _ => true,
        }
    }
}

pub type SharedAlerts = Mutex<AlertEngine>;
