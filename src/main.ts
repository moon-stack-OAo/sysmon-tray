import { initializeDiagnostics } from './diagnostics';
import {
  chartPoints,
  chartX,
  mergeHistory,
  type HistoryPoint,
  type HistoryBatch,
} from './history-chart';
import { getVersion } from '@tauri-apps/api/app';
import { initializeCodex } from './codex';
import { initializeHardware, isHardwareVisible, hardwareRevision } from './hardware';
import './codex.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { isPermissionGranted, requestPermission } from '@tauri-apps/plugin-notification';

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
  diskPercent: number;
  gpuPercent: number;
  cooldownSecs: number;
}

type NetworkSelection = { mode: 'auto' | 'all' } | { mode: 'interface'; interface: string };

interface AppConfig {
  networkSelection: NetworkSelection;
  alert: AlertThresholds;
  notificationEnabled: boolean;
  historyRangeMinutes: number;
  preciseTempEnabled: boolean;
  lhmBaseUrl: string;
  overlayEnabled: boolean;
  codexEnabled: boolean;
  autostartEnabled: boolean;
  overlayAutoHide: boolean;
  overlayClickThrough: boolean;
  overlayStyle: 'capsule' | 'vertical' | 'numeric';
  processTopEnabled: boolean;
  alertSustainSecs: number;
  quietHoursEnabled: boolean;
  quietStartMinute: number;
  quietEndMinute: number;
  hotkeyPanel: string;
  hotkeyOverlay: string;
  hotkeyClickThrough: string;
  overlayMetrics: string[];
  overlayX?: number | null;
  overlayY?: number | null;
}

interface ProcessTopEntry {
  name: string;
  cpuPercent: number;
  memoryBytes: number;
  count: number;
}

interface ProcessTopSnapshot {
  enabled: boolean;
  byCpu: ProcessTopEntry[];
  byMemory: ProcessTopEntry[];
  sampledAtMs: number;
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
  disk: boolean;
  gpu: boolean;
  active: boolean;
  messages: string[];
  thresholds: AlertThresholds;
}

interface GpuMetrics {
  name: string;
  loadPercent: number | null;
  memoryUsedBytes: number | null;
  memoryTotalBytes: number | null;
  memoryPercent: number | null;
  tempCelsius: number | null;
}

interface FanMetrics {
  name: string;
  rpm: number;
  kind: string;
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
  netInterface: string | null;
  netLink: string;
  disks: DiskStat[];
  cpuTempCelsius: number | null;
  gpu: GpuMetrics | null;
  fans: FanMetrics[];
  sampledAtMs: number;
  alert: AlertStatus;
  tempSource: string;
  stale: boolean;
  error: string | null;
}

type TabId = 'monitor' | 'settings';

const ALLOWED_HISTORY_RANGE_MINUTES = [1, 5, 15, 60] as const;

let currentTab: TabId = 'monitor';
let settingsMessageTimer: number | undefined;
let historyRangeMinutes = 1;
let processTopEnabled = false;
let lastProcessTopFetchAt = 0;
const PROCESS_TOP_UI_INTERVAL_MS = 4000;

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

function renderDiskList(
  diskList: HTMLElement,
  disks: DiskStat[],
  diskAlert = false,
  diskThreshold = 90,
) {
  if (!disks.length) {
    diskList.replaceChildren();
    diskList.textContent = '未检测到磁盘';
    return;
  }

  if (diskList.childElementCount === 0) {
    diskList.textContent = '';
  }

  const items = disks;
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
    const used = disk.availableBytes > disk.totalBytes ? 0 : disk.totalBytes - disk.availableBytes;
    const over = diskAlert && disk.usedPercent >= diskThreshold;

    if (titleEl) titleEl.textContent = disk.mountPoint || disk.name;
    setBar(barEl, disk.usedPercent, over);
    item.classList.toggle('alert', over);
    if (subEl) {
      subEl.textContent = `${formatBytes(used)} / ${formatBytes(disk.totalBytes)} · ${disk.usedPercent.toFixed(1)}%`;
    }
  }
}

function fanKindLabel(kind: string): string {
  switch (kind) {
    case 'cpu':
      return 'CPU';
    case 'gpu':
      return 'GPU';
    case 'chassis':
      return '机箱';
    case 'pump':
      return '水泵';
    default:
      return '风扇';
  }
}

function formatFanSummary(fans: FanMetrics[]): string {
  if (!fans.length) return '--';
  const primary = fans[0];
  return `${fanKindLabel(primary.kind)} ${primary.rpm}`;
}

function renderFanList(fanList: HTMLElement, fans: FanMetrics[], tempSource: string) {
  if (!fans.length) {
    fanList.textContent = tempSource === 'lhm' ? '暂不可用' : '需启用 LibreHardwareMonitor';
    return;
  }

  fanList.textContent = '';
  for (const fan of fans.slice(0, 4)) {
    const item = document.createElement('div');
    item.className = 'fan-item';
    const name = document.createElement('span');
    name.className = 'fan-name';
    name.textContent = fan.name;
    const rpm = document.createElement('span');
    rpm.className = 'fan-rpm';
    rpm.textContent = `${fan.rpm} RPM`;
    item.append(name, rpm);
    fanList.appendChild(item);
  }
}

