import { invoke } from '@tauri-apps/api/core';
import { initializeCodex } from './codex';
import { initializeHardware, isHardwareVisible } from './hardware';
import './codex.css';
import { listen } from '@tauri-apps/api/event';
import { cursorPosition, getCurrentWindow } from '@tauri-apps/api/window';

interface AlertStatus {
  cpu: boolean;
  memory: boolean;
  temperature: boolean;
  disk: boolean;
  gpu: boolean;
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

interface FanMetrics {
  name: string;
  rpm: number;
  kind: string;
}

interface Metrics {
  cpuPercent: number;
  memoryPercent: number;
  swapUsedBytes: number;
  swapTotalBytes: number;
  netDownBps: number;
  netUpBps: number;
  netInterface: string | null;
  netLink: string;
  cpuTempCelsius: number | null;
  gpu: GpuMetrics | null;
  fans: FanMetrics[];
  sampledAtMs: number;
  alert: AlertStatus;
  tempSource: string;
  stale: boolean;
  error: string | null;
}

type OverlayStyle = 'capsule' | 'vertical' | 'numeric';

interface AppConfig {
  overlayEnabled?: boolean;
  overlayAutoHide: boolean;
  overlayClickThrough?: boolean;
  overlayStyle?: OverlayStyle;
  overlayEdgeX?: 'left' | 'right' | null;
  overlayMetrics?: string[];
}

type OverlayMode = 'collapsed' | 'expanded' | 'peek';

let mode: OverlayMode = 'collapsed';
let lastMetrics: Metrics | null = null;
let style: OverlayStyle = 'capsule';
let layoutQueue: Promise<void> = Promise.resolve();
let layoutDepth = 0;
let autoHide = false;
let edgeX: 'left' | 'right' | null = null;
let hideTimer: number | undefined;
let pinnedExpanded = false;
let overlayEnabled = true;
let clickThroughEnabled = false;
let ignoreActive = false;
let ignoreDesired = false;
let ignoreApplyPromise: Promise<void> | null = null;
let clickThroughPollTimer: number | undefined;
let clickThroughLeaveTimer: number | undefined;
let pointerDown = false;
const DEFAULT_OVERLAY_METRICS = [
  'cpu',
  'mem',
  'temp',
  'gpu',
  'net',
  'codex_primary',
  'codex_secondary',
];
let overlayMetrics = new Set(DEFAULT_OVERLAY_METRICS);

const HIDE_DELAY_MS = 900;
const CLICK_THROUGH_POLL_MS = 60;
const CLICK_THROUGH_LEAVE_MS = 250;

function clearClickThroughLeaveTimer() {
  if (clickThroughLeaveTimer !== undefined) {
    window.clearTimeout(clickThroughLeaveTimer);
    clickThroughLeaveTimer = undefined;
  }
}

async function applyIgnoreCursorEvents(ignore: boolean) {
  ignoreDesired = ignore;
  const apply = async () => {
    if (ignoreActive === ignore) return;
    try {
      await getCurrentWindow().setIgnoreCursorEvents(ignore);
      ignoreActive = ignore;
    } catch (error) {
      if (ignoreDesired === ignore) {
        console.error('设置鼠标穿透失败', error);
        ignoreActive = !ignore;
      }
    }
  };
  ignoreApplyPromise = (ignoreApplyPromise ?? Promise.resolve()).then(apply, apply);
  await ignoreApplyPromise;
}

function stopClickThroughPolling() {
  if (clickThroughPollTimer !== undefined) {
    window.clearInterval(clickThroughPollTimer);
    clickThroughPollTimer = undefined;
  }
  clearClickThroughLeaveTimer();
}

async function disableClickThroughRuntime() {
  stopClickThroughPolling();
  await applyIgnoreCursorEvents(false);
}

async function isCursorOverOverlay(): Promise<boolean> {
  try {
    const win = getCurrentWindow();
    const [cursor, pos, size] = await Promise.all([
      cursorPosition(),
      win.outerPosition(),
      win.outerSize(),
    ]);
    return (
      cursor.x >= pos.x &&
      cursor.x < pos.x + size.width &&
      cursor.y >= pos.y &&
      cursor.y < pos.y + size.height
    );
  } catch (error) {
    console.error('读取光标/窗口位置失败', error);
    return false;
  }
}

async function tickClickThrough() {
  if (!clickThroughEnabled || !overlayEnabled || layoutDepth > 0) return;
  const over = await isCursorOverOverlay();
  if (over) {
    clearClickThroughLeaveTimer();
    if (mode === 'peek') {
      void revealFromPeek();
    }
    if (ignoreActive) {
      await applyIgnoreCursorEvents(false);
    }
    return;
  }
  if (pointerDown || ignoreActive || clickThroughLeaveTimer !== undefined) return;
  clickThroughLeaveTimer = window.setTimeout(() => {
    clickThroughLeaveTimer = undefined;
    if (!clickThroughEnabled || !overlayEnabled || pointerDown || layoutDepth > 0) return;
    void applyIgnoreCursorEvents(true);
  }, CLICK_THROUGH_LEAVE_MS);
}

function startClickThroughPolling() {
  if (clickThroughPollTimer !== undefined) return;
  clickThroughPollTimer = window.setInterval(() => {
    void tickClickThrough();
  }, CLICK_THROUGH_POLL_MS);
  void tickClickThrough();
}

async function syncClickThroughRuntime() {
  if (clickThroughEnabled && overlayEnabled) {
    clearClickThroughLeaveTimer();
    await applyIgnoreCursorEvents(true);
    startClickThroughPolling();
    return;
  }
  await disableClickThroughRuntime();
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

function fanKindShort(kind: string): string {
  switch (kind) {
    case 'cpu':
      return 'CPU';
    case 'gpu':
      return 'GPU';
    case 'chassis':
      return '机箱';
    case 'pump':
      return '泵';
    default:
      return '风扇';
  }
}

function formatFansExpanded(
  fans: FanMetrics[],
  tempSource: string,
): {
  primary: string;
  detail: string;
} {
  if (!fans.length) {
    return {
      primary: '--',
      detail: tempSource === 'lhm' ? '暂不可用' : '需启用 LibreHardwareMonitor',
    };
  }
  const primary = fans[0];
  const primaryText = `${fanKindShort(primary.kind)} ${primary.rpm}`;
  const detail = fans
    .slice(0, 3)
    .map((f) => `${fanKindShort(f.kind)} ${f.rpm}`)
    .join(' · ');
  return { primary: primaryText, detail };
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

function enqueueLayout(task: () => Promise<void>): Promise<void> {
  const run = layoutQueue.then(task, task);
  layoutQueue = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

async function applyLayout(next: OverlayMode, force = false) {
  if (!force && mode === next) return;
  await enqueueLayout(async () => {
    if (!force && mode === next) return;
    layoutDepth += 1;
    const root = document.querySelector('#overlay-root') as HTMLElement | null;
    try {
      root?.classList.add('layout-switching');
      await invoke('set_overlay_layout', { mode: next });
      mode = next;
      applyModeClass(mode);
      // 每秒只更新当前可见视图，切换后需立即刷新一次新视图
      if (lastMetrics && isHardwareVisible()) refreshActiveView(lastMetrics);
    } catch (error) {
      console.error('切换叠加层布局失败', error);
      applyModeClass(mode);
      throw error;
    } finally {
      requestAnimationFrame(() => {
        root?.classList.remove('layout-switching');
      });
      layoutDepth -= 1;
    }
  });
}

async function setMode(next: OverlayMode) {
  await applyLayout(next, false);
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

function applyOverlayMetrics(ids: string[] | undefined) {
  const next = new Set((ids ?? []).filter((id) => DEFAULT_OVERLAY_METRICS.includes(id)));
  overlayMetrics = next.size ? next : new Set(DEFAULT_OVERLAY_METRICS);

  const hideMetric = (el: Element, metric: string) => {
    if (el instanceof HTMLElement) el.hidden = !overlayMetrics.has(metric);
  };

  document.querySelectorAll('.strip-metric[data-metric]').forEach((el) => {
    hideMetric(el, el.getAttribute('data-metric') ?? '');
  });
  document.querySelectorAll('.numeric-strip [data-metric]').forEach((el) => {
    hideMetric(el, el.getAttribute('data-metric') ?? '');
  });
  document.querySelectorAll('.vertical-strip [data-metric]').forEach((el) => {
    hideMetric(el, el.getAttribute('data-metric') ?? '');
  });

  const syncSeps = (container: Element | null, sepSelector: string) => {
    if (!container) return;
    const children = Array.from(container.children);
    let seenMetric = false;
    let pendingSeparator: HTMLElement | null = null;
    for (const child of children) {
      if (!(child instanceof HTMLElement)) continue;
      if (child.matches(sepSelector)) {
        child.hidden = true;
        pendingSeparator = child;
      } else if (child.hasAttribute('data-metric') && !child.hidden) {
        if (seenMetric && pendingSeparator) pendingSeparator.hidden = false;
        seenMetric = true;
        pendingSeparator = null;
      }
    }
  };

  syncSeps(document.querySelector('.strip'), '.strip-sep');
  syncSeps(document.querySelector('.numeric-strip'), '.num-sep');
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
  await applyLayout(mode, true);
}

async function loadOverlayConfig() {
  try {
    const cfg = await invoke<AppConfig>('get_app_config');
    overlayEnabled = cfg.overlayEnabled !== false;
    autoHide = !!cfg.overlayAutoHide;
    clickThroughEnabled = !!cfg.overlayClickThrough;
    edgeX = cfg.overlayEdgeX === 'left' || cfg.overlayEdgeX === 'right' ? cfg.overlayEdgeX : null;
    applyOverlayMetrics(cfg.overlayMetrics);
    await applyOverlayStyle(normalizeStyle(cfg.overlayStyle));
    if (canPeekHide() && mode === 'collapsed') {
      await setMode('peek');
    } else if (mode === 'peek' && !canPeekHide()) {
      await setMode('collapsed');
    }
    await syncClickThroughRuntime();
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
      setFill(document.querySelector('#ov-swap-bar') as HTMLElement | null, swapPercent);
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

  const fanView = formatFansExpanded(metrics.fans ?? [], metrics.tempSource);
  setText('ov-fan', fanView.primary);
  setText('ov-fan-detail', fanView.detail);

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

async function refreshOverlay(metrics: Metrics) {
  if (!isHardwareVisible() || !overlayEnabled) return;
  if (metrics.stale) {
    const updated = document.querySelector<HTMLElement>('#overlay-updated');
    if (updated) {
      updated.textContent = '数据过期';
      updated.title = metrics.error ?? '等待后台采样恢复';
    }
    document.querySelector('#overlay-root')?.classList.add('metrics-stale');
    return;
  }
  lastMetrics = metrics;
  document.querySelector('#overlay-root')?.classList.remove('metrics-stale');
  const updated = document.querySelector<HTMLElement>('#overlay-updated');
  if (updated) updated.title = '最近成功采样时间';
  refreshActiveView(metrics);
}

function bindWindowDrag(root: HTMLElement) {
  const DRAG_THRESHOLD_PX = 3;

  root.addEventListener('pointerdown', (event) => {
    if (event.button !== 0) return;
    if (event.detail > 1) return;
    const target = event.target as HTMLElement | null;
    if (!target || target.closest('button')) return;

    pointerDown = true;
    clearClickThroughLeaveTimer();
    try {
      root.setPointerCapture(event.pointerId);
    } catch {
      // 捕获失败时仍靠 pointerup / pointercancel / lostpointercapture 解绑
    }

    const startX = event.clientX;
    const startY = event.clientY;
    let started = false;
    let done = false;

    const cleanup = () => {
      if (done) return;
      done = true;
      pointerDown = false;
      root.removeEventListener('pointermove', onMove);
      root.removeEventListener('pointerup', cleanup);
      root.removeEventListener('pointercancel', cleanup);
      root.removeEventListener('lostpointercapture', cleanup);
      if (clickThroughEnabled && overlayEnabled) {
        void tickClickThrough();
      }
    };

    const onMove = (moveEvent: PointerEvent) => {
      if (started || done) return;
      if (
        Math.abs(moveEvent.clientX - startX) < DRAG_THRESHOLD_PX &&
        Math.abs(moveEvent.clientY - startY) < DRAG_THRESHOLD_PX
      ) {
        return;
      }
      started = true;
      cleanup();
      void getCurrentWindow().startDragging();
    };

    root.addEventListener('pointermove', onMove);
    root.addEventListener('pointerup', cleanup);
    root.addEventListener('pointercancel', cleanup);
    root.addEventListener('lostpointercapture', cleanup);
  });
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

  const root = document.querySelector('#overlay-root') as HTMLElement | null;
  if (root) {
    bindWindowDrag(root);
  }

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
  initializeCodex();
  initializeHardware<Metrics>(refreshOverlay);
  bindCollapseUi();
  applyStyleClass('capsule');
  applyModeClass('collapsed');
  mode = 'collapsed';
  // 仅按最终配置 resize 一次；窗口初始尺寸即 collapsed capsule 尺寸，配置加载失败也无需回退
  void loadOverlayConfig();

  void listen<boolean>('overlay-enabled-changed', (event) => {
    overlayEnabled = !!event.payload;
    if (overlayEnabled) {
      void syncClickThroughRuntime();
    } else {
      void disableClickThroughRuntime();
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

  void listen<boolean>('overlay-click-through-changed', (event) => {
    clickThroughEnabled = !!event.payload;
    void syncClickThroughRuntime();
  });

  void listen<string>('overlay-style-changed', (event) => {
    void applyOverlayStyle(normalizeStyle(event.payload));
  });

  void listen<string[]>('overlay-metrics-changed', (event) => {
    applyOverlayMetrics(event.payload);
    if (mode === 'collapsed') {
      void applyLayout(mode, true);
    }
  });

  void listen<'left' | 'right'>('overlay-snap-edge', (event) => {
    edgeX = event.payload === 'left' || event.payload === 'right' ? event.payload : edgeX;
    if (!canPeekHide() || pinnedExpanded) return;
    scheduleAutoHide();
  });
});
