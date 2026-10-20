# 更新日志

本文件记录系统监测（sysmon-tray）的重要变更。

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 新增

- GPU 占用告警与历史曲线
- 托盘图标在告警时变色
- 网络卡片显示当前网卡；无网卡时提示未检测到
- 磁盘按挂载点排序并可滚动
- 告警持续秒数与安静时段
- 全局快捷键：显示面板、开关叠加层、开关鼠标穿透
- 叠加层收起态可勾选显示项
- 监测页导出历史 CSV

## [0.1.4]

### 修复

- 启动首帧网速不再把网卡累计字节当成瞬时速率
- 磁盘可用空间大于总量时不再下溢误报
- 进程 Top 刷新不再长时间占用指标采样锁
- WMI 超时结果不再被下一轮当成当前温度；COM 初始化失败后不再反复建线程
- 告警仅在通知发送成功后计入冷却，贴线抖动增加滞回
- LHM 无 CPU 温度时不再把 GPU / 风扇贴到 WMI 回退帧；请求不再跟随重定向
- 配置落盘不再持有配置锁；主面板坐标会收拢到当前工作区
- 叠加层布局切换失败或并发时不再与窗口尺寸分叉；穿透开启时移入 peek 热区仍会展开
- 进入设置页前先锁定失焦保护；采样失败时显示数据中断

### 变更

- 叠加层展开高度调整为 360，容纳 Swap 与 GPU / 风扇详情
- LHM 地址仅允许 `127.0.0.1` 与 `[::1]`，不再接受 `localhost`
- 移除未使用的 opener 插件
- Release 要求 tag 与版本号一致，产物缺失则工作流失败
- Release 产物改为 `sysmon-tray-setup.exe` 与 `sysmon-tray-portable.exe`

## [0.1.3]

### 新增

- 风扇转速：启用 LHM 时解析最多 4 路风扇（CPU / 泵 / GPU / 机箱优先）；主面板卡片与叠加层展开态展示
- 历史曲线增加主风扇 RPM（独立转速轴）；无风扇数据时不绘制
- 叠加层鼠标穿透：设置项 `overlayClickThrough`（默认关闭）；开启后默认点穿桌面，光标进入窗口热区可操作

### 说明

- 风扇与 GPU 一样仅来自 LHM，无独立回退；不做风扇告警
- 穿透依赖整窗外框命中轮询；拖拽中不会重新开启穿透

## [0.1.2]

### 新增

- Swap 占用展示：主面板紧凑卡与叠加层展开态；无交换分区时隐藏
- 磁盘占用告警：`diskPercent` 默认 90%，文案含挂载点
- 进程 Top 5：设置项 `processTopEnabled`（默认关闭），约 4 秒降频，按进程名合并
- GPU 占用 / 显存 / 温度：启用 LibreHardwareMonitor 时从同一 `data.json` 解析主 GPU；无 LHM 时显示依赖说明（无独立回退）
- 主面板 GPU 卡片；叠加层胶囊 / 数值 / 竖条 / 展开态展示 GPU

### 变更

- 叠加层尺寸：胶囊宽 378、数值条宽 246、竖条约 76×200、展开高 252
- 胶囊 / 数值收起态 GPU 仅显示占用%；竖条合并为单行；竖条去掉展开按钮（仍可双击展开）

### 说明

- GPU 复用「精确温度」开关与 LHM 地址，不另增开关
- 暂未加入 GPU 告警阈值与历史曲线

## [0.1.1]

### 新增

- 单实例限制：再次启动时唤醒已有进程并显示主面板，不再拉起第二个进程

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
- GitHub Actions Release 正式发布：`sysmon-tray-setup.exe` + `sysmon-tray-portable.exe`
- NSIS 安装界面支持中英双语选择
- Release 资源英文命名：`sysmon-tray-setup.exe`、`sysmon-tray-portable.exe`

[Unreleased]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/moon-stack-OAo/sysmon-tray/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/moon-stack-OAo/sysmon-tray/releases/tag/v0.1.0
