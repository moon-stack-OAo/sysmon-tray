export interface HistoryPoint {
  sequence: number;
  breakBefore?: boolean;
  cpuPercent: number;
  memoryPercent: number;
  cpuTempCelsius: number | null;
  gpuLoadPercent: number | null;
  gpuTempCelsius: number | null;
  fanRpm: number | null;
  fanName: string | null;
  netDownBps: number;
  netUpBps: number;
  timestampMs: number;
}

export interface HistoryBatch {
  points: HistoryPoint[];
  cursor: number;
  oldest: number;
  capacity: number;
  reset: boolean;
}
export function mergeHistory(previous: HistoryPoint[], batch: HistoryBatch): HistoryPoint[] {
  const map = new Map<number, HistoryPoint>();
  for (const p of batch.reset ? [] : previous)
    if (p.sequence >= batch.oldest) map.set(p.sequence, p);
  for (const p of batch.points) map.set(p.sequence, p);
  return [...map.values()].sort((a, b) => a.sequence - b.sequence).slice(-batch.capacity);
}
export function chartPoints(
  points: HistoryPoint[],
  end: number,
  rangeMs: number,
  pixels: number,
): HistoryPoint[] {
  const start = end - rangeMs;
  const series = [
    'cpuPercent',
    'memoryPercent',
    'gpuLoadPercent',
    'cpuTempCelsius',
    'fanRpm',
  ] as const;
  let interrupted = false;
  const visible: HistoryPoint[] = [];
  points.forEach((p, i) => {
    if (
      i > 0 &&
      (p.timestampMs <= points[i - 1].timestampMs ||
        p.timestampMs - points[i - 1].timestampMs > 5000)
    )
      interrupted = true;
    if (p.timestampMs >= start && p.timestampMs <= end) {
      visible.push({ ...p, breakBefore: interrupted });
      interrupted = false;
    } else if (visible.length) interrupted = true;
  });
  if (visible.length < 3) return visible;
  const buckets = new Map<number, number[]>();
  const count = Math.max(1, Math.floor(pixels / 12));
  visible.forEach((p, i) => {
    const key = Math.min(count - 1, Math.floor(((p.timestampMs - start) / rangeMs) * count));
    const bucket = buckets.get(key) ?? [];
    bucket.push(i);
    buckets.set(key, bucket);
  });
  const selected = new Set<number>([0, visible.length - 1]);
  for (const bucket of buckets.values()) {
    selected.add(bucket[0]);
    selected.add(bucket[bucket.length - 1]);
    for (const field of series) {
      const values = bucket.filter(
        (i) => visible[i][field] != null && Number.isFinite(visible[i][field]),
      );
      if (values.length) {
        selected.add(values.reduce((a, b) => (visible[a][field]! < visible[b][field]! ? a : b)));
        selected.add(values.reduce((a, b) => (visible[a][field]! > visible[b][field]! ? a : b)));
      }
    }
  }
  visible.forEach((p, i) => {
    if (
      i > 0 &&
      (p.breakBefore ||
        series.some((field) => (p[field] == null) != (visible[i - 1][field] == null)))
    ) {
      selected.add(i - 1);
      selected.add(i);
    }
  });
  return [...selected].sort((a, b) => a - b).map((i) => visible[i]);
}
export function chartX(timestamp: number, end: number, rangeMs: number, width: number) {
  return Math.max(0, Math.min(width, ((timestamp - (end - rangeMs)) / rangeMs) * width));
}
