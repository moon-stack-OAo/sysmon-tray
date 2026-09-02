# 更新日志

本文件记录系统监测（sysmon-tray）的重要变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [0.1.1]

### 变更

- 主面板与叠加层视觉升级为 Obsidian Instrument（近黑精密仪表盘、冷青蓝强调色）
- 设置页底部「保存 / 恢复默认」固定，内容区独立滚动
- 应用图标改为精密仪表环 + 冷青蓝波形，并重新生成 Tauri 图标集

## [0.1.0]

首个公开发布版本。

### 新增

- Windows 托盘常驻系统监测（Tauri 2）
- 主面板实时展示 CPU / 内存 / 网络 / 磁盘 / CPU 温度
- 告警阈值、冷却时间与系统通知（启动请求权限；开启时可发测试通知）
- 历史曲线：1 / 5 / 15 / 60 分钟
- 桌面叠加层：胶囊细条 / 竖条 / 纯数值条；展开详情
- 叠加层四边吸附（工作区，避开任务栏），角落优先并持久化贴边方向
- 叠加层贴边自动隐藏：仅左右贴边收成竖热区，鼠标靠近展开
- 主面板标题栏可拖动并记住位置（`main_x` / `main_y`）
- 可选 LibreHardwareMonitor（LHM）精确温度
- 设置页：告警、通知与显示、精确温度、开机自启
- 常用脚本：`tauri:dev`、`typecheck`、`format` / `format:check`、`clean`

### 说明

- 产品标识：`productName = SysMon`，安装目录与快捷方式使用英文展示名
- 主面板尺寸：`380×690`；叠加层默认宽度：`310`（纯数值条 `258`）
- 托盘菜单：显示面板 / 叠加层 / 设置 / 退出

### 构建

- `npm run build:exe` 本地打包
- GitHub Actions Release 正式发布：NSIS 安装包 + 便携 `sysmon-tray.exe`
- NSIS 安装界面支持中英双语选择
- Release 资源英文命名：`sysmon-tray_*_x64-setup.exe`

[Unreleased]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/moon-stack-OAo/sysmon-tray/releases/tag/v0.1.0
