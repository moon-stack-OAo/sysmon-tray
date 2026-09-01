import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface DiskStat {
  name: string;
  mountPoint: string;
  totalBytes: number;
  availableBytes: number;
  usedPercent: number;
}

interface AlertThresholds {
  cpuPercent: number;
  memoryPercent: number;
  cpuTempCelsius: number;
  cooldownSecs: number;
}

interface AppConfig {
  alert: AlertThresholds;
  notificationEnabled: boolean;
  historyRangeMinutes: number;
  preciseTempEnabled: boolean;
  lhmBaseUrl: string;
  overlayEnabled: boolean;
  autostartEnabled: boolean;
  overlayAutoHide: boolean;
  overlayStyle: 'capsule' | 'vertical' | 'numeric';
  overlayX?: number | null;
  overlayY?: number | null;
}

interface TempSourceStatus {
  source: string;
  message: string;
  preciseTempEnabled: boolean;
  lhmBaseUrl: string;
}

const DEFAULT_LHM_BASE_URL = 'http://127.0.0.1:8085';

interface AlertStatus {
  cpu: boolean;
  memory: boolean;
  temperature: boolean;
  active: boolean;
  messages: string[];
  thresholds: AlertThresholds;
}

interface Metrics {
  cpuPercent: number;
  memoryUsedBytes: number;
  memoryTotalBytes: number;
  memoryPercent: number;
  swapUsedBytes: number;
  swapTotalBytes: number;
  netDownBps: number;
  netUpBps: number;
  disks: DiskStat[];
  cpuTempCelsius: number | null;
  sampledAtMs: number;
  alert: AlertStatus;
  tempSource?: string;
}

interface HistoryPoint {
  cpuPercent: number;
  memoryPercent: number;
  cpuTempCelsius: number | null;
  timestampMs: number;
}

type TabId = 'monitor' | 'settings';

const ALLOWED_HISTORY_RANGE_MINUTES = [1, 5, 15, 60] as const;

let currentTab: TabId = 'monitor';
let settingsMessageTimer: number | undefined;
let historyRangeMinutes = 1;

