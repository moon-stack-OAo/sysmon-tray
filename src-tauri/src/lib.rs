mod alert;
mod config;
mod codex;
mod history;
mod monitor;
mod temperature;
mod temperature_lhm;

use alert::{AlertEngine, AlertKind, AlertStatus, AlertThresholds, SharedAlerts};
use config::{
    mutate_config, normalize_lhm_base_url, normalize_overlay_metrics, validate_alert_sustain_secs,
    validate_hotkey, validate_quiet_minute, AppConfig, OverlayEdgeX, OverlayEdgeY, OverlayStyle,
    SharedConfig, DEFAULT_LHM_BASE_URL,
};
use history::{history_capacity_for_minutes, history_to_csv, HistoryPoint};
use monitor::{
    sample_cached_shared, sample_process_top_shared, Metrics, MonitorState, ProcessTopSnapshot,
    SharedMonitor,
    SharedTemperature, METRICS_CACHE_MIN_INTERVAL,
};
use chrono::Timelike;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    window::{Effect, EffectState, EffectsBuilder},
    AppHandle, Emitter, LogicalPosition, Manager, Monitor, PhysicalPosition, PhysicalSize, RunEvent,
    WindowEvent,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use temperature::ChainedTemperatureProvider;
use temperature_lhm::{LhmExtras, LhmRuntimeConfig, SharedLhmExtras, TempSourceKind, TempSourceTracker};

pub type SharedSettingsOpen = Mutex<bool>;
pub type SharedLhmRuntime = Arc<LhmRuntimeConfig>;
pub type SharedTempTracker = Arc<TempSourceTracker>;
pub type SharedOverlayMenu = Mutex<Option<CheckMenuItem<tauri::Wry>>>;
pub struct SharedOverlayPosSave(pub Mutex<Option<Instant>>);
pub struct SharedOverlayLastMove(pub Mutex<Option<Instant>>);
pub struct SharedOverlaySnapQuietUntil(pub Mutex<Option<Instant>>);
pub struct SharedOverlayPosDebounce(pub AtomicBool);
pub struct SharedMainPosSave(pub Mutex<Option<Instant>>);
pub struct SharedMainLastMove(pub Mutex<Option<Instant>>);
pub struct SharedMainPosDebounce(pub AtomicBool);
pub struct SharedTrayAlert(pub Mutex<bool>);
pub type SharedCodex = Arc<codex::CodexMonitor>;
pub type SharedCodexSyncMenu = Mutex<Option<MenuItem<tauri::Wry>>>;

const OVERLAY_SNAP_THRESHOLD_PX: i32 = 36;
const OVERLAY_SNAP_CORNER_BONUS_PX: i32 = 12;
const OVERLAY_SNAP_DEBOUNCE_MS: u64 = 220;
const OVERLAY_LAYOUT_QUIET_MS: u64 = 500;
const MAIN_POS_SAVE_DEBOUNCE_MS: u64 = 250;
const OVERLAY_COLLAPSED_WIDTH: u32 = 378;
const OVERLAY_COLLAPSED_HEIGHT: u32 = 38;
const OVERLAY_VERTICAL_WIDTH: u32 = 76;
const OVERLAY_VERTICAL_HEIGHT: u32 = 200;
const OVERLAY_NUMERIC_WIDTH: u32 = 246;
const OVERLAY_NUMERIC_HEIGHT: u32 = 34;
const OVERLAY_EXPANDED_WIDTH: u32 = 310;
const OVERLAY_EXPANDED_HEIGHT: u32 = 500;
const OVERLAY_PEEK_THICKNESS: u32 = 8;
const OVERLAY_PEEK_LENGTH: u32 = 96;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricsResponse {
    #[serde(flatten)]
    metrics: Metrics,
    alert: AlertStatus,
    /// 本次采样温度来源：`lhm` | `wmi` | `sysinfo` | `unavailable`
    temp_source: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TempSourceStatus {
    /// `lhm` | `wmi` | `sysinfo` | `unavailable`
    source: String,
    message: String,
    precise_temp_enabled: bool,
    lhm_base_url: String,
}

#[tauri::command]
fn get_codex_quota(monitor: tauri::State<'_, SharedCodex>, config: tauri::State<'_, SharedConfig>) -> codex::QuotaSnapshot {
    let mut snapshot = monitor.snapshot();
    if let Ok(cfg) = config.lock() {
        snapshot.enabled = cfg.codex_enabled;
        if !cfg.codex_enabled { snapshot.stale = true; }
    }
    snapshot
}

#[tauri::command]
fn refresh_codex_quota(app: AppHandle, monitor: tauri::State<'_, SharedCodex>) -> codex::QuotaSnapshot {
    monitor.refresh();
    publish_codex_state(&app, &monitor);
    monitor.snapshot()
}

pub(crate) fn publish_codex_state(app: &AppHandle, monitor: &codex::CodexMonitor) {
    let snapshot = monitor.snapshot();
    let menu = app.state::<SharedCodexSyncMenu>().lock().ok().and_then(|slot| slot.clone());
    if let Some(menu) = menu {
        let (title, enabled) = snapshot.sync_menu_state();
        let _ = app.run_on_main_thread(move || {
            let _ = menu.set_text(title);
            let _ = menu.set_enabled(enabled);
        });
    }
    let _ = app.emit("codex-quota-updated", ());
}

pub fn probe_codex_quota() -> Result<String, String> {
    serde_json::to_string_pretty(&codex::probe()?).map_err(|_| "无法格式化额度数据。".to_string())
}

fn ensure_notification_permission(app: &tauri::AppHandle) -> bool {
    use tauri::plugin::PermissionState;
    match app.notification().permission_state() {
        Ok(PermissionState::Granted) => true,
        Ok(PermissionState::Denied) => false,
        Ok(_) => app
            .notification()
            .request_permission()
            .map(|state| matches!(state, PermissionState::Granted))
            .unwrap_or(false),
        Err(err) => {
            eprintln!("读取通知权限失败: {err}");
            false
        }
    }
}

fn send_alert_notification(app: &tauri::AppHandle, newly_fired: &[(AlertKind, String)]) -> bool {
    if newly_fired.is_empty() {
        return false;
    }
    if !ensure_notification_permission(app) {
        eprintln!("系统通知权限未授予，跳过告警通知");
        return false;
    }
    let body = newly_fired
        .iter()
        .map(|(_, msg)| msg.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    // 通知失败不影响采样主路径；只有 show 成功才允许调用方记账冷却
    match app
        .notification()
        .builder()
        .title("系统监测告警")
        .body(&body)
        .show()
    {
        Ok(()) => true,
        Err(err) => {
            eprintln!("发送系统通知失败: {err}");
            false
        }
    }
}

fn temp_source_message(
    enabled: bool,
    kind: TempSourceKind,
) -> String {
    if !enabled {
        return match kind {
            TempSourceKind::Wmi => "精确温度已关闭 · 当前：ACPI/WMI".to_string(),
            TempSourceKind::Sysinfo => "精确温度已关闭 · 当前：sysinfo".to_string(),
            TempSourceKind::Lhm => "精确温度已关闭".to_string(),
            TempSourceKind::Unavailable => "精确温度已关闭 · 温度暂不可用".to_string(),
        };
    }

    match kind {
        TempSourceKind::Lhm => "已连接 LibreHardwareMonitor（含 GPU / 风扇）".to_string(),
        TempSourceKind::Wmi => "未检测到 LHM · 已回退 ACPI/WMI（GPU / 风扇需 LHM）".to_string(),
        TempSourceKind::Sysinfo => "未检测到 LHM · 已回退 sysinfo（GPU / 风扇需 LHM）".to_string(),
        TempSourceKind::Unavailable => "未检测到 LHM · 温度/GPU/风扇暂不可用".to_string(),
    }
}

#[tauri::command]
fn get_metrics(
    app: tauri::AppHandle,
    monitor: tauri::State<'_, SharedMonitor>,
    temperature: tauri::State<'_, SharedTemperature>,
    lhm_extras: tauri::State<'_, SharedLhmExtras>,
    alerts: tauri::State<'_, SharedAlerts>,
    config: tauri::State<'_, SharedConfig>,
    tracker: tauri::State<'_, SharedTempTracker>,
) -> Result<MetricsResponse, String> {
    // 主面板与叠加层共用短时缓存，避免双采样打乱网速差分；温度 IO 在 monitor 锁外进行
    let metrics = sample_cached_shared(
        &monitor,
        &temperature,
        &lhm_extras,
        METRICS_CACHE_MIN_INTERVAL,
    )?;

    let temp_source = tracker.get().as_str().to_string();

    let (notification_enabled, sustain_secs, quiet) = {
        let cfg = config.lock().map_err(|e| e.to_string())?;
        (
            cfg.notification_enabled,
            cfg.alert_sustain_secs,
            quiet_hours_active(&cfg),
        )
    };

    let (mut alert, newly_fired) = {
        let mut engine = alerts.lock().map_err(|e| e.to_string())?;
        engine.evaluate(&metrics, Duration::from_secs(sustain_secs))
    };

    // 安静时段仍保留横幅/tooltip，但不发通知、不记账，时段结束后下一次仍能响
    if notification_enabled && !quiet && send_alert_notification(&app, &newly_fired) {
        let kinds: Vec<AlertKind> = newly_fired.iter().map(|(kind, _)| *kind).collect();
        let mut engine = alerts.lock().map_err(|e| e.to_string())?;
        engine.mark_fired(&kinds);
    }

    let codex_messages = codex::low_messages(&app.state::<SharedCodex>().snapshot());
    if !codex_messages.is_empty() {
        alert.active = true;
        alert.messages.extend(codex_messages);
    }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(alert.tooltip_text()));
        sync_tray_alert_icon(&app, &tray, alert.active);
    }

    Ok(MetricsResponse {
        metrics,
        alert,
        temp_source,
    })
}

