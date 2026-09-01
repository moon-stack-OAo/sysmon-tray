# AGENTS.md

面向 AI 编码助手与协作者的项目指引。修改代码前请先阅读本文件与 `README.md`。

## 项目概述

**sysmon-tray（系统监测）** 是基于 **Tauri 2** 的 Windows 托盘常驻轻量系统监测工具。

- 标识符：`com.sysmon.tray`
- 产品名：`系统监测`
- 当前版本以 `package.json` / `src-tauri/Cargo.toml` / `src-tauri/tauri.conf.json` 为准（三者须保持一致）
- 配置文件：`%APPDATA%\com.sysmon.tray\config.json`

核心能力：托盘迷你面板、可选桌面叠加层、CPU/内存/网络/磁盘/温度实时监测、阈值告警、历史曲线、配置持久化、开机自启。

## 技术栈

| 层              | 技术                                                                         |
| --------------- | ---------------------------------------------------------------------------- |
| 桌面框架        | Tauri 2（`tray-icon`）                                                       |
| 前端            | TypeScript + Vite 6，原生 DOM（无框架）                                      |
| 后端            | Rust 2021                                                                    |
| 系统信息        | `sysinfo`                                                                    |
| 温度（Windows） | LHM（可选）→ WMI → sysinfo Components                                        |
| 插件            | `tauri-plugin-notification`、`tauri-plugin-autostart`、`tauri-plugin-opener` |
| 本机 HTTP       | `ureq`（仅 localhost，关 TLS）                                               |

## 目录结构

```text
sysmon-tray/
├── index.html                 # 主面板 DOM
├── overlay.html               # 叠加层 DOM
├── vite.config.ts             # 双入口：main / overlay
├── src/
│   ├── main.ts                # 主面板逻辑
│   ├── styles.css
│   ├── overlay.ts             # 叠加层逻辑
│   └── overlay.css
└── src-tauri/
    ├── tauri.conf.json        # 双窗口与打包
    ├── capabilities/          # 权限（main + overlay）
    └── src/
        ├── main.rs            # 入口（release 隐藏控制台）
        ├── lib.rs             # 托盘、窗口生命周期、commands
        ├── monitor.rs         # 指标采样与短时缓存
        ├── temperature.rs     # 温度 Provider 链
        ├── temperature_lhm.rs # LHM HTTP 与熔断
        ├── history.rs         # 环形历史缓冲
        ├── alert.rs           # 告警引擎
        └── config.rs          # 配置读写
```

## 常用命令

```bash
npm install
npm run tauri dev          # 开发（前端 http://localhost:1420）
npm run build              # 前端：tsc && vite build
npm run build:exe          # 等同 tauri build，产出 exe / NSIS / MSI
npx tauri icon app-icon.png # 从源图生成全套图标
```

验证建议：

- 前端类型检查：`npx tsc --noEmit`（或 `npm run build` 中的 `tsc`）
- Rust：在 `src-tauri` 下执行 `cargo check` / `cargo clippy`（若环境可用）

## 架构约定

### 双窗口

- `main`：380×690，无边框、透明、置顶、不进任务栏；标题栏可拖；位置写入 `main_x`/`main_y`；失焦隐藏（设置页打开时除外）；关闭仅隐藏。
- `overlay`：默认收起约 310×38；可展开；失焦不隐藏；关闭叠加层窗口会关闭该功能，不退出应用。
- Vite 双入口对应 `index.html` / `overlay.html`，勿合并为单页。

### 前后端通信

- 前端通过 `@tauri-apps/api` 的 `invoke` / `listen` 调用 Rust commands。
- 序列化字段统一 **camelCase**（Rust `#[serde(rename_all = "camelCase")]`）。
- 新增 command 时：在 `lib.rs` 注册，并评估是否需更新 `capabilities/default.json`。

主要 commands（以 `lib.rs` 为准）：

- 指标：`get_metrics`、`get_metrics_history`
- 告警：`get_alert_thresholds`、`set_alert_thresholds`、`reset_alert_thresholds`
- 配置：`get_app_config`、`set_notification_enabled`、`set_history_range_minutes`、`set_precise_temp_enabled`、`set_lhm_base_url`、`get_temp_source_status`
- UI 状态：`set_settings_open`、`set_overlay_enabled`、`set_overlay_collapsed`、`set_overlay_layout`、`set_overlay_auto_hide`、`set_overlay_style`
- 自启：`set_autostart_enabled`

### 指标与缓存

- 刷新约 1 秒；后端对指标有约 **700ms** 短时缓存（`METRICS_CACHE_MIN_INTERVAL`）。
- **主面板与叠加层必须共用缓存采样**，避免双路采样打乱网速差分。

