import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface AlertStatus {
  cpu: boolean;
  memory: boolean;
  temperature: boolean;
  disk: boolean;
  active: boolean;
  messages: string[];
}

interface GpuMetrics {
  name: string;
  loadPercent: number | null;
  memoryUsedBytes: number | null;
  memoryTotalBytes: number | null;
  memoryPercent: number | null;
  tempCelsius: number | null;
}

interface Metrics {
  cpuPercent: number;
  memoryPercent: number;
  swapUsedBytes: number;
  swapTotalBytes: number;
  netDownBps: number;
  netUpBps: number;
  cpuTempCelsius: number | null;
  gpu: GpuMetrics | null;
  sampledAtMs: number;
  alert: AlertStatus;
  tempSource: string;
}

type OverlayStyle = 'capsule' | 'vertical' | 'numeric';

interface AppConfig {
  overlayEnabled?: boolean;
  overlayAutoHide: boolean;
  overlayStyle?: OverlayStyle;
  overlayEdgeX?: 'left' | 'right' | null;
  overlayEdgeY?: 'top' | 'bottom' | null;
}

type OverlayMode = 'collapsed' | 'expanded' | 'peek';

let mode: OverlayMode = 'collapsed';
let style: OverlayStyle = 'capsule';
let resizing = false;
let autoHide = false;
let edgeX: 'left' | 'right' | null = null;
let hideTimer: number | undefined;
let pinnedExpanded = false;
let pollTimer: number | undefined;
let overlayEnabled = true;

const HIDE_DELAY_MS = 900;

function startPolling() {
  if (pollTimer !== undefined) return;
  pollTimer = window.setInterval(() => {
    void refreshOverlay();
  }, 1000);
}

function stopPolling() {
  if (pollTimer !== undefined) {
    window.clearInterval(pollTimer);
    pollTimer = undefined;
  }
}

function canPeekHide(): boolean {
  return autoHide && !!edgeX;
}

function normalizeStyle(value: string | null | undefined): OverlayStyle {
  if (value === 'vertical' || value === 'numeric' || value === 'capsule') return value;
  return 'capsule';
}

function formatSpeedShort(bps: number): string {
  if (bps <= 0) return '0B';
  const units = ['B', 'K', 'M', 'G', 'T'];
  const exp = Math.min(Math.floor(Math.log(bps) / Math.log(1024)), units.length - 1);
  const value = bps / Math.pow(1024, exp);
  if (exp === 0) return `${Math.round(value)}B`;
  if (value >= 100) return `${Math.round(value)}${units[exp]}`;
  return `${value.toFixed(1)}${units[exp]}`;
}

function setText(id: string, text: string) {
  const el = document.querySelector(`#${id}`);
  if (el) el.textContent = text;
}

function setRowAlert(id: string, active: boolean) {
  document.querySelector(`#${id}`)?.classList.toggle('alert', active);
}

function levelClass(percent: number, alert = false): 'danger' | 'warn' | '' {
  if (alert || percent >= 90) return 'danger';
  if (percent >= 75) return 'warn';
  return '';
}

function setFill(el: HTMLElement | null, percent: number, alert = false) {
  if (!el) return;
  const value = Math.max(0, Math.min(100, percent));
  el.style.width = `${value}%`;
  el.classList.remove('warn', 'danger');
  const level = levelClass(value, alert);
  if (level) el.classList.add(level);
}

function setDot(el: HTMLElement | null, percent: number, alert = false) {
  if (!el) return;
  el.classList.remove('warn', 'danger');
  const level = levelClass(percent, alert);
  if (level) el.classList.add(level);
}

function tempPercent(temp: number | null): number {
  if (temp == null) return 0;
  return Math.max(0, Math.min(100, ((temp - 30) / 70) * 100));
}

function formatBytesShort(bytes: number): string {
  if (bytes <= 0) return '0B';
  const units = ['B', 'K', 'M', 'G', 'T'];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / Math.pow(1024, exp);
  if (exp === 0) return `${Math.round(value)}B`;
  if (value >= 100) return `${Math.round(value)}${units[exp]}`;
  return `${value.toFixed(1)}${units[exp]}`;
}