fn local_minute_of_day() -> u16 {
    let now = chrono::Local::now();
    now.time().hour() as u16 * 60 + now.time().minute() as u16
}

fn quiet_hours_active(cfg: &AppConfig) -> bool {
    if !cfg.quiet_hours_enabled {
        return false;
    }
    let now = local_minute_of_day();
    let start = cfg.quiet_start_minute;
    let end = cfg.quiet_end_minute;
    if start == end {
        return true;
    }
    if start < end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

fn default_tray_icon(app: &tauri::AppHandle) -> tauri::image::Image<'static> {
    let fallback = || tauri::include_image!("icons/32x32.png");
    let Some(icon) = app.default_window_icon() else {
        return fallback();
    };
    let rgba = icon.rgba();
    let width = icon.width();
    let height = icon.height();
    if rgba.len() != (width * height * 4) as usize {
        return fallback();
    }
    tauri::image::Image::new_owned(rgba.to_vec(), width, height)
}

fn alert_tray_icon(base: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let rgba = base.rgba();
    let width = base.width();
    let height = base.height();
    if width < 8 || height < 8 || rgba.len() != (width * height * 4) as usize {
        return tauri::image::Image::new_owned(rgba.to_vec(), width, height);
    }
    let mut next = rgba.to_vec();
    let badge = (width.min(height) / 3).max(6);
    let x0 = width - badge - 1;
    let y0 = height - badge - 1;
    let cx = x0 as f32 + (badge as f32 - 1.0) / 2.0;
    let cy = y0 as f32 + (badge as f32 - 1.0) / 2.0;
    let radius = badge as f32 / 2.0;
    for y in y0..height {
        for x in x0..width {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            let i = ((y * width + x) * 4) as usize;
            next[i] = 220;
            next[i + 1] = 64;
            next[i + 2] = 64;
            next[i + 3] = 255;
        }
    }
    tauri::image::Image::new_owned(next, width, height)
}

fn sync_tray_alert_icon(app: &tauri::AppHandle, tray: &tauri::tray::TrayIcon<tauri::Wry>, active: bool) {
    let changed = match app.state::<SharedTrayAlert>().0.lock() {
        Ok(mut last) => {
            if *last == active {
                false
            } else {
                *last = active;
                true
            }
        }
        Err(err) => {
            eprintln!("读取托盘告警状态失败: {err}");
            false
        }
    };
    if !changed {
        return;
    }
    let base = default_tray_icon(app);
    let image = if active {
        alert_tray_icon(&base)
    } else {
        base
    };
    if let Err(err) = tray.set_icon(Some(image)) {
        eprintln!("更新托盘图标失败: {err}");
    }
}

#[tauri::command]
fn export_metrics_csv(monitor: tauri::State<'_, SharedMonitor>) -> Result<String, String> {
    let monitor = monitor.lock().map_err(|e| e.to_string())?;
    Ok(history_to_csv(&monitor.history_snapshot()))
}

#[tauri::command]
fn get_metrics_history(
    monitor: tauri::State<'_, SharedMonitor>,
) -> Result<Vec<HistoryPoint>, String> {
    let monitor = monitor.lock().map_err(|e| e.to_string())?;
    Ok(monitor.history_snapshot())
}

#[tauri::command]
fn get_process_top(
    monitor: tauri::State<'_, SharedMonitor>,
    config: tauri::State<'_, SharedConfig>,
) -> Result<ProcessTopSnapshot, String> {
    let enabled = {
        let cfg = config.lock().map_err(|e| e.to_string())?;
        cfg.process_top_enabled
    };
    sample_process_top_shared(&monitor, enabled)
}

#[tauri::command]
fn set_process_top_enabled(
    enabled: bool,
    config: tauri::State<'_, SharedConfig>,
) -> Result<bool, String> {
    mutate_config(&config, |next| {
        next.process_top_enabled = enabled;
        Ok(())
    })?;
    Ok(enabled)
}

#[tauri::command]
fn get_alert_thresholds(alerts: tauri::State<'_, SharedAlerts>) -> Result<AlertThresholds, String> {
    let engine = alerts.lock().map_err(|e| e.to_string())?;
    Ok(engine.thresholds())
}

#[tauri::command]
fn set_alert_thresholds(
    thresholds: AlertThresholds,
    alerts: tauri::State<'_, SharedAlerts>,
    config: tauri::State<'_, SharedConfig>,
) -> Result<AlertThresholds, String> {
    thresholds.validate()?;

    mutate_config(&config, |next| {
        next.alert = thresholds.clone();
        Ok(())
    })?;

    let saved = {
        let mut engine = alerts.lock().map_err(|e| e.to_string())?;
        engine.set_thresholds(thresholds);
        engine.thresholds()
    };

    Ok(saved)
}

#[tauri::command]
fn reset_alert_thresholds(
    alerts: tauri::State<'_, SharedAlerts>,
    config: tauri::State<'_, SharedConfig>,
) -> Result<AlertThresholds, String> {
    let defaults = AlertThresholds::default();
    set_alert_thresholds(defaults, alerts, config)
}

#[tauri::command]
fn get_app_config(config: tauri::State<'_, SharedConfig>) -> Result<AppConfig, String> {
    let cfg = config.lock().map_err(|e| e.to_string())?;
    Ok(cfg.clone())
}

#[tauri::command]
fn set_notification_enabled(
    enabled: bool,
    app: AppHandle,
    config: tauri::State<'_, SharedConfig>,
) -> Result<bool, String> {
    // 权限校验前置，失败时零副作用
    if enabled && !ensure_notification_permission(&app) {
        return Err("系统通知权限未授予，请在 Windows 通知设置中允许本应用".to_string());
    }

    mutate_config(&config, |next| {
        next.notification_enabled = enabled;
        Ok(())
    })?;

    if enabled {
        // 开启时发一条测试通知，便于确认权限与系统开关正常
        if let Err(err) = app
            .notification()
            .builder()
            .title("系统监测")
            .body("已启用系统通知")
            .show()
        {
            eprintln!("发送测试通知失败: {err}");
        }
    }
    Ok(enabled)
}

#[tauri::command]
fn set_history_range_minutes(
    minutes: u32,
    monitor: tauri::State<'_, SharedMonitor>,
    config: tauri::State<'_, SharedConfig>,
) -> Result<u32, String> {
    let capacity = history_capacity_for_minutes(minutes)?;

    mutate_config(&config, |next| {
        next.history_range_minutes = minutes;
        Ok(())
    })?;

    // 历史容量与配置分时加锁，提交成功后再调整内存容量
    {
        let mut monitor = monitor.lock().map_err(|e| e.to_string())?;
        monitor.set_history_capacity(capacity);
    }

    Ok(minutes)
}

#[tauri::command]
fn set_precise_temp_enabled(
    enabled: bool,
    config: tauri::State<'_, SharedConfig>,
    lhm: tauri::State<'_, SharedLhmRuntime>,
) -> Result<bool, String> {
    mutate_config(&config, |next| {
        next.precise_temp_enabled = enabled;
        Ok(())
    })?;
    lhm.set_enabled(enabled);
    Ok(enabled)
}

