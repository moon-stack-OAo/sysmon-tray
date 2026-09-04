//! LibreHardwareMonitor Web Server 读取（探测本机 data.json，失败静默回退）。
//! 同一次 HTTP 解析 CPU 温度与 GPU（占用 / 显存 / 温度）。

use crate::temperature::{is_plausible_celsius, pick_cpu_temp, TemperatureProvider};
use serde::Deserialize;
use serde::Serialize;
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

/// GPU 指标（仅 LHM 可提供；无可靠低成本回退）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuMetrics {
    pub name: String,
    pub load_percent: Option<f32>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_percent: Option<f32>,
    pub temp_celsius: Option<f32>,
}

pub type SharedGpuCache = Arc<Mutex<Option<GpuMetrics>>>;

pub struct LibreHardwareMonitorProvider {
    runtime: Arc<LhmRuntimeConfig>,
    gpu_cache: SharedGpuCache,
    agent: Agent,
    consecutive_failures: u32,
    circuit_open_until: Option<Instant>,
    seen_epoch: u64,
}

impl LibreHardwareMonitorProvider {
    pub fn new(runtime: Arc<LhmRuntimeConfig>, gpu_cache: SharedGpuCache) -> Self {
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(true)
            .build()
            .into();

        let seen_epoch = runtime.epoch();
        Self {
            runtime,
            gpu_cache,
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
        matches!(self.circuit_open_until, Some(until) if Instant::now() < until)
    }

    fn clear_gpu_cache(&self) {
        if let Ok(mut guard) = self.gpu_cache.lock() {
            *guard = None;
        }
    }

    fn store_gpu_cache(&self, gpu: Option<GpuMetrics>) {
        if let Ok(mut guard) = self.gpu_cache.lock() {
            *guard = gpu;
        }
    }

    fn fetch_and_parse(&self, url: &str) -> Option<LhmParseResult> {
        let mut response = self.agent.get(url).call().ok()?;
        let root: Value = response.body_mut().read_json().ok()?;
        Some(extract_lhm_metrics(&root))
    }
}

impl TemperatureProvider for LibreHardwareMonitorProvider {
    fn read_cpu_temp_celsius(&mut self) -> Option<f32> {
        self.sync_epoch();

        if !self.runtime.is_enabled() {
            self.clear_gpu_cache();
            return None;
        }

        if self.circuit_blocked() {
            self.clear_gpu_cache();
            return None;
        }

        if self.circuit_open_until.is_some() {
            self.circuit_open_until = None;
        }

        let url = Self::data_url(&self.runtime.base_url());
        match self.fetch_and_parse(&url) {
            Some(parsed) => {
                self.record_success();
                self.store_gpu_cache(parsed.gpu);
                parsed.cpu_temp
            }
            None => {
                self.record_failure();
                self.clear_gpu_cache();
                None
            }
        }
    }
}

#[derive(Debug, Default)]
struct LhmParseResult {
    cpu_temp: Option<f32>,
    gpu: Option<GpuMetrics>,
}

#[derive(Debug, Deserialize)]
struct LhmNode {
    #[serde(rename = "Text")]
    text: Option<String>,
    #[serde(rename = "Value")]
    value: Option<String>,
    #[serde(rename = "Type")]
    sensor_type: Option<String>,
    #[serde(rename = "ImageURL")]
    image_url: Option<String>,
    #[serde(rename = "Children")]
    children: Option<Vec<LhmNode>>,
}

fn extract_lhm_metrics(root: &Value) -> LhmParseResult {
    if let Ok(node) = serde_json::from_value::<LhmNode>(root.clone()) {
        let mut temp_readings = Vec::new();
        collect_temp_readings(&node, &mut temp_readings);
        let cpu_temp = pick_cpu_temp(&temp_readings);
        let gpu = pick_primary_gpu(collect_gpu_candidates(&node));
        return LhmParseResult { cpu_temp, gpu };
    }

    let mut temp_readings = Vec::new();
    collect_temp_from_value(root, &mut temp_readings);
    LhmParseResult {
        cpu_temp: pick_cpu_temp(&temp_readings),
        gpu: None,
    }
}

fn collect_temp_readings(node: &LhmNode, out: &mut Vec<(String, f32)>) {
    let is_temp = node
        .sensor_type
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("Temperature"))
        .unwrap_or(false);

