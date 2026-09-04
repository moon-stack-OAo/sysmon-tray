use crate::temperature_lhm::{
    LhmRuntimeConfig, LibreHardwareMonitorProvider, SharedGpuCache, TempSourceKind,
    TempSourceTracker,
};
use std::sync::Arc;
use sysinfo::Components;

/// 温度读取抽象。可扩展接入 LibreHardwareMonitor 等方案。
pub trait TemperatureProvider: Send {
    fn read_cpu_temp_celsius(&mut self) -> Option<f32>;
}

/// 按顺序尝试多个提供者，返回第一个有效读数，并记录来源。
pub struct ChainedTemperatureProvider {
    providers: Vec<(TempSourceKind, Box<dyn TemperatureProvider>)>,
    tracker: Arc<TempSourceTracker>,
}

impl ChainedTemperatureProvider {
    pub fn new(
        providers: Vec<(TempSourceKind, Box<dyn TemperatureProvider>)>,
        tracker: Arc<TempSourceTracker>,
    ) -> Self {
        Self { providers, tracker }
    }

    /// 构建默认链。
    /// - 启用精确温度时：LHM → WMI → sysinfo
    /// - 关闭时：WMI → sysinfo（Windows 上 WMI 通常更有效）
    /// - LHM 内部读共享开关，设置页可热更新。
    /// - GPU 仅经 LHM 同一次请求解析，写入 gpu_cache。
    pub fn platform_default(
        lhm_runtime: Arc<LhmRuntimeConfig>,
        tracker: Arc<TempSourceTracker>,
        gpu_cache: SharedGpuCache,
    ) -> Self {
        let mut providers: Vec<(TempSourceKind, Box<dyn TemperatureProvider>)> = Vec::new();

        providers.push((
            TempSourceKind::Lhm,
            Box::new(LibreHardwareMonitorProvider::new(lhm_runtime, gpu_cache)),
        ));

        #[cfg(windows)]
        {
            providers.push((
                TempSourceKind::Wmi,
                Box::new(WindowsWmiThermalProvider::new()),
            ));
        }

        providers.push((
            TempSourceKind::Sysinfo,
            Box::new(SysinfoComponentsProvider::new()),
        ));

        Self::new(providers, tracker)
    }
}

impl TemperatureProvider for ChainedTemperatureProvider {
    fn read_cpu_temp_celsius(&mut self) -> Option<f32> {
        for (kind, provider) in &mut self.providers {
            if let Some(temp) = provider.read_cpu_temp_celsius() {
                self.tracker.set(*kind);
                return Some(temp);
            }
        }
        self.tracker.set(TempSourceKind::Unavailable);
        None
    }
}

/// 通过 sysinfo Components 读取。Windows 上通常依赖管理员权限且常为空。
/// 延迟初始化，避免在主线程建窗前触发 COM(MTA) 与 Tauri OleInitialize(STA) 冲突。
pub struct SysinfoComponentsProvider {
    components: Option<Components>,
}

impl SysinfoComponentsProvider {
    pub fn new() -> Self {
        Self { components: None }
    }

    fn ensure_components(&mut self) -> &mut Components {
        self.components
            .get_or_insert_with(Components::new_with_refreshed_list)
    }
}

impl TemperatureProvider for SysinfoComponentsProvider {
    fn read_cpu_temp_celsius(&mut self) -> Option<f32> {
        let components = self.ensure_components();
        components.refresh(true);

        let readings: Vec<(String, f32)> = components
            .iter()
            .filter_map(|c| {
                let temp = c.temperature()?;
                if !is_plausible_celsius(temp) {
                    return None;
                }
                Some((c.label().to_string(), temp))
            })
            .collect();

        pick_cpu_temp(&readings)
    }
}

#[cfg(windows)]
mod windows_wmi {
    use super::{is_plausible_celsius, pick_cpu_temp, TemperatureProvider};
    use serde::Deserialize;
    use std::sync::mpsc;
    use std::time::Duration;
    use wmi::{COMLibrary, WMIConnection};

    type WmiQueryResult = Option<Vec<(String, f32)>>;
    type QueryResponder = mpsc::Sender<WmiQueryResult>;