#[tauri::command]
fn set_lhm_base_url(
    url: String,
    config: tauri::State<'_, SharedConfig>,
    lhm: tauri::State<'_, SharedLhmRuntime>,
) -> Result<String, String> {
    let normalized = normalize_lhm_base_url(&url)?;
    mutate_config(&config, |next| {
        next.lhm_base_url = normalized.clone();
        Ok(())
    })?;
    lhm.set_base_url(normalized.clone());
    Ok(normalized)
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInput {
    thresholds: AlertThresholds,
    notification_enabled: bool,
    history_range_minutes: u32,
    precise_temp_enabled: bool,
    lhm_base_url: String,
    overlay_enabled: bool,
    codex_enabled: bool,
    autostart_enabled: bool,
    overlay_auto_hide: bool,
    overlay_click_through: bool,
    overlay_style: String,
    process_top_enabled: bool,
    alert_sustain_secs: u64,
    quiet_hours_enabled: bool,
    quiet_start_minute: u16,
    quiet_end_minute: u16,
    hotkey_panel: String,
    hotkey_overlay: String,
    hotkey_click_through: String,
    overlay_metrics: Vec<String>,
}

impl SettingsInput {
    fn defaults() -> Self {
        let cfg = AppConfig::default();
        Self {
            thresholds: AlertThresholds::default(),
            notification_enabled: true,
            history_range_minutes: 1,
            precise_temp_enabled: false,
            lhm_base_url: DEFAULT_LHM_BASE_URL.to_string(),
            overlay_enabled: false,
            codex_enabled: cfg.codex_enabled,
            autostart_enabled: false,
            overlay_auto_hide: false,
            overlay_click_through: false,
            overlay_style: "capsule".to_string(),
            process_top_enabled: false,
            alert_sustain_secs: cfg.alert_sustain_secs,
            quiet_hours_enabled: cfg.quiet_hours_enabled,
            quiet_start_minute: cfg.quiet_start_minute,
            quiet_end_minute: cfg.quiet_end_minute,
            hotkey_panel: cfg.hotkey_panel,
            hotkey_overlay: cfg.hotkey_overlay,
            hotkey_click_through: cfg.hotkey_click_through,
            overlay_metrics: cfg.overlay_metrics,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsResponse {
    alert: AlertThresholds,
    notification_enabled: bool,
    history_range_minutes: u32,
    precise_temp_enabled: bool,
    lhm_base_url: String,
    overlay_enabled: bool,
    codex_enabled: bool,
    autostart_enabled: bool,
    overlay_auto_hide: bool,
    overlay_click_through: bool,
    overlay_style: String,
    process_top_enabled: bool,
    alert_sustain_secs: u64,
    quiet_hours_enabled: bool,
    quiet_start_minute: u16,
    quiet_end_minute: u16,
    hotkey_panel: String,
    hotkey_overlay: String,
    hotkey_click_through: String,
    overlay_metrics: Vec<String>,
}

/// 设置页聚合保存：全部校验与可失败操作通过后仅落盘一次，避免中途失败导致部分生效
fn apply_settings_impl(app: &AppHandle, input: SettingsInput) -> Result<SettingsResponse, String> {
    let alerts = app.state::<SharedAlerts>();
    let monitor = app.state::<SharedMonitor>();
    let config = app.state::<SharedConfig>();
    let lhm = app.state::<SharedLhmRuntime>();

    // 校验前置：最易失败的操作先做，任一失败即中止且无副作用
    let normalized_lhm = normalize_lhm_base_url(&input.lhm_base_url)?;
    input.thresholds.validate()?;
    let capacity = history_capacity_for_minutes(input.history_range_minutes)?;
    let parsed_style =
        OverlayStyle::parse(&input.overlay_style).ok_or_else(|| "无效的叠加层形态".to_string())?;
    validate_alert_sustain_secs(input.alert_sustain_secs)?;
    validate_quiet_minute(input.quiet_start_minute, "安静开始")?;
    validate_quiet_minute(input.quiet_end_minute, "安静结束")?;
    validate_hotkey(&input.hotkey_panel)?;
    validate_hotkey(&input.hotkey_overlay)?;
    validate_hotkey(&input.hotkey_click_through)?;
    let overlay_metrics = normalize_overlay_metrics(&input.overlay_metrics);

    // 外部可失败操作（通知权限、自启）在落盘前；落盘后的内存副作用失败只打日志
    if input.notification_enabled && !ensure_notification_permission(app) {
        return Err("系统通知权限未授予，请在 Windows 通知设置中允许本应用".to_string());
    }
    // 自启特例：先执行系统操作，成功后才随下方事务落盘；save 失败时系统已改而配置未变，
    // 由下次启动 sync_autostart_with_config 收敛（自愈），无需回滚系统状态
    apply_autostart_system(app, input.autostart_enabled)?;

    // 关闭叠加层时把最后位置并入本次唯一一次落盘，避免 hide 内部再写盘
    let overlay_pos = if !input.overlay_enabled {
        app.get_webview_window("overlay")
            .and_then(|window| window.outer_position().ok())
    } else {
        None
    };

    // 副本事务：唯一一次落盘，失败则内存与磁盘保持原状
    mutate_config(&config, |next| {
        next.alert = input.thresholds.clone();
        next.notification_enabled = input.notification_enabled;
        next.history_range_minutes = input.history_range_minutes;
        next.precise_temp_enabled = input.precise_temp_enabled;
        next.lhm_base_url = normalized_lhm.clone();
        next.overlay_enabled = input.overlay_enabled;
        next.codex_enabled = input.codex_enabled;
        next.autostart_enabled = input.autostart_enabled;
        next.overlay_auto_hide = input.overlay_auto_hide;
        next.overlay_click_through = input.overlay_click_through;
        next.overlay_style = parsed_style;
        next.process_top_enabled = input.process_top_enabled;
        next.alert_sustain_secs = input.alert_sustain_secs;
        next.quiet_hours_enabled = input.quiet_hours_enabled;
        next.quiet_start_minute = input.quiet_start_minute;
        next.quiet_end_minute = input.quiet_end_minute;
        next.hotkey_panel = input.hotkey_panel.trim().to_string();
        next.hotkey_overlay = input.hotkey_overlay.trim().to_string();
        next.hotkey_click_through = input.hotkey_click_through.trim().to_string();
        next.overlay_metrics = overlay_metrics.clone();
        if let Some(pos) = overlay_pos {
            next.overlay_x = Some(pos.x);
            next.overlay_y = Some(pos.y);
        }
        Ok(())
    })?;

    match alerts.lock() {
        Ok(mut engine) => engine.set_thresholds(input.thresholds.clone()),
        Err(err) => eprintln!("更新告警阈值失败: {err}"),
    }
    match monitor.lock() {
        Ok(mut state) => state.set_history_capacity(capacity),
        Err(err) => eprintln!("更新历史容量失败: {err}"),
    }

    lhm.set_enabled(input.precise_temp_enabled);
    lhm.set_base_url(normalized_lhm.clone());

    if let Ok(cfg) = config.lock() {
        sync_hotkeys(app, &cfg);
    }

    if input.overlay_enabled {
        show_overlay_window(app);
    } else {
        hide_overlay_window(app, false);
    }
    sync_overlay_menu_checked(app, input.overlay_enabled);
    let _ = app.emit("overlay-enabled-changed", input.overlay_enabled);
    let _ = app.emit("overlay-auto-hide-changed", input.overlay_auto_hide);
    let _ = app.emit("overlay-click-through-changed", input.overlay_click_through);
    let _ = app.emit("overlay-style-changed", parsed_style.as_str());
    let _ = app.emit("overlay-metrics-changed", overlay_metrics.clone());
    app.state::<SharedCodex>().settings_changed(input.codex_enabled);
    publish_codex_state(app, &app.state::<SharedCodex>());
    let _ = app.emit("codex-settings-changed", input.codex_enabled);
    if input.overlay_enabled {
        if let Err(err) = apply_overlay_layout(OverlayLayoutMode::Collapsed, app) {
            eprintln!("按指标重算叠加层尺寸失败: {err}");
        }
    }

    if input.notification_enabled {
        // 测试通知失败不影响保存结果
        if let Err(err) = app
            .notification()
            .builder()
            .title("系统监测")
            .body("已启用系统通知")
            .show()
        {
            eprintln!("发送测试通知失败: {err}");
        }
    }

    Ok(SettingsResponse {
        alert: input.thresholds,
        notification_enabled: input.notification_enabled,
        history_range_minutes: input.history_range_minutes,
        precise_temp_enabled: input.precise_temp_enabled,
        lhm_base_url: normalized_lhm,
        overlay_enabled: input.overlay_enabled,
        codex_enabled: input.codex_enabled,
        autostart_enabled: input.autostart_enabled,
        overlay_auto_hide: input.overlay_auto_hide,
        overlay_click_through: input.overlay_click_through,
        overlay_style: parsed_style.as_str().to_string(),
        process_top_enabled: input.process_top_enabled,
        alert_sustain_secs: input.alert_sustain_secs,
        quiet_hours_enabled: input.quiet_hours_enabled,
        quiet_start_minute: input.quiet_start_minute,
        quiet_end_minute: input.quiet_end_minute,
        hotkey_panel: input.hotkey_panel.trim().to_string(),
        hotkey_overlay: input.hotkey_overlay.trim().to_string(),
        hotkey_click_through: input.hotkey_click_through.trim().to_string(),
        overlay_metrics,
    })
}

#[tauri::command]
fn apply_settings(settings: SettingsInput, app: AppHandle) -> Result<SettingsResponse, String> {
    apply_settings_impl(&app, settings)
}

#[tauri::command]
fn apply_settings_reset(app: AppHandle) -> Result<SettingsResponse, String> {
    apply_settings_impl(&app, SettingsInput::defaults())
}

#[tauri::command]
fn get_temp_source_status(
    config: tauri::State<'_, SharedConfig>,
    tracker: tauri::State<'_, SharedTempTracker>,
) -> Result<TempSourceStatus, String> {
    let (enabled, url) = {
        let cfg = config.lock().map_err(|e| e.to_string())?;
        (cfg.precise_temp_enabled, cfg.lhm_base_url.clone())
    };
    let kind = tracker.get();
    // 开关关闭时，对外展示来源不强调 LHM（即使链路上曾命中过）
    let display_kind = if !enabled && kind == TempSourceKind::Lhm {
        TempSourceKind::Unavailable
    } else {
        kind
    };

    Ok(TempSourceStatus {
        source: display_kind.as_str().to_string(),
        message: temp_source_message(enabled, display_kind),
        precise_temp_enabled: enabled,
        lhm_base_url: url,
    })
}

#[tauri::command]
fn set_settings_open(
    open: bool,
    settings_open: tauri::State<'_, SharedSettingsOpen>,
) -> Result<(), String> {
    let mut flag = settings_open.lock().map_err(|e| e.to_string())?;
    *flag = open;
    Ok(())
}

#[tauri::command]
fn set_overlay_enabled(enabled: bool, app: AppHandle) -> Result<bool, String> {
    apply_overlay_enabled(&app, enabled)
}

#[tauri::command]
fn set_autostart_enabled(enabled: bool, app: AppHandle) -> Result<bool, String> {
    apply_autostart_enabled(&app, enabled)
}

fn apply_autostart_system(app: &AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;

    let autostart = app.autolaunch();
    if enabled {
        autostart
            .enable()
            .map_err(|e| format!("启用开机自启失败: {e}"))
    } else {
        autostart
            .disable()
            .map_err(|e| format!("关闭开机自启失败: {e}"))
    }
}

fn apply_autostart_enabled(app: &AppHandle, enabled: bool) -> Result<bool, String> {
    // 特例：先执行系统操作，成功后才落盘；save 失败时系统已改而配置未变，
    // 由下次启动 sync_autostart_with_config 收敛（自愈），无需回滚系统状态
    apply_autostart_system(app, enabled)?;

    mutate_config(&app.state::<SharedConfig>(), |next| {
        next.autostart_enabled = enabled;
        Ok(())
    })?;

    Ok(enabled)
}

fn sync_autostart_with_config(app: &AppHandle) {
    let enabled = app
        .state::<SharedConfig>()
        .lock()
        .map(|cfg| cfg.autostart_enabled)
        .unwrap_or(false);
    if let Err(err) = apply_autostart_enabled(app, enabled) {
        eprintln!("同步开机自启状态失败: {err}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayLayoutMode {
    Collapsed,
    Expanded,
    Peek,
}

fn parse_overlay_layout_mode(mode: &str) -> Result<OverlayLayoutMode, String> {
    match mode {
        "collapsed" => Ok(OverlayLayoutMode::Collapsed),
        "expanded" => Ok(OverlayLayoutMode::Expanded),
        "peek" => Ok(OverlayLayoutMode::Peek),
        _ => Err(format!("未知叠加层布局: {mode}")),
    }
}

fn overlay_metric_count(metrics: &[String]) -> u32 {
    let count = metrics
        .iter()
        .filter(|id| matches!(id.as_str(), "cpu" | "mem" | "temp" | "gpu" | "net" | "codex_primary" | "codex_secondary"))
        .count() as u32;
    count.clamp(1, 7)
}

fn collapsed_overlay_size(style: OverlayStyle, metrics: &[String]) -> (u32, u32) {
    let count = overlay_metric_count(metrics);
    let codex_count = metrics.iter().filter(|id| id.starts_with("codex_")).count().min(count as usize) as u32;
    match style {
        OverlayStyle::Capsule => (
            OVERLAY_COLLAPSED_WIDTH - 5 * 62 + (count - codex_count) * 62 + codex_count * 150,
            OVERLAY_COLLAPSED_HEIGHT,
        ),
        OverlayStyle::Vertical => (
            OVERLAY_VERTICAL_WIDTH,
            OVERLAY_VERTICAL_HEIGHT - 5 * 32 + count * 32 + codex_count * 16,
        ),
        OverlayStyle::Numeric => (OVERLAY_NUMERIC_WIDTH - 5 * 36 + (count - codex_count) * 36 + codex_count * 118, OVERLAY_NUMERIC_HEIGHT),
    }
}

fn overlay_layout_size(
    mode: OverlayLayoutMode,
    style: OverlayStyle,
    _edge_x: Option<OverlayEdgeX>,
    _edge_y: Option<OverlayEdgeY>,
    metrics: &[String],
) -> (u32, u32) {
    match mode {
        OverlayLayoutMode::Collapsed => collapsed_overlay_size(style, metrics),
        OverlayLayoutMode::Expanded => (OVERLAY_EXPANDED_WIDTH, OVERLAY_EXPANDED_HEIGHT),
        OverlayLayoutMode::Peek => {
            // 自动隐藏热区仅服务左右贴边，固定为竖细条
            (OVERLAY_PEEK_THICKNESS, OVERLAY_PEEK_LENGTH)
        }
    }
}

fn apply_overlay_layout(mode: OverlayLayoutMode, app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("overlay")
        .ok_or_else(|| "叠加层窗口不存在".to_string())?;

    let quiet_until = Instant::now() + Duration::from_millis(OVERLAY_LAYOUT_QUIET_MS);
    if let Ok(mut quiet) = app.state::<SharedOverlaySnapQuietUntil>().0.lock() {
        *quiet = Some(quiet_until);
    }

    let (edge_x, edge_y, style, metrics) = {
        let state = app.state::<SharedConfig>();
        let cfg = state.lock().map_err(|e| e.to_string())?;
        (
            cfg.overlay_edge_x,
            cfg.overlay_edge_y,
            cfg.overlay_style,
            cfg.overlay_metrics.clone(),
        )
    };

    let (width, height) = overlay_layout_size(mode, style, edge_x, edge_y, &metrics);

    let scale = window.scale_factor().unwrap_or(1.0);
    let prev_pos = window
        .outer_position()
        .map_err(|e| format!("读取叠加层位置失败: {e}"))?;
    let prev_size = window
        .outer_size()
        .map_err(|e| format!("读取叠加层尺寸失败: {e}"))?;

    let prev_x = prev_pos.x as f64 / scale;
    let prev_y = prev_pos.y as f64 / scale;
    let prev_w = prev_size.width as f64 / scale;
    let prev_h = prev_size.height as f64 / scale;
    let next_w = width as f64;
    let next_h = height as f64;
    let prev_right = prev_x + prev_w;
    let prev_bottom = prev_y + prev_h;

    let mut x = prev_x;
    let mut y = prev_y;
    let mut next_edge_x = edge_x;
    let mut next_edge_y = edge_y;

    if let Ok(Some(monitor)) = window.current_monitor() {
        let work = monitor.work_area();
        let wx = work.position.x as f64 / scale;
        let wy = work.position.y as f64 / scale;
        let ww = work.size.width as f64 / scale;
        let wh = work.size.height as f64 / scale;
        let max_x = wx + ww - next_w;
        let max_y = wy + wh - next_h;

        let dist_left = prev_x - wx;
        let dist_right = wx + ww - prev_right;
        let dist_top = prev_y - wy;
        let dist_bottom = wy + wh - prev_bottom;

        match edge_x {
            Some(OverlayEdgeX::Right) => {
                x = max_x;
                next_edge_x = Some(OverlayEdgeX::Right);
            }
            Some(OverlayEdgeX::Left) => {
                x = wx;
                next_edge_x = Some(OverlayEdgeX::Left);
            }
            None => {
                if dist_right <= dist_left {
                    x = prev_right - next_w;
                } else {
                    x = prev_x;
                }
                next_edge_x = None;
            }
        }

        match edge_y {
            Some(OverlayEdgeY::Bottom) => {
                y = max_y;
                next_edge_y = Some(OverlayEdgeY::Bottom);
            }
            Some(OverlayEdgeY::Top) => {
                y = wy;
                next_edge_y = Some(OverlayEdgeY::Top);
            }
            None => {
                if dist_bottom <= dist_top {
                    y = prev_bottom - next_h;
                } else {
                    y = prev_y;
                }
                next_edge_y = None;
            }
        }

        // 热区在角落时，沿贴边方向对齐到角点
        if mode == OverlayLayoutMode::Peek {
            if matches!(next_edge_x, Some(OverlayEdgeX::Right))
                && matches!(next_edge_y, Some(OverlayEdgeY::Bottom))
            {
                x = max_x;
                y = max_y;
            } else if matches!(next_edge_x, Some(OverlayEdgeX::Right))
                && matches!(next_edge_y, Some(OverlayEdgeY::Top))
            {
                x = max_x;
                y = wy;
            } else if matches!(next_edge_x, Some(OverlayEdgeX::Left))
                && matches!(next_edge_y, Some(OverlayEdgeY::Bottom))
            {
                x = wx;
                y = max_y;
            } else if matches!(next_edge_x, Some(OverlayEdgeX::Left))
                && matches!(next_edge_y, Some(OverlayEdgeY::Top))
            {
                x = wx;
                y = wy;
            } else if matches!(next_edge_x, Some(OverlayEdgeX::Right | OverlayEdgeX::Left)) {
                // 仅左右贴边：热区垂直居中于原窗口
                let center_y = prev_y + prev_h * 0.5;
                y = (center_y - next_h * 0.5).clamp(wy, max_y);
            } else if matches!(next_edge_y, Some(OverlayEdgeY::Top | OverlayEdgeY::Bottom)) {
                let center_x = prev_x + prev_w * 0.5;
                x = (center_x - next_w * 0.5).clamp(wx, max_x);
            }
        }

        x = x.clamp(wx, max_x);
        y = y.clamp(wy, max_y);
    }

    let physical_w = (next_w * scale).round().max(1.0) as u32;
    let physical_h = (next_h * scale).round().max(1.0) as u32;
    let physical_x = (x * scale).round() as i32;
    let physical_y = (y * scale).round() as i32;

    window
        .set_size(PhysicalSize::new(physical_w, physical_h))
        .map_err(|e| format!("设置叠加层尺寸失败: {e}"))?;
    window
        .set_position(PhysicalPosition::new(physical_x, physical_y))
        .map_err(|e| format!("设置叠加层位置失败: {e}"))?;

    persist_overlay_position(app, physical_x, physical_y, next_edge_x, next_edge_y);
    Ok(())
}

#[tauri::command]
fn set_overlay_collapsed(collapsed: bool, app: AppHandle) -> Result<(), String> {
    let mode = if collapsed {
        OverlayLayoutMode::Collapsed
    } else {
        OverlayLayoutMode::Expanded
    };
    apply_overlay_layout(mode, &app)
}

#[tauri::command]
fn set_overlay_layout(mode: String, app: AppHandle) -> Result<(), String> {
    let parsed = parse_overlay_layout_mode(mode.trim())?;
    apply_overlay_layout(parsed, &app)
}

#[tauri::command]
fn set_overlay_auto_hide(enabled: bool, app: AppHandle) -> Result<bool, String> {
    mutate_config(&app.state::<SharedConfig>(), |next| {
        next.overlay_auto_hide = enabled;
        Ok(())
    })?;
    let _ = app.emit("overlay-auto-hide-changed", enabled);
    Ok(enabled)
}

#[tauri::command]
fn set_overlay_click_through(enabled: bool, app: AppHandle) -> Result<bool, String> {
    mutate_config(&app.state::<SharedConfig>(), |next| {
        next.overlay_click_through = enabled;
        Ok(())
    })?;
    let _ = app.emit("overlay-click-through-changed", enabled);
    Ok(enabled)
}

#[tauri::command]
fn set_overlay_style(style: String, app: AppHandle) -> Result<String, String> {
    let parsed = OverlayStyle::parse(&style).ok_or_else(|| "无效的叠加层形态".to_string())?;
    mutate_config(&app.state::<SharedConfig>(), |next| {
        next.overlay_style = parsed;
        Ok(())
    })?;
    let _ = app.emit("overlay-style-changed", parsed.as_str());
    Ok(parsed.as_str().to_string())
}

/// 主面板：透明窗 hide/show 后毛玻璃常失效，显示时重新施加。
fn apply_panel_effects(window: &tauri::WebviewWindow) {
    let effects = EffectsBuilder::new()
        .effects([Effect::Acrylic, Effect::Mica, Effect::Blur])
        .state(EffectState::Active)
        .radius(16.0)
        .build();
    if let Err(err) = window.set_effects(effects) {
        eprintln!("施加窗口毛玻璃效果失败: {err}");
    }
}

/// 叠加层：不使用 Acrylic/Mica（易呈不透明色块），仅靠 CSS 半透明 + 窗口透明透出桌面。
fn clear_overlay_effects(window: &tauri::WebviewWindow) {
    if let Err(err) = window.set_effects(None) {
        eprintln!("清除叠加层窗口效果失败: {err}");
    }
}

fn sync_overlay_menu_checked(app: &AppHandle, enabled: bool) {
    if let Ok(guard) = app.state::<SharedOverlayMenu>().lock() {
        if let Some(item) = guard.as_ref() {
            let _ = item.set_checked(enabled);
        }
    }
}

fn restore_overlay_position(window: &tauri::WebviewWindow, cfg: &AppConfig) {
    if let (Some(x), Some(y)) = (cfg.overlay_x, cfg.overlay_y) {
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
}

fn persist_overlay_xy_keeping_edge(app: &AppHandle) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    let Ok(pos) = window.outer_position() else {
        return;
    };
    let (edge_x, edge_y) = app
        .state::<SharedConfig>()
        .lock()
        .map(|cfg| (cfg.overlay_edge_x, cfg.overlay_edge_y))
        .unwrap_or((None, None));
    persist_overlay_position(app, pos.x, pos.y, edge_x, edge_y);
}

fn persist_overlay_position(
    app: &AppHandle,
    x: i32,
    y: i32,
    edge_x: Option<OverlayEdgeX>,
    edge_y: Option<OverlayEdgeY>,
) {
    // 位置落盘失败仅保留旧值，下次移动再写
    if let Err(err) = mutate_config(&app.state::<SharedConfig>(), |next| {
        next.overlay_x = Some(x);
        next.overlay_y = Some(y);
        next.overlay_edge_x = edge_x;
        next.overlay_edge_y = edge_y;
        Ok(())
    }) {
        eprintln!("保存叠加层位置失败: {err}");
    }
}

#[derive(Debug, Clone, Copy)]
struct OverlaySnapResult {
    x: i32,
    y: i32,
    edge_x: Option<OverlayEdgeX>,
    edge_y: Option<OverlayEdgeY>,
}

fn snap_overlay_to_edges(window: &tauri::WebviewWindow) -> Option<OverlaySnapResult> {
    let pos = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    let monitor = window.current_monitor().ok()??;
    // 使用工作区（排除任务栏/Dock），避免吸附到任务栏下方或被遮挡
    let work = monitor.work_area();
    let left = work.position.x;
    let top = work.position.y;
    let right = work.position.x.saturating_add(work.size.width as i32);
    let bottom = work.position.y.saturating_add(work.size.height as i32);
    let width = size.width as i32;
    let height = size.height as i32;
    if width <= 0 || height <= 0 {
        return Some(OverlaySnapResult {
            x: pos.x,
            y: pos.y,
            edge_x: None,
            edge_y: None,
        });
    }

    let max_x = (right - width).max(left);
    let max_y = (bottom - height).max(top);
    let mut x = pos.x.clamp(left, max_x);
    let mut y = pos.y.clamp(top, max_y);

    let dist_left = x - left;
    let dist_right = right - (x + width);
    let dist_top = y - top;
    let dist_bottom = bottom - (y + height);

    // 角落优先：同时接近两边时，两边都吸附
    let near_left = dist_left <= OVERLAY_SNAP_THRESHOLD_PX;
    let near_right = dist_right <= OVERLAY_SNAP_THRESHOLD_PX;
    let near_top = dist_top <= OVERLAY_SNAP_THRESHOLD_PX;
    let near_bottom = dist_bottom <= OVERLAY_SNAP_THRESHOLD_PX;
    let corner_boost = if (near_left || near_right) && (near_top || near_bottom) {
        OVERLAY_SNAP_CORNER_BONUS_PX
    } else {
        0
    };

    let mut edge_x = None;
    let mut edge_y = None;

    if near_left || near_right {
        if dist_left - corner_boost <= dist_right {
            x = left;
            edge_x = Some(OverlayEdgeX::Left);
        } else {
            x = max_x;
            edge_x = Some(OverlayEdgeX::Right);
        }
    }

    if near_top || near_bottom {
        if dist_top - corner_boost <= dist_bottom {
            y = top;
            edge_y = Some(OverlayEdgeY::Top);
        } else {
            y = max_y;
            edge_y = Some(OverlayEdgeY::Bottom);
        }
    }

    Some(OverlaySnapResult {
        x,
        y,
        edge_x,
        edge_y,
    })
}

fn apply_overlay_snap_and_persist(app: &AppHandle) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    let Some(snap) = snap_overlay_to_edges(&window) else {
        return;
    };

    if let Ok(pos) = window.outer_position() {
        if pos.x != snap.x || pos.y != snap.y {
            let _ = window.set_position(PhysicalPosition::new(snap.x, snap.y));
        }
    } else {
        let _ = window.set_position(PhysicalPosition::new(snap.x, snap.y));
    }

    persist_overlay_position(app, snap.x, snap.y, snap.edge_x, snap.edge_y);

    let auto_hide = app
        .state::<SharedConfig>()
        .lock()
        .map(|cfg| cfg.overlay_auto_hide)
        .unwrap_or(false);
    // 贴边自动隐藏仅以左右边为准
    if auto_hide {
        if let Some(edge) = snap.edge_x {
            let _ = app.emit("overlay-snap-edge", edge.as_str());
        }
    }
}

fn schedule_overlay_position_save(app: &AppHandle) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    let Ok(pos) = window.outer_position() else {
        return;
    };

    // 仅在 quiet 锁内读取判断结果，cfg 锁移到临界区外，避免嵌套持锁
    let in_quiet = app
        .state::<SharedOverlaySnapQuietUntil>()
        .0
        .lock()
        .ok()
        .and_then(|quiet| *quiet)
        .map(|t| Instant::now() < t)
        .unwrap_or(false);
    if in_quiet {
        if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
            cfg.overlay_x = Some(pos.x);
            cfg.overlay_y = Some(pos.y);
        }
        let now = Instant::now();
        if let Ok(mut last) = app.state::<SharedOverlayLastMove>().0.lock() {
            *last = Some(now);
        }
        let debounce = app.state::<SharedOverlayPosDebounce>();
        if debounce
            .0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            spawn_overlay_position_worker(app.clone());
        }
        return;
    }

    if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
        cfg.overlay_x = Some(pos.x);
        cfg.overlay_y = Some(pos.y);
    }

    let now = Instant::now();
    if let Ok(mut last) = app.state::<SharedOverlayLastMove>().0.lock() {
        *last = Some(now);
    }

    let debounce = app.state::<SharedOverlayPosDebounce>();
    if debounce
        .0
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    spawn_overlay_position_worker(app.clone());
}

fn spawn_overlay_position_worker(app_handle: AppHandle) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(OVERLAY_SNAP_DEBOUNCE_MS));
            let observed = app_handle
                .state::<SharedOverlayLastMove>()
                .0
                .lock()
                .ok()
                .and_then(|guard| *guard);
            let settled = observed
                .map(|t| {
                    Instant::now().duration_since(t)
                        >= Duration::from_millis(OVERLAY_SNAP_DEBOUNCE_MS - 20)
                })
                .unwrap_or(false);
            if !settled {
                continue;
            }

            let now = Instant::now();
            let should_write = {
                let state = app_handle.state::<SharedOverlayPosSave>();
                let Ok(mut last) = state.0.lock() else {
                    break;
                };
                let due = last
                    .map(|t| now.duration_since(t) >= Duration::from_millis(200))
                    .unwrap_or(true);
                if due {
                    *last = Some(now);
                }
                due
            };
            let still_quiet = app_handle
                .state::<SharedOverlaySnapQuietUntil>()
                .0
                .lock()
                .ok()
                .and_then(|quiet| *quiet)
                .map(|t| Instant::now() < t)
                .unwrap_or(false);
            if should_write {
                if still_quiet {
                    persist_overlay_xy_keeping_edge(&app_handle);
                } else {
                    apply_overlay_snap_and_persist(&app_handle);
                }
            }

            let moved_again = app_handle
                .state::<SharedOverlayLastMove>()
                .0
                .lock()
                .ok()
                .and_then(|guard| *guard)
                != observed;
            // quiet 未结束前不退出：否则只落盘一次后 worker 放锁，期间 hide 会丢掉后续坐标
            if moved_again || still_quiet {
                continue;
            }
            break;
        }
        app_handle
            .state::<SharedOverlayPosDebounce>()
            .0
            .store(false, Ordering::Release);
        // 释放期间若又有 Moved，仅更新了时间戳而未拉起 worker，补一次调度
        let needs_rearm = app_handle
            .state::<SharedOverlayLastMove>()
            .0
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|t| Instant::now().duration_since(t) < Duration::from_millis(OVERLAY_SNAP_DEBOUNCE_MS))
            .unwrap_or(false);
        if needs_rearm {
            schedule_overlay_position_save(&app_handle);
        }
    });
}

