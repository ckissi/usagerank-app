import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../dist/main.js', import.meta.url), 'utf8');
const snapshot = () => ({
  signedIn: false, settings: { display: 'tokens', claude: true, codex: true, intervalMinutes: 30 },
  webUrl: 'https://example.test', sources: [], ui: {}, errors: [], limits: {},
});
const flush = async () => {
  for (let i = 0; i < 4; i++) await new Promise((resolve) => setImmediate(resolve));
};

function app({ invoke, listen } = {}) {
  const cardHandlers = new Map();
  const windowHandlers = new Map();
  const subscriptions = new Map();
  const calls = [];
  const errors = [];
  let resize;
  const element = () => ({
    innerHTML: '', style: {}, offsetHeight: 760,
    classList: { add() {}, remove() {}, toggle() {} },
    setAttribute() {}, querySelector() { return null; },
    addEventListener(name, callback) { cardHandlers.set(name, callback); },
  });
  const card = element();
  const context = vm.createContext({
    window: {
      __TAURI__: {
        core: { invoke: async (command, args) => {
          calls.push([command, args]);
          if (invoke) return invoke(command, args);
          return command === 'get_snapshot' ? snapshot() : undefined;
        } },
        event: { listen: async (name, callback) => {
          if (listen) await listen(name);
          subscriptions.set(name, callback);
          return () => subscriptions.delete(name);
        } },
      },
      addEventListener(name, callback) { windowHandlers.set(name, callback); },
    },
    document: {
      getElementById: (id) => ['card', 'shell'].includes(id) ? card : null,
      createElement: element, body: { appendChild() {} }, activeElement: null,
    },
    localStorage: { getItem() { return null; }, setItem() {} },
    ResizeObserver: class { constructor(callback) { resize = callback; } observe() {} },
    setInterval() {}, console: { error(error) { errors.push(error); } },
  });
  vm.runInContext(source, context);
  return {
    card, calls, errors, subscriptions, context,
    evaluate: (code) => vm.runInContext(code, context),
    click: (button) => cardHandlers.get('click')({ target: { closest: () => ({ dataset: {}, ...button }) } }),
    change: (id, value) => cardHandlers.get('change')({ target: { id, value } }),
    key: (key) => windowHandlers.get('keydown')({ metaKey: true, key, preventDefault() {} }),
    resize: () => resize(),
  };
}

test('startup rejection displays an error and retry recovers the snapshot', async () => {
  let broken = true;
  const ui = app({ invoke: (command) => {
    if (command === 'get_snapshot') {
      if (broken) throw new Error('snapshot unavailable');
      return snapshot();
    }
  } });
  await flush();
  assert.match(ui.card.innerHTML, /snapshot unavailable/);
  assert.match(ui.card.innerHTML, /Try again/);
  broken = false;
  await ui.click({ dataset: { act: 'retry' } });
  assert.match(ui.card.innerHTML, /This Mac/);
  assert.doesNotMatch(ui.card.innerHTML, /snapshot unavailable/);
  assert.equal(ui.subscriptions.size, 2);
});

test('failed event registration reports the failure and retry registers only missing listeners', async () => {
  let broken = true;
  const ui = app({ listen: (name) => {
    if (broken && name === 'snapshot') throw new Error('listener unavailable');
  } });
  await flush();
  assert.match(ui.card.innerHTML, /listener unavailable/);
  assert.equal(ui.subscriptions.size, 1);
  broken = false;
  await ui.click({ dataset: { act: 'retry' } });
  assert.equal(ui.subscriptions.size, 2);
});

test('malformed cached lists and preferences do not break rendering', async () => {
  const ui = app();
  await flush();
  assert.doesNotThrow(() => ui.evaluate(`
    prefsApplied = false;
    applyPrefs({expanded: [], tool: {}});
    snap = {...snap, signedIn:true, stats:{periods:{'1':{neighbors:{}, models:{}}}}};
    ui.tab='share'; render();
  `));
  assert.match(ui.card.innerHTML, /Your rank/);
  assert.doesNotMatch(ui.card.innerHTML, /Cannot show this view/);
  assert.equal(ui.errors.length, 0);
});

test('settings save failures remain visible and preserve the previous setting', async () => {
  const ui = app({ invoke: (command) => {
    if (command === 'get_snapshot') return snapshot();
    if (command === 'set_settings') throw new Error('disk full');
  } });
  await flush();
  await ui.click({ dataset: { display: 'cost' } });
  assert.match(ui.card.innerHTML, /disk full/);
  assert.equal(ui.evaluate('snap.settings.display'), 'tokens');
  ui.change('interval', '60');
  await flush();
  assert.match(ui.card.innerHTML, /disk full/);
  assert.equal(ui.evaluate('snap.settings.intervalMinutes'), 30);
});

test('browser and autostart errors display in local view and signed-out preferences', async () => {
  const ui = app({ invoke: (command) => {
    if (command === 'get_snapshot') return snapshot();
    if (command === 'open_leaderboard') throw new Error('browser failed');
    if (command === 'set_autostart') throw new Error('autostart failed');
  } });
  await flush();
  await ui.click({ dataset: { act: 'leaderboard' } });
  assert.match(ui.card.innerHTML, /browser failed/);
  await ui.click({ dataset: { act: 'prefs' } });
  await ui.click({ id: 'autostart' });
  assert.match(ui.card.innerHTML, /autostart failed/);
  assert.match(ui.card.innerHTML, /Preferences/);
});

test('background errors display on every tab and while signed out', async () => {
  const ui = app();
  await flush();
  for (const tab of ['local', 'limits', 'share']) {
    ui.evaluate(`snap.errors=['cache save failed']; ui.tab='${tab}'; render();`);
    assert.match(ui.card.innerHTML, /cache save failed/);
  }
  ui.evaluate("ui.view='prefs'; render()");
  assert.match(ui.card.innerHTML, /cache save failed/);
});

test('keyboard and resize command failures are caught', async () => {
  const ui = app({ invoke: (command) => {
    if (command === 'get_snapshot') return snapshot();
    throw new Error(`${command} failed`);
  } });
  await flush();
  ui.key('r');
  await flush();
  assert.match(ui.card.innerHTML, /sync_now failed/);
  ui.resize();
  await flush();
  assert.match(ui.card.innerHTML, /set_popover_height failed/);
});

test('invalid snapshots preserve the previous usable snapshot', async () => {
  const ui = app();
  await flush();
  ui.subscriptions.get('snapshot')({ payload: null });
  await flush();
  assert.match(ui.card.innerHTML, /invalid snapshot/);
  assert.match(ui.card.innerHTML, /This Mac/);
});
