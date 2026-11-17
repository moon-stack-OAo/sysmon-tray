import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import vm from 'node:vm';
import test from 'node:test';
const require = createRequire(import.meta.url),
  ts = require('typescript');
const code = ts.transpileModule(readFileSync(new URL('../src/codex.ts', import.meta.url), 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText;
const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};
function fixture(deferred = false) {
  let visible = false,
    revision = 0,
    subscription,
    unload,
    resolve,
    calls = 0,
    writes = 0,
    stops = 0;
  const events = new Map(),
    timers = new Map();
  const value = {
    set textContent(v) {
      writes++;
    },
  };
  const snapshot = {
    enabled: true,
    refreshing: false,
    stale: false,
    observedAtMs: Date.now(),
    nextRefreshAtMs: null,
    error: null,
    buckets: [],
  };
  const module = { exports: {} };
  vm.runInNewContext(code, {
    module,
    exports: module.exports,
    Date,
    console,
    require(name) {
      if (name === './hardware')
        return {
          isHardwareVisible: () => visible,
          hardwareRevision: () => revision,
          onHardwareVisibility: (fn) => {
            subscription = fn;
            return () => stops++;
          },
        };
      if (name.includes('/core'))
        return {
          invoke: () => {
            calls++;
            return deferred ? new Promise((done) => (resolve = done)) : Promise.resolve(snapshot);
          },
        };
      if (name.includes('/event'))
        return {
          listen: async (name, fn) => {
            events.set(name, fn);
            return () => stops++;
          },
        };
      throw Error(name);
    },
    document: {
      querySelectorAll: (selector) => (selector === '[data-codex-value="primary"]' ? [value] : []),
    },
    window: {
      setInterval(fn, ms) {
        timers.set(ms, fn);
        return ms;
      },
      clearInterval(id) {
        timers.delete(id);
      },
      addEventListener(name, fn) {
        unload = fn;
      },
      setTimeout() {},
    },
  });
  module.exports.initializeCodex();
  return {
    snapshot,
    events,
    timers,
    get calls() {
      return calls;
    },
    get writes() {
      return writes;
    },
    get stops() {
      return stops;
    },
    show(next) {
      visible = next;
      revision++;
      subscription(next);
    },
    finish() {
      resolve(snapshot);
    },
    unload() {
      unload();
    },
  };
}
test('Codex does no cache polling or countdown rendering while hidden, resumes immediately', async () => {
  const f = fixture();
  await settle();
  f.events.get('codex-quota-updated')();
  f.timers.get(60000)();
  f.timers.get(1000)();
  await settle();
  assert.equal(f.calls, 0);
  assert.equal(f.writes, 0);
  f.show(true);
  await settle();
  assert.equal(f.calls, 1);
  assert.ok(f.writes > 0);
  f.show(false);
  const before = f.writes;
  f.timers.get(1000)();
  assert.equal(f.writes, before);
});
test('Codex drops hidden responses and releases timers and listeners on unload', async () => {
  const f = fixture(true);
  await settle();
  f.show(true);
  await settle();
  f.show(false);
  f.finish();
  await settle();
  assert.equal(f.writes, 0);
  f.unload();
  assert.equal(f.timers.size, 0);
  assert.equal(f.stops, 3);
});