fn show_overlay_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("overlay") {
        if let Ok(cfg) = app.state::<SharedConfig>().lock() {
            restore_overlay_position(&window, &cfg);
        }
        let _ = window.show();
        clear_overlay_effects(&window);
    }
}

fn hide_overlay_window(app: &AppHandle, persist_position: bool) {
    if let Some(window) = app.get_webview_window("overlay") {
        // 隐藏前落盘当前位置（保留已记录的贴边方向）；聚合保存路径已并入唯一一次落盘，跳过
        if persist_position {
            if let Ok(pos) = window.outer_position() {
                let (edge_x, edge_y) = app
                    .state::<SharedConfig>()
                    .lock()
                    .map(|cfg| (cfg.overlay_edge_x, cfg.overlay_edge_y))
                    .unwrap_or((None, None));
                persist_overlay_position(app, pos.x, pos.y, edge_x, edge_y);
            }
        }
        let _ = window.hide();
    }
}

fn apply_overlay_enabled(app: &AppHandle, enabled: bool) -> Result<bool, String> {
    mutate_config(&app.state::<SharedConfig>(), |next| {
        next.overlay_enabled = enabled;
        Ok(())
    })?;

    if enabled {
        show_overlay_window(app);
    } else {
        hide_overlay_window(app, true);
    }

    sync_overlay_menu_checked(app, enabled);
    let _ = app.emit("overlay-enabled-changed", enabled);
    Ok(enabled)
}