    const QUERY_TIMEOUT: Duration = Duration::from_millis(300);

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct MsAcpiThermalZoneTemperature {
        instance_name: Option<String>,
        current_temperature: Option<u32>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ThermalZonePerfCounter {
        name: Option<String>,
        temperature: Option<u32>,
        high_precision_temperature: Option<u32>,
    }

    /// Windows WMI 温度提供者：
    /// 1. `MSAcpi_ThermalZoneTemperature`（常需管理员权限）
    /// 2. `Win32_PerfFormattedData_Counters_ThermalZoneInformation`（通常无需提权）
    ///
    /// 二者均为 ACPI 热区，不是逐核传感器；逐核需 LibreHardwareMonitor 等驱动方案。
    pub struct WindowsWmiThermalProvider {
        req_tx: Option<mpsc::SyncSender<QueryResponder>>,
    }

    impl WindowsWmiThermalProvider {
        pub fn new() -> Self {
            Self { req_tx: None }
        }

        /// 常驻 worker：仅首次采样时创建（延迟初始化，避免建窗前污染 Tauri 主线程 STA）。
        /// COM 库不可跨线程传递，因此在 worker 线程内一次性 COMLibrary::new 并持有整个生命周期。
        fn spawn_worker(&mut self) -> Option<&mpsc::SyncSender<QueryResponder>> {
            let (req_tx, req_rx) = mpsc::sync_channel::<QueryResponder>(1);
            std::thread::Builder::new()
                .name("wmi-temp-worker".into())
                .spawn(move || {
                    let Ok(com) = COMLibrary::new() else {
                        return;
                    };
                    for resp in req_rx {
                        let _ = resp.send(
                            Self::query_msacpi(&com).or_else(|| Self::query_perf_counters(&com)),
                        );
                    }
                })
                .ok()?;
            Some(self.req_tx.insert(req_tx))
        }

        /// 请求通道容量 1：try_send 失败即 worker 忙（上轮查询超时未归或挂起），
        /// 本轮直接放弃并回退下一 Provider，绝不额外 spawn 线程；通道断开（worker panic
        /// 或 COM 初始化失败退出）时重建一次，仍失败则视本 Provider 不可用。
        fn query_on_worker(&mut self) -> WmiQueryResult {
            let (resp_tx, resp_rx) = mpsc::channel();
            let tx = match self.req_tx.clone() {
                Some(tx) => tx,
                None => self.spawn_worker()?.clone(),
            };
            match tx.try_send(resp_tx.clone()) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => return None,
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    let tx = self.spawn_worker()?.clone();
                    if tx.try_send(resp_tx).is_err() {
                        return None;
                    }
                }
            }
            resp_rx.recv_timeout(QUERY_TIMEOUT).ok().flatten()
        }

        fn query_msacpi(com: &COMLibrary) -> Option<Vec<(String, f32)>> {
            let conn = WMIConnection::with_namespace_path(r"root\WMI", *com).ok()?;
            let rows: Vec<MsAcpiThermalZoneTemperature> = conn
                .raw_query(
                    "SELECT InstanceName, CurrentTemperature FROM MSAcpi_ThermalZoneTemperature",
                )
                .ok()?;

            let readings = rows
                .into_iter()
                .filter_map(|row| {
                    let raw = row.current_temperature?;
                    let temp = decikelvin_to_celsius(raw);
                    if !is_plausible_celsius(temp) {
                        return None;
                    }
                    let name = clean_zone_name(row.instance_name.as_deref().unwrap_or("Thermal"));
                    Some((name, temp))
                })
                .collect::<Vec<_>>();

            if readings.is_empty() {
                None
            } else {
                Some(readings)
            }
        }

        fn query_perf_counters(com: &COMLibrary) -> Option<Vec<(String, f32)>> {
            let conn = WMIConnection::new(*com).ok()?;
            let rows: Vec<ThermalZonePerfCounter> = conn
                .raw_query(
                    "SELECT Name, Temperature, HighPrecisionTemperature \
                     FROM Win32_PerfFormattedData_Counters_ThermalZoneInformation",
                )
                .ok()?;

            let readings = rows
                .into_iter()
                .filter_map(|row| {
                    let temp = row
                        .high_precision_temperature
                        .map(decikelvin_to_celsius)
                        .filter(|t| is_plausible_celsius(*t))
                        .or_else(|| {
                            row.temperature
                                .map(kelvin_to_celsius)
                                .filter(|t| is_plausible_celsius(*t))
                        })?;
                    let name = clean_zone_name(row.name.as_deref().unwrap_or("Thermal"));
                    Some((name, temp))
                })
                .collect::<Vec<_>>();

            if readings.is_empty() {
                None
            } else {
                Some(readings)
            }
        }
    }

    impl TemperatureProvider for WindowsWmiThermalProvider {
        fn read_cpu_temp_celsius(&mut self) -> Option<f32> {
            let readings = self.query_on_worker()?;
            pick_cpu_temp(&readings)
        }
    }

    fn decikelvin_to_celsius(value: u32) -> f32 {
        (value as f32) / 10.0 - 273.15
    }

    fn kelvin_to_celsius(value: u32) -> f32 {
        value as f32 - 273.15
    }

    fn clean_zone_name(name: &str) -> String {
        name.trim()
            .trim_start_matches('\\')
            .trim_start_matches('_')
            .replace("TZ.", "")
            .replace("ThermalZone", "")
            .trim()
            .to_string()
    }
}

#[cfg(windows)]
use windows_wmi::WindowsWmiThermalProvider;

pub(crate) fn is_plausible_celsius(temp: f32) -> bool {
    temp.is_finite() && (-40.0..=150.0).contains(&temp)
}

pub(crate) fn pick_cpu_temp(readings: &[(String, f32)]) -> Option<f32> {
    if readings.is_empty() {
        return None;
    }

    let lower = |s: &str| s.to_ascii_lowercase();

    if let Some((_, temp)) = readings.iter().find(|(name, _)| {
        let n = lower(name);
        n.contains("cpu package") || n.contains("package")
    }) {
        return Some(*temp);
    }

    if let Some((_, temp)) = readings.iter().find(|(name, _)| {
        let n = lower(name);
        n.contains("tctl") || n.contains("tdie")
    }) {
        return Some(*temp);
    }

    if let Some((_, temp)) = readings.iter().find(|(name, _)| {
        let n = lower(name);
        n.contains("cpu") || n.contains("proc")
    }) {
        return Some(*temp);
    }

    readings
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, temp)| *temp)
}