### 温度 Provider

顺序：**LibreHardwareMonitor → WMI → sysinfo**，失败自动回退。

- LHM 默认关闭；仅允许 localhost；超时与连续失败会熔断。
- 应用不捆绑硬件驱动；精确温度依赖本机 LHM + Remote Web Server。
- 读不到时前端显示「暂不可用」，历史图不绘制温度曲线。

### 叠加层

- 形态：`capsule` / `vertical` / `numeric`（`OverlayStyle`）。
- 布局模式：`collapsed` / `expanded` / `peek`（贴边自动隐藏热区）。
- 位置、贴边方向（`overlay_edge_x` / `overlay_edge_y`）、自动隐藏写入配置；吸附逻辑在 `lib.rs`，改尺寸常量时同步前后端。

## 代码规范

### 通用

- 回复与文档优先中文；代码标识符保持英文。
- **不要主动添加注释**，除非用户要求或解释非显而易见的约束（如安全边界、熔断、缓存语义）。
- 已有中文注释风格可沿用；新增注释优先中文。
- 保持现有代码风格，优先改现有文件，避免无关重构。
- 禁止引入会暴露或记录密钥的逻辑；本仓库无云端密钥场景，LHM URL 仅限本机。

### TypeScript（`src/`）

- 严格模式（`strict`、`noUnusedLocals`、`noUnusedParameters`）。
- Prettier：单引号、分号、尾逗号 `all`、`printWidth: 100`、`tabWidth: 2`、LF。
- 原生 DOM 操作；接口定义靠近使用处；与 Rust 侧 DTO 字段名保持 camelCase 一致。
- 主面板与叠加层逻辑分离（`main.ts` / `overlay.ts`），共享类型可就地重复小接口，勿强行抽公共包 unless 明显重复膨胀。

### Rust（`src-tauri/src/`）

- Edition 2021；错误对前端多为 `Result<T, String>`。
- 共享状态用 `Mutex` / `Arc`（见 `SharedMonitor`、`SharedConfig` 等）；锁失败转字符串错误。
- Windows 专用依赖放在 `[target.'cfg(windows)'.dependencies]`。
- 告警通知失败不得影响采样主路径。
- 修改窗口尺寸、吸附阈值等常量时，核对前端 CSS/布局与配置默认值。

### 权限与安全

- 最小权限：仅在 `capabilities/default.json` 增加确有需要的 permission。
- LHM 地址必须经 `normalize_lhm_base_url` 校验（仅本机）。
- 不扩大网络访问面；`ureq` 保持默认 features 关闭 TLS 的现有策略，除非有明确需求。

## 版本与发布

发布前同步更新：

1. `package.json` 的 `version`
2. `src-tauri/Cargo.toml` 的 `version`
3. `src-tauri/tauri.conf.json` 的 `version`
4. `CHANGELOG.md`（Keep a Changelog + SemVer）

发布方式：推送 `v*` tag 或手动跑 GitHub Actions **Release** 工作流；产物含 NSIS / MSI / 便携 `sysmon-tray.exe`。

**未明确要求打 TAG / 发布前，不要随意改版本号。** 未 TAG 的改动视为同一版本内的变动。

## Git 规范

- 提交信息使用中文。
- **未经用户明确授权，禁止**：`git add` / `git commit` / `git push` / `git reset` / `git rebase` / `git checkout --`、删分支或改写历史。
- 需要 Git 操作时，先说明影响范围并征求确认。

## 修改时的注意点

1. **双端联动**：改 metrics/config/alert 字段时，同时改 Rust 结构体与 `main.ts` / `overlay.ts` 接口。
2. **缓存语义**：不要绕过 `sample_cached` 让 overlay 与 main 各自裸采样。
3. **窗口行为**：主面板失焦隐藏与 `set_settings_open` 联动；叠加层关闭 ≠ 退出应用。
4. **图标**：源图为根目录 `app-icon.svg` / `app-icon.png`，用 `npx tauri icon` 再生，勿手改 `src-tauri/icons` 零散文件 unless 必要。
5. **CI**：`.github/workflows/release.yml` 面向 Windows；改打包路径时同步 workflow artifact 路径。

## 不要做的事

- 不要引入 React/Vue 等前端框架，除非用户明确要求重构。
- 不要把温度逻辑改成捆绑驱动或远程非本机 HTTP。
- 不要提交 `node_modules/`、`src-tauri/target/`、本地 `.env`、IDE 私有配置。
- 不要在未确认时升级大版本依赖（Tauri / sysinfo / Vite 等）。
