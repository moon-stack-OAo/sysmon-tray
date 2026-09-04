use crate::monitor::Metrics;
use crate::temperature_lhm::FanMetrics;
use serde::Serialize;
use std::collections::VecDeque;

/// 默认保留约 1 分钟（按 1s 采样）
pub const DEFAULT_HISTORY_CAPACITY: usize = 60;

/// 允许的历史时长（分钟）
pub const ALLOWED_HISTORY_RANGE_MINUTES: &[u32] = &[1, 5, 15, 60];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPoint {
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub cpu_temp_celsius: Option<f32>,
    pub fan_rpm: Option<u32>,
    pub fan_name: Option<String>,
    pub timestamp_ms: u128,
}

pub struct MetricsHistory {
    capacity: usize,
    points: VecDeque<HistoryPoint>,
}

impl MetricsHistory {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            points: VecDeque::with_capacity(capacity),
        }
    }

    /// 调整容量：缩容丢弃最旧点；扩容保留已有点
    pub fn set_capacity(&mut self, capacity: usize) {
        let capacity = capacity.max(1);
        self.capacity = capacity;
        while self.points.len() > self.capacity {
            self.points.pop_front();
        }
        // 扩容时预留空间，避免频繁重分配
        if self.points.capacity() < self.capacity {
            self.points.reserve(self.capacity - self.points.capacity());
        }
    }

    pub fn push_from_metrics(&mut self, metrics: &Metrics) {
        if self.points.len() >= self.capacity {
            self.points.pop_front();
        }
        let primary = pick_primary_fan(&metrics.fans);
        self.points.push_back(HistoryPoint {
            cpu_percent: metrics.cpu_percent,
            memory_percent: metrics.memory_percent,
            cpu_temp_celsius: metrics.cpu_temp_celsius,
            fan_rpm: primary.map(|f| f.rpm),
            fan_name: primary.map(|f| f.name.clone()),
            timestamp_ms: metrics.sampled_at_ms,
        });
    }

    pub fn snapshot(&self) -> Vec<HistoryPoint> {
        self.points.iter().cloned().collect()
    }
}

/// 主风扇：CPU → 泵 → GPU → 机箱 → 其余第一条
fn pick_primary_fan(fans: &[FanMetrics]) -> Option<&FanMetrics> {
    const ORDER: &[&str] = &["cpu", "pump", "gpu", "chassis"];
    for kind in ORDER {
        if let Some(fan) = fans.iter().find(|f| f.kind == *kind) {
            return Some(fan);
        }
    }
    fans.first()
}

/// 按分钟换算容量（约 1 秒采样一次）
pub fn history_capacity_for_minutes(minutes: u32) -> Result<usize, String> {
    if !ALLOWED_HISTORY_RANGE_MINUTES.contains(&minutes) {
        return Err("历史时长仅支持 1/5/15/60 分钟".to_string());
    }
    Ok((minutes as usize).saturating_mul(60))
}

/// 非法值回落到默认 1 分钟
pub fn normalize_history_range_minutes(minutes: u32) -> u32 {
    if ALLOWED_HISTORY_RANGE_MINUTES.contains(&minutes) {
        minutes
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::pick_primary_fan;
    use crate::temperature_lhm::FanMetrics;

    fn fan(kind: &str, rpm: u32) -> FanMetrics {
        FanMetrics {
            name: format!("{kind}-fan"),
            rpm,
            kind: kind.to_string(),
        }
    }

    #[test]
    fn pick_primary_prefers_cpu_then_pump_gpu_chassis() {
        let fans = vec![fan("chassis", 800), fan("gpu", 1200), fan("pump", 2000)];
        assert_eq!(pick_primary_fan(&fans).map(|f| f.kind.as_str()), Some("pump"));

        let fans = vec![fan("chassis", 800), fan("cpu", 1500), fan("gpu", 1200)];
        assert_eq!(pick_primary_fan(&fans).map(|f| f.kind.as_str()), Some("cpu"));

        let fans = vec![fan("other", 500)];
        assert_eq!(pick_primary_fan(&fans).map(|f| f.rpm), Some(500));

        assert!(pick_primary_fan(&[]).is_none());
    }
}
