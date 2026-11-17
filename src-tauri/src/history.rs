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
    pub sequence: u64,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub cpu_temp_celsius: Option<f32>,
    pub gpu_load_percent: Option<f32>,
    pub gpu_temp_celsius: Option<f32>,
    pub fan_rpm: Option<u32>,
    pub fan_name: Option<String>,
    pub net_down_bps: u64,
    pub net_up_bps: u64,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryBatch {
    pub points: Vec<HistoryPoint>,
    pub cursor: u64,
    pub oldest: u64,
    pub capacity: usize,
    pub reset: bool,
}

pub struct MetricsHistory {
    sequence: u64,
    capacity: usize,
    points: VecDeque<HistoryPoint>,
}

impl MetricsHistory {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            sequence: 0,
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
            self.points
                .reserve(self.capacity.saturating_sub(self.points.len()));
        }
    }

    pub fn push_from_metrics(&mut self, metrics: &Metrics) {
        if self.points.len() >= self.capacity {
            self.points.pop_front();
        }
        self.sequence += 1;
        let primary = pick_primary_fan(&metrics.fans);
        let gpu = metrics.gpu.as_ref();
        self.points.push_back(HistoryPoint {
            sequence: self.sequence,
            cpu_percent: metrics.cpu_percent,
            memory_percent: metrics.memory_percent,
            cpu_temp_celsius: metrics.cpu_temp_celsius,
            gpu_load_percent: gpu.and_then(|g| g.load_percent),
            gpu_temp_celsius: gpu.and_then(|g| g.temp_celsius),
            fan_rpm: primary.map(|f| f.rpm),
            fan_name: primary.map(|f| f.name.clone()),
            net_down_bps: metrics.net_down_bps,
            net_up_bps: metrics.net_up_bps,
            timestamp_ms: metrics.sampled_at_ms,
        });
    }

    pub fn batch(&self, cursor: Option<u64>) -> HistoryBatch {
        let oldest = self
            .points
            .front()
            .map(|p| p.sequence)
            .unwrap_or(self.sequence + 1);
        let reset = cursor.is_none_or(|c| c > self.sequence || c.saturating_add(1) < oldest);
        HistoryBatch {
            points: self
                .points
                .iter()
                .filter(|p| reset || cursor.is_none_or(|c| p.sequence > c))
                .cloned()
                .collect(),
            cursor: self.sequence,
            oldest,
            capacity: self.capacity,
            reset,
        }
    }

    pub fn snapshot(&self) -> Vec<HistoryPoint> {
        self.points.iter().cloned().collect()
    }
}

fn csv_field_f32(value: Option<f32>) -> String {
    match value {
        Some(v) => format!("{v:.1}"),
        None => String::new(),
    }
}

fn csv_field_u32(value: Option<u32>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// 由历史点生成 CSV 文本。无温度 / GPU / 风扇留空字段，不把 None 写成 0。
pub fn history_to_csv(points: &[HistoryPoint]) -> String {
    let mut out = String::from(
        "timestamp_ms,cpu_percent,memory_percent,cpu_temp_celsius,gpu_load_percent,gpu_temp_celsius,fan_rpm,net_down_bps,net_up_bps\n",
    );
    for point in points {
        out.push_str(&format!(
            "{},{:.1},{:.1},{},{},{},{},{},{}\n",
            point.timestamp_ms,
            point.cpu_percent,
            point.memory_percent,
            csv_field_f32(point.cpu_temp_celsius),
            csv_field_f32(point.gpu_load_percent),
            csv_field_f32(point.gpu_temp_celsius),
            csv_field_u32(point.fan_rpm),
            point.net_down_bps,
            point.net_up_bps,
        ));
    }
    out
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
        assert_eq!(
            pick_primary_fan(&fans).map(|f| f.kind.as_str()),
            Some("pump")
        );

        let fans = vec![fan("chassis", 800), fan("cpu", 1500), fan("gpu", 1200)];
        assert_eq!(
            pick_primary_fan(&fans).map(|f| f.kind.as_str()),
            Some("cpu")
        );

        let fans = vec![fan("other", 500)];
        assert_eq!(pick_primary_fan(&fans).map(|f| f.rpm), Some(500));

        assert!(pick_primary_fan(&[]).is_none());
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    fn push(history: &mut MetricsHistory, timestamp: u128) {
        history.push_from_metrics(&crate::monitor::Metrics {
            cpu_percent: 5.0,
            memory_percent: 6.0,
            memory_used_bytes: 0,
            memory_total_bytes: 0,
            swap_used_bytes: 0,
            swap_total_bytes: 0,
            net_down_bps: 0,
            net_up_bps: 0,
            net_interface: None,
            net_link: "down".into(),
            disks: Vec::new(),
            cpu_temp_celsius: None,
            gpu: None,
            fans: Vec::new(),
            sampled_at_ms: timestamp,
        });
    }
    #[test]
    fn cursors_do_not_depend_on_wall_clock_and_expired_clients_reset() {
        let mut history = MetricsHistory::new(2);
        assert!(history.batch(None).reset);
        push(&mut history, 1000);
        push(&mut history, 500);
        let initial = history.batch(None);
        assert_eq!(initial.cursor, 2);
        assert_eq!(initial.points.len(), 2);
        assert!(history.batch(Some(2)).points.is_empty());
        push(&mut history, 500);
        let delta = history.batch(Some(2));
        assert!(!delta.reset);
        assert_eq!(delta.points[0].sequence, 3);
        assert!(history.batch(Some(0)).reset);
        assert!(history.batch(Some(99)).reset);
        history.set_capacity(1);
        assert_eq!(history.batch(Some(2)).oldest, 3);
        history.set_capacity(60);
        push(&mut history, 400);
        assert_eq!(history.batch(Some(3)).points.len(), 1);
        assert_eq!(history_to_csv(&history.snapshot()).lines().count(), 3);
    }
}
