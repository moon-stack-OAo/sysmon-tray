use crate::monitor::Metrics;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_COOLDOWN_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AlertKind {
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
}

impl AlertEngine {
    pub fn with_thresholds(thresholds: AlertThresholds) -> Self {
        Self {
            thresholds,
            last_fired: HashMap::new(),
        }
    }

    pub fn thresholds(&self) -> AlertThresholds {
        self.thresholds.clone()
    }

    pub fn set_thresholds(&mut self, thresholds: AlertThresholds) {
        self.thresholds = thresholds;
    }

    /// 根据当前指标评估告警。返回 (状态, 本次新触发且已过冷却的种类)
    pub fn evaluate(&mut self, metrics: &Metrics) -> (AlertStatus, Vec<String>) {
        let t = self.thresholds.clone();
        let mut messages = Vec::new();
        let mut newly_fired = Vec::new();

        let cpu = metrics.cpu_percent >= t.cpu_percent;
        if cpu {
            messages.push(format!("CPU {:.0}%", metrics.cpu_percent));
            if self.try_fire(AlertKind::Cpu) {
                newly_fired.push(format!(
                    "CPU 占用过高：{:.1}%（阈值 {:.0}%）",
                    metrics.cpu_percent, t.cpu_percent
                ));
            }
        }

        let memory = metrics.memory_percent >= t.memory_percent;
        if memory {
            messages.push(format!("内存 {:.0}%", metrics.memory_percent));
            if self.try_fire(AlertKind::Memory) {
                newly_fired.push(format!(
                    "内存占用过高：{:.1}%（阈值 {:.0}%）",
                    metrics.memory_percent, t.memory_percent
                ));
            }
        }

        let temperature = match metrics.cpu_temp_celsius {
            Some(temp) if temp >= t.cpu_temp_celsius => {
                messages.push(format!("温度 {:.0}°C", temp));
                if self.try_fire(AlertKind::Temperature) {
                    newly_fired.push(format!(
                        "CPU 温度过高：{:.1}°C（阈值 {:.0}°C）",
                        temp, t.cpu_temp_celsius
                    ));
                }
                true
            }
            _ => false,
        };

        let mut disk = false;
        let mut disk_notify = Vec::new();
        for d in &metrics.disks {
            if d.used_percent < t.disk_percent {
                continue;
            }
            disk = true;
            let mount = if d.mount_point.is_empty() {
                d.name.as_str()
            } else {
                d.mount_point.as_str()
            };
            messages.push(format!("{mount} {:.0}%", d.used_percent));
            disk_notify.push(format!(
                "磁盘 {mount} 占用过高：{:.1}%（阈值 {:.0}%）",
                d.used_percent, t.disk_percent
            ));
        }
        if disk && self.try_fire(AlertKind::Disk) {
            newly_fired.extend(disk_notify);
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

    fn try_fire(&mut self, kind: AlertKind) -> bool {
        let cooldown = Duration::from_secs(self.thresholds.cooldown_secs.max(1));
        let now = Instant::now();
        match self.last_fired.get(&kind) {
            Some(last) if now.duration_since(*last) < cooldown => false,
            _ => {
                self.last_fired.insert(kind, now);
                true
            }
        }
    }
}

pub type SharedAlerts = Mutex<AlertEngine>;