function setCardAlert(selector: string, active: boolean) {
  const card = document.querySelector(selector);
  if (!card) return;
  card.classList.toggle('alert', active);
}

function renderProcessList(
  listEl: HTMLElement,
  entries: ProcessTopEntry[],
  mode: 'cpu' | 'memory',
) {
  if (!entries.length) {
    listEl.replaceChildren();
    listEl.textContent = '暂无数据';
    return;
  }

  if (listEl.childElementCount === 0) {
    listEl.textContent = '';
  }

  const existing = Array.from(listEl.querySelectorAll<HTMLElement>('.process-item'));
  while (existing.length > entries.length) {
    existing.pop()?.remove();
  }

  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    let item = existing[i];
    if (!item) {
      item = document.createElement('div');
      item.className = 'process-item';
      const name = document.createElement('div');
      name.className = 'process-name';
      const meta = document.createElement('div');
      meta.className = 'process-meta';
      item.append(name, meta);
      listEl.appendChild(item);
    }

    const nameEl = item.querySelector('.process-name');
    const metaEl = item.querySelector('.process-meta');
    const suffix = entry.count > 1 ? ` ×${entry.count}` : '';
    if (nameEl) nameEl.textContent = `${entry.name}${suffix}`;
    if (metaEl) {
      metaEl.textContent =
        mode === 'cpu' ? `${entry.cpuPercent.toFixed(1)}%` : formatBytes(entry.memoryBytes);
    }
  }
}

function setProcessTopVisible(enabled: boolean) {
  const card = document.querySelector('#process-top-card') as HTMLElement | null;
  if (card) card.hidden = !enabled;
}

