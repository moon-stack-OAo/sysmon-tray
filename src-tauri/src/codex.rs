use crate::config::{AppConfig, SharedConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::Manager;
use tauri_plugin_notification::NotificationExt;

const REFRESH_SECS: u64 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub window_duration_mins: Option<u64>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBucket {
    pub id: String,
    pub name: String,
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    pub enabled: bool,
    pub refreshing: bool,
    pub stale: bool,
    pub observed_at_ms: Option<u64>,
    pub next_refresh_at_ms: Option<u64>,
    pub error: Option<String>,
    pub buckets: Vec<QuotaBucket>,
}

impl QuotaSnapshot {
    pub fn sync_menu_state(&self) -> (String, bool) {
        if !self.enabled {
            return ("同步 Codex 额度（监测已关闭）".to_string(), false);
        }
        if self.refreshing {
            return ("正在同步 Codex 额度…".to_string(), false);
        }
        if self.error.is_some() {
            return ("重试同步 Codex 额度".to_string(), true);
        }
        let updated = self.observed_at_ms.and_then(|at| {
            chrono::DateTime::from_timestamp_millis(i64::try_from(at).ok()?).map(|time| {
                time.with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string()
            })
        });
        let title = updated.map_or_else(
            || "同步 Codex 额度".to_string(),
            |time| format!("同步 Codex 额度（上次 {time}）"),
        );
        (title, true)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn retry_delay_secs(failures: u32) -> u64 {
    match failures {
        0 => REFRESH_SECS,
        1 => 15,
        2 => 30,
        3 => 60,
        _ => REFRESH_SECS,
    }
}

struct QueryCancellation<'a> {
    stop: &'a AtomicBool,
    epoch: &'a AtomicU64,
    expected: u64,
}

impl QueryCancellation<'_> {
    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.epoch.load(Ordering::Acquire) != self.expected
    }
}

fn query_with_reconnect<C, T>(
    connection: &mut Option<C>,
    cancellation: &QueryCancellation<'_>,
    mut connect: impl FnMut() -> Result<Option<C>, String>,
    mut query: impl FnMut(&mut C) -> Result<Option<T>, String>,
) -> Result<Option<T>, String> {
    for attempt in 0..2 {
        if cancellation.cancelled() {
            connection.take();
            return Ok(None);
        }
        let result = (|| {
            if connection.is_none() {
                *connection = connect()?;
            }
            if cancellation.cancelled() {
                return Ok(None);
            }
            match connection.as_mut() {
                Some(connection) => query(connection),
                None => Ok(None),
            }
        })();
        if cancellation.cancelled() || matches!(result, Ok(None)) {
            connection.take();
            return Ok(None);
        }
        match result {
            Ok(result) => return Ok(result),
            Err(error) => {
                connection.take();
                if attempt == 1 {
                    return Err(error);
                }
            }
        }
    }
    unreachable!()
}

fn wait_response(
    messages: &mpsc::Receiver<Value>,
    id: u64,
    cancellation: &QueryCancellation<'_>,
) -> Result<Option<Value>, String> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    while Instant::now() < deadline {
        if cancellation.cancelled() {
            return Ok(None);
        }
        let message = messages.recv_timeout(Duration::from_millis(50));
        if cancellation.cancelled() {
            return Ok(None);
        }
        match message {
            Ok(message) if message["id"].as_u64() == Some(id) => {
                if message.get("error").is_some() {
                    return Err("额度查询失败，请检查网络并确认 Codex 已登录 ChatGPT 账户。"
                        .to_string());
                }
                return message
                    .get("result")
                    .cloned()
                    .map(Some)
                    .ok_or_else(|| "Codex 返回了无效的额度响应。".to_string());
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err("Codex 查询进程已退出，请检查登录状态后刷新。".to_string()),
        }
    }
    Err("额度查询超时，稍后会自动重试。".to_string())
}