function formatBytes(bytes: number): string {
  if (bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / Math.pow(1024, exp);
  return `${value.toFixed(exp === 0 ? 0 : 1)} ${units[exp]}`;
}

function formatSpeed(bps: number): string {
  return `${formatBytes(bps)}/s`;
}

function setBar(el: HTMLElement | null, percent: number, alert = false) {
  if (!el) return;
  const value = Math.max(0, Math.min(100, percent));
  el.style.width = `${value}%`;
  el.classList.remove('warn', 'danger');
  if (alert || value >= 90) el.classList.add('danger');
  else if (value >= 75) el.classList.add('warn');
}

function renderDiskList(diskList: HTMLElement, disks: DiskStat[]) {
  if (!disks.length) {
    diskList.replaceChildren();
    diskList.textContent = '未检测到磁盘';
    return;
  }

  if (diskList.childElementCount === 0) {
    diskList.textContent = '';
  }

  const items = disks.slice(0, 4);
  const existing = Array.from(diskList.querySelectorAll<HTMLElement>('.disk-item'));

  while (existing.length > items.length) {
    const node = existing.pop();
    node?.remove();
  }

  for (let i = 0; i < items.length; i++) {
    const disk = items[i];
    let item = existing[i];
    if (!item) {
      item = document.createElement('div');
      item.className = 'disk-item';
      const title = document.createElement('div');
      title.className = 'disk-title';
      const progress = document.createElement('div');
      progress.className = 'progress thin';
      const bar = document.createElement('div');
      bar.className = 'progress-bar';
      progress.appendChild(bar);
      const sub = document.createElement('div');
      sub.className = 'metric-sub';
      item.append(title, progress, sub);
      diskList.appendChild(item);
    }

    const titleEl = item.querySelector('.disk-title');
    const barEl = item.querySelector('.progress-bar') as HTMLElement | null;
    const subEl = item.querySelector('.metric-sub');
    const used = disk.totalBytes - disk.availableBytes;

    if (titleEl) titleEl.textContent = disk.mountPoint || disk.name;
    setBar(barEl, disk.usedPercent);
    if (subEl) {
      subEl.textContent = `${formatBytes(used)} / ${formatBytes(disk.totalBytes)} · ${disk.usedPercent.toFixed(1)}%`;
    }
  }
}

function setCardAlert(selector: string, active: boolean) {
  const card = document.querySelector(selector);
  if (!card) return;
  card.classList.toggle('alert', active);
}

function normalizeHistoryRangeMinutes(minutes: number): number {
  return (ALLOWED_HISTORY_RANGE_MINUTES as readonly number[]).includes(minutes) ? minutes : 1;
}

function updateChartHint(minutes: number) {
  const hint = document.querySelector('#chart-hint');
  if (hint) {
    hint.textContent = `最近约 ${normalizeHistoryRangeMinutes(minutes)} 分钟`;
  }
}

/** 点数过多时按 stride 抽样绘制，数据本身仍全保留 */
function downsampleForDraw(points: HistoryPoint[], maxPoints = 600): HistoryPoint[] {
  if (points.length <= maxPoints) return points;
  const stride = Math.ceil(points.length / maxPoints);
  const sampled: HistoryPoint[] = [];
  for (let i = 0; i < points.length; i += stride) {
    sampled.push(points[i]);
  }
  const last = points[points.length - 1];
  if (sampled[sampled.length - 1] !== last) {
    sampled.push(last);
  }
  return sampled;
}

function hexToRgba(hex: string, alpha: number): string {
  const raw = hex.replace('#', '');
  const r = Number.parseInt(raw.slice(0, 2), 16);
  const g = Number.parseInt(raw.slice(2, 4), 16);
  const b = Number.parseInt(raw.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function updateChartReadings(point: HistoryPoint | null, hasTemp: boolean) {
  const cpuReading = document.querySelector('#chart-cpu-reading');
  const memReading = document.querySelector('#chart-mem-reading');
  const tempReading = document.querySelector('#chart-temp-reading') as HTMLElement | null;

  if (cpuReading) {
    cpuReading.textContent = point != null ? `CPU ${point.cpuPercent.toFixed(1)}%` : 'CPU --';
  }
  if (memReading) {
    memReading.textContent = point != null ? `内存 ${point.memoryPercent.toFixed(1)}%` : '内存 --';
  }
  if (tempReading) {
    if (hasTemp && point?.cpuTempCelsius != null) {
      tempReading.hidden = false;
      tempReading.textContent = `温度 ${point.cpuTempCelsius.toFixed(1)}°C`;
    } else {
      tempReading.hidden = true;
      tempReading.textContent = '温度 --';
    }
  }
}

function drawHistoryChart(points: HistoryPoint[]) {
  const canvas = document.querySelector('#history-chart') as HTMLCanvasElement | null;
  const tempLegend = document.querySelector('#temp-legend') as HTMLElement | null;
  if (!canvas) return;

  const dpr = window.devicePixelRatio || 1;
  const cssWidth = canvas.clientWidth || 280;
  const cssHeight = canvas.clientHeight || 96;
  const width = Math.round(cssWidth * dpr);
  const height = Math.round(cssHeight * dpr);
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }

  const rawCtx = canvas.getContext('2d');
  if (!rawCtx) return;
  const ctx: CanvasRenderingContext2D = rawCtx;

  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, cssWidth, cssHeight);

  ctx.strokeStyle = 'rgba(148, 163, 184, 0.12)';
  ctx.lineWidth = 1;
  for (let i = 1; i <= 3; i++) {
    const y = (cssHeight / 4) * i;
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(cssWidth, y);
    ctx.stroke();
  }

  const drawPoints = downsampleForDraw(points);
  if (drawPoints.length < 2) {
    ctx.fillStyle = '#9aa6b2';
    ctx.font = '12px Segoe UI, Microsoft YaHei, sans-serif';
    ctx.fillText('采集中…', 10, cssHeight / 2 + 4);
    if (tempLegend) tempLegend.hidden = true;
    updateChartReadings(null, false);
    return;
  }

  const padX = 2;
  const padY = 4;
  const plotW = cssWidth - padX * 2;
  const plotH = cssHeight - padY * 2;
  const n = drawPoints.length;
  const stepX = plotW / Math.max(n - 1, 1);

  const hasTemp = drawPoints.some((p) => p.cpuTempCelsius != null);
  if (tempLegend) tempLegend.hidden = !hasTemp;

  const hint = document.querySelector('#chart-hint');
  if (hint) {
    const base = `最近约 ${normalizeHistoryRangeMinutes(historyRangeMinutes)} 分钟`;
    hint.textContent = hasTemp ? `${base} · 温度独立轴` : base;
  }

  const tempValues = drawPoints.map((p) => p.cpuTempCelsius).filter((v): v is number => v != null);
  const tempMin = hasTemp ? Math.min(...tempValues, 30) : 0;
  const tempMax = hasTemp ? Math.max(...tempValues, 90) : 100;
  const tempRange = Math.max(tempMax - tempMin, 1);

  const yPercent = (v: number): number => padY + plotH * (1 - Math.max(0, Math.min(100, v)) / 100);

  const yTemp = (v: number): number => padY + plotH * (1 - (v - tempMin) / tempRange);

  const drawSeries = (values: Array<number | null>, color: string, mapY: (v: number) => number) => {
    const segments: Array<Array<{ x: number; y: number }>> = [];
    let current: Array<{ x: number; y: number }> = [];
    for (let i = 0; i < values.length; i++) {
      const v = values[i];
      if (v == null) {
        if (current.length) {
          segments.push(current);
          current = [];
        }
        continue;
      }
      current.push({ x: padX + i * stepX, y: mapY(v) });
    }
    if (current.length) segments.push(current);

    const baseline = padY + plotH;
    for (const segment of segments) {
      if (segment.length < 2) continue;
      ctx.beginPath();
      ctx.moveTo(segment[0].x, baseline);
      for (const point of segment) {
        ctx.lineTo(point.x, point.y);
      }
      ctx.lineTo(segment[segment.length - 1].x, baseline);
      ctx.closePath();
      ctx.fillStyle = hexToRgba(color, 0.15);
      ctx.fill();
    }

    ctx.strokeStyle = color;
    ctx.lineWidth = 1.85;
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    for (const segment of segments) {
      if (!segment.length) continue;
      ctx.beginPath();
      ctx.moveTo(segment[0].x, segment[0].y);
      for (let i = 1; i < segment.length; i++) {
        ctx.lineTo(segment[i].x, segment[i].y);
      }
      ctx.stroke();
    }

    for (let i = values.length - 1; i >= 0; i--) {
      const v = values[i];
      if (v == null) continue;
      const x = padX + i * stepX;
      const y = mapY(v);
      ctx.beginPath();
      ctx.arc(x, y, 2.5, 0, Math.PI * 2);
      ctx.fillStyle = color;
      ctx.fill();
      break;
    }
  };

  drawSeries(
    drawPoints.map((p) => p.cpuPercent),
    '#38bdf8',
    yPercent,
  );
  drawSeries(
    drawPoints.map((p) => p.memoryPercent),
    '#a78bfa',
    yPercent,
  );
  if (hasTemp) {
    drawSeries(
      drawPoints.map((p) => p.cpuTempCelsius),
      '#fb923c',
      yTemp,
    );
  }

  updateChartReadings(drawPoints[drawPoints.length - 1] ?? null, hasTemp);
}

async function refreshMetrics() {
  try {
    const metrics = await invoke<Metrics>('get_metrics');
    const alert = metrics.alert;

    const cpuValue = document.querySelector('#cpu-value');
    const cpuBar = document.querySelector('#cpu-bar') as HTMLElement | null;
    const cpuTemp = document.querySelector('#cpu-temp');
    const memValue = document.querySelector('#mem-value');
    const memBar = document.querySelector('#mem-bar') as HTMLElement | null;
    const memDetail = document.querySelector('#mem-detail');
    const netDown = document.querySelector('#net-down');
    const netUp = document.querySelector('#net-up');
    const diskList = document.querySelector('#disk-list');
    const updatedAt = document.querySelector('#updated-at');
    const alertBanner = document.querySelector('#alert-banner') as HTMLElement | null;

    if (cpuValue) cpuValue.textContent = `${metrics.cpuPercent.toFixed(1)}%`;
    setBar(cpuBar, metrics.cpuPercent, alert.cpu);
    setCardAlert('#cpu-card', alert.cpu || alert.temperature);
    if (cpuTemp) {
      if (metrics.cpuTempCelsius == null) {
        cpuTemp.textContent = '温度：暂不可用';
      } else {
        const sourceHint =
          metrics.tempSource === 'lhm'
            ? ' · LHM'
            : metrics.tempSource === 'wmi'
              ? ' · ACPI'
              : metrics.tempSource === 'sysinfo'
                ? ' · sysinfo'
                : '';
        cpuTemp.textContent = `温度：${metrics.cpuTempCelsius.toFixed(1)} °C${sourceHint}`;
      }
      cpuTemp.classList.toggle('alert-text', alert.temperature);
    }

    if (memValue) memValue.textContent = `${metrics.memoryPercent.toFixed(1)}%`;
    setBar(memBar, metrics.memoryPercent, alert.memory);
    setCardAlert('#mem-card', alert.memory);
    if (memDetail) {
      memDetail.textContent = `${formatBytes(metrics.memoryUsedBytes)} / ${formatBytes(metrics.memoryTotalBytes)}`;
    }

    if (netDown) netDown.textContent = formatSpeed(metrics.netDownBps);
    if (netUp) netUp.textContent = formatSpeed(metrics.netUpBps);

    if (diskList) {
      renderDiskList(diskList as HTMLElement, metrics.disks);
    }

    if (updatedAt) {
      const date = new Date(Number(metrics.sampledAtMs));
      updatedAt.textContent = date.toLocaleTimeString();
    }

    if (alertBanner) {
      const alertText = alertBanner.querySelector('span');
      if (alert.active) {
        if (alertText) alertText.textContent = `告警：${alert.messages.join(' · ')}`;
        alertBanner.hidden = false;
        alertBanner.classList.add('show');
      } else {
        alertBanner.classList.remove('show');
        alertBanner.hidden = true;
        if (alertText) alertText.textContent = '';
      }
    }

    // get_metrics 已写入历史，再拉取快照绘制
    const history = await invoke<HistoryPoint[]>('get_metrics_history');
    drawHistoryChart(history);

    if (currentTab === 'settings') {
      await refreshTempSourceStatus();
    }
  } catch (error) {
    console.error('获取监测数据失败', error);
  }
}

function showSettingsMessage(text: string, kind: 'ok' | 'error' | 'muted' = 'muted') {
  const el = document.querySelector('#settings-message') as HTMLElement | null;
  if (!el) return;
  el.hidden = !text;
  el.textContent = text;
  el.classList.remove('ok', 'error', 'muted');
  el.classList.add(kind === 'muted' ? 'muted' : kind);

  if (settingsMessageTimer) window.clearTimeout(settingsMessageTimer);
  if (text && kind === 'ok') {
    settingsMessageTimer = window.setTimeout(() => {
      el.hidden = true;
      el.textContent = '';
    }, 2500);
  }
}

function normalizeOverlayStyle(
  value: string | null | undefined,
): 'capsule' | 'vertical' | 'numeric' {
  if (value === 'vertical' || value === 'numeric' || value === 'capsule') return value;
  return 'capsule';
}

function fillSettingsForm(
  thresholds: AlertThresholds,
  notificationEnabled?: boolean,
  rangeMinutes?: number,
  preciseTempEnabled?: boolean,
  lhmBaseUrl?: string,
  overlayEnabled?: boolean,
  autostartEnabled?: boolean,
  overlayAutoHide?: boolean,
  overlayStyle?: 'capsule' | 'vertical' | 'numeric',
) {
  const cpu = document.querySelector('#setting-cpu') as HTMLInputElement | null;
  const memory = document.querySelector('#setting-memory') as HTMLInputElement | null;
  const temp = document.querySelector('#setting-temp') as HTMLInputElement | null;
  const cooldown = document.querySelector('#setting-cooldown') as HTMLInputElement | null;
  const notification = document.querySelector('#setting-notification') as HTMLInputElement | null;
  const overlay = document.querySelector('#setting-overlay') as HTMLInputElement | null;
  const autostart = document.querySelector('#setting-autostart') as HTMLInputElement | null;
  const overlayAutoHideEl = document.querySelector(
    '#setting-overlay-auto-hide',
  ) as HTMLInputElement | null;
  const overlayStyleEl = document.querySelector(
    '#setting-overlay-style',
  ) as HTMLSelectElement | null;
  const historyRange = document.querySelector('#setting-history-range') as HTMLSelectElement | null;
  const preciseTemp = document.querySelector('#setting-precise-temp') as HTMLInputElement | null;
  const lhmUrl = document.querySelector('#setting-lhm-url') as HTMLInputElement | null;

  if (cpu) cpu.value = String(Math.round(thresholds.cpuPercent));
  if (memory) memory.value = String(Math.round(thresholds.memoryPercent));
  if (temp) temp.value = String(Math.round(thresholds.cpuTempCelsius));
  if (cooldown) cooldown.value = String(thresholds.cooldownSecs);
  if (notification && notificationEnabled !== undefined) {
    notification.checked = notificationEnabled;
  }
  if (overlay && overlayEnabled !== undefined) {
    overlay.checked = overlayEnabled;
  }
  if (autostart && autostartEnabled !== undefined) {
    autostart.checked = autostartEnabled;
  }
  if (overlayAutoHideEl && overlayAutoHide !== undefined) {
    overlayAutoHideEl.checked = overlayAutoHide;
  }
  if (overlayStyleEl && overlayStyle !== undefined) {
    overlayStyleEl.value = normalizeOverlayStyle(overlayStyle);
  }
  if (historyRange && rangeMinutes !== undefined) {
    historyRange.value = String(normalizeHistoryRangeMinutes(rangeMinutes));
  }
  if (preciseTemp && preciseTempEnabled !== undefined) {
    preciseTemp.checked = preciseTempEnabled;
  }
  if (lhmUrl && lhmBaseUrl !== undefined) {
    lhmUrl.value = lhmBaseUrl || DEFAULT_LHM_BASE_URL;
  }
}

function readNotificationEnabled(): boolean {
  const notification = document.querySelector('#setting-notification') as HTMLInputElement | null;
  return notification?.checked ?? true;
}

function readOverlayEnabled(): boolean {
  const overlay = document.querySelector('#setting-overlay') as HTMLInputElement | null;
  return overlay?.checked ?? false;
}

function readAutostartEnabled(): boolean {
  const autostart = document.querySelector('#setting-autostart') as HTMLInputElement | null;
  return autostart?.checked ?? false;
}

function readOverlayAutoHide(): boolean {
  const el = document.querySelector('#setting-overlay-auto-hide') as HTMLInputElement | null;
  return el?.checked ?? false;
}

function readOverlayStyle(): 'capsule' | 'vertical' | 'numeric' {
  const el = document.querySelector('#setting-overlay-style') as HTMLSelectElement | null;
  return normalizeOverlayStyle(el?.value);
}

function readHistoryRangeMinutes(): number {
  const historyRange = document.querySelector('#setting-history-range') as HTMLSelectElement | null;
  const value = Number(historyRange?.value ?? 1);
  return normalizeHistoryRangeMinutes(value);
}

function readPreciseTempEnabled(): boolean {
  const el = document.querySelector('#setting-precise-temp') as HTMLInputElement | null;
  return el?.checked ?? false;
}

function readLhmBaseUrl(): string {
  const el = document.querySelector('#setting-lhm-url') as HTMLInputElement | null;
  const value = (el?.value ?? '').trim();
  return value || DEFAULT_LHM_BASE_URL;
}

async function refreshTempSourceStatus() {
  const el = document.querySelector('#temp-source-status') as HTMLElement | null;
  if (!el) return;
  try {
    const status = await invoke<TempSourceStatus>('get_temp_source_status');
    el.textContent = `状态：${status.message}`;
  } catch (error) {
    console.error('获取温度来源状态失败', error);
    el.textContent = '状态：获取失败';
  }
}

function readSettingsForm(): AlertThresholds | null {
  const cpu = document.querySelector('#setting-cpu') as HTMLInputElement | null;
  const memory = document.querySelector('#setting-memory') as HTMLInputElement | null;
  const temp = document.querySelector('#setting-temp') as HTMLInputElement | null;
  const cooldown = document.querySelector('#setting-cooldown') as HTMLInputElement | null;
  if (!cpu || !memory || !temp || !cooldown) return null;

  const thresholds: AlertThresholds = {
    cpuPercent: Number(cpu.value),
    memoryPercent: Number(memory.value),
    cpuTempCelsius: Number(temp.value),
    cooldownSecs: Number(cooldown.value),
  };

  if (
    !Number.isFinite(thresholds.cpuPercent) ||
    !Number.isFinite(thresholds.memoryPercent) ||
    !Number.isFinite(thresholds.cpuTempCelsius) ||
    !Number.isFinite(thresholds.cooldownSecs)
  ) {
    return null;
  }

  return thresholds;
}

async function loadSettingsForm() {
  const cfg = await invoke<AppConfig>('get_app_config');
  historyRangeMinutes = normalizeHistoryRangeMinutes(cfg.historyRangeMinutes);
  fillSettingsForm(
    cfg.alert,
    cfg.notificationEnabled,
    historyRangeMinutes,
    cfg.preciseTempEnabled,
    cfg.lhmBaseUrl,
    cfg.overlayEnabled,
    cfg.autostartEnabled,
    cfg.overlayAutoHide,
    normalizeOverlayStyle(cfg.overlayStyle),
  );
  updateChartHint(historyRangeMinutes);
  await refreshTempSourceStatus();
}

async function applyHistoryRange(minutes: number): Promise<number> {
  const saved = await invoke<number>('set_history_range_minutes', { minutes });
  historyRangeMinutes = normalizeHistoryRangeMinutes(saved);
  updateChartHint(historyRangeMinutes);
  return historyRangeMinutes;
}

async function setSettingsOpen(open: boolean) {
  await invoke('set_settings_open', { open });
}

async function switchTab(tab: TabId) {
  if (currentTab === tab) {
    if (tab === 'settings') {
      await setSettingsOpen(true);
    }
    return;
  }

  currentTab = tab;

  const tabs = document.querySelectorAll<HTMLButtonElement>('.tab');
  tabs.forEach((btn) => {
    const active = btn.dataset.tab === tab;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-selected', active ? 'true' : 'false');
  });

  const monitorView = document.querySelector('#view-monitor') as HTMLElement | null;
  const settingsView = document.querySelector('#view-settings') as HTMLElement | null;
  if (monitorView) monitorView.hidden = tab !== 'monitor';
  if (settingsView) settingsView.hidden = tab !== 'settings';

  if (tab === 'settings') {
    await setSettingsOpen(true);
    try {
      await loadSettingsForm();
      showSettingsMessage('');
    } catch (error) {
      console.error('加载告警设置失败', error);
      showSettingsMessage('加载设置失败', 'error');
    }
  } else {
    await setSettingsOpen(false);
  }
}

function bindSettingsUi() {
  const tabs = document.querySelectorAll<HTMLButtonElement>('.tab');
  tabs.forEach((btn) => {
    btn.addEventListener('click', () => {
      const tab = btn.dataset.tab;
      if (tab === 'monitor' || tab === 'settings') {
        void switchTab(tab);
      }
    });
  });

  const tablist = document.querySelector('.tabs');
  tablist?.addEventListener('keydown', (event) => {
    if (!(event instanceof KeyboardEvent)) return;
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
    event.preventDefault();
    const nextTab: TabId = currentTab === 'monitor' ? 'settings' : 'monitor';
    void switchTab(nextTab).then(() => {
      const target = document.querySelector<HTMLButtonElement>(`.tab[data-tab="${nextTab}"]`);
      target?.focus();
    });
  });

  document.querySelectorAll<HTMLButtonElement>('.hint-toggle').forEach((btn) => {
    btn.addEventListener('click', () => {
      const card = btn.closest('.settings-card');
      if (!card) return;
      const expanded = card.classList.toggle('expanded');
      btn.textContent = expanded ? '收起' : '了解更多';
    });
  });

  const form = document.querySelector('#settings-form') as HTMLFormElement | null;
  form?.addEventListener('submit', async (event) => {
    event.preventDefault();
    const thresholds = readSettingsForm();
    if (!thresholds) {
      showSettingsMessage('请填写有效数值', 'error');
      return;
    }

    const saveBtn = document.querySelector('#btn-save-settings') as HTMLButtonElement | null;
    const resetBtn = document.querySelector('#btn-reset-settings') as HTMLButtonElement | null;
    if (saveBtn) saveBtn.disabled = true;
    if (resetBtn) resetBtn.disabled = true;

    try {
      // 先校验并写入 LHM URL，失败则整次保存中止
      const lhmBaseUrl = await invoke<string>('set_lhm_base_url', {
        url: readLhmBaseUrl(),
      });
      const preciseTempEnabled = await invoke<boolean>('set_precise_temp_enabled', {
        enabled: readPreciseTempEnabled(),
      });
      const saved = await invoke<AlertThresholds>('set_alert_thresholds', { thresholds });
      const notificationEnabled = await invoke<boolean>('set_notification_enabled', {
        enabled: readNotificationEnabled(),
      });
      const overlayEnabled = await invoke<boolean>('set_overlay_enabled', {
        enabled: readOverlayEnabled(),
      });
      const autostartEnabled = await invoke<boolean>('set_autostart_enabled', {
        enabled: readAutostartEnabled(),
      });
      const overlayAutoHide = await invoke<boolean>('set_overlay_auto_hide', {
        enabled: readOverlayAutoHide(),
      });
      const overlayStyle = await invoke<string>('set_overlay_style', {
        style: readOverlayStyle(),
      });
      const rangeMinutes = await applyHistoryRange(readHistoryRangeMinutes());
      fillSettingsForm(
        saved,
        notificationEnabled,
        rangeMinutes,
        preciseTempEnabled,
        lhmBaseUrl,
        overlayEnabled,
        autostartEnabled,
        overlayAutoHide,
        normalizeOverlayStyle(overlayStyle),
      );
      await refreshTempSourceStatus();
      showSettingsMessage('已保存', 'ok');
    } catch (error) {
      console.error('保存告警设置失败', error);
      const message = typeof error === 'string' ? error : '保存失败';
      showSettingsMessage(message, 'error');
    } finally {
      if (saveBtn) saveBtn.disabled = false;
      if (resetBtn) resetBtn.disabled = false;
    }
  });

  const resetBtn = document.querySelector('#btn-reset-settings') as HTMLButtonElement | null;
  resetBtn?.addEventListener('click', async () => {
    const saveBtn = document.querySelector('#btn-save-settings') as HTMLButtonElement | null;
    if (saveBtn) saveBtn.disabled = true;
    if (resetBtn) resetBtn.disabled = true;

    try {
      const restored = await invoke<AlertThresholds>('reset_alert_thresholds');
      const notificationEnabled = await invoke<boolean>('set_notification_enabled', {
        enabled: true,
      });
      const overlayEnabled = await invoke<boolean>('set_overlay_enabled', {
        enabled: false,
      });
      const autostartEnabled = await invoke<boolean>('set_autostart_enabled', {
        enabled: false,
      });
      const overlayAutoHide = await invoke<boolean>('set_overlay_auto_hide', {
        enabled: false,
      });
      const overlayStyle = await invoke<string>('set_overlay_style', {
        style: 'capsule',
      });
      const rangeMinutes = await applyHistoryRange(1);
      const preciseTempEnabled = await invoke<boolean>('set_precise_temp_enabled', {
        enabled: false,
      });
      const lhmBaseUrl = await invoke<string>('set_lhm_base_url', {
        url: DEFAULT_LHM_BASE_URL,
      });
      fillSettingsForm(
        restored,
        notificationEnabled,
        rangeMinutes,
        preciseTempEnabled,
        lhmBaseUrl,
        overlayEnabled,
        autostartEnabled,
        overlayAutoHide,
        normalizeOverlayStyle(overlayStyle),
      );
      await refreshTempSourceStatus();
      showSettingsMessage('已恢复默认', 'ok');
    } catch (error) {
      console.error('恢复默认失败', error);
      const message = typeof error === 'string' ? error : '恢复默认失败';
      showSettingsMessage(message, 'error');
    } finally {
      if (saveBtn) saveBtn.disabled = false;
      if (resetBtn) resetBtn.disabled = false;
    }
  });
}

async function bootstrapHistoryHint() {
  try {
    const cfg = await invoke<AppConfig>('get_app_config');
    historyRangeMinutes = normalizeHistoryRangeMinutes(cfg.historyRangeMinutes);
    updateChartHint(historyRangeMinutes);
  } catch (error) {
    console.error('加载历史时长失败', error);
    updateChartHint(1);
  }
}

window.addEventListener('contextmenu', (event) => {
  event.preventDefault();
});

window.addEventListener('DOMContentLoaded', () => {
  bindSettingsUi();
  void bootstrapHistoryHint();
  refreshMetrics();
  window.setInterval(refreshMetrics, 1000);

  void listen('open-settings', () => {
    void switchTab('settings');
  });

  void listen<boolean>('overlay-enabled-changed', (event) => {
    const overlay = document.querySelector('#setting-overlay') as HTMLInputElement | null;
    if (overlay) overlay.checked = event.payload;
  });
});
