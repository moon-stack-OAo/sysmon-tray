# 系统监测（托盘版）

基于 **Tauri 2** 的 Windows 托盘常驻轻量系统监测工具。左键弹出无边框迷你面板，可选桌面叠加层；实时显示 CPU / 内存 / 网络 / 磁盘与 CPU 温度，支持阈值告警、系统通知、历史曲线与配置持久化。

当前版本：`0.1.0` · 标识符：`com.sysmon.tray` · 产品名：`系统监测`

## 功能

### 托盘与主面板

- 托盘图标常驻，tooltip 显示应用名或告警摘要
- 左键点击：切换主面板显示/隐藏，并定位到托盘图标上方
- 右键菜单：显示面板 / 叠加层（可勾选）/ 告警设置 / 退出
- 主面板 `340×560`：无边框、毛玻璃、置顶、不进任务栏
- 失焦自动隐藏（设置页打开时除外）；关闭仅隐藏，进程继续常驻

### 桌面叠加层

- 可选启用，默认关闭；托盘菜单与设置页双向同步
- 收起 `300×38`：CPU / 内存 / 温度 / 上下行网速
- 展开 `300×176`：带进度条的详情视图
- 可拖拽、双击展开/收起；拖动后吸附屏幕边缘，位置写入配置
- 失焦不隐藏；关闭叠加层窗口会关闭该功能（不退出应用）

### 实时监测（约 1 秒刷新）

| 监测项    | 说明                                                     |
|--------|--------------------------------------------------------|
| CPU    | 占用率 + 进度条                                              |
| 内存     | 占用率、已用/总量 + 进度条                                        |
| 网络     | 下行 / 上行速率（自适应单位）                                       |
| 磁盘     | 各盘占用情况（最多展示 4 块）                                       |
| CPU 温度 | 有值显示 °C 与来源后缀（`LHM` / `ACPI` / `sysinfo`）；读不到则提示「暂不可用」 |

后端对指标有约 700ms 短时缓存，避免主面板与叠加层同时拉取时打乱网速差分。

### 告警

- 默认阈值：CPU ≥ 90%、内存 ≥ 90%、温度 ≥ 85°C
- 同类告警默认冷却 60 秒（可配 5–3600）
- 告警时：托盘 tooltip 更新、主面板横幅、相关卡片高亮、叠加层红点/边框高亮
- 可选 Windows 系统通知（默认开启，可在设置中关闭）

### 历史曲线

- 环形缓冲，默认约 60 个采样点（约 1 分钟）
- 可配置时长：1 / 5 / 15 / 60 分钟
- Canvas 折线 + 面积：CPU（蓝）、内存（紫）、温度（橙，有温度时绘制，独立 Y 轴）

### 设置与配置持久化

主面板「设置」Tab（托盘「告警设置」可直达）支持：

- 告警阈值（CPU / 内存 / 温度 / 冷却）
- 启用系统通知
- 启用叠加层
- 历史时长
- LibreHardwareMonitor 精确温度开关与 Web 地址
- 温度来源状态提示
- 保存 / 恢复默认

配置文件：

```text
%APPDATA%\com.sysmon.tray\config.json
```

## 技术栈

| 层           | 技术                                                              |
|-------------|-----------------------------------------------------------------|
| 桌面框架        | Tauri 2（`tray-icon`）                                            |
| 前端          | TypeScript + Vite 6（原生 DOM，双入口：`index.html` / `overlay.html`）   |
| 后端          | Rust 2021                                                       |
| 系统信息        | `sysinfo`                                                       |
| 温度（Windows） | LibreHardwareMonitor（可选）→ WMI（ACPI / 性能计数器）→ sysinfo Components |
| 其他          | `tauri-plugin-notification`、`dirs`、`ureq`（本机 HTTP）              |

## 快速开始

### 环境要求

- Node.js + npm
- Rust toolchain
- Windows WebView2

### 开发

```bash
npm install
npm run tauri dev
```

开发前端地址：`http://localhost:1420`

### 构建

```bash
npm run build:exe
# 等同于：npm run tauri build
```

可执行文件（便携版）：

```text
src-tauri/target/release/sysmon-tray.exe
```

安装包：

```text
src-tauri/target/release/bundle/nsis/   # NSIS 安装包 .exe
src-tauri/target/release/bundle/nsis/   # NSIS 安装包
```

### 发布（GitHub Actions）

1. 更新 `package.json` / `src-tauri/Cargo.toml` / `src-tauri/tauri.conf.json` 版本号与 `CHANGELOG.md`
2. 推送 tag，例如：

```bash
git tag v0.1.0
git push origin v0.1.0
```

或在 Actions 页手动运行 **Release** 工作流。成功后会生成 Draft Release，可下载 Windows 安装包与便携 exe。

### 应用图标

源图位于仓库根目录 `app-icon.svg` / `app-icon.png`。更新全套平台图标：

```bash
npx tauri icon app-icon.png
```

## 项目结构

```text
sysmon-tray/
├── index.html                 # 主面板 DOM（监测 + 设置）
├── overlay.html               # 叠加层 DOM
├── app-icon.png / app-icon.svg# 应用图标源
├── package.json
├── vite.config.ts             # 双入口构建
├── src/
│   ├── main.ts                # 主面板：指标、告警、历史、设置
│   ├── styles.css             # 主面板深色玻璃态样式
│   ├── overlay.ts             # 叠加层逻辑
│   ├── overlay.css            # 叠加层样式
│   └── assets/
└── src-tauri/
    ├── tauri.conf.json        # 双窗口 / 打包配置
    ├── capabilities/          # 权限（main + overlay）
    ├── icons/                 # 托盘与安装包图标
    └── src/
        ├── main.rs            # 入口（release 隐藏控制台）
        ├── lib.rs             # 托盘、双窗口生命周期、commands
        ├── monitor.rs         # 指标采样、短时缓存、历史写入
        ├── temperature.rs     # 温度 Provider 链
        ├── temperature_lhm.rs # LHM HTTP 接入与熔断
        ├── history.rs         # 环形历史缓冲
        ├── alert.rs           # 阈值告警引擎
        └── config.rs          # config.json 读写
```

## 温度说明

1. Provider 顺序：**LibreHardwareMonitor → WMI → sysinfo Components**，失败自动回退。
2. LHM 默认关闭；开启后请求本机 Web Server（默认 `http://127.0.0.1:8085/data.json`），仅允许 localhost，超时与连续失败会熔断。
3. WMI / sysinfo 读取的是 **ACPI 热区温度**，不是逐核传感器精确值；LHM 通常更接近 CPU Package / Tctl 等读数。
4. 应用不捆绑硬件驱动；使用精确温度需本机运行 LibreHardwareMonitor 并开启 Remote Web Server。
5. 读不到时前端显示「温度：暂不可用」，历史图不绘制温度曲线。
