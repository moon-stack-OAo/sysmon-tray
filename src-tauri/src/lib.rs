mod alert;
mod config;
mod history;
mod monitor;
mod temperature;
mod temperature_lhm;

use alert::{AlertEngine, AlertStatus, AlertThresholds, SharedAlerts};
use config::{
    normalize_lhm_base_url, AppConfig, OverlayEdgeX, OverlayEdgeY, OverlayStyle, SharedConfig,
};
use history::{history_capacity_for_minutes, HistoryPoint};
use monitor::{Metrics, MonitorState, SharedMonitor, METRICS_CACHE_MIN_INTERVAL};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    window::{Effect, EffectState, EffectsBuilder},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, RunEvent, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;
use temperature_lhm::{LhmRuntimeConfig, TempSourceKind, TempSourceTracker};

pub type SharedSettingsOpen = Mutex<bool>;
pub type SharedLhmRuntime = Arc<LhmRuntimeConfig>;
pub type SharedTempTracker = Arc<TempSourceTracker>;
pub type SharedOverlayMenu = Mutex<Option<CheckMenuItem<tauri::Wry>>>;
pub struct SharedOverlayPosSave(pub Mutex<Option<Instant>>);
pub struct SharedOverlayLastMove(pub Mutex<Option<Instant>>);
pub struct SharedOverlaySnapQuietUntil(pub Mutex<Option<Instant>>);

const OVERLAY_SNAP_THRESHOLD_PX: i32 = 36;
const OVERLAY_SNAP_CORNER_BONUS_PX: i32 = 12;
const OVERLAY_SNAP_DEBOUNCE_MS: u64 = 220;
const OVERLAY_LAYOUT_QUIET_MS: u64 = 500;
const OVERLAY_COLLAPSED_WIDTH: u32 = 310;
const OVERLAY_COLLAPSED_HEIGHT: u32 = 38;
const OVERLAY_VERTICAL_WIDTH: u32 = 72;
const OVERLAY_VERTICAL_HEIGHT: u32 = 168;
const OVERLAY_NUMERIC_WIDTH: u32 = 258;
const OVERLAY_NUMERIC_HEIGHT: u32 = 34;
const OVERLAY_EXPANDED_WIDTH: u32 = 310;
const OVERLAY_EXPANDED_HEIGHT: u32 = 176;
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

fn send_alert_notification(app: &tauri::AppHandle, newly_fired: &[String]) {
    if newly_fired.is_empty() {
        return;
    }
    let body = newly_fired.join(" · ");
    // 通知失败不影响采样主路径
    if let Err(err) = app
        .notification()
        .builder()
        .title("系统监测告警")
        .body(&body)
        .show()
    {
        eprintln!("发送系统通知失败: {err}");
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
        TempSourceKind::Lhm => "已连接 LibreHardwareMonitor".to_string(),
        TempSourceKind::Wmi => "未检测到 LHM · 已回退 ACPI/WMI".to_string(),
        TempSourceKind::Sysinfo => "未检测到 LHM · 已回退 sysinfo".to_string(),
        TempSourceKind::Unavailable => "未检测到 LHM · 温度暂不可用".to_string(),
    }
}

#[tauri::command]
fn get_metrics(
    app: tauri::AppHandle,
    monitor: tauri::State<'_, SharedMonitor>,
    alerts: tauri::State<'_, SharedAlerts>,
    config: tauri::State<'_, SharedConfig>,
    tracker: tauri::State<'_, SharedTempTracker>,
) -> Result<MetricsResponse, String> {
    let metrics = {
        let mut monitor = monitor.lock().map_err(|e| e.to_string())?;
        // 主面板与叠加层共用短时缓存，避免双采样打乱网速差分
        monitor.sample_cached(METRICS_CACHE_MIN_INTERVAL)
    };

    let temp_source = tracker.get().as_str().to_string();

    let (alert, newly_fired) = {
        let mut engine = alerts.lock().map_err(|e| e.to_string())?;
        engine.evaluate(&metrics)
    };

    let notification_enabled = {
        let cfg = config.lock().map_err(|e| e.to_string())?;
        cfg.notification_enabled
    };

    if notification_enabled {
        send_alert_notification(&app, &newly_fired);
    }

    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(alert.tooltip_text()));
    }

    Ok(MetricsResponse {
        metrics,
        alert,
        temp_source,
    })
}

