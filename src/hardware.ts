import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

let visible = false;
let revision = 0;
const visibilityListeners = new Set<(visible: boolean) => void>();
export function onHardwareVisibility(callback: (visible: boolean) => void) {
  visibilityListeners.add(callback);
  return () => {
    visibilityListeners.delete(callback);
  };
}

export function isHardwareVisible() {
  return visible;
}

export function hardwareRevision() {
  return revision;
}

export function initializeHardware<T>(render: (metrics: T) => Promise<void>) {
  let disposed = false;
  let busy = false;
  let requested = false;
  let pending: T | undefined;
  const unlisteners: UnlistenFn[] = [];
  const flush = async () => {
    if (busy || disposed || !visible) return;
    busy = true;
    const currentRevision = revision;
    requested = false;
    const snapshot = pending;
    pending = undefined;
    try {
      const metrics = snapshot ?? (await invoke<T>('get_metrics'));
      if (!disposed && visible && currentRevision === revision) await render(metrics);
    } catch (error) {
      if (!disposed && visible && currentRevision === revision) {
        document.querySelector('#updated-at, #overlay-updated')?.replaceChildren('数据中断');
        document.querySelector('.overlay')?.classList.add('metrics-stale');
        console.error('读取后台监测快照失败', error);
      }
    } finally {
      busy = false;
      if (requested) void flush();
    }
  };
  const request = (snapshot?: T) => {
    if (!visible || disposed) return;
    pending = snapshot;
    requested = true;
    void flush();
  };
  const setVisible = (next: boolean) => {
    if (disposed) return;
    visible = next;
    revision += 1;
    pending = undefined;
    requested = false;
    visibilityListeners.forEach((callback) => callback(visible));
    if (visible) request();
  };
  const register = async (promise: Promise<UnlistenFn>) => {
    const unlisten = await promise;
    if (disposed) unlisten();
    else unlisteners.push(unlisten);
  };
  void (async () => {
    await register(listen<T>('metrics-updated', (event) => request(event.payload)));
    await register(
      listen<boolean>('window-visibility-changed', (event) => setVisible(event.payload)),
    );
    const requestedRevision = revision;
    const initialVisible = await invoke<boolean>('get_window_visibility');
    if (requestedRevision === revision) setVisible(initialVisible);
  })().catch((error) => console.error('初始化监测显示失败', error));
  const watchdog = window.setInterval(() => {
    const currentRevision = revision;
    void invoke<boolean>('get_window_visibility')
      .then((next) => {
        if (disposed || currentRevision !== revision) return;
        if (next !== visible) setVisible(next);
        else if (visible) request();
      })
      .catch((error) => console.error('读取窗口可见状态失败', error));
  }, 5000);
  window.addEventListener(
    'beforeunload',
    () => {
      disposed = true;
      visible = false;
      revision += 1;
      window.clearInterval(watchdog);
      unlisteners.splice(0).forEach((unlisten) => unlisten());
    },
    { once: true },
  );
}
