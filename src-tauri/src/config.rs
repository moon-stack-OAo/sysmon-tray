use crate::alert::AlertThresholds;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const APP_CONFIG_DIR: &str = "com.sysmon.tray";
const CONFIG_FILE_NAME: &str = "config.json";

pub const DEFAULT_LHM_BASE_URL: &str = "http://127.0.0.1:8085";

fn default_notification_enabled() -> bool {
    true
}

fn default_history_range_minutes() -> u32 {
    1
}

fn default_precise_temp_enabled() -> bool {
    false
}

fn default_lhm_base_url() -> String {
    DEFAULT_LHM_BASE_URL.to_string()
}

fn default_overlay_enabled() -> bool {
    false
}

fn default_autostart_enabled() -> bool {
    false
}

fn default_overlay_auto_hide() -> bool {
    false
}

fn default_overlay_style() -> OverlayStyle {
    OverlayStyle::Capsule
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverlayStyle {
    Capsule,
    Vertical,
    Numeric,
}

impl OverlayStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayStyle::Capsule => "capsule",
            OverlayStyle::Vertical => "vertical",
            OverlayStyle::Numeric => "numeric",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "capsule" => Some(OverlayStyle::Capsule),
            "vertical" => Some(OverlayStyle::Vertical),
            "numeric" => Some(OverlayStyle::Numeric),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverlayEdgeX {
    Left,
    Right,
}

impl OverlayEdgeX {
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayEdgeX::Left => "left",
            OverlayEdgeX::Right => "right",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OverlayEdgeY {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub alert: AlertThresholds,
    /// 是否启用系统通知
    #[serde(default = "default_notification_enabled")]
    pub notification_enabled: bool,
    /// 历史曲线时间范围（分钟：1/5/15/60）
    #[serde(default = "default_history_range_minutes")]
    pub history_range_minutes: u32,
    /// 是否启用 LibreHardwareMonitor 精确温度（默认关，避免未安装时每秒白打 HTTP）
    #[serde(default = "default_precise_temp_enabled")]
    pub precise_temp_enabled: bool,
    /// LHM Web Server 根地址（仅允许本机）
    #[serde(default = "default_lhm_base_url")]
    pub lhm_base_url: String,
    /// 是否启用独立叠加层浮窗
    #[serde(default = "default_overlay_enabled")]
    pub overlay_enabled: bool,
    /// 是否开机自启
    #[serde(default = "default_autostart_enabled")]
    pub autostart_enabled: bool,
    /// 叠加层贴边后自动隐藏为热区细边条
    #[serde(default = "default_overlay_auto_hide")]
    pub overlay_auto_hide: bool,
    /// 叠加层收起态形态：capsule / vertical / numeric
    #[serde(default = "default_overlay_style")]
    pub overlay_style: OverlayStyle,
    /// 主面板窗口物理坐标 X（None 表示首次按托盘定位）
    #[serde(default)]
    pub main_x: Option<i32>,
    /// 主面板窗口物理坐标 Y
    #[serde(default)]
    pub main_y: Option<i32>,
    /// 叠加层窗口物理坐标 X（None 表示使用系统默认位置）
    #[serde(default)]
    pub overlay_x: Option<i32>,
    /// 叠加层窗口物理坐标 Y
    #[serde(default)]
    pub overlay_y: Option<i32>,
    /// 叠加层水平贴边锚定（展开/收起时保该边）
    #[serde(default)]
    pub overlay_edge_x: Option<OverlayEdgeX>,
    /// 叠加层垂直贴边锚定（展开/收起时保该边）
    #[serde(default)]
    pub overlay_edge_y: Option<OverlayEdgeY>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            alert: AlertThresholds::default(),
            notification_enabled: default_notification_enabled(),
            history_range_minutes: default_history_range_minutes(),
            precise_temp_enabled: default_precise_temp_enabled(),
            lhm_base_url: default_lhm_base_url(),
            overlay_enabled: default_overlay_enabled(),
            autostart_enabled: default_autostart_enabled(),
            overlay_auto_hide: default_overlay_auto_hide(),
            overlay_style: default_overlay_style(),
            main_x: None,
            main_y: None,
            overlay_x: None,
            overlay_y: None,
            overlay_edge_x: None,
            overlay_edge_y: None,
        }
    }
}