function gpuHasData(gpu: GpuMetrics | null): boolean {
  return !!(
    gpu &&
    (gpu.loadPercent != null || gpu.tempCelsius != null || gpu.memoryPercent != null)
  );
}

function gpuUnavailableHint(tempSource: string): string {
  return tempSource === 'lhm' ? '暂不可用' : '需启用 LHM';
}

function formatGpuCompact(gpu: GpuMetrics | null, tempSource: string): string {
  if (!gpuHasData(gpu) || !gpu) return gpuUnavailableHint(tempSource);
  const parts: string[] = [];
  if (gpu.loadPercent != null) parts.push(`${gpu.loadPercent.toFixed(0)}%`);
  if (gpu.tempCelsius != null) parts.push(`${gpu.tempCelsius.toFixed(0)}°`);
  if (gpu.memoryPercent != null) {
    parts.push(`显${gpu.memoryPercent.toFixed(0)}%`);
  } else if (gpu.memoryUsedBytes != null && gpu.memoryTotalBytes != null) {
    parts.push(`${formatBytesShort(gpu.memoryUsedBytes)}`);
  }
  return parts.join('·') || '--';
}

/** 胶囊/数值收起态：仅占用%，温度与显存留给展开态 */
function formatGpuStrip(gpu: GpuMetrics | null): string {
  if (!gpu) return '--';
  if (gpu.loadPercent != null) return `${gpu.loadPercent.toFixed(0)}%`;
  if (gpu.tempCelsius != null) return `${gpu.tempCelsius.toFixed(0)}°`;
  return '--';
}

function formatGpuNumeric(gpu: GpuMetrics | null): string {
  if (!gpu) return '--';
  if (gpu.loadPercent != null) return gpu.loadPercent.toFixed(0);
  if (gpu.tempCelsius != null) return gpu.tempCelsius.toFixed(0);
  return '--';
}

function formatGpuMemoryDetail(gpu: GpuMetrics): string {
  if (gpu.memoryUsedBytes != null && gpu.memoryTotalBytes != null) {
    return `显存 ${formatBytesShort(gpu.memoryUsedBytes)} / ${formatBytesShort(gpu.memoryTotalBytes)}`;
  }
  if (gpu.memoryPercent != null) {
    return `显存 ${gpu.memoryPercent.toFixed(0)}%`;
  }
  return '显存 --';
}

function gpuLevelPercent(gpu: GpuMetrics | null): number {
  if (!gpu) return 0;
  if (gpu.loadPercent != null) return gpu.loadPercent;
  if (gpu.memoryPercent != null) return gpu.memoryPercent;
  return tempPercent(gpu.tempCelsius);
}

function applyStyleClass(next: OverlayStyle) {
  const root = document.querySelector('#overlay-root');
  if (!root) return;
  root.classList.remove('style-capsule', 'style-vertical', 'style-numeric');
  root.classList.add(`style-${next}`);
}

function applyModeClass(next: OverlayMode) {
  const root = document.querySelector('#overlay-root');
  if (!root) return;
  root.classList.toggle('collapsed', next === 'collapsed');
  root.classList.toggle('peek', next === 'peek');
  root.classList.toggle('expanded', next === 'expanded');
}

function clearHideTimer() {
  if (hideTimer !== undefined) {
    window.clearTimeout(hideTimer);
    hideTimer = undefined;
  }
}

async function setMode(next: OverlayMode) {
  if (mode === next || resizing) return;
  resizing = true;
  const root = document.querySelector('#overlay-root') as HTMLElement | null;
  try {
    root?.classList.add('layout-switching');
    await invoke('set_overlay_layout', { mode: next });
    mode = next;
    applyModeClass(mode);
    // 每秒只更新当前可见视图，切换后需立即刷新一次新视图
    void refreshOverlay();
  } catch (error) {
    console.error('切换叠加层布局失败', error);
  } finally {
    requestAnimationFrame(() => {
      root?.classList.remove('layout-switching');
      resizing = false;
    });
  }
}

async function setCollapsed(nextCollapsed: boolean) {
  pinnedExpanded = !nextCollapsed;
  clearHideTimer();
  if (nextCollapsed) {
    if (canPeekHide()) {
      await setMode('peek');
    } else {
      await setMode('collapsed');
    }
  } else {
    await setMode('expanded');
  }
}

