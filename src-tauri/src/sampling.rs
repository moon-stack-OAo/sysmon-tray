use crate::monitor::{sample_cached_shared, SharedMonitor, SharedTemperature, SAMPLE_INTERVAL};
use crate::temperature_lhm::SharedLhmExtras;
use crate::{MetricsResponse, SharedAlerts, SharedCodex, SharedConfig, SharedTempTracker};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorDiagnostics {
    pub running: bool,
    pub sample_interval_ms: u64,
    pub temperature_interval_ms: u64,
    pub disk_interval_ms: u64,
    pub discovery_interval_ms: u64,
    pub total_samples: u64,
    pub last_success_at_ms: Option<u128>,
    pub last_duration_ms: Option<u128>,
    pub last_error: Option<String>,
}

impl Default for MonitorDiagnostics {
    fn default() -> Self {
        Self {
            running: false,
            sample_interval_ms: 1000,
            temperature_interval_ms: 2000,
            disk_interval_ms: 15000,
            discovery_interval_ms: 30000,
            total_samples: 0,
            last_success_at_ms: None,
            last_duration_ms: None,
            last_error: None,
        }
    }
}

#[derive(Default)]
struct SnapshotStore {
    response: Option<MetricsResponse>,
    diagnostics: MonitorDiagnostics,
    last_success: Option<Instant>,
}

impl SnapshotStore {
    fn read(&self, now: Instant) -> Result<MetricsResponse, String> {
        let mut response = self.response.clone().ok_or_else(|| {
            self.diagnostics
                .last_error
                .clone()
                .unwrap_or_else(|| "正在初始化监测数据".to_string())
        })?;
        response.stale = self.diagnostics.last_error.is_some()
            || self
                .last_success
                .is_none_or(|at| now.saturating_duration_since(at) > Duration::from_secs(3));
        response.error = self.diagnostics.last_error.clone();
        if response.stale && response.error.is_none() {
            response.error = Some("监测数据已过期".to_string());
        }
        Ok(response)
    }

    fn success(&mut self, response: MetricsResponse, duration: Duration, now: Instant) {
        self.last_success = Some(now);
        self.diagnostics.total_samples = self.diagnostics.total_samples.saturating_add(1);
        self.diagnostics.last_success_at_ms = Some(response.metrics.sampled_at_ms);
        self.diagnostics.last_duration_ms = Some(duration.as_millis());
        self.diagnostics.last_error = None;
        self.response = Some(response);
    }

    fn failure(&mut self, error: String, duration: Duration) {
        self.diagnostics.last_duration_ms = Some(duration.as_millis());
        self.diagnostics.last_error = Some(error);
    }
}

#[derive(Default)]
pub struct Sampler {
    started: AtomicBool,
    stopped: AtomicBool,
    store: Mutex<SnapshotStore>,
    wake: Condvar,
    wait: Mutex<()>,
}

pub type SharedSampler = Arc<Sampler>;

struct RunningGuard(SharedSampler);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Ok(mut store) = self.0.store.lock() {
            store.diagnostics.running = false;
            if !self.0.stopped.load(Ordering::Acquire) {
                store.diagnostics.last_error =
                    Some("后台采样线程已停止，请重启 SysMon".to_string());
            }
        }
    }
}

impl Sampler {
    pub fn snapshot(&self) -> Result<MetricsResponse, String> {
        self.store
            .lock()
            .map_err(|e| e.to_string())?
            .read(Instant::now())
    }

    pub fn diagnostics(&self) -> Result<MonitorDiagnostics, String> {
        Ok(self
            .store
            .lock()
            .map_err(|e| e.to_string())?
            .diagnostics
            .clone())
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_all();
        if let Ok(mut store) = self.store.lock() {
            store.diagnostics.running = false;
        }
    }

    fn publish(self: &Arc<Self>, app: &AppHandle) {
        let owner = Arc::clone(self);
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if owner.stopped.load(Ordering::Acquire) {
                return;
            }
            let Ok(response) = owner.snapshot() else {
                return;
            };
            if let Some(tray) = handle.tray_by_id("main") {
                let tooltip = response
                    .error
                    .as_deref()
                    .map(|error| format!("系统监测 · {error}"))
                    .unwrap_or_else(|| response.alert.tooltip_text());
                let _ = tray.set_tooltip(Some(tooltip));
                crate::sync_tray_alert_icon(&handle, &tray, response.alert.active);
            }
            for label in ["main", "overlay"] {
                if let Some(window) = handle.get_webview_window(label) {
                    if window.is_visible().unwrap_or(false) {
                        let _ = window.emit("metrics-updated", &response);
                    }
                }
            }
        });
    }
}