impl AppConfig {
    pub fn config_path() -> Result<PathBuf, String> {
        let base = dirs::config_dir().ok_or_else(|| "无法获取配置目录".to_string())?;
        Ok(base.join(APP_CONFIG_DIR).join(CONFIG_FILE_NAME))
    }

    pub fn load_or_default() -> Self {
        match Self::load() {
            Ok(cfg) => cfg,
            Err(_) => Self::default(),
        }
    }

    pub fn load() -> Result<Self, String> {
        let path = Self::config_path()?;
        let raw = fs::read_to_string(&path).map_err(|e| format!("读取配置失败: {e}"))?;
        let mut cfg: Self =
            serde_json::from_str(&raw).map_err(|e| format!("解析配置失败: {e}"))?;
        cfg.history_range_minutes =
            crate::history::normalize_history_range_minutes(cfg.history_range_minutes);
        cfg.lhm_base_url = normalize_lhm_base_url(&cfg.lhm_base_url)
            .unwrap_or_else(|_| DEFAULT_LHM_BASE_URL.to_string());
        Ok(cfg)
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
        }
        let raw =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;
        fs::write(&path, raw).map_err(|e| format!("写入配置失败: {e}"))
    }
}

/// 校验并规范化 LHM 根 URL：仅允许本机 http(s)，防 SSRF。
pub fn normalize_lhm_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("LHM 地址不能为空".to_string());
    }

    let lower = trimmed.to_ascii_lowercase();
    // 仅允许 http：本机 Web Server，且避免引入 TLS 依赖
    let rest = if let Some(r) = lower.strip_prefix("http://") {
        let _ = r;
        &trimmed["http://".len()..]
    } else if lower.starts_with("https://") {
        return Err("LHM 地址仅支持 http://（本机 Web Server）".to_string());
    } else {
        return Err("LHM 地址须以 http:// 开头".to_string());
    };

    if rest.contains('@') {
        return Err("LHM 地址不允许包含用户信息".to_string());
    }

    let authority = rest.split('/').next().unwrap_or("").trim();
    if authority.is_empty() {
        return Err("LHM 地址缺少主机".to_string());
    }

    let host = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| "LHM 地址 IPv6 格式无效".to_string())?;
        &authority[..=end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };

    let host_ok = host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1";
    if !host_ok {
        return Err("出于安全，仅允许 localhost / 127.0.0.1".to_string());
    }

    // 端口可选；路径部分丢弃，统一为根 URL
    let port_part = if host.starts_with('[') {
        authority[host.len()..].to_string()
    } else if let Some(idx) = authority.find(':') {
        authority[idx..].to_string()
    } else {
        String::new()
    };

    if !port_part.is_empty() {
        let port_str = port_part.trim_start_matches(':');
        if port_str.is_empty()
            || !port_str.chars().all(|c| c.is_ascii_digit())
            || port_str.parse::<u16>().is_err()
        {
            return Err("LHM 端口无效".to_string());
        }
    }

    let host_out = if host == "::1" { "[::1]" } else { host };
    Ok(format!("http://{host_out}{port_part}"))
}

pub type SharedConfig = std::sync::Mutex<AppConfig>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_localhost() {
        assert_eq!(
            normalize_lhm_base_url("http://127.0.0.1:8085").unwrap(),
            "http://127.0.0.1:8085"
        );
        assert_eq!(
            normalize_lhm_base_url("http://localhost:8085/").unwrap(),
            "http://localhost:8085"
        );
    }

    #[test]
    fn rejects_remote_and_https() {
        assert!(normalize_lhm_base_url("http://192.168.1.1:8085").is_err());
        assert!(normalize_lhm_base_url("https://127.0.0.1:8085").is_err());
        assert!(normalize_lhm_base_url("http://evil.com").is_err());
    }
}