fn persist_main_position(app: &AppHandle, x: i32, y: i32) {
    // 位置落盘失败仅保留旧值，下次移动再写
    let _ = mutate_config(&app.state::<SharedConfig>(), |next| {
        next.main_x = Some(x);
        next.main_y = Some(y);
        Ok(())
    });
}

fn clamp_to_work_area(x: i32, y: i32, win_w: i32, win_h: i32, monitor: &Monitor) -> (i32, i32) {
    let work = monitor.work_area();
    let left = work.position.x;
    let top = work.position.y;
    let right = work.position.x.saturating_add(work.size.width as i32);
    let bottom = work.position.y.saturating_add(work.size.height as i32);
    let max_x = (right - win_w).max(left);
    let max_y = (bottom - win_h).max(top);
    (x.clamp(left, max_x), y.clamp(top, max_y))
}

fn restore_main_position(
    app: &AppHandle,
    window: &tauri::WebviewWindow,
    cfg: &AppConfig,
    tray_rect: Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>,
) -> bool {
    let (Some(x), Some(y)) = (cfg.main_x, cfg.main_y) else {
        return false;
    };
    let win_size = window.outer_size().unwrap_or(PhysicalSize::new(380, 690));
    let win_w = win_size.width as i32;
    let win_h = win_size.height as i32;
    let monitor = monitor_at_physical_point(window, x, y).or_else(|| {
        tray_rect.and_then(|(pos, _)| monitor_at_physical_point(window, pos.x, pos.y))
    });
    let (cx, cy) = if let Some(monitor) = monitor {
        clamp_to_work_area(x, y, win_w, win_h, &monitor)
    } else {
        (x.max(0), y.max(0))
    };
    let _ = window.set_position(PhysicalPosition::new(cx, cy));
    if cx != x || cy != y {
        persist_main_position(app, cx, cy);
    }
    true
}

