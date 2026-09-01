//! LibreHardwareMonitor Web Server 温度读取（探测本机 data.json，失败静默回退）。

use crate::temperature::{is_plausible_celsius, pick_cpu_temp, TemperatureProvider};
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ureq::Agent;

const REQUEST_TIMEOUT: Duration = Duration::from_millis(150);
const CIRCUIT_FAIL_THRESHOLD: u32 = 5;
const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(30);

/// 运行时共享：开关与 URL 可热更新，无需重建 Monitor。
#[derive(Debug)]
pub struct LhmRuntimeConfig {
    enabled: AtomicBool,
    base_url: Mutex<String>,
    /// 配置变更代数；Provider 据此重置熔断
    epoch: AtomicU64,
}

impl LhmRuntimeConfig {
    pub fn new(enabled: bool, base_url: String) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            base_url: Mutex::new(base_url),
            epoch: AtomicU64::new(0),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .lock()
            .map(|u| u.clone())
            .unwrap_or_else(|_| crate::config::DEFAULT_LHM_BASE_URL.to_string())
    }

    pub fn set_base_url(&self, url: String) {
        if let Ok(mut guard) = self.base_url.lock() {
            *guard = url;
        }
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }
}

/// 最近一次成功/探测结果，供设置页展示（与采样路径解耦）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TempSourceKind {
    Unavailable = 0,
    Lhm = 1,
    Wmi = 2,
    Sysinfo = 3,
}

impl TempSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Lhm => "lhm",
            Self::Wmi => "wmi",
            Self::Sysinfo => "sysinfo",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Lhm,
            2 => Self::Wmi,
            3 => Self::Sysinfo,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Debug)]
pub struct TempSourceTracker {
    last_source: AtomicU8,
}

impl TempSourceTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_source: AtomicU8::new(TempSourceKind::Unavailable as u8),
        })
    }

    pub fn set(&self, kind: TempSourceKind) {
        self.last_source.store(kind as u8, Ordering::Relaxed);
    }

    pub fn get(&self) -> TempSourceKind {
        TempSourceKind::from_u8(self.last_source.load(Ordering::Relaxed))
    }
}

pub struct LibreHardwareMonitorProvider {
    runtime: Arc<LhmRuntimeConfig>,
    agent: Agent,
    consecutive_failures: u32,
    circuit_open_until: Option<Instant>,
    seen_epoch: u64,
}

impl LibreHardwareMonitorProvider {
    pub fn new(runtime: Arc<LhmRuntimeConfig>) -> Self {
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(true)
            .build()
            .into();

        let seen_epoch = runtime.epoch();
        Self {
            runtime,
            agent,
            consecutive_failures: 0,
            circuit_open_until: None,
            seen_epoch,
        }
    }

    fn data_url(base: &str) -> String {
        let base = base.trim_end_matches('/');
        format!("{base}/data.json")
    }

    fn sync_epoch(&mut self) {
        let epoch = self.runtime.epoch();
        if epoch != self.seen_epoch {
            self.seen_epoch = epoch;
            self.consecutive_failures = 0;
            self.circuit_open_until = None;
        }
    }

    fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= CIRCUIT_FAIL_THRESHOLD {
            self.circuit_open_until = Some(Instant::now() + CIRCUIT_COOLDOWN);
            self.consecutive_failures = 0;
        }
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_open_until = None;
    }

    fn circuit_blocked(&self) -> bool {
        match self.circuit_open_until {
            Some(until) if Instant::now() < until => true,
            _ => false,
        }
    }

    fn fetch_and_parse(&self, url: &str) -> Option<f32> {
        let mut response = self.agent.get(url).call().ok()?;
        let root: Value = response.body_mut().read_json().ok()?;
        extract_cpu_temp(&root)
    }
}

impl TemperatureProvider for LibreHardwareMonitorProvider {
    fn read_cpu_temp_celsius(&mut self) -> Option<f32> {
        self.sync_epoch();

        if !self.runtime.is_enabled() {
            return None;
        }

        // 熔断期内直接跳过，避免每秒超时拖垮采样
        if self.circuit_blocked() {
            return None;
        }

        // 熔断到期后清状态，允许再试
        if self.circuit_open_until.is_some() {
            self.circuit_open_until = None;
        }

        let url = Self::data_url(&self.runtime.base_url());
        match self.fetch_and_parse(&url) {
            Some(temp) => {
                self.record_success();
                Some(temp)
            }
            None => {
                self.record_failure();
                None
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct LhmNode {
    #[serde(rename = "Text")]
    text: Option<String>,
    #[serde(rename = "Value")]
    value: Option<String>,
    #[serde(rename = "Type")]
    sensor_type: Option<String>,
    #[serde(rename = "Children")]
    children: Option<Vec<LhmNode>>,
}

fn extract_cpu_temp(root: &Value) -> Option<f32> {
    // 优先结构化反序列化；失败则兜底递归 Value
    if let Ok(node) = serde_json::from_value::<LhmNode>(root.clone()) {
        let mut readings = Vec::new();
        collect_temp_readings(&node, &mut readings);
        if let Some(temp) = pick_cpu_temp(&readings) {
            return Some(temp);
        }
    }
    let mut readings = Vec::new();
    collect_temp_from_value(root, &mut readings);
    pick_cpu_temp(&readings)
}

fn collect_temp_readings(node: &LhmNode, out: &mut Vec<(String, f32)>) {
    let is_temp = node
        .sensor_type
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("Temperature"))
        .unwrap_or(false);

    if is_temp {
        if let (Some(text), Some(value)) = (node.text.as_deref(), node.value.as_deref()) {
            if let Some(temp) = parse_lhm_temp_value(value) {
                if is_plausible_celsius(temp) {
                    out.push((text.to_string(), temp));
                }
            }
        }
    }

    if let Some(children) = &node.children {
        for child in children {
            collect_temp_readings(child, out);
        }
    }
}

fn collect_temp_from_value(value: &Value, out: &mut Vec<(String, f32)>) {
    match value {
        Value::Object(map) => {
            let sensor_type = map
                .get("Type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sensor_type.eq_ignore_ascii_case("Temperature") {
                let text = map.get("Text").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(raw) = map.get("Value").and_then(|v| v.as_str()) {
                    if let Some(temp) = parse_lhm_temp_value(raw) {
                        if is_plausible_celsius(temp) {
                            out.push((text.to_string(), temp));
                        }
                    }
                }
            }
            if let Some(children) = map.get("Children").and_then(|v| v.as_array()) {
                for child in children {
                    collect_temp_from_value(child, out);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_temp_from_value(item, out);
            }
        }
        _ => {}
    }
}

/// 解析 LHM 温度字符串，如 `"46.9 °C"` / `"46.9"`。
fn parse_lhm_temp_value(raw: &str) -> Option<f32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let num: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    if num.is_empty() {
        return None;
    }
    num.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_temp_string() {
        assert_eq!(parse_lhm_temp_value("46.9 °C"), Some(46.9));
        assert_eq!(parse_lhm_temp_value("  72.0°C"), Some(72.0));
        assert!(parse_lhm_temp_value("").is_none());
    }
}
