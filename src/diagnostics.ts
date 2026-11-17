import { invoke } from '@tauri-apps/api/core';
import { isHardwareVisible, hardwareRevision, onHardwareVisibility } from './hardware';
interface Diagnostics {
  running: boolean;
  sampleIntervalMs: number;
  temperatureIntervalMs: number;
  diskIntervalMs: number;
  discoveryIntervalMs: number;
  totalSamples: number;
  lastSuccessAtMs: number | null;
  lastDurationMs: number | null;
  lastError: string | null;
}
export function initializeDiagnostics() {
  const detail = document.querySelector<HTMLDetailsElement>('#monitor-diagnostics');
  const output = document.querySelector<HTMLElement>('#diagnostics-output');
  if (!detail || !output) return;
  let busy = false;
  let disposed = false;
  const eligible = () =>
    !disposed && isHardwareVisible() && detail.open && !detail.closest('[hidden]');
  const refresh = async () => {
    if (!eligible() || busy) return;
    const revision = hardwareRevision();
    busy = true;
    try {
      const [data, temp] = await Promise.all([
        invoke<Diagnostics>('get_monitor_diagnostics'),
        invoke<{ message: string }>('get_temp_source_status'),
      ]);
      if (!eligible() || revision !== hardwareRevision()) return;
      output.textContent = [
        data.running ? '后台采样运行中' : '后台采样已停止',
        '累计采样：' + data.totalSamples,
        '最近成功：' +
          (data.lastSuccessAtMs
            ? new Date(data.lastSuccessAtMs).toLocaleString('zh-CN')
            : '尚无数据'),
        '本次耗时：' + (data.lastDurationMs ?? '--') + ' ms',
        '刷新间隔：硬件 ' +
          data.sampleIntervalMs / 1000 +
          ' 秒 / 传感器 ' +
          data.temperatureIntervalMs / 1000 +
          ' 秒',
        '磁盘容量 ' +
          data.diskIntervalMs / 1000 +
          ' 秒 / 磁盘发现 ' +
          data.discoveryIntervalMs / 1000 +
          ' 秒',
        temp.message,
        data.lastError ? '最近错误：' + data.lastError : '最近错误：无',
      ].join('\n');
    } catch (error) {
      if (eligible() && revision === hardwareRevision())
        output.textContent = '读取诊断失败：' + String(error);
    } finally {
      busy = false;
    }
  };
  detail.addEventListener('toggle', refresh);
  document.querySelector('#refresh-diagnostics')?.addEventListener('click', refresh);
  const stop = onHardwareVisibility((value) => {
    if (value) void refresh();
  });
  window.addEventListener(
    'beforeunload',
    () => {
      disposed = true;
      stop();
    },
    { once: true },
  );
}
