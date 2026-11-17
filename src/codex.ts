import { isHardwareVisible, hardwareRevision, onHardwareVisibility } from './hardware';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

interface QuotaWindow {
  id: string;
  label: string;
  windowDurationMins: number | null;
  remainingPercent: number | null;
  resetsAt: number | null;
}

interface QuotaBucket {
  id: string;
  name: string;
  plan: string | null;
  windows: QuotaWindow[];
}

interface QuotaSnapshot {
  enabled: boolean;
  refreshing: boolean;
  stale: boolean;
  observedAtMs: number | null;
  nextRefreshAtMs: number | null;
  error: string | null;
  buckets: QuotaBucket[];
}

function resetText(at: number | null): string {
  if (!at) return '重置时间未知';
  const seconds = Math.ceil(at - Date.now() / 1000);
  if (seconds <= 0) return '已到重置时间，等待同步';
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  return `${days ? `${days}天 ` : ''}${hours}小时 ${minutes}分后重置`;
}

function percentage(window: QuotaWindow | undefined): string {
  return window?.remainingPercent == null ? '--' : `${window.remainingPercent}%`;
}

function resetAtText(at: number | null | undefined): string {
  if (!at) return '重置时间未知';
  const date = new Date(at * 1000);
  if (!Number.isFinite(date.getTime())) return '重置时间未知';
  return date.toLocaleString('zh-CN', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  });
}

function compactResetText(at: number | null | undefined): string {
  if (!at || !Number.isFinite(at)) return '未知';
  const minutes = Math.ceil((at * 1000 - Date.now()) / 60000);
  if (minutes <= 0) return '待同步';
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  const remainingMinutes = minutes % 60;
  return [
    days ? `${days}d` : '',
    hours ? `${hours}h` : '',
    remainingMinutes ? `${remainingMinutes}m` : '',
  ]
    .filter(Boolean)
    .join(' ');
}

function shortLabel(window: QuotaWindow | undefined, position: string): string {
  if (window?.windowDurationMins === 10080) return '周';
  const minutes = window?.windowDurationMins;
  if (minutes) return minutes % 60 === 0 ? `${minutes / 60}h` : `${minutes}m`;
  return position === 'primary' ? '额度一' : '额度二';
}

function element(tag: string, className: string, text?: string): HTMLElement {
  const result = document.createElement(tag);
  result.className = className;
  if (text !== undefined) result.textContent = text;
  return result;
}