function scheduleAutoHide() {
  if (!canPeekHide() || pinnedExpanded || mode === 'peek') return;
  clearHideTimer();
  hideTimer = window.setTimeout(() => {
    void setMode('peek');
  }, HIDE_DELAY_MS);
}

async function revealFromPeek() {
  clearHideTimer();
  if (mode !== 'peek') return;
  await setMode('collapsed');
}

async function applyOverlayStyle(next: OverlayStyle) {
  style = next;
  applyStyleClass(style);
  if (mode === 'expanded') {
    pinnedExpanded = false;
    clearHideTimer();
    await setMode('collapsed');
    return;
  }
  if (mode === 'peek') {
    return;
  }
  // 强制按当前形态重算 collapsed 尺寸
  const prev = mode;
  mode = 'expanded';
  await setMode(prev);
}

async function loadOverlayConfig() {
  try {
    const cfg = await invoke<AppConfig>('get_app_config');
    overlayEnabled = cfg.overlayEnabled !== false;
    autoHide = !!cfg.overlayAutoHide;
    edgeX = cfg.overlayEdgeX === 'left' || cfg.overlayEdgeX === 'right' ? cfg.overlayEdgeX : null;
    await applyOverlayStyle(normalizeStyle(cfg.overlayStyle));
    if (canPeekHide() && mode === 'collapsed') {
      await setMode('peek');
    } else if (mode === 'peek' && !canPeekHide()) {
      await setMode('collapsed');
    }
  } catch (error) {
    console.error('加载叠加层配置失败', error);
  }
}

function updateExpandedPanel(metrics: Metrics) {
  const alert = metrics.alert;

  const alertFlag = document.querySelector('#overlay-alert-flag') as HTMLElement | null;
  if (alertFlag) alertFlag.hidden = !alert.active;

  setText('ov-cpu', `${metrics.cpuPercent.toFixed(0)}%`);
  setText('ov-mem', `${metrics.memoryPercent.toFixed(0)}%`);
  setText(
    'ov-temp',
    metrics.cpuTempCelsius == null ? '--' : `${metrics.cpuTempCelsius.toFixed(0)}°C`,
  );
  setText('ov-net-down', formatSpeedShort(metrics.netDownBps));
  setText('ov-net-up', formatSpeedShort(metrics.netUpBps));

  const updated = document.querySelector('#overlay-updated');
  if (updated) {
    updated.textContent = new Date(Number(metrics.sampledAtMs)).toLocaleTimeString();
  }

  const swapCard = document.querySelector('#ov-swap-card') as HTMLElement | null;
  if (swapCard) {
    if (metrics.swapTotalBytes === 0) {
      swapCard.hidden = true;
    } else {
      swapCard.hidden = false;
      const swapPercent = (metrics.swapUsedBytes / metrics.swapTotalBytes) * 100;
      setText('ov-swap', `${swapPercent.toFixed(0)}%`);
      setFill(
        document.querySelector('#ov-swap-bar') as HTMLElement | null,
        swapPercent,
      );
    }
  }

  const gpu = metrics.gpu;
  if (gpuHasData(gpu) && gpu) {
    const parts: string[] = [];
    if (gpu.loadPercent != null) parts.push(`${gpu.loadPercent.toFixed(0)}%`);
    if (gpu.tempCelsius != null) parts.push(`${gpu.tempCelsius.toFixed(0)}°C`);
    setText('ov-gpu', parts.join(' · ') || '--');
    setFill(document.querySelector('#ov-gpu-bar') as HTMLElement | null, gpuLevelPercent(gpu));
    const detailParts: string[] = [];
    if (gpu.name) detailParts.push(gpu.name);
    detailParts.push(formatGpuMemoryDetail(gpu));
    setText('ov-gpu-detail', detailParts.join(' · '));
  } else {
    setText('ov-gpu', '--');
    setFill(document.querySelector('#ov-gpu-bar') as HTMLElement | null, 0);
    setText(
      'ov-gpu-detail',
      metrics.tempSource === 'lhm' ? '暂不可用' : '需启用 LibreHardwareMonitor',
    );
  }

  setFill(
    document.querySelector('#ov-cpu-bar') as HTMLElement | null,
    metrics.cpuPercent,
    alert.cpu,
  );
  setFill(
    document.querySelector('#ov-mem-bar') as HTMLElement | null,
    metrics.memoryPercent,
    alert.memory,
  );
  setFill(
    document.querySelector('#ov-temp-bar') as HTMLElement | null,
    tempPercent(metrics.cpuTempCelsius),
    alert.temperature,
  );

  setRowAlert('ov-cpu-card', alert.cpu);
  setRowAlert('ov-mem-card', alert.memory);
  setRowAlert('ov-temp-card', alert.temperature);
}