pub fn normalize_quota(result: &Value) -> Result<Vec<QuotaBucket>, String> {
    let mut entries: Vec<(String, &Value)> = result["rateLimitsByLimitId"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(_, value)| value.is_object())
                .map(|(id, value)| (id.clone(), value))
                .collect()
        })
        .unwrap_or_default();
    if entries.is_empty() && result["rateLimits"].is_object() {
        let bucket = &result["rateLimits"];
        entries.push((
            bucket["limitId"].as_str().unwrap_or("codex").to_string(),
            bucket,
        ));
    }
    let buckets: Vec<_> = entries
        .into_iter()
        .map(|(id, value)| {
            let windows = ["primary", "secondary"]
                .into_iter()
                .filter_map(|position| {
                    let window = &value[position];
                    if !window.is_object() {
                        return None;
                    }
                    let minutes = window["windowDurationMins"].as_u64().filter(|v| *v > 0);
                    let label = match minutes {
                        Some(300) => "5 小时额度".to_string(),
                        Some(10080) => "每周额度".to_string(),
                        Some(v) if v % 60 == 0 => format!("{} 小时额度", v / 60),
                        Some(v) => format!("{v} 分钟额度"),
                        None => if position == "primary" {
                            "主要额度"
                        } else {
                            "次要额度"
                        }
                        .to_string(),
                    };
                    Some(QuotaWindow {
                        id: position.to_string(),
                        label,
                        window_duration_mins: minutes,
                        remaining_percent: window["usedPercent"]
                            .as_f64()
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .map(|v| ((100.0 - v).clamp(0.0, 100.0) * 10.0).round() / 10.0),
                        resets_at: window["resetsAt"]
                            .as_u64()
                            .filter(|v| *v > 0 && *v < 253402300800),
                    })
                })
                .collect();
            QuotaBucket {
                name: value["limitName"]
                    .as_str()
                    .unwrap_or(if id == "codex" { "Codex" } else { &id })
                    .to_string(),
                id,
                plan: value["planType"].as_str().map(String::from),
                windows,
            }
        })
        .collect();
    if !buckets
        .iter()
        .any(|b| b.windows.iter().any(|w| w.remaining_percent.is_some()))
    {
        return Err("未返回可用额度，请确认 Codex 已使用 ChatGPT 账户登录。".to_string());
    }
    Ok(buckets)
}

fn codex_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("MONITOR_CODEX_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err("MONITOR_CODEX_PATH 指向的 Codex 程序不存在。".to_string());
    }
    if let Some(base) = dirs::data_local_dir() {
        let bin = base.join("OpenAI/Codex/bin");
        let mut candidates: Vec<_> = std::fs::read_dir(bin)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join("codex.exe"))
            .filter(|path| path.is_file())
            .collect();
        candidates.sort_by_key(|path| {
            std::cmp::Reverse(
                path.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH),
            )
        });
        if let Some(path) = candidates.into_iter().next() {
            return Ok(path);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            let path = directory.join(if cfg!(windows) { "codex.exe" } else { "codex" });
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err("未找到 Codex，请安装并登录 Codex，或设置 MONITOR_CODEX_PATH。".to_string())
}

struct CodexProcess {
    child: Child,
    messages: mpsc::Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
}

impl CodexProcess {
    fn start(cancellation: &QueryCancellation<'_>) -> Result<Option<Self>, String> {
        if cancellation.cancelled() {
            return Ok(None);
        }
        let mut command = Command::new(codex_executable()?);
        command
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|_| "无法启动 Codex 查询进程，请检查安装与运行权限。".to_string())?;
        let stdout = child.stdout.take().ok_or("无法读取 Codex 输出。")?;
        let (sender, messages) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(message) = serde_json::from_str(&line) {
                    if sender.send(message).is_err() {
                        break;
                    }
                }
            }
        });
        let mut process = Self {
            child,
            messages,
            reader: Some(reader),
            next_id: 1,
        };
        if process.request("initialize", Some(json!({"clientInfo":{"name":"sysmon_codex_quota","title":"SysMon","version":env!("CARGO_PKG_VERSION")},"capabilities":{}})), cancellation)?.is_none()
            || cancellation.cancelled()
        {
            return Ok(None);
        }
        process.write(&json!({"method":"initialized"}))?;
        Ok(Some(process))
    }

    fn write(&mut self, message: &Value) -> Result<(), String> {
        let input = self.child.stdin.as_mut().ok_or("Codex 查询连接已关闭。")?;
        writeln!(input, "{message}")
            .and_then(|_| input.flush())
            .map_err(|_| "Codex 查询连接已关闭。".to_string())
    }

    fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        cancellation: &QueryCancellation<'_>,
    ) -> Result<Option<Value>, String> {
        if cancellation.cancelled() {
            return Ok(None);
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"id":id,"method":method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.write(&message)?;
        wait_response(&self.messages, id, cancellation)
    }
}