fn schedule_main_position_save(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let Ok(pos) = window.outer_position() else {
        return;
    };

    if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
        cfg.main_x = Some(pos.x);
        cfg.main_y = Some(pos.y);
    }

    // 双重节流：单 worker 去抖（Moved 风暴时不重复 spawn）+ PosSave 限制最小写盘间隔
    let now = Instant::now();
    if let Ok(mut last) = app.state::<SharedMainLastMove>().0.lock() {
        *last = Some(now);
    }

    let debounce = app.state::<SharedMainPosDebounce>();
    if debounce
        .0
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let app_handle = app.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(MAIN_POS_SAVE_DEBOUNCE_MS));
            let observed = app_handle
                .state::<SharedMainLastMove>()
                .0
                .lock()
                .ok()
                .and_then(|guard| *guard);
            let settled = observed
                .map(|t| {
                    Instant::now().duration_since(t)
                        >= Duration::from_millis(MAIN_POS_SAVE_DEBOUNCE_MS - 20)
                })
                .unwrap_or(false);
            if !settled {
                continue;
            }

            let now = Instant::now();
            let should_write = {
                let state = app_handle.state::<SharedMainPosSave>();
                let Ok(mut last) = state.0.lock() else {
                    break;
                };
                let due = last
                    .map(|t| now.duration_since(t) >= Duration::from_millis(200))
                    .unwrap_or(true);
                if due {
                    *last = Some(now);
                }
                due
            };
            if should_write {
                if let Some(window) = app_handle.get_webview_window("main") {
                    if let Ok(pos) = window.outer_position() {
                        persist_main_position(&app_handle, pos.x, pos.y);
                    }
                }
            }

            let moved_again = app_handle
                .state::<SharedMainLastMove>()
                .0
                .lock()
                .ok()
                .and_then(|guard| *guard)
                != observed;
            if moved_again {
                continue;
            }
            break;
        }
        app_handle
            .state::<SharedMainPosDebounce>()
            .0
            .store(false, Ordering::Release);
        let needs_rearm = app_handle
            .state::<SharedMainLastMove>()
            .0
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|t| Instant::now().duration_since(t) < Duration::from_millis(MAIN_POS_SAVE_DEBOUNCE_MS))
            .unwrap_or(false);
        if needs_rearm {
            schedule_main_position_save(&app_handle);
        }
    });
}