pub fn start(app: AppHandle, sampler: SharedSampler) -> Result<(), String> {
    if sampler
        .started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("后台采样已启动".to_string());
    }
    let owner = Arc::clone(&sampler);
    std::thread::Builder::new()
        .name("sysmon-sampler".to_string())
        .spawn(move || {
            let _running = RunningGuard(Arc::clone(&owner));
            if let Ok(mut store) = owner.store.lock() {
                store.diagnostics.running = true;
            }
            if let Ok(mut monitor) = app.state::<SharedMonitor>().lock() {
                monitor.prime_cpu();
            }
            if let Ok(guard) = owner.wait.lock() {
                let _ = owner.wake.wait_timeout_while(
                    guard,
                    sysinfo::MINIMUM_CPU_UPDATE_INTERVAL,
                    |_| !owner.stopped.load(Ordering::Acquire),
                );
            }
            while !owner.stopped.load(Ordering::Acquire) {
                let started = Instant::now();
                let result = sample_once(&app, &owner);
                if owner.stopped.load(Ordering::Acquire) {
                    break;
                }
                if let Ok(mut store) = owner.store.lock() {
                    match result {
                        Ok(response) => store.success(response, started.elapsed(), Instant::now()),
                        Err(error) => {
                            if let Ok(mut alerts) = app.state::<SharedAlerts>().lock() {
                                alerts.interrupt_sampling();
                            }
                            store.failure(error, started.elapsed());
                        }
                    }
                }
                owner.publish(&app);
                if let Ok(guard) = owner.wait.lock() {
                    let _ = owner.wake.wait_timeout_while(
                        guard,
                        SAMPLE_INTERVAL.saturating_sub(started.elapsed()),
                        |_| !owner.stopped.load(Ordering::Acquire),
                    );
                }
            }
            if let Ok(mut store) = owner.store.lock() {
                store.diagnostics.running = false;
            }
        })
        .map_err(|error| {
            sampler.started.store(false, Ordering::Release);
            error.to_string()
        })?;
    Ok(())
}

fn sample_once(app: &AppHandle, sampler: &Sampler) -> Result<MetricsResponse, String> {
    let config = app
        .state::<SharedConfig>()
        .lock()
        .map_err(|e| e.to_string())?
        .clone();
    let metrics = sample_cached_shared(
        &app.state::<SharedMonitor>(),
        &app.state::<SharedTemperature>(),
        &app.state::<SharedLhmExtras>(),
        &config.network_selection,
    )?;
    if sampler.stopped.load(Ordering::Acquire) {
        return Err("后台采样已停止".to_string());
    }
    let (mut alert, newly_fired) = app
        .state::<SharedAlerts>()
        .lock()
        .map_err(|e| e.to_string())?
        .evaluate(&metrics, Duration::from_secs(config.alert_sustain_secs));
    if config.notification_enabled
        && !crate::quiet_hours_active(&config)
        && !sampler.stopped.load(Ordering::Acquire)
        && crate::send_alert_notification(app, &newly_fired)
    {
        let kinds = newly_fired
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>();
        app.state::<SharedAlerts>()
            .lock()
            .map_err(|e| e.to_string())?
            .mark_fired(&kinds);
    }
    let codex_messages = crate::codex::low_messages(&app.state::<SharedCodex>().snapshot());
    if !codex_messages.is_empty() {
        alert.active = true;
        alert.messages.extend(codex_messages);
    }
    Ok(MetricsResponse {
        metrics,
        alert,
        temp_source: app.state::<SharedTempTracker>().get().as_str().to_string(),
        stale: false,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response() -> MetricsResponse {
        MetricsResponse {
            metrics: crate::monitor::Metrics {
                cpu_percent: 0.0,
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
                sampled_at_ms: 42,
            },
            alert: crate::alert::AlertStatus {
                cpu: false,
                memory: false,
                temperature: false,
                disk: false,
                gpu: false,
                active: false,
                messages: Vec::new(),
                thresholds: crate::alert::AlertThresholds::default(),
            },
            temp_source: "unavailable".to_string(),
            stale: false,
            error: None,
        }
    }
    #[test]
    fn successful_snapshot_reads_preserve_samples_and_use_monotonic_freshness() {
        let now = Instant::now();
        let mut store = SnapshotStore::default();
        store.success(response(), Duration::from_millis(20), now);
        for _ in 0..10 {
            let frame = store.read(now + Duration::from_secs(1)).unwrap();
            assert_eq!(frame.metrics.sampled_at_ms, 42);
            assert!(!frame.stale);
        }
        assert_eq!(store.diagnostics.total_samples, 1);
        assert!(store.read(now + Duration::from_secs(4)).unwrap().stale);
        store.failure("采样失败".to_string(), Duration::from_millis(12));
        let failed = store.read(now + Duration::from_secs(1)).unwrap();
        assert!(failed.stale);
        assert_eq!(failed.metrics.sampled_at_ms, 42);
        assert_eq!(failed.error.as_deref(), Some("采样失败"));
        assert_eq!(store.diagnostics.total_samples, 1);
        store.success(
            response(),
            Duration::from_millis(10),
            now + Duration::from_secs(2),
        );
        assert!(!store.read(now + Duration::from_secs(2)).unwrap().stale);
        assert_eq!(store.diagnostics.total_samples, 2);
    }
    #[test]
    fn getters_are_read_only_and_failure_does_not_advance_samples() {
        let mut store = SnapshotStore::default();
        assert!(store.read(Instant::now()).is_err());
        for _ in 0..10 {
            assert!(store.read(Instant::now()).is_err());
        }
        assert_eq!(store.diagnostics.total_samples, 0);
        store.failure("采样失败".to_string(), Duration::from_millis(12));
        assert_eq!(store.diagnostics.total_samples, 0);
        assert_eq!(store.diagnostics.last_duration_ms, Some(12));
        assert_eq!(store.read(Instant::now()).unwrap_err(), "采样失败");
    }
    #[test]
    fn stop_is_idempotent_and_no_second_worker_can_start() {
        let sampler = Sampler::default();
        assert!(sampler
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok());
        assert!(sampler
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err());
        sampler.stop();
        sampler.stop();
        assert!(sampler.stopped.load(Ordering::Acquire));
        assert!(!sampler.diagnostics().unwrap().running);
    }
}