#[tauri::command]
fn get_metrics_history(
    monitor: tauri::State<'_, SharedMonitor>,
) -> Result<Vec<HistoryPoint>, String> {
    let monitor = monitor.lock().map_err(|e| e.to_string())?;
    Ok(monitor.history_snapshot())
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

    let saved = {
        let mut engine = alerts.lock().map_err(|e| e.to_string())?;
        engine.set_thresholds(thresholds);
        engine.thresholds()
    };

    {
        let mut cfg = config.lock().map_err(|e| e.to_string())?;
        cfg.alert = saved.clone();
        cfg.save()?;
    }

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
    config: tauri::State<'_, SharedConfig>,
) -> Result<bool, String> {
    let mut cfg = config.lock().map_err(|e| e.to_string())?;
    cfg.notification_enabled = enabled;
    cfg.save()?;
    Ok(cfg.notification_enabled)
}

#[tauri::command]
fn set_history_range_minutes(
    minutes: u32,
    monitor: tauri::State<'_, SharedMonitor>,
    config: tauri::State<'_, SharedConfig>,
) -> Result<u32, String> {
    let capacity = history_capacity_for_minutes(minutes)?;

    // 先改内存历史容量，再持久化配置，避免持有两把锁
    {
        let mut monitor = monitor.lock().map_err(|e| e.to_string())?;
        monitor.set_history_capacity(capacity);
    }

    {
        let mut cfg = config.lock().map_err(|e| e.to_string())?;
        cfg.history_range_minutes = minutes;
        cfg.save()?;
    }

    Ok(minutes)
}