    if is_temp {
        if let (Some(text), Some(value)) = (node.text.as_deref(), node.value.as_deref()) {
            if let Some(temp) = parse_lhm_number(value) {
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
                    if let Some(temp) = parse_lhm_number(raw) {
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

#[derive(Debug, Default)]
struct GpuCandidate {
    name: String,
    score: i32,
    load_percent: Option<f32>,
    memory_used_mb: Option<f32>,
    memory_total_mb: Option<f32>,
    memory_percent: Option<f32>,
    temp_celsius: Option<f32>,
}

fn collect_gpu_candidates(root: &LhmNode) -> Vec<GpuCandidate> {
    let mut out = Vec::new();
    walk_for_gpu(root, &mut out);
    out
}

fn walk_for_gpu(node: &LhmNode, out: &mut Vec<GpuCandidate>) {
    if is_gpu_hardware(node) {
        if let Some(candidate) = parse_gpu_hardware(node) {
            out.push(candidate);
        }
        return;
    }
    if let Some(children) = &node.children {
        for child in children {
            walk_for_gpu(child, out);
        }
    }
}

fn is_gpu_hardware(node: &LhmNode) -> bool {
    let text = node.text.as_deref().unwrap_or("").to_ascii_lowercase();
    let image = node.image_url.as_deref().unwrap_or("").to_ascii_lowercase();

    if image.contains("nvidia")
        || image.contains("ati")
        || image.contains("amd")
        || image.contains("gpu")
    {
        return true;
    }

    if text.contains("nvidia")
        || text.contains("geforce")
        || text.contains("quadro")
        || text.contains("rtx ")
        || text.contains("gtx ")
        || text.contains("radeon")
        || text.contains("amd ")
        || text.contains("intel arc")
        || text.contains("iris")
        || text.contains("uhd graphics")
        || text.contains("hd graphics")
    {
        return true;
    }

    if let Some(children) = &node.children {
        return children.iter().any(|c| {
            let name = c.text.as_deref().unwrap_or("").to_ascii_lowercase();
            name == "gpu core" || name.starts_with("gpu core")
        });
    }

    false
}

fn gpu_priority_score(name: &str, image: &str) -> i32 {
    let n = name.to_ascii_lowercase();
    let img = image.to_ascii_lowercase();
    if n.contains("nvidia")
        || n.contains("geforce")
        || n.contains("quadro")
        || n.contains("rtx")
        || n.contains("gtx")
        || img.contains("nvidia")
    {
        return 100;
    }
    if n.contains("radeon") || n.contains("amd") || img.contains("ati") || img.contains("amd") {
        return 100;
    }
    if n.contains("intel arc") || n.contains("arc ") {
        return 80;
    }
    if n.contains("iris") || n.contains("uhd") || n.contains("hd graphics") || n.contains("intel") {
        return 10;
    }
    40
}

fn parse_gpu_hardware(node: &LhmNode) -> Option<GpuCandidate> {
    let name = node.text.as_deref()?.trim();
    if name.is_empty() {
        return None;
    }
    let image = node.image_url.as_deref().unwrap_or("");
    let mut candidate = GpuCandidate {
        name: name.to_string(),
        score: gpu_priority_score(name, image),
        ..GpuCandidate::default()
    };

    if let Some(children) = &node.children {
        collect_gpu_sensors(children, &mut candidate);
    }

    if candidate.load_percent.is_none()
        && candidate.temp_celsius.is_none()
        && candidate.memory_used_mb.is_none()
        && candidate.memory_total_mb.is_none()
        && candidate.memory_percent.is_none()
    {
        return None;
    }

    if let Some(total) = candidate.memory_total_mb {
        candidate.score += (total / 256.0) as i32;
    }
    if candidate.load_percent.is_some() {
        candidate.score += 20;
    }

    Some(candidate)
}

fn collect_gpu_sensors(nodes: &[LhmNode], candidate: &mut GpuCandidate) {
    for node in nodes {
        let sensor_type = node.sensor_type.as_deref().unwrap_or("");
        let text = node.text.as_deref().unwrap_or("");
        let lower = text.to_ascii_lowercase();
        let value = node.value.as_deref().and_then(parse_lhm_number);

        if sensor_type.eq_ignore_ascii_case("Load") {
            if lower == "gpu core" || lower == "d3d 3d" || lower == "gpu render/compute" {
                if candidate.load_percent.is_none() {
                    if let Some(v) = value.filter(|v| v.is_finite() && (0.0..=100.0).contains(v)) {
                        candidate.load_percent = Some(v);
                    }
                }
            } else if lower == "gpu memory" || lower == "d3d dedicated memory used" {
                if candidate.memory_percent.is_none() {
                    if let Some(v) = value.filter(|v| v.is_finite() && (0.0..=100.0).contains(v)) {
                        candidate.memory_percent = Some(v);
                    }
                }
            }
        } else if sensor_type.eq_ignore_ascii_case("Temperature") {
            if lower == "gpu core"
                || lower == "gpu hotspot"
                || lower == "gpu hot spot"
                || lower == "temperature"
            {
                let prefer = lower == "gpu core";
                if let Some(v) = value.filter(|t| is_plausible_celsius(*t)) {
                    if prefer || candidate.temp_celsius.is_none() {
                        candidate.temp_celsius = Some(v);
                    }
                }
            }
        } else if sensor_type.eq_ignore_ascii_case("SmallData")
            || sensor_type.eq_ignore_ascii_case("Data")
        {
            if lower == "gpu memory used"
                || lower == "d3d dedicated memory used"
                || lower == "d3d shared memory used"
            {
                if candidate.memory_used_mb.is_none() || lower.starts_with("gpu memory") {
                    candidate.memory_used_mb = value.filter(|v| v.is_finite() && *v >= 0.0);
                }
            } else if lower == "gpu memory total"
                || lower == "d3d dedicated memory total"
                || lower == "d3d shared memory total"
            {
                if candidate.memory_total_mb.is_none() || lower.starts_with("gpu memory") {
                    candidate.memory_total_mb = value.filter(|v| v.is_finite() && *v > 0.0);
                }
            }
        }

        if let Some(children) = &node.children {
            collect_gpu_sensors(children, candidate);
        }
    }
}

fn pick_primary_gpu(mut candidates: Vec<GpuCandidate>) -> Option<GpuMetrics> {
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    let best = candidates.into_iter().next()?;

    let memory_used_bytes = best
        .memory_used_mb
        .map(|mb| (mb as f64 * 1024.0 * 1024.0) as u64);
    let memory_total_bytes = best
        .memory_total_mb
        .map(|mb| (mb as f64 * 1024.0 * 1024.0) as u64);
    let memory_percent = best.memory_percent.or_else(|| match (best.memory_used_mb, best.memory_total_mb) {
        (Some(used), Some(total)) if total > 0.0 => Some((used / total * 100.0).clamp(0.0, 100.0)),
        _ => None,
    });

    if best.load_percent.is_none()
        && best.temp_celsius.is_none()
        && memory_used_bytes.is_none()
        && memory_total_bytes.is_none()
        && memory_percent.is_none()
    {
        return None;
    }

    Some(GpuMetrics {
        name: best.name,
        load_percent: best.load_percent,
        memory_used_bytes,
        memory_total_bytes,
        memory_percent,
        temp_celsius: best.temp_celsius,
    })
}

/// 解析 LHM 数值字符串，如 `"46.9 °C"` / `"12.9 %"` / `"1593"`。
fn parse_lhm_number(raw: &str) -> Option<f32> {
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
        assert_eq!(parse_lhm_number("46.9 °C"), Some(46.9));
        assert_eq!(parse_lhm_number("  72.0°C"), Some(72.0));
        assert_eq!(parse_lhm_number("12.9 %"), Some(12.9));
        assert!(parse_lhm_number("").is_none());
    }

    #[test]
    fn extract_gpu_from_sample_tree() {
        let json = r#"{
          "Text": "Computer",
          "Children": [
            {
              "Text": "NVIDIA GeForce RTX 3080",
              "ImageURL": "images_icon/nvidia.png",
              "Children": [
                { "Text": "GPU Core", "Value": "45.0 %", "Type": "Load", "Children": [] },
                { "Text": "GPU Core", "Value": "53.0 °C", "Type": "Temperature", "Children": [] },
                { "Text": "GPU Memory Used", "Value": "1593", "Type": "SmallData", "Children": [] },
                { "Text": "GPU Memory Total", "Value": "12288", "Type": "SmallData", "Children": [] },
                { "Text": "GPU Memory", "Value": "12.9 %", "Type": "Load", "Children": [] }
              ]
            },
            {
              "Text": "Intel(R) UHD Graphics",
              "ImageURL": "images_icon/intel.png",
              "Children": [
                { "Text": "D3D 3D", "Value": "3.0 %", "Type": "Load", "Children": [] }
              ]
            }
          ]
        }"#;
        let root: Value = serde_json::from_str(json).unwrap();
        let parsed = extract_lhm_metrics(&root);
        let gpu = parsed.gpu.expect("gpu");
        assert!(gpu.name.contains("3080"));
        assert_eq!(gpu.load_percent, Some(45.0));
        assert_eq!(gpu.temp_celsius, Some(53.0));
        assert!(gpu.memory_total_bytes.unwrap() > 0);
        assert!(gpu.memory_percent.unwrap() > 0.0);
    }
}
