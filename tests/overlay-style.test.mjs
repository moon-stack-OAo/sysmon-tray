import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';

const require = createRequire(import.meta.url);
const ts = require('typescript');
const source = ts.createSourceFile(
  'overlay.ts',
  readFileSync(new URL('../src/overlay.ts', import.meta.url), 'utf8'),
  ts.ScriptTarget.Latest,
  true,
);
const names = new Set([
  'mode',
  'style',
  'layoutQueue',
  'layoutDepth',
  'autoHide',
  'edgeX',
  'hideTimer',
  'pinnedExpanded',
  'overlayEnabled',
  'clickThroughEnabled',
  'overlaySettingsRevision',
  'overlayConfigRequest',
  'overlayConfigLoaded',
  'overlayStyleApplied',
  'normalizeStyle',
  'applyStyleClass',
  'applyModeClass',
  'clearHideTimer',
  'enqueueLayout',
  'applyLayout',
  'setMode',
  'canPeekHide',
  'applyOverlayStyle',
  'loadOverlayConfig',
  'initializeOverlaySettings',
]);
const code = ts.transpileModule(
  source.statements
    .filter((node) => {
      if (ts.isFunctionDeclaration(node)) return names.has(node.name?.text);
      return (
        ts.isVariableStatement(node) &&
        node.declarationList.declarations.every((declaration) =>
          names.has(declaration.name.getText()),
        )
      );
    })
    .map((node) => node.getText(source))
    .join('\n'),
  { compilerOptions: { target: ts.ScriptTarget.ES2020 } },
).outputText;

function fixture() {
  const classes = new Set(['overlay', 'collapsed', 'style-capsule']);
  const calls = [];
  const errors = [];
  const listeners = new Map();
  const intervals = new Map();
  const failedEvents = new Set();
  let visible = true;
  let metrics = '[]';
  let config = { overlayEnabled: true, overlayStyle: 'capsule', overlayAutoHide: false };
  let readConfig = async () => config;
  let resize = async () => {};
  const root = {
    classList: {
      add: (...values) => values.forEach((value) => classes.add(value)),
      remove: (...values) => values.forEach((value) => classes.delete(value)),
      toggle: (value, active) => (active ? classes.add(value) : classes.delete(value)),
    },
  };
  const context = vm.createContext({
    document: { querySelector: () => root },
    window: {
      clearTimeout() {},
      setInterval: (callback, delay) => {
        intervals.set(delay, callback);
        return delay;
      },
      clearInterval: (id) => intervals.delete(id),
      addEventListener() {},
    },
    requestAnimationFrame: (callback) => callback(),
    invoke: async (command, args) => {
      calls.push({ command, args });
      if (command === 'get_app_config') return readConfig();
      if (command === 'set_overlay_layout') return resize(args);
      throw new Error(`Unexpected command: ${command}`);
    },
    listen: async (name, callback) => {
      if (failedEvents.has(name)) throw new Error(`Cannot listen: ${name}`);
      listeners.set(name, callback);
      return () => listeners.delete(name);
    },
    applyOverlayMetrics: (ids) => {
      const next = JSON.stringify(ids ?? []);
      const changed = metrics !== next;
      metrics = next;
      return changed;
    },
    syncClickThroughRuntime: async () => {},
    disableClickThroughRuntime: async () => {},
    scheduleAutoHide() {},
    lastMetrics: null,
    isHardwareVisible: () => visible,
    console: { error: (...args) => errors.push(args) },
  });
  vm.runInContext(code, context);
  return {
    context,
    classes,
    calls,
    errors,
    listeners,
    intervals,
    setConfig: (value) => (config = { ...config, ...value }),
    deferConfig: (reader) => (readConfig = reader),
    setResize: (callback) => (resize = callback),
    setVisible: (value) => (visible = value),
    failEvent: (name) => failedEvents.add(name),
    state: (expression) => vm.runInContext(expression, context),
    layouts: () => calls.filter(({ command }) => command === 'set_overlay_layout'),
  };
}

test('a shape change leaves the peek hot zone and updates both layout and classes', async () => {
  const view = fixture();
  view.state("mode = 'peek'; applyModeClass(mode)");
  await view.context.applyOverlayStyle('vertical');
  assert.equal(view.state('mode'), 'collapsed');
  assert.equal(view.classes.has('style-vertical'), true);
  assert.equal(view.classes.has('peek'), false);
  assert.equal(view.layouts().at(-1).args.mode, 'collapsed');
});