function updateCapsuleStrip(metrics: Metrics) {
  const alert = metrics.alert;
  const gpu = metrics.gpu;
  const gpuPct = gpuLevelPercent(gpu);

  const stripAlert = document.querySelector('#strip-alert') as HTMLElement | null;
  if (stripAlert) stripAlert.hidden = !alert.active;

  setText('strip-cpu', `${metrics.cpuPercent.toFixed(0)}%`);
  setText('strip-mem', `${metrics.memoryPercent.toFixed(0)}%`);
  setText(
    'strip-temp',
    metrics.cpuTempCelsius == null ? '--' : `${metrics.cpuTempCelsius.toFixed(0)}°C`,
  );
  setText('strip-gpu', formatGpuStrip(gpu));
  setText('strip-net-down', formatSpeedShort(metrics.netDownBps));
  setText('strip-net-up', formatSpeedShort(metrics.netUpBps));

  setDot(
    document.querySelector('#strip-cpu-dot') as HTMLElement | null,
    metrics.cpuPercent,
    alert.cpu,
  );
  setDot(
    document.querySelector('#strip-mem-dot') as HTMLElement | null,
    metrics.memoryPercent,
    alert.memory,
  );
  setDot(
    document.querySelector('#strip-temp-dot') as HTMLElement | null,
    tempPercent(metrics.cpuTempCelsius),
    alert.temperature,
  );
  setDot(document.querySelector('#strip-gpu-dot') as HTMLElement | null, gpuPct);

  document.querySelector('.strip-metric[data-metric="cpu"]')?.classList.toggle('alert', alert.cpu);
  document
    .querySelector('.strip-metric[data-metric="mem"]')
    ?.classList.toggle('alert', alert.memory);
  document
    .querySelector('.strip-metric[data-metric="temp"]')
    ?.classList.toggle('alert', alert.temperature);
}

function updateNumericStrip(metrics: Metrics) {
  const alert = metrics.alert;
  const gpu = metrics.gpu;

  setText('num-cpu', metrics.cpuPercent.toFixed(0));
  setText('num-mem', metrics.memoryPercent.toFixed(0));
  setText('num-temp', metrics.cpuTempCelsius == null ? '--' : metrics.cpuTempCelsius.toFixed(0));
  setText('num-gpu', formatGpuNumeric(gpu));
  setText(
    'num-net',
    `↓${formatSpeedShort(metrics.netDownBps)} ↑${formatSpeedShort(metrics.netUpBps)}`,
  );

  document.querySelector('#num-cpu')?.classList.toggle('alert', alert.cpu);
  document.querySelector('#num-mem')?.classList.toggle('alert', alert.memory);
  document.querySelector('#num-temp')?.classList.toggle('alert', alert.temperature);
}

function updateVerticalStrip(metrics: Metrics) {
  const alert = metrics.alert;
  const gpu = metrics.gpu;

  setText('v-cpu', `${metrics.cpuPercent.toFixed(0)}%`);
  setText('v-mem', `${metrics.memoryPercent.toFixed(0)}%`);
  setText(
    'v-temp',
    metrics.cpuTempCelsius == null ? '--' : `${metrics.cpuTempCelsius.toFixed(0)}°C`,
  );
  setText('v-gpu', formatGpuCompact(gpu, metrics.tempSource));
  setText('v-net', `↓${formatSpeedShort(metrics.netDownBps)}`);
  setText('v-net-up', `↑${formatSpeedShort(metrics.netUpBps)}`);

  document.querySelector('.v-item[data-metric="cpu"]')?.classList.toggle('alert', alert.cpu);
  document.querySelector('.v-item[data-metric="mem"]')?.classList.toggle('alert', alert.memory);
  document
    .querySelector('.v-item[data-metric="temp"]')
    ?.classList.toggle('alert', alert.temperature);
}