async function refreshProcessTop(force = false) {
  if (!isHardwareVisible()) return;
  const revision = hardwareRevision();
  const card = document.querySelector('#process-top-card') as HTMLElement | null;
  if (!processTopEnabled) {
    setProcessTopVisible(false);
    return;
  }
  setProcessTopVisible(true);

  const now = Date.now();
  if (!force && now - lastProcessTopFetchAt < PROCESS_TOP_UI_INTERVAL_MS) {
    return;
  }

  try {
    const snap = await invoke<ProcessTopSnapshot>('get_process_top');
    if (!isHardwareVisible() || revision !== hardwareRevision()) return;
    lastProcessTopFetchAt = now;
    processTopEnabled = snap.enabled;
    setProcessTopVisible(snap.enabled);
    if (!snap.enabled) return;

    const cpuList = document.querySelector('#process-top-cpu') as HTMLElement | null;
    const memList = document.querySelector('#process-top-mem') as HTMLElement | null;
    if (cpuList) renderProcessList(cpuList, snap.byCpu, 'cpu');
    if (memList) renderProcessList(memList, snap.byMemory, 'memory');
    if (card) card.hidden = false;
  } catch (error) {
    console.error('获取进程 Top 失败', error);
  }
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

function hexToRgba(hex: string, alpha: number): string {
  const raw = hex.replace('#', '');
  const r = Number.parseInt(raw.slice(0, 2), 16);
  const g = Number.parseInt(raw.slice(2, 4), 16);
  const b = Number.parseInt(raw.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function updateChartReadings(
  point: HistoryPoint | null,
  hasTemp: boolean,
  hasFan: boolean,
  hasGpu: boolean,
) {
  const cpuReading = document.querySelector('#chart-cpu-reading');
  const memReading = document.querySelector('#chart-mem-reading');
  const gpuReading = document.querySelector('#chart-gpu-reading') as HTMLElement | null;
  const tempReading = document.querySelector('#chart-temp-reading') as HTMLElement | null;
  const fanReading = document.querySelector('#chart-fan-reading') as HTMLElement | null;

  if (cpuReading) {
    cpuReading.textContent = point != null ? `CPU ${point.cpuPercent.toFixed(1)}%` : 'CPU --';
  }
  if (memReading) {
    memReading.textContent = point != null ? `内存 ${point.memoryPercent.toFixed(1)}%` : '内存 --';
  }
  if (gpuReading) {
    if (hasGpu && point?.gpuLoadPercent != null) {
      gpuReading.hidden = false;
      gpuReading.textContent = `GPU ${point.gpuLoadPercent.toFixed(1)}%`;
    } else {
      gpuReading.hidden = true;
      gpuReading.textContent = 'GPU --';
    }
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
  if (fanReading) {
    if (hasFan && point?.fanRpm != null) {
      fanReading.hidden = false;
      const label = point.fanName?.trim() || '风扇';
      fanReading.textContent = `${label} ${point.fanRpm}`;
    } else {
      fanReading.hidden = true;
      fanReading.textContent = '风扇 --';
    }
  }
}

function drawHistoryChart(points: HistoryPoint[]) {
  const canvas = document.querySelector('#history-chart') as HTMLCanvasElement | null;
  const gpuLegend = document.querySelector('#gpu-legend') as HTMLElement | null;
  const tempLegend = document.querySelector('#temp-legend') as HTMLElement | null;
  const fanLegend = document.querySelector('#fan-legend') as HTMLElement | null;
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

  ctx.strokeStyle = 'rgba(255, 255, 255, 0.04)';
  ctx.lineWidth = 1;
  for (let i = 1; i <= 3; i++) {
    const y = (cssHeight / 4) * i;
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(cssWidth, y);
    ctx.stroke();
  }

  const endTime = Date.now();
  const rangeMs = historyRangeMinutes * 60000;
  const drawPoints = chartPoints(points, endTime, rangeMs, cssWidth);
  if (drawPoints.length < 2) {
    ctx.fillStyle = '#9c9c9d';
    ctx.font = '12px Inter, Segoe UI, Microsoft YaHei, sans-serif';
    ctx.fillText('采集中…', 10, cssHeight / 2 + 4);
    if (gpuLegend) gpuLegend.hidden = true;
    if (tempLegend) tempLegend.hidden = true;
    if (fanLegend) fanLegend.hidden = true;
    updateChartReadings(null, false, false, false);
    return;
  }

  const padX = 2;
  const padY = 4;
  const plotW = cssWidth - padX * 2;
  const plotH = cssHeight - padY * 2;
  const xAt = (i: number) => padX + chartX(drawPoints[i].timestampMs, endTime, rangeMs, plotW);

  const hasGpu = drawPoints.some((p) => p.gpuLoadPercent != null);
  const hasTemp = drawPoints.some((p) => p.cpuTempCelsius != null);
  const hasFan = drawPoints.some((p) => p.fanRpm != null);
  if (gpuLegend) gpuLegend.hidden = !hasGpu;
  if (tempLegend) tempLegend.hidden = !hasTemp;
  if (fanLegend) {
    fanLegend.hidden = !hasFan;
    if (hasFan) {
      const latestName = [...drawPoints]
        .reverse()
        .find((p) => p.fanName != null && p.fanName.trim())?.fanName;
      fanLegend.textContent = latestName?.trim() || '风扇';
    }
  }

  const hint = document.querySelector('#chart-hint');
  if (hint) {
    const base = `最近约 ${normalizeHistoryRangeMinutes(historyRangeMinutes)} 分钟`;
    const axes: string[] = [];
    if (hasTemp) axes.push('温度');
    if (hasFan) axes.push('转速');
    hint.textContent = axes.length ? `${base} · ${axes.join('/')}独立轴` : base;
  }

  const tempValues = drawPoints.map((p) => p.cpuTempCelsius).filter((v): v is number => v != null);
  const tempMin = hasTemp ? Math.min(...tempValues, 30) : 0;
  const tempMax = hasTemp ? Math.max(...tempValues, 90) : 100;
  const tempRange = Math.max(tempMax - tempMin, 1);

  const fanValues = drawPoints.map((p) => p.fanRpm).filter((v): v is number => v != null);
  const fanMin = hasFan ? Math.min(...fanValues, 0) : 0;
  const fanMax = hasFan ? Math.max(...fanValues, 1000) : 1000;
  const fanRange = Math.max(fanMax - fanMin, 1);

  const yPercent = (v: number): number => padY + plotH * (1 - Math.max(0, Math.min(100, v)) / 100);

  const yTemp = (v: number): number => padY + plotH * (1 - (v - tempMin) / tempRange);

  const yFan = (v: number): number => padY + plotH * (1 - (v - fanMin) / fanRange);

  const drawSeries = (values: Array<number | null>, color: string, mapY: (v: number) => number) => {
    const segments: Array<Array<{ x: number; y: number }>> = [];
    let current: Array<{ x: number; y: number }> = [];
    for (let i = 0; i < values.length; i++) {
      const v = values[i];
      if (v == null || !Number.isFinite(v) || drawPoints[i].breakBefore) {
        if (current.length) {
          segments.push(current);
          current = [];
        }
        if (v == null || !Number.isFinite(v)) continue;
      }
      current.push({ x: xAt(i), y: mapY(v) });
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
      const x = xAt(i);
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
    '#55b3ff',
    yPercent,
  );
  drawSeries(
    drawPoints.map((p) => p.memoryPercent),
    '#5fc992',
    yPercent,
  );
  if (hasGpu) {
    drawSeries(
      drawPoints.map((p) => p.gpuLoadPercent),
      '#3dd68c',
      yPercent,
    );
  }
  if (hasTemp) {
    drawSeries(
      drawPoints.map((p) => p.cpuTempCelsius),
      '#ff9a4a',
      yTemp,
    );
  }
  if (hasFan) {
    drawSeries(
      drawPoints.map((p) => p.fanRpm),
      '#c084fc',
      yFan,
    );
  }

  updateChartReadings(drawPoints[drawPoints.length - 1] ?? null, hasTemp, hasFan, hasGpu);
}

let historyPoints: HistoryPoint[] = [];
let historyCursor: number | null = null;
let historyBusy = false;
async function refreshHistoryChart() {
  if (!isHardwareVisible() || currentTab !== 'monitor' || historyBusy) return;
  const revision = hardwareRevision();
  historyBusy = true;
  try {
    const batch = await invoke<HistoryBatch>('get_metrics_history_batch', {
      cursor: historyCursor,
    });
    if (!isHardwareVisible() || revision !== hardwareRevision() || currentTab !== 'monitor') return;
    const changed =
      batch.reset ||
      batch.points.length > 0 ||
      historyPoints.some((p) => p.sequence < batch.oldest) ||
      historyPoints.length > batch.capacity;
    historyPoints = mergeHistory(historyPoints, batch);
    historyCursor = batch.cursor;
    if (changed) drawHistoryChart(historyPoints);
  } catch (error) {
    console.error('获取历史增量失败', error);
  } finally {
    historyBusy = false;
  }
}

async function refreshMetrics(metrics: Metrics) {
  if (!isHardwareVisible()) return;
  try {
    if (metrics.stale) {
      const updated = document.querySelector<HTMLElement>('#updated-at');
      if (updated) {
        updated.textContent = '数据过期';
        updated.title = metrics.error ?? '等待后台采样恢复';
      }
      return;
    }
    const alert = metrics.alert;

    const cpuValue = document.querySelector('#cpu-value');
    const cpuBar = document.querySelector('#cpu-bar') as HTMLElement | null;
    const cpuTemp = document.querySelector('#cpu-temp');
    const memValue = document.querySelector('#mem-value');
    const memBar = document.querySelector('#mem-bar') as HTMLElement | null;
    const memDetail = document.querySelector('#mem-detail');
    const swapCard = document.querySelector('#swap-card') as HTMLElement | null;
    const swapValue = document.querySelector('#swap-value');
    const swapBar = document.querySelector('#swap-bar') as HTMLElement | null;
    const swapDetail = document.querySelector('#swap-detail');
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

    if (swapCard) {
      if (metrics.swapTotalBytes === 0) {
        swapCard.hidden = true;
      } else {
        swapCard.hidden = false;
        const swapPercent = (metrics.swapUsedBytes / metrics.swapTotalBytes) * 100;
        if (swapValue) swapValue.textContent = `${swapPercent.toFixed(1)}%`;
        setBar(swapBar, swapPercent);
        if (swapDetail) {
          swapDetail.textContent = `${formatBytes(metrics.swapUsedBytes)} / ${formatBytes(metrics.swapTotalBytes)}`;
        }
      }
    }

    const gpuValue = document.querySelector('#gpu-value');
    const gpuBar = document.querySelector('#gpu-bar') as HTMLElement | null;
    const gpuDetail = document.querySelector('#gpu-detail');
    const gpu = metrics.gpu;
    if (gpu && (gpu.loadPercent != null || gpu.tempCelsius != null || gpu.memoryPercent != null)) {
      if (gpu.loadPercent != null) {
        if (gpuValue) gpuValue.textContent = `${gpu.loadPercent.toFixed(1)}%`;
        setBar(gpuBar, gpu.loadPercent);
      } else if (gpuValue) {
        gpuValue.textContent = gpu.tempCelsius != null ? `${gpu.tempCelsius.toFixed(0)}°C` : '--';
        setBar(gpuBar, 0);
      }
      if (gpuDetail) {
        const parts: string[] = [];
        if (gpu.name) parts.push(gpu.name);
        if (gpu.tempCelsius != null) parts.push(`${gpu.tempCelsius.toFixed(1)} °C`);
        if (gpu.memoryUsedBytes != null && gpu.memoryTotalBytes != null) {
          parts.push(`${formatBytes(gpu.memoryUsedBytes)} / ${formatBytes(gpu.memoryTotalBytes)}`);
        } else if (gpu.memoryPercent != null) {
          parts.push(`显存 ${gpu.memoryPercent.toFixed(1)}%`);
        }
        gpuDetail.textContent = parts.join(' · ') || 'GPU';
      }
      setCardAlert('#gpu-card', alert.gpu);
    } else {
      setCardAlert('#gpu-card', false);
      if (gpuValue) gpuValue.textContent = '--';
      setBar(gpuBar, 0);
      if (gpuDetail) {
        gpuDetail.textContent =
          metrics.tempSource === 'lhm' ? '暂不可用' : '需启用 LibreHardwareMonitor';
      }
    }

    const fans = metrics.fans ?? [];
    const fanSummary = document.querySelector('#fan-summary');
    const fanList = document.querySelector('#fan-list');
    if (fanSummary) fanSummary.textContent = formatFanSummary(fans);
    if (fanList) {
      renderFanList(fanList as HTMLElement, fans, metrics.tempSource);
    }

    const netInterface = document.querySelector('#net-interface');
    if (netInterface) {
      if (metrics.netLink === 'down') {
        netInterface.textContent = metrics.netInterface
          ? `${metrics.netInterface} · 当前不可用`
          : '未检测到网卡';
      } else if (metrics.netInterface) {
        netInterface.textContent = metrics.netInterface;
      } else {
        netInterface.textContent = '';
      }
    }
    if (netDown) netDown.textContent = formatSpeed(metrics.netDownBps);
    if (netUp) netUp.textContent = formatSpeed(metrics.netUpBps);

    if (diskList) {
      renderDiskList(
        diskList as HTMLElement,
        metrics.disks,
        alert.disk,
        alert.thresholds.diskPercent,
      );
      const diskCard = diskList.closest('.metric-card');
      diskCard?.classList.toggle('alert', alert.disk);
    }

    if (updatedAt) {
      const date = new Date(Number(metrics.sampledAtMs));
      updatedAt.textContent = date.toLocaleTimeString();
      (updatedAt as HTMLElement).title = '最近成功采样时间';
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

    if (currentTab === 'monitor') {
      await refreshHistoryChart();
      await refreshProcessTop();
    }

    if (currentTab === 'settings') {
      await refreshTempSourceStatus();
    }
  } catch (error) {
    console.error('获取监测数据失败', error);
    const updatedAt = document.querySelector('#updated-at');
    if (updatedAt) updatedAt.textContent = '数据中断';
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

const DEFAULT_OVERLAY_METRICS = [
  'cpu',
  'mem',
  'temp',
  'gpu',
  'net',
  'codex_primary',
  'codex_secondary',
];

function minuteToTimeValue(minute: number): string {
  const clamped = Math.max(0, Math.min(1439, Math.round(minute)));
  const hours = Math.floor(clamped / 60);
  const mins = clamped % 60;
  return `${String(hours).padStart(2, '0')}:${String(mins).padStart(2, '0')}`;
}

function timeValueToMinute(value: string, fallback: number): number {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!match) return fallback;
  const hours = Number(match[1]);
  const mins = Number(match[2]);
  if (!Number.isFinite(hours) || !Number.isFinite(mins) || hours > 23 || mins > 59) {
    return fallback;
  }
  return hours * 60 + mins;
}

interface SettingsFormValues {
  thresholds: AlertThresholds;
  notificationEnabled?: boolean;
  rangeMinutes?: number;
  preciseTempEnabled?: boolean;
  lhmBaseUrl?: string;
  overlayEnabled?: boolean;
  codexEnabled?: boolean;
  autostartEnabled?: boolean;
  overlayAutoHide?: boolean;
  overlayClickThrough?: boolean;
  overlayStyle?: 'capsule' | 'vertical' | 'numeric';
  processTopEnabled?: boolean;
  alertSustainSecs?: number;
  quietHoursEnabled?: boolean;
  quietStartMinute?: number;
  quietEndMinute?: number;
  hotkeyPanel?: string;
  hotkeyOverlay?: string;
  hotkeyClickThrough?: string;
  overlayMetrics?: string[];
}

function fillSettingsForm(values: SettingsFormValues) {
  const {
    thresholds,
    notificationEnabled,
    rangeMinutes,
    preciseTempEnabled,
    lhmBaseUrl,
    overlayEnabled,
    codexEnabled,
    autostartEnabled,
    overlayAutoHide,
    overlayClickThrough,
    overlayStyle,
    processTopEnabled: processTop,
    alertSustainSecs,
    quietHoursEnabled,
    quietStartMinute,
    quietEndMinute,
    hotkeyPanel,
    hotkeyOverlay,
    hotkeyClickThrough,
    overlayMetrics,
  } = values;
  const cpu = document.querySelector('#setting-cpu') as HTMLInputElement | null;
  const memory = document.querySelector('#setting-memory') as HTMLInputElement | null;
  const temp = document.querySelector('#setting-temp') as HTMLInputElement | null;
  const disk = document.querySelector('#setting-disk') as HTMLInputElement | null;
  const gpu = document.querySelector('#setting-gpu') as HTMLInputElement | null;
  const cooldown = document.querySelector('#setting-cooldown') as HTMLInputElement | null;
  const sustain = document.querySelector('#setting-sustain') as HTMLInputElement | null;
  const notification = document.querySelector('#setting-notification') as HTMLInputElement | null;
  const overlay = document.querySelector('#setting-overlay') as HTMLInputElement | null;
  const codex = document.querySelector('#setting-codex') as HTMLInputElement | null;
  if (codex && codexEnabled !== undefined) codex.checked = codexEnabled;
  const autostart = document.querySelector('#setting-autostart') as HTMLInputElement | null;
  const overlayAutoHideEl = document.querySelector(
    '#setting-overlay-auto-hide',
  ) as HTMLInputElement | null;
  const overlayClickThroughEl = document.querySelector(
    '#setting-overlay-click-through',
  ) as HTMLInputElement | null;
  const overlayStyleEl = document.querySelector(
    '#setting-overlay-style',
  ) as HTMLSelectElement | null;
  const historyRange = document.querySelector('#setting-history-range') as HTMLSelectElement | null;
  const preciseTemp = document.querySelector('#setting-precise-temp') as HTMLInputElement | null;
  const lhmUrl = document.querySelector('#setting-lhm-url') as HTMLInputElement | null;
  const processTopEl = document.querySelector('#setting-process-top') as HTMLInputElement | null;
  const quietEnabled = document.querySelector('#setting-quiet-enabled') as HTMLInputElement | null;
  const quietStart = document.querySelector('#setting-quiet-start') as HTMLInputElement | null;
  const quietEnd = document.querySelector('#setting-quiet-end') as HTMLInputElement | null;
  const hotkeyPanelEl = document.querySelector('#setting-hotkey-panel') as HTMLInputElement | null;
  const hotkeyOverlayEl = document.querySelector(
    '#setting-hotkey-overlay',
  ) as HTMLInputElement | null;
  const hotkeyClickThroughEl = document.querySelector(
    '#setting-hotkey-click-through',
  ) as HTMLInputElement | null;

  if (cpu) cpu.value = String(Math.round(thresholds.cpuPercent));
  if (memory) memory.value = String(Math.round(thresholds.memoryPercent));
  if (temp) temp.value = String(Math.round(thresholds.cpuTempCelsius));
  if (disk) disk.value = String(Math.round(thresholds.diskPercent));
  if (gpu) gpu.value = String(Math.round(thresholds.gpuPercent));
  if (cooldown) cooldown.value = String(thresholds.cooldownSecs);
  if (sustain && alertSustainSecs !== undefined) {
    sustain.value = String(alertSustainSecs);
  }
  if (quietEnabled && quietHoursEnabled !== undefined) {
    quietEnabled.checked = quietHoursEnabled;
  }
  if (quietStart && quietStartMinute !== undefined) {
    quietStart.value = minuteToTimeValue(quietStartMinute);
  }
  if (quietEnd && quietEndMinute !== undefined) {
    quietEnd.value = minuteToTimeValue(quietEndMinute);
  }
  if (hotkeyPanelEl && hotkeyPanel !== undefined) hotkeyPanelEl.value = hotkeyPanel;
  if (hotkeyOverlayEl && hotkeyOverlay !== undefined) hotkeyOverlayEl.value = hotkeyOverlay;
  if (hotkeyClickThroughEl && hotkeyClickThrough !== undefined) {
    hotkeyClickThroughEl.value = hotkeyClickThrough;
  }
  if (overlayMetrics) {
    const selected = new Set(overlayMetrics);
    document
      .querySelectorAll<HTMLInputElement>('.metric-checks input[data-metric]')
      .forEach((input) => {
        input.checked = selected.has(input.dataset.metric ?? '');
      });
  }
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
  if (overlayClickThroughEl && overlayClickThrough !== undefined) {
    overlayClickThroughEl.checked = overlayClickThrough;
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
  if (processTopEl && processTop !== undefined) {
    processTopEl.checked = processTop;
  }
}

function readAlertSustainSecs(): number {
  const el = document.querySelector('#setting-sustain') as HTMLInputElement | null;
  const value = Number(el?.value ?? 5);
  return Number.isFinite(value) ? value : 5;
}

function readQuietHoursEnabled(): boolean {
  const el = document.querySelector('#setting-quiet-enabled') as HTMLInputElement | null;
  return el?.checked ?? false;
}

function readQuietStartMinute(): number {
  const el = document.querySelector('#setting-quiet-start') as HTMLInputElement | null;
  return timeValueToMinute(el?.value ?? '', 23 * 60);
}

function readQuietEndMinute(): number {
  const el = document.querySelector('#setting-quiet-end') as HTMLInputElement | null;
  return timeValueToMinute(el?.value ?? '', 7 * 60);
}

function readHotkey(id: string): string {
  const el = document.querySelector(id) as HTMLInputElement | null;
  return el?.value ?? '';
}

function readOverlayMetrics(): string[] {
  const selected = Array.from(
    document.querySelectorAll<HTMLInputElement>('.metric-checks input[data-metric]:checked'),
  )
    .map((input) => input.dataset.metric ?? '')
    .filter((id) => DEFAULT_OVERLAY_METRICS.includes(id));
  return selected.length ? selected : [...DEFAULT_OVERLAY_METRICS];
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

function readOverlayClickThrough(): boolean {
  const el = document.querySelector('#setting-overlay-click-through') as HTMLInputElement | null;
  return el?.checked ?? false;
}

function readOverlayStyle(): 'capsule' | 'vertical' | 'numeric' {
  const el = document.querySelector('#setting-overlay-style') as HTMLSelectElement | null;
  return normalizeOverlayStyle(el?.value);
}

function readProcessTopEnabled(): boolean {
  const el = document.querySelector('#setting-process-top') as HTMLInputElement | null;
  return el?.checked ?? false;
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
  const disk = document.querySelector('#setting-disk') as HTMLInputElement | null;
  const gpu = document.querySelector('#setting-gpu') as HTMLInputElement | null;
  const cooldown = document.querySelector('#setting-cooldown') as HTMLInputElement | null;
  if (!cpu || !memory || !temp || !disk || !gpu || !cooldown) return null;

  const thresholds: AlertThresholds = {
    cpuPercent: Number(cpu.value),
    memoryPercent: Number(memory.value),
    cpuTempCelsius: Number(temp.value),
    diskPercent: Number(disk.value),
    gpuPercent: Number(gpu.value),
    cooldownSecs: Number(cooldown.value),
  };

  if (
    !Number.isFinite(thresholds.cpuPercent) ||
    !Number.isFinite(thresholds.memoryPercent) ||
    !Number.isFinite(thresholds.cpuTempCelsius) ||
    !Number.isFinite(thresholds.diskPercent) ||
    !Number.isFinite(thresholds.gpuPercent) ||
    !Number.isFinite(thresholds.cooldownSecs)
  ) {
    return null;
  }

  return thresholds;
}

interface AppliedSettings {
  networkSelection: NetworkSelection;
  alert: AlertThresholds;
  notificationEnabled: boolean;
  historyRangeMinutes: number;
  preciseTempEnabled: boolean;
  lhmBaseUrl: string;
  overlayEnabled: boolean;
  codexEnabled: boolean;
  autostartEnabled: boolean;
  overlayAutoHide: boolean;
  overlayClickThrough: boolean;
  overlayStyle: string;
  processTopEnabled: boolean;
  alertSustainSecs: number;
  quietHoursEnabled: boolean;
  quietStartMinute: number;
  quietEndMinute: number;
  hotkeyPanel: string;
  hotkeyOverlay: string;
  hotkeyClickThrough: string;
  overlayMetrics: string[];
}

async function loadSettingsForm() {
  const cfg = await invoke<AppConfig>('get_app_config');
  historyRangeMinutes = normalizeHistoryRangeMinutes(cfg.historyRangeMinutes);
  processTopEnabled = !!cfg.processTopEnabled;
  fillSettingsForm({
    thresholds: cfg.alert,
    notificationEnabled: cfg.notificationEnabled,
    rangeMinutes: historyRangeMinutes,
    preciseTempEnabled: cfg.preciseTempEnabled,
    lhmBaseUrl: cfg.lhmBaseUrl,
    overlayEnabled: cfg.overlayEnabled,
    codexEnabled: cfg.codexEnabled,
    autostartEnabled: cfg.autostartEnabled,
    overlayAutoHide: cfg.overlayAutoHide,
    overlayClickThrough: cfg.overlayClickThrough,
    overlayStyle: normalizeOverlayStyle(cfg.overlayStyle),
    processTopEnabled,
    alertSustainSecs: cfg.alertSustainSecs,
    quietHoursEnabled: cfg.quietHoursEnabled,
    quietStartMinute: cfg.quietStartMinute,
    quietEndMinute: cfg.quietEndMinute,
    hotkeyPanel: cfg.hotkeyPanel,
    hotkeyOverlay: cfg.hotkeyOverlay,
    hotkeyClickThrough: cfg.hotkeyClickThrough,
    overlayMetrics: cfg.overlayMetrics,
  });
  updateChartHint(historyRangeMinutes);
  await loadNetworkInterfaces(cfg.networkSelection);
  await refreshTempSourceStatus();
}

function fillSettingsFormFromApplied(applied: AppliedSettings) {
  historyRangeMinutes = normalizeHistoryRangeMinutes(applied.historyRangeMinutes);
  processTopEnabled = !!applied.processTopEnabled;
  setProcessTopVisible(processTopEnabled);
  updateChartHint(historyRangeMinutes);
  fillSettingsForm({
    thresholds: applied.alert,
    notificationEnabled: applied.notificationEnabled,
    rangeMinutes: historyRangeMinutes,
    preciseTempEnabled: applied.preciseTempEnabled,
    lhmBaseUrl: applied.lhmBaseUrl,
    overlayEnabled: applied.overlayEnabled,
    codexEnabled: applied.codexEnabled,
    autostartEnabled: applied.autostartEnabled,
    overlayAutoHide: applied.overlayAutoHide,
    overlayClickThrough: applied.overlayClickThrough,
    overlayStyle: normalizeOverlayStyle(applied.overlayStyle),
    processTopEnabled,
    alertSustainSecs: applied.alertSustainSecs,
    quietHoursEnabled: applied.quietHoursEnabled,
    quietStartMinute: applied.quietStartMinute,
    quietEndMinute: applied.quietEndMinute,
    hotkeyPanel: applied.hotkeyPanel,
    hotkeyOverlay: applied.hotkeyOverlay,
    hotkeyClickThrough: applied.hotkeyClickThrough,
    overlayMetrics: applied.overlayMetrics,
  });
  void loadNetworkInterfaces(applied.networkSelection);
  if (processTopEnabled) {
    void refreshProcessTop(true);
  }
}

async function loadNetworkInterfaces(selection: NetworkSelection) {
  const select = document.querySelector<HTMLSelectElement>('#setting-network-interface');
  if (!select) return;
  const interfaces =
    await invoke<Array<{ name: string; virtualInterface: boolean }>>('get_network_interfaces');
  const options = [
    new Option('自动选择单一网卡', 'auto'),
    new Option('全部接口汇总（可能重复统计）', 'all'),
  ];
  interfaces.forEach((item) =>
    options.push(
      new Option(item.name + (item.virtualInterface ? '（虚拟）' : ''), 'interface:' + item.name),
    ),
  );
  const selected =
    selection.mode === 'interface' ? 'interface:' + selection.interface : selection.mode;
  if (!options.some((option) => option.value === selected))
    options.push(
      new Option((selection as { interface: string }).interface + '（当前不可用）', selected),
    );
  select.replaceChildren(...options);
  select.value = selected;
}

function readNetworkSelection(): NetworkSelection {
  const value =
    document.querySelector<HTMLSelectElement>('#setting-network-interface')?.value ?? 'auto';
  return value.startsWith('interface:')
    ? { mode: 'interface', interface: value.slice(10) }
    : { mode: value === 'all' ? 'all' : 'auto' };
}

async function setSettingsOpen(open: boolean) {
  await invoke('set_settings_open', { open });
}

function showTab(tab: TabId) {
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
  const closeBtn = document.querySelector<HTMLButtonElement>('#btn-close-settings');
  if (closeBtn) closeBtn.hidden = tab !== 'settings';
}

async function switchTab(tab: TabId) {
  if (currentTab === tab) {
    if (tab === 'settings') {
      await setSettingsOpen(true);
    }
    return;
  }

  if (tab === 'settings') {
    try {
      await setSettingsOpen(true);
    } catch (error) {
      console.error('无法锁定设置页，未切换', error);
      showSettingsMessage('无法锁定设置页，未切换', 'error');
      return;
    }
    showTab('settings');
    try {
      await loadSettingsForm();
      showSettingsMessage('');
    } catch (error) {
      console.error('加载告警设置失败', error);
      showSettingsMessage('加载设置失败', 'error');
    }
    return;
  }

  showTab(tab);
  try {
    await setSettingsOpen(false);
  } catch (error) {
    try {
      await setSettingsOpen(false);
    } catch (retryError) {
      console.error('关闭设置页锁定失败', error, retryError);
    }
  }
  await refreshHistoryChart();
}

function bindSettingsUi() {
  const closeBtn = document.querySelector<HTMLButtonElement>('#btn-close-settings');
  closeBtn?.addEventListener('click', async () => {
    if (closeBtn.disabled) return;
    closeBtn.disabled = true;
    try {
      await getCurrentWindow().close();
    } catch (error) {
      console.error('关闭设置面板失败', error);
      showSettingsMessage('关闭面板失败，请重试', 'error');
    } finally {
      closeBtn.disabled = false;
    }
  });

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

  const exportBtn = document.querySelector('#btn-export-csv') as HTMLButtonElement | null;
  exportBtn?.addEventListener('click', () => {
    void exportMetricsCsv(exportBtn);
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
      const applied = await invoke<AppliedSettings>('apply_settings', {
        settings: {
          thresholds,
          networkSelection: readNetworkSelection(),
          notificationEnabled: readNotificationEnabled(),
          historyRangeMinutes: readHistoryRangeMinutes(),
          preciseTempEnabled: readPreciseTempEnabled(),
          lhmBaseUrl: readLhmBaseUrl(),
          overlayEnabled: readOverlayEnabled(),
          codexEnabled: document.querySelector<HTMLInputElement>('#setting-codex')?.checked ?? true,
          autostartEnabled: readAutostartEnabled(),
          overlayAutoHide: readOverlayAutoHide(),
          overlayClickThrough: readOverlayClickThrough(),
          overlayStyle: readOverlayStyle(),
          processTopEnabled: readProcessTopEnabled(),
          alertSustainSecs: readAlertSustainSecs(),
          quietHoursEnabled: readQuietHoursEnabled(),
          quietStartMinute: readQuietStartMinute(),
          quietEndMinute: readQuietEndMinute(),
          hotkeyPanel: readHotkey('#setting-hotkey-panel'),
          hotkeyOverlay: readHotkey('#setting-hotkey-overlay'),
          hotkeyClickThrough: readHotkey('#setting-hotkey-click-through'),
          overlayMetrics: readOverlayMetrics(),
        },
      });
      fillSettingsFormFromApplied(applied);
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
      const applied = await invoke<AppliedSettings>('apply_settings_reset');
      fillSettingsFormFromApplied(applied);
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

async function exportMetricsCsv(button: HTMLButtonElement) {
  const previousTitle = button.title;
  try {
    const csv = await invoke<string>('export_metrics_csv');
    const blob = new Blob([csv], { type: 'text/csv;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = 'sysmon-history.csv';
    link.rel = 'noopener';
    document.body.appendChild(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
    button.title = previousTitle;
  } catch (error) {
    console.error('导出 CSV 失败', error);
    button.title = '导出失败';
  }
}

async function bootstrapHistoryHint() {
  try {
    const cfg = await invoke<AppConfig>('get_app_config');
    historyRangeMinutes = normalizeHistoryRangeMinutes(cfg.historyRangeMinutes);
    processTopEnabled = !!cfg.processTopEnabled;
    setProcessTopVisible(processTopEnabled);
    updateChartHint(historyRangeMinutes);
    if (processTopEnabled) {
      void refreshProcessTop(true);
    }
  } catch (error) {
    console.error('加载历史时长失败', error);
    updateChartHint(1);
  }
}

window.addEventListener('contextmenu', (event) => {
  event.preventDefault();
});

async function ensureFrontendNotificationPermission() {
  try {
    let granted = await isPermissionGranted();
    if (!granted) {
      granted = (await requestPermission()) === 'granted';
    }
    if (!granted) {
      console.warn('系统通知权限未授予');
    }
  } catch (error) {
    console.error('请求系统通知权限失败', error);
  }
}

async function showAppVersion() {
  const el = document.querySelector('#app-version');
  if (!el) return;
  try {
    el.textContent = `v${await getVersion()}`;
  } catch (error) {
    console.error('读取应用版本失败', error);
    el.textContent = '';
  }
}

window.addEventListener('DOMContentLoaded', () => {
  initializeCodex();
  initializeDiagnostics();
  void showAppVersion();
  bindSettingsUi();
  void bootstrapHistoryHint();
  void ensureFrontendNotificationPermission();
  initializeHardware<Metrics>(refreshMetrics);

  void listen('open-settings', () => {
    void switchTab('settings');
  });

  void listen<boolean>('overlay-enabled-changed', (event) => {
    const overlay = document.querySelector('#setting-overlay') as HTMLInputElement | null;
    if (overlay) overlay.checked = event.payload;
  });
});