impl Drop for CodexProcess {
    fn drop(&mut self) {
        self.child.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub fn probe() -> Result<Vec<QuotaBucket>, String> {
    let stop = AtomicBool::new(false);
    let epoch = AtomicU64::new(0);
    let cancellation = QueryCancellation {
        stop: &stop,
        epoch: &epoch,
        expected: 0,
    };
    read_quota(&mut None, &cancellation)?
        .ok_or_else(|| "额度监测已停止。".to_string())
}

fn read_quota(
    process: &mut Option<CodexProcess>,
    cancellation: &QueryCancellation<'_>,
) -> Result<Option<Vec<QuotaBucket>>, String> {
    query_with_reconnect(
        process,
        cancellation,
        || CodexProcess::start(cancellation),
        |process| {
            process
                .request("account/rateLimits/read", None, cancellation)?
                .map(|result| normalize_quota(&result))
                .transpose()
        },
    )
}

pub struct CodexMonitor {
    state: Mutex<QuotaSnapshot>,
    wake: mpsc::SyncSender<()>,
    manual_requested: AtomicBool,
    stop: AtomicBool,
    epoch: AtomicU64,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl CodexMonitor {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (wake, receiver) = mpsc::sync_channel(1);
        (
            Arc::new(Self {
                state: Mutex::new(QuotaSnapshot::default()),
                wake,
                manual_requested: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                worker: Mutex::new(None),
            }),
            receiver,
        )
    }
    pub fn snapshot(&self) -> QuotaSnapshot {
        let mut state = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        state.stale = !state.enabled
            || state.error.is_some()
            || state
                .observed_at_ms
                .is_none_or(|at| now_ms().saturating_sub(at) > 360_000);
        state
    }
    pub fn refresh(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if !state.enabled || state.refreshing || self.stop.load(Ordering::Relaxed) {
            return false;
        }
        state.refreshing = true;
        self.manual_requested.store(true, Ordering::Release);
        if !self.wake() {
            self.manual_requested.store(false, Ordering::Release);
            state.refreshing = false;
            state.error = Some("额度查询服务已停止，请重启 SysMon。".to_string());
            return false;
        }
        true
    }
    pub fn settings_changed(&self, enabled: bool) {
        if let Ok(mut state) = self.state.lock() {
            if state.enabled != enabled {
                self.epoch.fetch_add(1, Ordering::AcqRel);
                state.enabled = enabled;
                state.refreshing = false;
                state.next_refresh_at_ms = None;
                state.error = None;
                self.manual_requested.store(false, Ordering::Release);
            }
        }
        self.wake();
    }
    fn wake(&self) -> bool {
        matches!(
            self.wake.try_send(()),
            Ok(()) | Err(mpsc::TrySendError::Full(()))
        )
    }
    fn cancellation(&self, expected: u64) -> QueryCancellation<'_> {
        QueryCancellation {
            stop: &self.stop,
            epoch: &self.epoch,
            expected,
        }
    }

    fn commit_query(
        &self,
        expected: u64,
        result: Result<Option<Vec<QuotaBucket>>, String>,
        failures: &mut u32,
    ) -> Option<u64> {
        let mut state = self.state.lock().ok()?;
        if !state.enabled || self.cancellation(expected).cancelled() {
            return None;
        }
        let delay = match result {
            Ok(Some(buckets)) => {
                *failures = 0;
                state.buckets = buckets;
                state.observed_at_ms = Some(now_ms());
                state.error = None;
                REFRESH_SECS
            }
            Ok(None) => return None,
            Err(error) => {
                *failures = failures.saturating_add(1);
                state.error = Some(error);
                retry_delay_secs(*failures)
            }
        };
        state.refreshing = false;
        state.next_refresh_at_ms = Some(now_ms() + delay * 1000);
        Some(delay)
    }

    pub fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            self.stop.store(true, Ordering::Release);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            state.refreshing = false;
            state.next_refresh_at_ms = None;
            self.manual_requested.store(false, Ordering::Release);
        } else {
            self.stop.store(true, Ordering::Release);
        }
        self.wake();
        let worker = self.worker.lock().ok().and_then(|mut slot| slot.take());
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AlertMemory {
    sent: HashMap<String, u8>,
}

fn pending_alerts(
    snapshot: &QuotaSnapshot,
    memory: &AlertMemory,
    now_secs: u64,
) -> Vec<(String, u8, String)> {
    if !snapshot.enabled || snapshot.stale {
        return Vec::new();
    }
    snapshot
        .buckets
        .iter()
        .flat_map(|bucket| {
            bucket.windows.iter().filter_map(|window| {
                let remaining = window.remaining_percent?;
                let reset = window.resets_at?;
                if reset <= now_secs || remaining > 20.0 {
                    return None;
                }
                let level = if remaining <= 10.0 { 10 } else { 20 };
                let key = format!("{}/{}/{reset}", bucket.id, window.id);
                if memory
                    .sent
                    .get(&key)
                    .is_some_and(|previous| *previous <= level)
                {
                    return None;
                }
                Some((
                    key,
                    level,
                    format!("{} · {}剩余 {remaining:.0}%", bucket.name, window.label),
                ))
            })
        })
        .collect()
}

pub fn low_messages(snapshot: &QuotaSnapshot) -> Vec<String> {
    pending_alerts(snapshot, &AlertMemory::default(), now_ms() / 1000)
        .into_iter()
        .map(|(_, _, message)| message)
        .collect()
}

fn alert_path() -> Option<PathBuf> {
    AppConfig::config_path()
        .ok()
        .and_then(|path| path.parent().map(|p| p.join("codex-alerts.json")))
}

fn notify(
    app: &tauri::AppHandle,
    monitor: &CodexMonitor,
    memory: &mut AlertMemory,
    epoch: u64,
) {
    let permitted = app
        .state::<SharedConfig>()
        .lock()
        .map(|cfg| {
            cfg.notification_enabled && cfg.codex_enabled && !crate::quiet_hours_active(&cfg)
        })
        .unwrap_or(false);
    if !permitted {
        return;
    }
    let Ok(state) = monitor.state.lock() else {
        return;
    };
    if monitor.cancellation(epoch).cancelled() || state.error.is_some() {
        return;
    }
    let pending = pending_alerts(&state, memory, now_ms() / 1000);
    if pending.is_empty() {
        return;
    }
    if !matches!(
        app.notification().permission_state(),
        Ok(tauri::plugin::PermissionState::Granted)
    ) {
        return;
    }
    let body = pending
        .iter()
        .map(|(_, _, message)| message.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    if app
        .notification()
        .builder()
        .title("Codex 额度提醒")
        .body(body)
        .show()
        .is_ok()
    {
        for (key, level, _) in pending {
            memory.sent.insert(key, level);
        }
        memory.sent.retain(|key, _| {
            key.rsplit('/')
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|reset| reset > now_ms() / 1000)
        });
        if let Some(path) = alert_path() {
            if let Ok(raw) = serde_json::to_vec(memory) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let temporary = path.with_extension("json.tmp");
                if std::fs::write(&temporary, raw).is_ok() {
                    let _ = std::fs::rename(temporary, path);
                }
            }
        }
    }
}