#[tauri::command]
fn set_precise_temp_enabled(
    enabled: bool,
    config: tauri::State<'_, SharedConfig>,
    lhm: tauri::State<'_, SharedLhmRuntime>,
) -> Result<bool, String> {
    {
        let mut cfg = config.lock().map_err(|e| e.to_string())?;
        cfg.precise_temp_enabled = enabled;
        cfg.save()?;
    }
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
    {
        let mut cfg = config.lock().map_err(|e| e.to_string())?;
        cfg.lhm_base_url = normalized.clone();
        cfg.save()?;
    }
    lhm.set_base_url(normalized.clone());
    Ok(normalized)
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

fn apply_autostart_enabled(app: &AppHandle, enabled: bool) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;

    {
        let state = app.state::<SharedConfig>();
        let mut cfg = state.lock().map_err(|e| e.to_string())?;
        cfg.autostart_enabled = enabled;
        cfg.save()?;
    }

    let autostart = app.autolaunch();
    if enabled {
        autostart
            .enable()
            .map_err(|e| format!("启用开机自启失败: {e}"))?;
    } else {
        autostart
            .disable()
            .map_err(|e| format!("关闭开机自启失败: {e}"))?;
    }

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

fn overlay_layout_size(
    mode: OverlayLayoutMode,
    style: OverlayStyle,
    edge_x: Option<OverlayEdgeX>,
    edge_y: Option<OverlayEdgeY>,
) -> (u32, u32) {
    match mode {
        OverlayLayoutMode::Collapsed => match style {
            OverlayStyle::Capsule => (OVERLAY_COLLAPSED_WIDTH, OVERLAY_COLLAPSED_HEIGHT),
            OverlayStyle::Vertical => (OVERLAY_VERTICAL_WIDTH, OVERLAY_VERTICAL_HEIGHT),
            OverlayStyle::Numeric => (OVERLAY_NUMERIC_WIDTH, OVERLAY_NUMERIC_HEIGHT),
        },
        OverlayLayoutMode::Expanded => (OVERLAY_EXPANDED_WIDTH, OVERLAY_EXPANDED_HEIGHT),
        OverlayLayoutMode::Peek => {
            // 优先按水平贴边出竖条；仅上下贴边时出横条
            if edge_x.is_some() {
                (OVERLAY_PEEK_THICKNESS, OVERLAY_PEEK_LENGTH)
            } else if edge_y.is_some() {
                (OVERLAY_PEEK_LENGTH, OVERLAY_PEEK_THICKNESS)
            } else {
                (OVERLAY_PEEK_LENGTH, OVERLAY_PEEK_THICKNESS)
            }
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

    let (edge_x, edge_y, style) = {
        let state = app.state::<SharedConfig>();
        let cfg = state.lock().map_err(|e| e.to_string())?;
        (cfg.overlay_edge_x, cfg.overlay_edge_y, cfg.overlay_style)
    };

    let (width, height) = overlay_layout_size(mode, style, edge_x, edge_y);

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
    {
        let state = app.state::<SharedConfig>();
        let mut cfg = state.lock().map_err(|e| e.to_string())?;
        cfg.overlay_auto_hide = enabled;
        cfg.save()?;
    }
    let _ = app.emit("overlay-auto-hide-changed", enabled);
    Ok(enabled)
}

#[tauri::command]
fn set_overlay_style(style: String, app: AppHandle) -> Result<String, String> {
    let parsed = OverlayStyle::parse(&style).ok_or_else(|| "无效的叠加层形态".to_string())?;
    {
        let state = app.state::<SharedConfig>();
        let mut cfg = state.lock().map_err(|e| e.to_string())?;
        cfg.overlay_style = parsed;
        cfg.save()?;
    }
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

fn persist_overlay_position(
    app: &AppHandle,
    x: i32,
    y: i32,
    edge_x: Option<OverlayEdgeX>,
    edge_y: Option<OverlayEdgeY>,
) {
    if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
        cfg.overlay_x = Some(x);
        cfg.overlay_y = Some(y);
        cfg.overlay_edge_x = edge_x;
        cfg.overlay_edge_y = edge_y;
        if let Err(err) = cfg.save() {
            eprintln!("保存叠加层位置失败: {err}");
        }
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
    if auto_hide && (snap.edge_x.is_some() || snap.edge_y.is_some()) {
        let _ = app.emit("overlay-snap-edge", true);
    }
}

fn schedule_overlay_position_save(app: &AppHandle) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    let Ok(pos) = window.outer_position() else {
        return;
    };

    if let Ok(quiet) = app.state::<SharedOverlaySnapQuietUntil>().0.lock() {
        if quiet.map(|t| Instant::now() < t).unwrap_or(false) {
            if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
                cfg.overlay_x = Some(pos.x);
                cfg.overlay_y = Some(pos.y);
            }
            return;
        }
    }

    if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
        cfg.overlay_x = Some(pos.x);
        cfg.overlay_y = Some(pos.y);
    }

    let now = Instant::now();
    if let Ok(mut last) = app.state::<SharedOverlayLastMove>().0.lock() {
        *last = Some(now);
    }

    let app_handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(OVERLAY_SNAP_DEBOUNCE_MS));
        let still = app_handle
            .state::<SharedOverlayLastMove>()
            .0
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|t| Instant::now().duration_since(t) >= Duration::from_millis(OVERLAY_SNAP_DEBOUNCE_MS - 20))
            .unwrap_or(false);
        if !still {
            return;
        }

        let now = Instant::now();
        let should_write = {
            let state = app_handle.state::<SharedOverlayPosSave>();
            let Ok(mut last) = state.0.lock() else {
                return;
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
            apply_overlay_snap_and_persist(&app_handle);
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

fn hide_overlay_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("overlay") {
        // 隐藏前落盘当前位置（保留已记录的贴边方向）
        if let Ok(pos) = window.outer_position() {
            let (edge_x, edge_y) = app
                .state::<SharedConfig>()
                .lock()
                .map(|cfg| (cfg.overlay_edge_x, cfg.overlay_edge_y))
                .unwrap_or((None, None));
            persist_overlay_position(app, pos.x, pos.y, edge_x, edge_y);
        }
        let _ = window.hide();
    }
}

fn apply_overlay_enabled(app: &AppHandle, enabled: bool) -> Result<bool, String> {
    {
        let state = app.state::<SharedConfig>();
        let mut cfg = state.lock().map_err(|e| e.to_string())?;
        cfg.overlay_enabled = enabled;
        cfg.save()?;
    }

    if enabled {
        show_overlay_window(app);
    } else {
        hide_overlay_window(app);
    }

    sync_overlay_menu_checked(app, enabled);
    let _ = app.emit("overlay-enabled-changed", enabled);
    Ok(enabled)
}

fn persist_main_position(app: &AppHandle, x: i32, y: i32) {
    if let Ok(mut cfg) = app.state::<SharedConfig>().lock() {
        cfg.main_x = Some(x);
        cfg.main_y = Some(y);
        let _ = cfg.save();
    }
}

fn restore_main_position(window: &tauri::WebviewWindow, cfg: &AppConfig) -> bool {
    if let (Some(x), Some(y)) = (cfg.main_x, cfg.main_y) {
        let _ = window.set_position(PhysicalPosition::new(x, y));
        return true;
    }
    false
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

    let app_handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        let Some(window) = app_handle.get_webview_window("main") else {
            return;
        };
        let Ok(pos) = window.outer_position() else {
            return;
        };
        persist_main_position(&app_handle, pos.x, pos.y);
    });
}

