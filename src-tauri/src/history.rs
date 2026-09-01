use crate::monitor::Metrics;
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
        self.points.push_back(HistoryPoint {
            cpu_percent: metrics.cpu_percent,
            memory_percent: metrics.memory_percent,
            cpu_temp_celsius: metrics.cpu_temp_celsius,
            timestamp_ms: metrics.sampled_at_ms,
        });
    }

    pub fn snapshot(&self) -> Vec<HistoryPoint> {
        self.points.iter().cloned().collect()
    }
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
