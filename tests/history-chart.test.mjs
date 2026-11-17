import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import vm from 'node:vm';
import test from 'node:test';
const require = createRequire(import.meta.url);
const ts = require('typescript');
const module = { exports: {} };
vm.runInNewContext(
  ts.transpileModule(readFileSync(new URL('../src/history-chart.ts', import.meta.url), 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText,
  { module, exports: module.exports },
);
const { chartPoints, chartX, mergeHistory } = module.exports;
const point = (sequence, timestampMs, extra = {}) => ({
  sequence,
  timestampMs,
  cpuPercent: 1,
  memoryPercent: 1,
  gpuLoadPercent: 1,
  cpuTempCelsius: 40,
  fanRpm: 1000,
  ...extra,
});
test('timestamp axis uses the configured range rather than stretching startup points', () => {
  assert.equal(chartX(59000, 60000, 60000, 300), 295);
  assert.equal(chartX(30000, 60000, 60000, 300), 150);
});
test('time buckets preserve extrema across every series', () => {
  const points = Array.from({ length: 3600 }, (_, i) => point(i + 1, i * 1000));
  for (const [offset, key] of [
    'cpuPercent',
    'memoryPercent',
    'gpuLoadPercent',
    'cpuTempCelsius',
    'fanRpm',
  ].entries()) {
    points[100 + offset][key] = 999;
    points[200 + offset][key] = 0;
  }
  const result = chartPoints(points, 3600000, 3600000, 300);
  for (let i = 0; i < 5; i++) {
    assert.ok(result.some((p) => p.sequence === 101 + i));
    assert.ok(result.some((p) => p.sequence === 201 + i));
  }
  assert.ok(result.length < 600);
});
test('missing sensor intervals and suspended sampling are never bridged', () => {
  const points = [
    point(1, 1000),
    point(2, 2000, { cpuTempCelsius: null }),
    point(3, 3000),
    point(4, 30000),
  ];
  const result = chartPoints(points, 60000, 60000, 10);
  assert.ok(result.some((p) => p.cpuTempCelsius === null));
  assert.equal(result.find((p) => p.sequence === 4).breakBefore, true);
});
test('clock rollback outside the visible window propagates its break', () => {
  const result = chartPoints([point(1, 55000), point(2, 1000), point(3, 56000)], 60000, 10000, 300);
  assert.equal(result.length, 2);
  assert.equal(result[1].breakBefore, true);
});
test('delta merge trims expired points and handles resizes and reset', () => {
  const old = [point(1, 9), point(2, 8)];
  let result = mergeHistory(old, {
    points: [point(3, 8)],
    cursor: 3,
    oldest: 2,
    capacity: 2,
    reset: false,
  });
  assert.deepEqual(
    Array.from(result, (p) => p.sequence),
    [2, 3],
  );
  result = mergeHistory(result, {
    points: [point(1, 7)],
    cursor: 1,
    oldest: 1,
    capacity: 10,
    reset: true,
  });
  assert.deepEqual(
    Array.from(result, (p) => p.sequence),
    [1],
  );
});
