# 更新日志

本文件记录系统监测（sysmon-tray）的重要变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)

## [Unreleased]

## [0.1.0]

### 新增

- 基于 Tauri 2 的 Windows 托盘常驻系统监测
- 迷你面板：CPU / 内存 / 网络 / 磁盘 / CPU 温度实时展示
- 告警阈值、冷却时间、系统通知
- 历史曲线（可配置 1 / 5 / 15 / 60 分钟）
- 桌面叠加层：收起细条 / 展开进度条双态
- 叠加层四边吸附（基于工作区，避开任务栏）；角落优先、阈值提高，并持久化贴边方向
- 叠加层「贴边自动隐藏」：贴边后收成细边条热区，鼠标靠近展开
- 叠加层形态可选：胶囊细条 / 竖条 / 纯数值条（设置页下拉）
- LibreHardwareMonitor（LHM）精确温度可选接入
- 设置页：告警、通知与显示、精确温度分组
- 设置页支持「开机自启」开关（基于 tauri-plugin-autostart）

### 优化

- 主面板紧凑仪表盘布局（340×560）
- CPU / 内存双列核心指标，网络与磁盘更紧凑
- 深色玻璃拟态视觉与设计令牌统一
- 禁用 WebView 右键菜单
- 叠加层透明观感；展开/收起按贴边锚定（右/下）定位，减少漂移

### 构建

- 支持 `npm run build:exe` 本地打包
- GitHub Actions Release 工作流产出 Windows NSIS / MSI / 便携 exe

[Unreleased]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.0...HEAD

[0.1.0]: https://github.com/moon-stack-OAo/sysmon-tray/releases/tag/v0.1.0