fn monitor_at_physical_point(
    window: &tauri::WebviewWindow,
    x: i32,
    y: i32,
) -> Option<Monitor> {
    if let Ok(monitors) = window.available_monitors() {
        for monitor in monitors {
            let origin = monitor.position();
            let size = monitor.size();
            let right = origin.x.saturating_add(size.width as i32);
            let bottom = origin.y.saturating_add(size.height as i32);
            if x >= origin.x && x < right && y >= origin.y && y < bottom {
                return Some(monitor);
            }
        }
    }
    window.primary_monitor().ok().flatten()
}

fn monitor_at_logical_point(
    window: &tauri::WebviewWindow,
    pos: LogicalPosition<f64>,
) -> Option<Monitor> {
    if let Ok(monitors) = window.available_monitors() {
        for monitor in monitors {
            // 各显示器按自身 scale 折算为逻辑范围后匹配
            let scale = monitor.scale_factor();
            let origin = monitor.position();
            let size = monitor.size();
            let lx = origin.x as f64 / scale;
            let ly = origin.y as f64 / scale;
            let lw = size.width as f64 / scale;
            let lh = size.height as f64 / scale;
            if pos.x >= lx && pos.x < lx + lw && pos.y >= ly && pos.y < ly + lh {
                return Some(monitor);
            }
        }
    }
    window.primary_monitor().ok().flatten()
}

fn tray_event_scale(window: Option<tauri::WebviewWindow>, position: tauri::Position) -> f64 {
    let Some(window) = window else {
        return 1.0;
    };
    match position {
        tauri::Position::Physical(p) => monitor_at_physical_point(&window, p.x, p.y),
        tauri::Position::Logical(p) => monitor_at_logical_point(&window, p),
    }
    .map(|m| m.scale_factor())
    .unwrap_or(1.0)
}

fn show_panel(app: &tauri::AppHandle, tray_rect: Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>) {
    if let Some(window) = app.get_webview_window("main") {
        let restored = app
            .state::<SharedConfig>()
            .lock()
            .map(|cfg| restore_main_position(app, &window, &cfg, tray_rect))
            .unwrap_or(false);

        if !restored {
            if let Some((pos, size)) = tray_rect {
                let win_size = window.outer_size().unwrap_or(PhysicalSize::new(380, 690));
                let win_w = win_size.width as i32;
                let win_h = win_size.height as i32;
                let mut x = pos.x + (size.width as i32 / 2) - (win_w / 2);
                let mut y = pos.y.saturating_sub(win_h + 8);
                // 以托盘所在显示器工作区收拢，避免顶部任务栏遮挡或负坐标屏外落位
                if let Some(monitor) = monitor_at_physical_point(&window, pos.x, pos.y) {
                    (x, y) = clamp_to_work_area(x, y, win_w, win_h, &monitor);
                } else {
                    x = x.max(0);
                    y = y.max(0);
                }
                let physical = PhysicalPosition::new(x, y);
                let _ = window.set_position(physical);
                persist_main_position(app, physical.x, physical.y);
            }
        }

        let _ = window.show();
        apply_panel_effects(&window);
        let _ = window.set_focus();
    }
}

fn hide_panel(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Ok(pos) = window.outer_position() {
            persist_main_position(app, pos.x, pos.y);
        }
        let _ = window.hide();
    }
}

fn register_hotkey(app: &AppHandle, accelerator: &str, id: &'static str) {
    let trimmed = accelerator.trim();
    if trimmed.is_empty() {
        return;
    }
    let shortcut = match trimmed.parse::<tauri_plugin_global_shortcut::Shortcut>() {
        Ok(shortcut) => shortcut,
        Err(err) => {
            eprintln!("快捷键 {id} 无法解析（{trimmed}）: {err}");
            return;
        }
    };
    if let Err(err) = app.global_shortcut().on_shortcut(shortcut, move |app, _shortcut, event| {
        if event.state != ShortcutState::Pressed {
            return;
        }
        match id {
            "panel" => toggle_panel(app, None),
            "overlay" => {
                let currently = app
                    .state::<SharedConfig>()
                    .lock()
                    .map(|cfg| cfg.overlay_enabled)
                    .unwrap_or(false);
                if let Err(err) = apply_overlay_enabled(app, !currently) {
                    eprintln!("快捷键切换叠加层失败: {err}");
                }
            }
            "click_through" => {
                let next = app
                    .state::<SharedConfig>()
                    .lock()
                    .map(|cfg| !cfg.overlay_click_through)
                    .unwrap_or(false);
                if let Err(err) = mutate_config(&app.state::<SharedConfig>(), |cfg| {
                    cfg.overlay_click_through = next;
                    Ok(())
                }) {
                    eprintln!("快捷键切换穿透失败: {err}");
                    return;
                }
                let _ = app.emit("overlay-click-through-changed", next);
            }
            _ => {}
        }
    }) {
        eprintln!("注册快捷键 {id}（{trimmed}）失败: {err}");
    }
}

fn sync_hotkeys(app: &AppHandle, cfg: &AppConfig) {
    if let Err(err) = app.global_shortcut().unregister_all() {
        eprintln!("清除快捷键失败: {err}");
    }
    register_hotkey(app, &cfg.hotkey_panel, "panel");
    register_hotkey(app, &cfg.hotkey_overlay, "overlay");
    register_hotkey(app, &cfg.hotkey_click_through, "click_through");
}

fn toggle_panel(app: &tauri::AppHandle, tray_rect: Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            hide_panel(app);
        } else {
            show_panel(app, tray_rect);
        }
    }
}