export function initializeCodex() {
  let current: QuotaSnapshot | null = null;
  let busy = false;
  let requested = false;
  let disposed = false;
  const unlisteners: UnlistenFn[] = [];
  const register = async (promise: Promise<UnlistenFn>) => {
    const stop = await promise;
    if (disposed) stop();
    else unlisteners.push(stop);
  };
  let signature = '';
  const render = (snapshot: QuotaSnapshot) => {
    if (disposed || !isHardwareVisible()) return;
    current = snapshot;
    const fresh = snapshot.enabled && !snapshot.stale;
    const primaryBucket = snapshot.buckets.find((b) => b.id === 'codex') ?? snapshot.buckets[0];
    for (const position of ['primary', 'secondary']) {
      const window = primaryBucket?.windows.find((w) => w.id === position);
      document.querySelectorAll<HTMLElement>(`[data-codex-value="${position}"]`).forEach((node) => {
        node.textContent = !snapshot.enabled ? '关' : percentage(window);
      });
      document.querySelectorAll<HTMLElement>(`[data-codex-short="${position}"]`).forEach((node) => {
        node.textContent = shortLabel(window, position);
      });
      document
        .querySelectorAll<HTMLElement>(`[data-codex-reset-in="${position}"]`)
        .forEach((node) => {
          node.textContent = snapshot.enabled ? compactResetText(window?.resetsAt) : '--';
          if (snapshot.enabled && window?.resetsAt) {
            node.dataset.codexResetCompact = String(window.resetsAt);
          } else {
            delete node.dataset.codexResetCompact;
          }
          node.title = snapshot.enabled
            ? `距重置剩余时间\n重置时间：${resetAtText(window?.resetsAt)}（本地时间）`
            : 'Codex 额度监测已关闭';
        });
      document
        .querySelectorAll<HTMLElement>(`[data-metric="codex_${position}"]`)
        .forEach((node) => {
          const pendingReset = !!window?.resetsAt && window.resetsAt <= Date.now() / 1000;
          node.classList.toggle('quota-stale', !fresh || pendingReset);
          node.classList.toggle(
            'alert',
            fresh &&
              !pendingReset &&
              window?.remainingPercent != null &&
              window.remainingPercent <= 20,
          );
          node.title = !snapshot.enabled
            ? 'Codex 额度监测已关闭'
            : `${primaryBucket?.name ?? 'Codex'} · ${window?.label ?? '额度'}剩余 ${percentage(window)}${snapshot.stale ? '（数据过期）' : ''}\n重置时间：${resetAtText(window?.resetsAt)}（本地时间）\n${resetText(window?.resetsAt ?? null)}${snapshot.error ? `\n${snapshot.error}` : ''}`;
        });
    }
    document.querySelectorAll<HTMLElement>('[data-codex-status]').forEach((node) => {
      const time = snapshot.observedAtMs
        ? new Date(snapshot.observedAtMs).toLocaleTimeString('zh-CN', {
            hour: '2-digit',
            minute: '2-digit',
          })
        : '';
      const retrySeconds =
        snapshot.nextRefreshAtMs == null
          ? null
          : Math.max(0, Math.ceil((snapshot.nextRefreshAtMs - Date.now()) / 1000));
      const retryText =
        retrySeconds == null
          ? '等待自动重试'
          : retrySeconds === 0
            ? '即将自动重试'
            : `${retrySeconds} 秒后自动重试`;
      node.textContent = !snapshot.enabled
        ? '已关闭 · 可在设置中启用'
        : snapshot.refreshing
          ? '正在同步…'
          : snapshot.error
            ? `同步暂时失败 · ${retryText}`
            : snapshot.stale
              ? '等待同步 · 数据过期'
              : `${primaryBucket?.plan?.toUpperCase() ?? 'CODEX'} · ${time} 同步`;
      node.title = snapshot.error
        ? `${snapshot.error}\n${retryText}${time ? ` · 保留 ${time} 查询的额度数据` : ''}`
        : node.textContent;
      node.classList.toggle('quota-stale', !fresh);
    });
    document.querySelectorAll<HTMLButtonElement>('[data-codex-refresh]').forEach((button) => {
      button.disabled = !snapshot.enabled || snapshot.refreshing;
      button.textContent = snapshot.refreshing ? '同步中' : '刷新';
    });
    const pendingResets = snapshot.buckets.flatMap((b) =>
      b.windows.map((w) => !!w.resetsAt && w.resetsAt <= Date.now() / 1000),
    );
    const nextSignature = JSON.stringify([
      snapshot.enabled,
      snapshot.stale,
      snapshot.buckets,
      pendingResets,
    ]);
    if (nextSignature !== signature) {
      signature = nextSignature;
      document.querySelectorAll<HTMLElement>('[data-codex-windows]').forEach((container) => {
        const rows: HTMLElement[] = [];
        if (snapshot.enabled) {
          for (const bucket of snapshot.buckets) {
            for (const window of bucket.windows) {
              const row = element('div', 'quota-window');
              const heading = element('div', 'quota-heading');
              heading.append(
                element(
                  'span',
                  '',
                  snapshot.buckets.length > 1 ? `${bucket.name} · ${window.label}` : window.label,
                ),
                element('strong', 'quota-value', percentage(window)),
              );
              const track = element('div', 'quota-track'),
                fill = element('div', 'quota-fill');
              fill.style.width = `${window.remainingPercent ?? 0}%`;
              const pendingReset = !!window.resetsAt && window.resetsAt <= Date.now() / 1000;
              row.classList.toggle('quota-stale', !fresh || pendingReset);
              row.classList.toggle(
                'quota-low',
                fresh &&
                  !pendingReset &&
                  window.remainingPercent != null &&
                  window.remainingPercent <= 20,
              );
              row.classList.toggle(
                'quota-critical',
                fresh &&
                  !pendingReset &&
                  window.remainingPercent != null &&
                  window.remainingPercent <= 10,
              );
              track.append(fill);
              const reset = element('div', 'quota-reset', resetText(window.resetsAt));
              if (window.resetsAt) {
                reset.dataset.codexReset = String(window.resetsAt);
                reset.title = `重置时间：${new Date(window.resetsAt * 1000).toLocaleString('zh-CN')}`;
              }
              row.append(heading, track, reset);
              rows.push(row);
            }
          }
        }
        if (!rows.length)
          rows.push(
            element(
              'div',
              'quota-empty',
              snapshot.enabled ? '等待 Codex 返回额度' : '启用后显示套餐余量和重置时间',
            ),
          );
        container.replaceChildren(...rows);
      });
    }
  };
  const poll = async () => {
    if (disposed || !isHardwareVisible()) return;
    if (busy) {
      requested = true;
      return;
    }
    busy = true;
    requested = false;
    const revision = hardwareRevision();
    try {
      const snapshot = await invoke<QuotaSnapshot>('get_codex_quota');
      if (revision === hardwareRevision()) render(snapshot);
    } catch {
      if (current && revision === hardwareRevision())
        render({ ...current, stale: true, refreshing: false, error: '额度数据暂时无法读取' });
    } finally {
      busy = false;
      if (requested && !disposed) void poll();
    }
  };
  document.querySelectorAll<HTMLButtonElement>('[data-codex-refresh]').forEach((button) => {
    button.addEventListener('click', async () => {
      if (!isHardwareVisible() || disposed) return;
      const revision = hardwareRevision();
      button.disabled = true;
      try {
        const snapshot = await invoke<QuotaSnapshot>('refresh_codex_quota');
        if (revision === hardwareRevision()) render(snapshot);
      } catch {
        if (current && revision === hardwareRevision())
          render({ ...current, stale: true, refreshing: false, error: '刷新请求失败，请稍后重试' });
      } finally {
        void poll();
      }
    });
  });
  void register(
    listen('codex-settings-changed', () => {
      void poll();
    }),
  ).catch(console.error);
  void register(
    listen('codex-quota-updated', () => {
      void poll();
    }),
  ).catch(console.error);
  void poll();
  const fallback = window.setInterval(() => {
    void poll();
  }, 60000);
  const stopVisibility = onHardwareVisibility((visible) => {
    if (visible) void poll();
  });
  const countdown = window.setInterval(() => {
    if (disposed || !isHardwareVisible()) return;
    if (current) render(current);
    document.querySelectorAll<HTMLElement>('[data-codex-reset-compact]').forEach((node) => {
      node.textContent = compactResetText(Number(node.dataset.codexResetCompact));
    });
    document.querySelectorAll<HTMLElement>('[data-codex-reset]').forEach((node) => {
      node.textContent = resetText(Number(node.dataset.codexReset));
    });
  }, 1000);
  window.addEventListener(
    'beforeunload',
    () => {
      disposed = true;
      stopVisibility();
      window.clearInterval(fallback);
      window.clearInterval(countdown);
      unlisteners.forEach((stop) => stop());
    },
    { once: true },
  );
}
