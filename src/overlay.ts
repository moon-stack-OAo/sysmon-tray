import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface AlertStatus {
  cpu: boolean;
  memory: boolean;
  temperature: boolean;
  active: boolean;
  messages: string[];
}

interface Metrics {
  cpuPercent: number;
  memoryPercent: number;
  netDownBps: number;
  netUpBps: number;
  cpuTempCelsius: number | null;
  sampledAtMs: number;
  alert: AlertStatus;
}

type OverlayStyle = 'capsule' | 'vertical' | 'numeric';

interface AppConfig {
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
let hideTimer: number | undefined;
let pinnedExpanded = false;

const HIDE_DELAY_MS = 900;

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
    if (autoHide) {
      await setMode('peek');
    } else {
      await setMode('collapsed');
    }
  } else {
    await setMode('expanded');
  }
}

function scheduleAutoHide() {
  if (!autoHide || pinnedExpanded || mode === 'peek') return;
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
    autoHide = !!cfg.overlayAutoHide;
    await applyOverlayStyle(normalizeStyle(cfg.overlayStyle));
    if (autoHide && mode === 'collapsed') {
      await setMode('peek');
    } else if (!autoHide && mode === 'peek') {
      await setMode('collapsed');
    }
  } catch (error) {
    console.error('加载叠加层配置失败', error);
  }
}

async function refreshOverlay() {
  try {
    const metrics = await invoke<Metrics>('get_metrics');
    const alert = metrics.alert;

    document.querySelector('#overlay-root')?.classList.toggle('alert', alert.active);

    const alertFlag = document.querySelector('#overlay-alert-flag') as HTMLElement | null;
    if (alertFlag) alertFlag.hidden = !alert.active;

    const stripAlert = document.querySelector('#strip-alert') as HTMLElement | null;
    if (stripAlert) stripAlert.hidden = !alert.active;

    const cpuText = `${metrics.cpuPercent.toFixed(0)}%`;
    const memText = `${metrics.memoryPercent.toFixed(0)}%`;
    const tempText =
      metrics.cpuTempCelsius == null ? '--' : `${metrics.cpuTempCelsius.toFixed(0)}°C`;
    const downText = formatSpeedShort(metrics.netDownBps);
    const upText = formatSpeedShort(metrics.netUpBps);
    const cpuNum = metrics.cpuPercent.toFixed(0);
    const memNum = metrics.memoryPercent.toFixed(0);
    const tempNum = metrics.cpuTempCelsius == null ? '--' : metrics.cpuTempCelsius.toFixed(0);

    setText('ov-cpu', cpuText);
    setText('ov-mem', memText);
    setText('ov-temp', tempText);
    setText('ov-net-down', downText);
    setText('ov-net-up', upText);

    setText('strip-cpu', cpuText);
    setText('strip-mem', memText);
    setText('strip-temp', tempText);
    setText('strip-net-down', downText);
    setText('strip-net-up', upText);

    setText('num-cpu', cpuNum);
    setText('num-mem', memNum);
    setText('num-temp', tempNum);
    setText('num-net', `↓${downText} ↑${upText}`);

    setText('v-cpu', cpuText);
    setText('v-mem', memText);
    setText('v-temp', tempText);
    setText('v-net', `↓${downText}`);
    setText('v-net-up', `↑${upText}`);

    const updated = document.querySelector('#overlay-updated');
    if (updated) {
      updated.textContent = new Date(Number(metrics.sampledAtMs)).toLocaleTimeString();
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

    setRowAlert('ov-cpu-card', alert.cpu);
    setRowAlert('ov-mem-card', alert.memory);
    setRowAlert('ov-temp-card', alert.temperature);

    document
      .querySelector('.strip-metric[data-metric="cpu"]')
      ?.classList.toggle('alert', alert.cpu);
    document
      .querySelector('.strip-metric[data-metric="mem"]')
      ?.classList.toggle('alert', alert.memory);
    document
      .querySelector('.strip-metric[data-metric="temp"]')
      ?.classList.toggle('alert', alert.temperature);

    document.querySelector('.v-item[data-metric="cpu"]')?.classList.toggle('alert', alert.cpu);
    document.querySelector('.v-item[data-metric="mem"]')?.classList.toggle('alert', alert.memory);
    document
      .querySelector('.v-item[data-metric="temp"]')
      ?.classList.toggle('alert', alert.temperature);

    document.querySelector('#num-cpu')?.classList.toggle('alert', alert.cpu);
    document.querySelector('#num-mem')?.classList.toggle('alert', alert.memory);
    document.querySelector('#num-temp')?.classList.toggle('alert', alert.temperature);
  } catch (error) {
    console.error('叠加层获取监测数据失败', error);
  }
}

function bindCollapseUi() {
  const expandIds = ['#btn-expand', '#btn-expand-num', '#btn-expand-v'];
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
    if (!autoHide || pinnedExpanded) return;
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
  void invoke('set_overlay_layout', { mode: 'collapsed' }).catch((error) => {
    console.error('初始化叠加层尺寸失败', error);
  });
  void loadOverlayConfig();
  void refreshOverlay();
  window.setInterval(() => {
    void refreshOverlay();
  }, 1000);

  void listen<boolean>('overlay-auto-hide-changed', (event) => {
    autoHide = !!event.payload;
    if (autoHide) {
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

  void listen('overlay-snap-edge', () => {
    if (!autoHide || pinnedExpanded) return;
    scheduleAutoHide();
  });
});