fn open_alert_settings(app: &tauri::AppHandle) {
    if let Ok(mut flag) = app.state::<SharedSettingsOpen>().lock() {
        *flag = true;
    }
    show_panel(app, None);
    let _ = app.emit("open-settings", ());
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let initial_config = AppConfig::load_or_default();
    let initial_alerts = AlertEngine::with_thresholds(initial_config.alert.clone());
    let (codex_monitor, codex_receiver) = codex::CodexMonitor::new();
    let history_capacity = history_capacity_for_minutes(initial_config.history_range_minutes)
        .unwrap_or(history::DEFAULT_HISTORY_CAPACITY);

    let lhm_runtime = LhmRuntimeConfig::new(
        initial_config.precise_temp_enabled,
        initial_config.lhm_base_url.clone(),
    );
    let temp_tracker = TempSourceTracker::new();
    let lhm_extras: SharedLhmExtras = Arc::new(Mutex::new(LhmExtras::default()));
    let temperature_provider = ChainedTemperatureProvider::platform_default(
        Arc::clone(&lhm_runtime),
        Arc::clone(&temp_tracker),
        Arc::clone(&lhm_extras),
    );

    tauri::Builder::default()
        // 须最先注册：二次启动时唤醒已有实例并显示主面板
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_panel(app, None);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(
            Mutex::new(MonitorState::with_history_capacity(history_capacity)) as SharedMonitor,
        )
        .manage(Mutex::new(temperature_provider) as SharedTemperature)
        .manage(Mutex::new(initial_alerts) as SharedAlerts)
        .manage(Mutex::new(initial_config) as SharedConfig)
        .manage(Mutex::new(false) as SharedSettingsOpen)
        .manage(Mutex::new(None) as SharedOverlayMenu)
        .manage(Mutex::new(None) as SharedCodexSyncMenu)
        .manage(SharedOverlayPosSave(Mutex::new(None)))
        .manage(SharedOverlayLastMove(Mutex::new(None)))
        .manage(SharedOverlaySnapQuietUntil(Mutex::new(None)))
        .manage(SharedOverlayPosDebounce(AtomicBool::new(false)))
        .manage(SharedMainPosSave(Mutex::new(None)))
        .manage(SharedMainLastMove(Mutex::new(None)))
        .manage(SharedMainPosDebounce(AtomicBool::new(false)))
        .manage(SharedTrayAlert(Mutex::new(false)))
        .manage(lhm_runtime as SharedLhmRuntime)
        .manage(temp_tracker as SharedTempTracker)
        .manage(lhm_extras)
        .manage(codex_monitor)
        .invoke_handler(tauri::generate_handler![
            get_metrics,
            get_codex_quota,
            refresh_codex_quota,
            get_metrics_history,
            export_metrics_csv,
            get_process_top,
            set_process_top_enabled,
            get_alert_thresholds,
            set_alert_thresholds,
            reset_alert_thresholds,
            get_app_config,
            set_notification_enabled,
            set_history_range_minutes,
            set_precise_temp_enabled,
            set_lhm_base_url,
            get_temp_source_status,
            set_settings_open,
            set_overlay_enabled,
            set_overlay_collapsed,
            set_overlay_layout,
            set_overlay_auto_hide,
            set_overlay_click_through,
            set_overlay_style,
            set_autostart_enabled,
            apply_settings,
            apply_settings_reset
        ])
        .setup(|app| {
            let codex_monitor = Arc::clone(&app.state::<SharedCodex>());
            codex::start_worker(app.handle().clone(), codex_monitor, codex_receiver);
            sync_autostart_with_config(app.handle());
            if let Ok(cfg) = app.state::<SharedConfig>().lock() {
                sync_hotkeys(app.handle(), &cfg);
            }

            let overlay_enabled = app
                .state::<SharedConfig>()
                .lock()
                .map(|cfg| cfg.overlay_enabled)
                .unwrap_or(false);

            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let alert_settings =
                MenuItem::with_id(app, "alert_settings", "设置", true, None::<&str>)?;
            let (sync_title, sync_enabled) = app.state::<SharedCodex>().snapshot().sync_menu_state();
            let codex_sync = MenuItem::with_id(app, "codex_sync", sync_title, sync_enabled, None::<&str>)?;
            {
                let state = app.state::<SharedCodexSyncMenu>();
                let mut slot = state.lock().map_err(|e| e.to_string())?;
                *slot = Some(codex_sync.clone());
            }
            let overlay_item = CheckMenuItem::with_id(
                app,
                "overlay",
                "叠加层",
                true,
                overlay_enabled,
                None::<&str>,
            )?;
            {
                let state = app.state::<SharedOverlayMenu>();
                let mut slot = state.lock().map_err(|e| e.to_string())?;
                *slot = Some(overlay_item.clone());
            }
            let menu = Menu::with_items(app, &[&show, &overlay_item, &codex_sync, &alert_settings, &quit])?;
            publish_codex_state(app.handle(), &app.state::<SharedCodex>());

            let tray_icon = app
                .default_window_icon()
                .cloned()
                .unwrap_or_else(|| tauri::include_image!("icons/32x32.png"));
            let _tray = TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .tooltip("系统监测")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => {
                        app.exit(0);
                    }
                    "show" => {
                        show_panel(app, None);
                    }
                    "codex_sync" => {
                        let monitor = app.state::<SharedCodex>();
                        monitor.refresh();
                        publish_codex_state(app, &monitor);
                    }
                    "alert_settings" => {
                        open_alert_settings(app);
                    }
                    "overlay" => {
                        let currently = app
                            .state::<SharedConfig>()
                            .lock()
                            .map(|cfg| cfg.overlay_enabled)
                            .unwrap_or(false);
                        if let Err(err) = apply_overlay_enabled(app, !currently) {
                            eprintln!("切换叠加层失败: {err}");
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        // 托盘 rect 按点击点所在显示器 scale 换算，取不到时回退 1.0
                        let scale = tray_event_scale(app.get_webview_window("main"), rect.position);
                        let position = rect.position.to_physical(scale);
                        let size = rect.size.to_physical(scale);
                        toggle_panel(app, Some((position, size)));
                    }
                })
                .build(app)?;

            if let Some(window) = app.get_webview_window("main") {
                apply_panel_effects(&window);
                let panel = window.clone();
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| match event {
                    WindowEvent::Focused(false) => {
                        let should_hide = app_handle
                            .state::<SharedSettingsOpen>()
                            .lock()
                            .map(|flag| !*flag)
                            .unwrap_or(true);
                        if should_hide {
                            if let Ok(pos) = panel.outer_position() {
                                persist_main_position(&app_handle, pos.x, pos.y);
                            }
                            let _ = panel.hide();
                        }
                    }
                    WindowEvent::Moved(_) => {
                        schedule_main_position_save(&app_handle);
                    }
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        if let Ok(pos) = panel.outer_position() {
                            persist_main_position(&app_handle, pos.x, pos.y);
                        }
                        let _ = panel.hide();
                    }
                    _ => {}
                });
            }

            if let Some(window) = app.get_webview_window("overlay") {
                let overlay = window.clone();
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| match event {
                    // 关闭叠加层窗口 = 关闭功能，不退出应用
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        if let Err(err) = apply_overlay_enabled(&app_handle, false) {
                            eprintln!("关闭叠加层失败: {err}");
                            let _ = overlay.hide();
                        }
                    }
                    // 失焦不隐藏（与主面板不同）
                    WindowEvent::Moved(_) => {
                        schedule_overlay_position_save(&app_handle);
                    }
                    _ => {}
                });

                if overlay_enabled {
                    show_overlay_window(app.handle());
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { api, code, .. } = event {
                // 仅在用户主动退出时结束进程；关闭窗口只隐藏
                if code.is_none() {
                    api.prevent_exit();
                } else {
                    app_handle.state::<SharedCodex>().shutdown();
                    if let Some(window) = app_handle.get_webview_window("main") {
                        if let Ok(pos) = window.outer_position() {
                            persist_main_position(app_handle, pos.x, pos.y);
                        }
                    }
                    if let Some(window) = app_handle.get_webview_window("overlay") {
                        // 退出前尽量落盘叠加层位置（保留已记录的贴边方向）
                        if let Ok(pos) = window.outer_position() {
                            let (edge_x, edge_y) = app_handle
                                .state::<SharedConfig>()
                                .lock()
                                .map(|cfg| (cfg.overlay_edge_x, cfg.overlay_edge_y))
                                .unwrap_or((None, None));
                            persist_overlay_position(app_handle, pos.x, pos.y, edge_x, edge_y);
                        }
                    }
                }
            }
        });
}

#[cfg(test)]
mod overlay_layout_tests {
    use super::*;

    #[test]
    fn codex_only_layout_fits_both_quota_items() {
        let metrics = vec!["codex_primary".to_string(), "codex_secondary".to_string()];
        assert_eq!(collapsed_overlay_size(OverlayStyle::Capsule, &metrics), (368, 38));
        assert_eq!(collapsed_overlay_size(OverlayStyle::Numeric, &metrics), (302, 34));
        assert_eq!(collapsed_overlay_size(OverlayStyle::Vertical, &metrics), (76, 136));
    }

    #[test]
    fn hardware_only_keeps_existing_dimensions_and_mixed_layout_grows() {
        let mut metrics = vec!["cpu", "mem", "temp", "gpu", "net"].into_iter().map(String::from).collect::<Vec<_>>();
        assert_eq!(collapsed_overlay_size(OverlayStyle::Capsule, &metrics), (378, 38));
        metrics.extend(["codex_primary".to_string(), "codex_secondary".to_string()]);
        assert_eq!(collapsed_overlay_size(OverlayStyle::Capsule, &metrics), (678, 38));
        assert_eq!(collapsed_overlay_size(OverlayStyle::Vertical, &metrics), (76, 296));
    }
}
