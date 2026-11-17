import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';

const require = createRequire(import.meta.url);
const ts = require('typescript');
const code = ts.transpileModule(
  readFileSync(new URL('../src/hardware.ts', import.meta.url), 'utf8'),
  { compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS } },
).outputText;

async function settle() {
  for (let i = 0; i < 16; i += 1) await Promise.resolve();
}

function fixture({ visible = false, snapshot = () => Promise.resolve({ value: 0 }), render } = {}) {
  const listeners = new Map();
  const timers = new Map();
  const events = new Map();
  const calls = [];
  const rendered = [];
  let unlistened = 0;
  const module = { exports: {} };
  const context = vm.createContext({
    module,
    exports: module.exports,
    require(name) {
      if (name === '@tauri-apps/api/core')
        return {
          invoke: async (command) => {
            calls.push(command);
            return command === 'get_window_visibility' ? visible : snapshot();
          },
        };
      if (name === '@tauri-apps/api/event')
        return {
          listen: async (name, callback) => {
            listeners.set(name, callback);
            return () => {
              listeners.delete(name);
              unlistened += 1;
            };
          },
        };
      throw new Error(name);
    },
    console: { error() {} },
    document: { querySelector: () => null },
    window: {
      setInterval(callback) {
        timers.set(1, callback);
        return 1;
      },
      clearInterval(id) {
        timers.delete(id);
      },
      addEventListener(name, callback) {
        events.set(name, callback);
      },
    },
  });
  vm.runInContext(code, context);
  module.exports.initializeHardware(async (metrics) => {
    rendered.push(metrics.value);
    if (render) await render(metrics);
  });
  return {
    calls,
    rendered,
    timers,
    setVisible(value) {
      visible = value;
      listeners.get('window-visibility-changed')({ payload: value });
    },
    emit(value) {
      listeners.get('metrics-updated')({ payload: { value } });
    },
    unload() {
      events.get('beforeunload')();
    },
    get unlistened() {
      return unlistened;
    },
  };
}

test('hidden windows do not fetch or render metrics, and showing fetches immediately', async () => {
  const feed = fixture();
  await settle();
  feed.emit(1);
  feed.timers.get(1)();
  await settle();
  assert.equal(feed.calls.filter((command) => command === 'get_metrics').length, 0);
  assert.deepEqual(feed.rendered, []);
  feed.setVisible(true);
  await settle();
  assert.deepEqual(feed.rendered, [0]);
  feed.emit(2);
  await settle();
  assert.deepEqual(feed.rendered, [0, 2]);
  feed.unload();
  assert.equal(feed.timers.size, 0);
  assert.equal(feed.unlistened, 2);
});

test('a snapshot started before hiding cannot render after the visibility revision changes', async () => {
  let resolve;
  let attempt = 0;
  const feed = fixture({
    snapshot: () =>
      ++attempt === 1
        ? new Promise((done) => {
            resolve = done;
          })
        : Promise.resolve({ value: 2 }),
  });
  await settle();
  feed.setVisible(true);
  await settle();
  feed.setVisible(false);
  resolve({ value: 1 });
  await settle();
  assert.deepEqual(feed.rendered, []);
  feed.setVisible(true);
  await settle();
  assert.deepEqual(feed.rendered, [2]);
});

test('slow rendering coalesces updates and never renders concurrently', async () => {
  let release;
  let active = 0;
  let maximum = 0;
  const feed = fixture({
    visible: true,
    render: async ({ value }) => {
      active += 1;
      maximum = Math.max(maximum, active);
      if (value === 1)
        await new Promise((done) => {
          release = done;
        });
      active -= 1;
    },
  });
  await settle();
  feed.emit(1);
  await settle();
  feed.emit(2);
  feed.emit(3);
  release();
  await settle();
  assert.deepEqual(feed.rendered, [0, 1, 3]);
  assert.equal(maximum, 1);
});