function refreshActiveView(metrics: Metrics) {
  document.querySelector('#overlay-root')?.classList.toggle('alert', metrics.alert.active);
  if (mode === 'expanded') {
    updateExpandedPanel(metrics);
    return;
  }
  if (mode === 'peek') {
    return;
  }
  if (style === 'capsule') updateCapsuleStrip(metrics);
  else if (style === 'numeric') updateNumericStrip(metrics);
  else updateVerticalStrip(metrics);
}

async function refreshOverlay() {
  try {
    refreshActiveView(await invoke<Metrics>('get_metrics'));
  } catch (error) {
    console.error('叠加层获取监测数据失败', error);
  }
}

function bindCollapseUi() {
  const expandIds = ['#btn-expand', '#btn-expand-num'];
  for (const id of expandIds) {
    document.querySelector(id)?.addEventListener('click', (event) => {
      event.stopPropagation();
      void setCollapsed(false);
    });
  }

  document.querySelector('#btn-collapse')?.addEventListener('click', (event) => {
    event.stopPropagation();
    void setCollapsed(true);
  });

  document.querySelector('.strip')?.addEventListener('dblclick', () => {
    void setCollapsed(false);
  });
  document.querySelector('.numeric-strip')?.addEventListener('dblclick', () => {
    void setCollapsed(false);
  });
  document.querySelector('.vertical-strip')?.addEventListener('dblclick', () => {
    void setCollapsed(false);
  });

  document.querySelector('.panel-view')?.addEventListener('dblclick', (event) => {
    const target = event.target as HTMLElement | null;
    if (target?.closest('button')) return;
    void setCollapsed(true);
  });

  const root = document.querySelector('#overlay-root');
  root?.addEventListener('mouseenter', () => {
    clearHideTimer();
    if (mode === 'peek') {
      void revealFromPeek();
    }
  });

  root?.addEventListener('mouseleave', () => {
    if (!canPeekHide() || pinnedExpanded) return;
    if (mode === 'collapsed' || mode === 'expanded') {
      scheduleAutoHide();
    }
  });

  document.querySelector('#peek-bar')?.addEventListener('click', () => {
    void revealFromPeek();
  });
}

window.addEventListener('contextmenu', (event) => {
  event.preventDefault();
});

window.addEventListener('DOMContentLoaded', () => {
  bindCollapseUi();
  applyStyleClass('capsule');
  applyModeClass('collapsed');
  mode = 'collapsed';
  // 仅按最终配置 resize 一次；窗口初始尺寸即 collapsed capsule 尺寸，配置加载失败也无需回退
  void loadOverlayConfig().then(() => {
    // 后端仅在 overlay_enabled 时才显示窗口；禁用时 webview 虽存活，但不应驱动每秒采样
    if (overlayEnabled) {
      void refreshOverlay();
      startPolling();
    }
  });

  void listen<boolean>('overlay-enabled-changed', (event) => {
    overlayEnabled = !!event.payload;
    if (overlayEnabled) {
      startPolling();
      void refreshOverlay();
    } else {
      stopPolling();
    }
  });

  void listen<boolean>('overlay-auto-hide-changed', (event) => {
    autoHide = !!event.payload;
    if (canPeekHide()) {
      if (!pinnedExpanded && mode !== 'peek') {
        scheduleAutoHide();
      }
    } else if (mode === 'peek') {
      void setMode('collapsed');
    }
  });

  void listen<string>('overlay-style-changed', (event) => {
    void applyOverlayStyle(normalizeStyle(event.payload));
  });

  void listen<'left' | 'right'>('overlay-snap-edge', (event) => {
    edgeX = event.payload === 'left' || event.payload === 'right' ? event.payload : edgeX;
    if (!canPeekHide() || pinnedExpanded) return;
    scheduleAutoHide();
  });
});