pub fn start_worker(
    app: tauri::AppHandle,
    monitor: Arc<CodexMonitor>,
    receiver: mpsc::Receiver<()>,
) {
    if let Ok(config) = app.state::<SharedConfig>().lock() {
        monitor.settings_changed(config.codex_enabled);
    }
    let owner = Arc::clone(&monitor);
    let worker = std::thread::spawn(move || {
        let mut process: Option<CodexProcess> = None;
        let mut due = Instant::now();
        let mut failures: u32 = 0;
        let mut worker_epoch = None;
        let mut memory = alert_path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        while !monitor.stop.load(Ordering::Acquire) {
            let mut reset_connection = false;
            let query_epoch = {
                let Ok(mut state) = monitor.state.lock() else {
                    break;
                };
                let epoch = monitor.epoch.load(Ordering::Acquire);
                if worker_epoch != Some(epoch) {
                    reset_connection = true;
                    failures = 0;
                    due = Instant::now();
                    worker_epoch = Some(epoch);
                }
                let manual = monitor.manual_requested.swap(false, Ordering::AcqRel);
                if state.enabled
                    && (manual || Instant::now() >= due)
                    && !monitor.stop.load(Ordering::Acquire)
                {
                    state.refreshing = true;
                    Some(epoch)
                } else {
                    None
                }
            };
            if reset_connection {
                process = None;
            }
            if let Some(epoch) = query_epoch {
                crate::publish_codex_state(&app, &monitor);
                let result = read_quota(&mut process, &monitor.cancellation(epoch));
                let succeeded = matches!(result, Ok(Some(_)));
                let Some(delay) = monitor.commit_query(epoch, result, &mut failures) else {
                    process = None;
                    continue;
                };
                due = Instant::now() + Duration::from_secs(delay);
                crate::publish_codex_state(&app, &monitor);
                if succeeded {
                    notify(&app, &monitor, &mut memory, epoch);
                }
            }
            if receiver.recv_timeout(Duration::from_secs(1)).is_ok() {
                while receiver.try_recv().is_ok() {}
            }
        }
    });
    if let Ok(mut slot) = owner.worker.lock() {
        *slot = Some(worker);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_connection_is_replaced_before_retrying_and_success_is_reused() {
        use std::sync::atomic::AtomicUsize;
        struct Connection {
            id: usize,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Connection {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let cancellation = QueryCancellation {
            stop: &stop,
            epoch: &epoch,
            expected: 0,
        };
        let mut connection = None;
        let mut connected = 0;
        let mut queried = Vec::new();
        let result = query_with_reconnect(
            &mut connection,
            &cancellation,
            || {
                connected += 1;
                Ok(Some(Connection {
                    id: connected,
                    dropped: Arc::clone(&dropped),
                }))
            },
            |connection| {
                queried.push(connection.id);
                if connection.id == 1 {
                    Err("旧连接断开".to_string())
                } else {
                    Ok(Some(77))
                }
            },
        );
        assert_eq!(result, Ok(Some(77)));
        assert_eq!(queried, vec![1, 2]);
        assert_eq!(connected, 2);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        let next = query_with_reconnect(
            &mut connection,
            &cancellation,
            || -> Result<Option<Connection>, String> { panic!("健康连接不应重建") },
            |connection| Ok(Some(connection.id)),
        );
        assert_eq!(next, Ok(Some(2)));
    }

    #[test]
    fn repeated_failure_stops_after_one_reconnect_and_keeps_last_error() {
        let mut connection: Option<()> = None;
        let stop = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let cancellation = QueryCancellation {
            stop: &stop,
            epoch: &epoch,
            expected: 0,
        };
        let mut attempts = 0;
        let result: Result<Option<()>, String> = query_with_reconnect(
            &mut connection,
            &cancellation,
            || {
                attempts += 1;
                Err(format!("启动失败 {attempts}"))
            },
            |_| panic!("启动失败时不应发送查询"),
        );
        assert_eq!(attempts, 2);
        assert_eq!(result, Err("启动失败 2".to_string()));
        assert!(connection.is_none());
    }

    #[test]
    fn shutdown_prevents_reconnecting_after_a_failed_request() {
        let stop = AtomicBool::new(false);
        let epoch = AtomicU64::new(0);
        let cancellation = QueryCancellation {
            stop: &stop,
            epoch: &epoch,
            expected: 0,
        };
        let mut connection = Some(());
        let result: Result<Option<()>, String> = query_with_reconnect(
            &mut connection,
            &cancellation,
            || panic!("停止后不应创建连接"),
            |_| {
                stop.store(true, Ordering::Relaxed);
                Err("请求中断".to_string())
            },
        );
        assert_eq!(result, Ok(None));
        assert!(connection.is_none());
    }

    #[test]
    fn disabling_interrupts_initialize_and_quota_waits_without_reconnecting() {
        for method in ["initialize", "account/rateLimits/read"] {
            let (monitor, _receiver) = CodexMonitor::new();
            monitor.settings_changed(true);
            let expected = monitor.epoch.load(Ordering::Acquire);
            let (entered, waiting) = mpsc::channel();
            let (sender, messages) = mpsc::channel();
            let owner = Arc::clone(&monitor);
            let worker = std::thread::spawn(move || {
                let cancellation = owner.cancellation(expected);
                let mut connection = if method == "initialize" { None } else { Some(()) };
                let result = query_with_reconnect(
                    &mut connection,
                    &cancellation,
                    || {
                        assert_eq!(method, "initialize", "取消后不能重连");
                        entered.send(()).unwrap();
                        wait_response(&messages, 1, &cancellation).map(|value| value.map(|_| ()))
                    },
                    |_| {
                        assert_ne!(method, "initialize", "取消初始化后不能发送额度查询");
                        entered.send(()).unwrap();
                        wait_response(&messages, 1, &cancellation).map(|value| value.map(|_| ()))
                    },
                );
                assert!(connection.is_none());
                result
            });
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            monitor.settings_changed(false);
            assert_eq!(worker.join().unwrap(), Ok(None));
            drop(sender);
            let state = monitor.snapshot();
            assert!(!state.refreshing);
            assert!(state.error.is_none());
            assert!(state.next_refresh_at_ms.is_none());
        }
    }

    #[test]
    fn disable_after_failure_prevents_reconnect_and_drops_old_connection() {
        let (monitor, _receiver) = CodexMonitor::new();
        monitor.settings_changed(true);
        let cancellation = monitor.cancellation(monitor.epoch.load(Ordering::Acquire));
        struct Connection(Arc<AtomicBool>);
        impl Drop for Connection {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let mut connection = Some(Connection(Arc::clone(&dropped)));
        let result: Result<Option<()>, String> = query_with_reconnect(
            &mut connection,
            &cancellation,
            || panic!("关闭后不应重连"),
            |_| {
                monitor.settings_changed(false);
                Err("连接失败".to_string())
            },
        );
        assert_eq!(result, Ok(None));
        assert!(connection.is_none());
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn rapid_disable_enable_discards_old_results_and_recovers_immediately() {
        let (monitor, _receiver) = CodexMonitor::new();
        monitor.settings_changed(true);
        let old_epoch = monitor.epoch.load(Ordering::Acquire);
        let mut failures = 3;
        let buckets = normalize_quota(&json!({"rateLimits":{"primary":{"usedPercent":81,"resetsAt":2000000000u64}}})).unwrap();
        let mut connection = Some(1);
        let result = query_with_reconnect(
            &mut connection,
            &monitor.cancellation(old_epoch),
            || panic!("旧请求不应重连"),
            |_| {
                monitor.settings_changed(false);
                monitor.settings_changed(true);
                Ok(Some(buckets.clone()))
            },
        );
        assert!(matches!(result, Ok(None)));
        assert!(connection.is_none());
        assert_eq!(
            monitor.commit_query(old_epoch, Ok(Some(buckets.clone())), &mut failures),
            None
        );
        assert_eq!(
            monitor.commit_query(old_epoch, Err("迟到错误".to_string()), &mut failures),
            None
        );
        assert_eq!(failures, 3);
        let state = monitor.snapshot();
        assert!(state.buckets.is_empty());
        assert!(state.observed_at_ms.is_none());
        assert!(state.error.is_none());
        assert!(state.next_refresh_at_ms.is_none());
        assert!(pending_alerts(&state, &AlertMemory::default(), 1000).is_empty());
        let epoch = monitor.epoch.load(Ordering::Acquire);
        let result = query_with_reconnect(
            &mut connection,
            &monitor.cancellation(epoch),
            || Ok(Some(2)),
            |connection| {
                assert_eq!(*connection, 2);
                Ok(Some(buckets.clone()))
            },
        );
        assert_eq!(
            monitor.commit_query(epoch, result, &mut failures),
            Some(REFRESH_SECS)
        );
        assert_eq!(failures, 0);
        assert_eq!(
            monitor.snapshot().buckets[0].windows[0].remaining_percent,
            Some(19.0)
        );
        assert!(monitor.snapshot().observed_at_ms.is_some());
    }

    #[test]
    fn queued_response_from_old_epoch_is_not_accepted_after_reenable() {
        let (monitor, _receiver) = CodexMonitor::new();
        monitor.settings_changed(true);
        let epoch = monitor.epoch.load(Ordering::Acquire);
        let (sender, messages) = mpsc::channel();
        sender.send(json!({"id":1,"result":{"old":true}})).unwrap();
        monitor.settings_changed(false);
        monitor.settings_changed(true);
        assert_eq!(
            wait_response(&messages, 1, &monitor.cancellation(epoch)),
            Ok(None)
        );
        let epoch = monitor.epoch.load(Ordering::Acquire);
        sender.send(json!({"id":2,"result":{"fresh":true}})).unwrap();
        assert_eq!(
            wait_response(&messages, 2, &monitor.cancellation(epoch)),
            Ok(Some(json!({"fresh":true})))
        );
    }

    #[test]
    fn shutdown_interrupts_wait_and_rejects_late_worker_commit() {
        let (monitor, _receiver) = CodexMonitor::new();
        monitor.settings_changed(true);
        let epoch = monitor.epoch.load(Ordering::Acquire);
        let (entered, waiting) = mpsc::channel();
        let (sender, messages) = mpsc::channel();
        let owner = Arc::clone(&monitor);
        let worker = std::thread::spawn(move || {
            entered.send(()).unwrap();
            assert_eq!(
                wait_response(&messages, 1, &owner.cancellation(epoch)),
                Ok(None)
            );
        });
        *monitor.worker.lock().unwrap() = Some(worker);
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        monitor.shutdown();
        drop(sender);
        assert!(monitor.worker.lock().unwrap().is_none());
        let mut failures = 0;
        assert_eq!(
            monitor.commit_query(epoch, Err("迟到错误".to_string()), &mut failures),
            None
        );
        assert_eq!(failures, 0);
        assert!(monitor.snapshot().error.is_none());
        assert!(monitor.snapshot().next_refresh_at_ms.is_none());
    }

    #[test]
    fn automatic_recovery_uses_short_bounded_delays_and_resets_after_success() {
        assert_eq!(
            (0..6).map(retry_delay_secs).collect::<Vec<_>>(),
            vec![120, 15, 30, 60, 120, 120]
        );
        assert_eq!(retry_delay_secs(u32::MAX), 120);
        assert_eq!(retry_delay_secs(0), REFRESH_SECS);
    }

    #[test]
    fn manual_sync_is_immediate_and_coalesces_repeated_clicks() {
        let (monitor, receiver) = CodexMonitor::new();
        monitor.settings_changed(true);
        {
            let mut state = monitor.state.lock().unwrap();
            state.observed_at_ms = Some(now_ms());
            state.next_refresh_at_ms = Some(now_ms() + 900_000);
            state.error = Some("网络暂时不可用".to_string());
        }
        assert!(monitor.refresh());
        assert!(monitor.snapshot().refreshing);
        assert!(!monitor.refresh());
        assert!(monitor.manual_requested.swap(false, Ordering::AcqRel));
        assert!(!monitor.manual_requested.swap(false, Ordering::AcqRel));
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        monitor.state.lock().unwrap().refreshing = false;
        assert!(monitor.refresh());
    }

    #[test]
    fn disabled_or_stopped_monitor_does_not_accept_sync_requests() {
        let (monitor, _receiver) = CodexMonitor::new();
        assert!(!monitor.refresh());
        monitor.settings_changed(true);
        assert!(monitor.refresh());
        monitor.settings_changed(false);
        assert!(!monitor.snapshot().refreshing);
        assert!(!monitor.manual_requested.load(Ordering::Acquire));
        assert!(!monitor.refresh());
        monitor.settings_changed(true);
        monitor.shutdown();
        assert!(!monitor.refresh());
    }

    #[test]
    fn stopped_worker_reports_failure_instead_of_staying_busy() {
        let (monitor, receiver) = CodexMonitor::new();
        monitor.settings_changed(true);
        drop(receiver);
        assert!(!monitor.refresh());
        let state = monitor.snapshot();
        assert!(!state.refreshing);
        assert!(state.stale);
        assert!(state.error.unwrap().contains("请重启 SysMon"));
    }

    #[test]
    fn tray_menu_tracks_sync_retry_disabled_and_success_states() {
        let mut state = QuotaSnapshot::default();
        assert_eq!(
            state.sync_menu_state(),
            ("同步 Codex 额度（监测已关闭）".to_string(), false)
        );
        state.enabled = true;
        state.error = Some("网络暂时不可用".to_string());
        assert_eq!(
            state.sync_menu_state(),
            ("重试同步 Codex 额度".to_string(), true)
        );
        state.refreshing = true;
        assert_eq!(
            state.sync_menu_state(),
            ("正在同步 Codex 额度…".to_string(), false)
        );
        state.refreshing = false;
        state.error = None;
        state.observed_at_ms = Some(now_ms());
        let (title, enabled) = state.sync_menu_state();
        assert!(enabled);
        assert!(title.starts_with("同步 Codex 额度（上次 "));
        state.enabled = false;
        assert!(!state.sync_menu_state().1);
    }

    #[test]
    fn normalizes_all_buckets_without_leaking_account_fields() {
        let value = json!({"accountId":"private-account","rateLimits":{"primary":{"usedPercent":99}},"rateLimitsByLimitId":{"codex":{"planType":"plus","primary":{"usedPercent":23,"windowDurationMins":300,"resetsAt":1800000000},"secondary":{"usedPercent":4,"windowDurationMins":10080}},"extra":{"primary":{"usedPercent":105}}}});
        let buckets = normalize_quota(&value).unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].windows[0].remaining_percent, Some(77.0));
        assert_eq!(buckets[0].windows[1].remaining_percent, Some(96.0));
        assert_eq!(buckets[1].windows[0].remaining_percent, Some(0.0));
        assert!(!serde_json::to_string(&buckets)
            .unwrap()
            .contains("private-account"));
    }
    #[test]
    fn missing_fields_stay_unknown_and_windows_are_dynamic() {
        let buckets = normalize_quota(&json!({"rateLimits":{"primary":{"usedPercent":12,"windowDurationMins":60},"secondary":{"usedPercent":null}}})).unwrap();
        assert_eq!(buckets[0].windows[0].label, "1 小时额度");
        assert_eq!(buckets[0].windows[0].resets_at, None);
        assert_eq!(buckets[0].windows[1].remaining_percent, None);
        assert!(normalize_quota(&json!({"rateLimits":{"primary":{"usedPercent":-1}}})).is_err());
        assert!(normalize_quota(&json!({})).is_err());
    }
    #[test]
    fn alerts_deduplicate_escalate_and_rearm_after_reset() {
        let mut snapshot = QuotaSnapshot {
            enabled: true,
            stale: false,
            buckets: normalize_quota(
                &json!({"rateLimits":{"primary":{"usedPercent":81,"resetsAt":2000}}}),
            )
            .unwrap(),
            ..Default::default()
        };
        let mut memory = AlertMemory::default();
        let first = pending_alerts(&snapshot, &memory, 1000);
        assert_eq!(first.len(), 1);
        memory.sent.insert(first[0].0.clone(), first[0].1);
        let mut memory: AlertMemory =
            serde_json::from_str(&serde_json::to_string(&memory).unwrap()).unwrap();
        assert!(pending_alerts(&snapshot, &memory, 1000).is_empty());
        snapshot.buckets[0].windows[0].remaining_percent = Some(9.0);
        let second = pending_alerts(&snapshot, &memory, 1000);
        assert_eq!(second[0].1, 10);
        memory.sent.insert(second[0].0.clone(), second[0].1);
        assert!(pending_alerts(&snapshot, &memory, 1000).is_empty());
        snapshot.buckets[0].windows[0].resets_at = Some(3000);
        assert_eq!(pending_alerts(&snapshot, &memory, 1000).len(), 1);
        snapshot.stale = true;
        assert!(pending_alerts(&snapshot, &memory, 1000).is_empty());
        snapshot.stale = false;
        assert!(pending_alerts(&snapshot, &memory, 4000).is_empty());
    }
}