fn show_panel(app: &tauri::AppHandle, tray_rect: Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>) {
    if let Some(window) = app.get_webview_window("main") {
        let restored = app
            .state::<SharedConfig>()
            .lock()
            .map(|cfg| restore_main_position(&window, &cfg))
            .unwrap_or(false);

        if !restored {
            if let Some((pos, size)) = tray_rect {
                let win_size = window.outer_size().unwrap_or(PhysicalSize::new(380, 690));
                let x = pos.x + (size.width as i32 / 2) - (win_size.width as i32 / 2);
                let y = pos.y.saturating_sub(win_size.height as i32 + 8);
                let physical = PhysicalPosition::new(x.max(0), y.max(0));
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
    let history_capacity = history_capacity_for_minutes(initial_config.history_range_minutes)
        .unwrap_or(history::DEFAULT_HISTORY_CAPACITY);

    let lhm_runtime = LhmRuntimeConfig::new(
        initial_config.precise_temp_enabled,
        initial_config.lhm_base_url.clone(),
    );
    let temp_tracker = TempSourceTracker::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .manage(
            Mutex::new(MonitorState::with_history_capacity(
                history_capacity,
                Arc::clone(&lhm_runtime),
                Arc::clone(&temp_tracker),
            )) as SharedMonitor,
        )
        .manage(Mutex::new(initial_alerts) as SharedAlerts)
        .manage(Mutex::new(initial_config) as SharedConfig)
        .manage(Mutex::new(false) as SharedSettingsOpen)
        .manage(Mutex::new(None) as SharedOverlayMenu)
        .manage(SharedOverlayPosSave(Mutex::new(None)))
        .manage(SharedOverlayLastMove(Mutex::new(None)))
        .manage(SharedOverlaySnapQuietUntil(Mutex::new(None)))
        .manage(lhm_runtime as SharedLhmRuntime)
        .manage(temp_tracker as SharedTempTracker)
        .invoke_handler(tauri::generate_handler![
            get_metrics,
            get_metrics_history,
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
            set_overlay_style,
            set_autostart_enabled
        ])
        .setup(|app| {
            sync_autostart_with_config(app.handle());

            let overlay_enabled = app
                .state::<SharedConfig>()
                .lock()
                .map(|cfg| cfg.overlay_enabled)
                .unwrap_or(false);

            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let alert_settings =
                MenuItem::with_id(app, "alert_settings", "设置", true, None::<&str>)?;
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
            let menu = Menu::with_items(app, &[&show, &overlay_item, &alert_settings, &quit])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
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
                        let position = rect.position.to_physical(1.0);
                        let size = rect.size.to_physical(1.0);
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