test('a failed resize keeps the last applied shape and allows retry', async () => {
  const view = fixture();
  view.setResize(async () => {
    throw new Error('resize failed');
  });
  await assert.rejects(view.context.applyOverlayStyle('vertical'), /resize failed/);
  assert.equal(view.state('style'), 'capsule');
  assert.equal(view.classes.has('style-capsule'), true);
  assert.equal(view.classes.has('layout-switching'), false);
  view.setResize(async () => {});
  await view.context.applyOverlayStyle('vertical');
  assert.equal(view.classes.has('style-vertical'), true);
});

test('refreshing unchanged settings preserves an expanded panel', async () => {
  const view = fixture();
  await view.context.loadOverlayConfig();
  await view.context.setMode('expanded');
  const before = view.layouts().length;
  await view.context.loadOverlayConfig();
  assert.equal(view.state('mode'), 'expanded');
  assert.equal(view.layouts().length, before);
});

test('refreshing the config recovers a missed shape event', async () => {
  const view = fixture();
  await view.context.loadOverlayConfig();
  view.setConfig({ overlayStyle: 'numeric' });
  await view.context.loadOverlayConfig();
  assert.equal(view.state('style'), 'numeric');
  assert.equal(view.classes.has('style-numeric'), true);
  assert.equal(view.classes.has('style-capsule'), false);
});

test('the latest config request wins when responses arrive out of order', async () => {
  const view = fixture();
  let finishOldRead;
  view.deferConfig(() => new Promise((resolve) => (finishOldRead = resolve)));
  const oldRead = view.context.loadOverlayConfig();
  view.deferConfig(async () => ({ overlayStyle: 'vertical' }));
  await view.context.loadOverlayConfig();
  finishOldRead({ overlayStyle: 'capsule' });
  await oldRead;
  assert.equal(view.state('style'), 'vertical');
});

test('listeners are ready before reading config and a live event invalidates an older read', async () => {
  const view = fixture();
  let finishRead;
  view.deferConfig(() => new Promise((resolve) => (finishRead = resolve)));
  const initialization = view.context.initializeOverlaySettings();
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(view.listeners.has('overlay-style-changed'));
  view.listeners.get('overlay-style-changed')({ payload: 'vertical' });
  finishRead({ overlayStyle: 'capsule' });
  await initialization;
  await view.state('layoutQueue');
  assert.equal(view.state('style'), 'vertical');
});

test('visible overlays reconcile missed events periodically without repeated resizes', async () => {
  const view = fixture();
  await view.context.initializeOverlaySettings();
  const watchdog = view.intervals.get(5000);
  assert.equal(typeof watchdog, 'function');
  view.setConfig({ overlayStyle: 'vertical' });
  await watchdog();
  assert.equal(view.state('style'), 'vertical');
  const before = view.layouts().length;
  await watchdog();
  assert.equal(view.layouts().length, before);
});

test('a failed event subscription still loads the saved shape and starts reconciliation', async () => {
  const view = fixture();
  view.failEvent('overlay-style-changed');
  view.setConfig({ overlayStyle: 'numeric' });
  await view.context.initializeOverlaySettings();
  assert.equal(view.state('style'), 'numeric');
  assert.ok(view.intervals.has(5000));
  assert.equal(view.errors.length, 1);
});

test('hidden overlays skip periodic config reads', async () => {
  const view = fixture();
  await view.context.initializeOverlaySettings();
  view.setVisible(false);
  const before = view.calls.length;
  await view.intervals.get(5000)();
  assert.equal(view.calls.length, before);
});

test('showing the overlay reconciles its shape immediately', async () => {
  const view = fixture();
  await view.context.initializeOverlaySettings();
  view.setConfig({ overlayStyle: 'vertical' });
  view.listeners.get('window-visibility-changed')({ payload: true });
  await new Promise((resolve) => setImmediate(resolve));
  await view.state('layoutQueue');
  assert.equal(view.state('style'), 'vertical');
});

test('metric changes resize a collapsed shape but do not collapse an expanded panel', async () => {
  const view = fixture();
  await view.context.loadOverlayConfig();
  view.setConfig({ overlayMetrics: ['cpu', 'mem'] });
  const initial = view.layouts().length;
  await view.context.loadOverlayConfig();
  assert.equal(view.layouts().length, initial + 1);
  await view.context.setMode('expanded');
  const expanded = view.layouts().length;
  view.setConfig({ overlayMetrics: ['cpu'] });
  await view.context.loadOverlayConfig();
  assert.equal(view.state('mode'), 'expanded');
  assert.equal(view.layouts().length, expanded);
});
