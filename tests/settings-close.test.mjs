import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';
import vm from 'node:vm';

const require = createRequire(import.meta.url);
const ts = require('typescript');
const source = ts.createSourceFile(
  'main.ts',
  readFileSync(new URL('../src/main.ts', import.meta.url), 'utf8'),
  ts.ScriptTarget.Latest,
  true,
);
const code = ts.transpileModule(
  source.statements
    .filter(
      (node) =>
        ts.isFunctionDeclaration(node) && ['showTab', 'bindSettingsUi'].includes(node.name?.text),
    )
    .map((node) => node.getText(source))
    .join('\n'),
  { compilerOptions: { target: ts.ScriptTarget.ES2020 } },
).outputText;

function fixture(close = async () => {}) {
  const listeners = new Map();
  const button = {
    hidden: true,
    disabled: false,
    addEventListener: (name, callback) => listeners.set(name, callback),
  };
  const monitorView = { hidden: false };
  const settingsView = { hidden: true };
  const elements = {
    '#btn-close-settings': button,
    '#view-monitor': monitorView,
    '#view-settings': settingsView,
  };
  const messages = [];
  const context = vm.createContext({
    currentTab: 'monitor',
    document: {
      querySelector: (selector) => elements[selector] ?? null,
      querySelectorAll: () => [],
    },
    getCurrentWindow: () => ({ close }),
    showSettingsMessage: (...args) => messages.push(args),
    console: { error() {} },
  });
  vm.runInContext(code, context);
  context.bindSettingsUi();
  return { context, button, messages, click: () => listeners.get('click')() };
}

test('close control is only shown on the settings tab', () => {
  const { context, button } = fixture();
  context.showTab('settings');
  assert.equal(button.hidden, false);
  context.showTab('monitor');
  assert.equal(button.hidden, true);
});

test('closing requests native close without switching tabs and ignores duplicate clicks', async () => {
  let calls = 0;
  let finish;
  const view = fixture(() => {
    calls += 1;
    return new Promise((resolve) => (finish = resolve));
  });
  view.context.showTab('settings');
  const closing = view.click();
  assert.equal(view.button.disabled, true);
  await view.click();
  assert.equal(calls, 1);
  finish();
  await closing;
  assert.equal(view.context.currentTab, 'settings');
  assert.equal(view.button.disabled, false);
});

test('close failure is reported and the button allows retry', async () => {
  const view = fixture(async () => {
    throw new Error('close failed');
  });
  await view.click();
  assert.equal(view.button.disabled, false);
  assert.deepEqual(view.messages, [['关闭面板失败，请重试', 'error']]);
});
