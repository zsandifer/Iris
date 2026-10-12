import test from 'node:test';
import assert from 'node:assert/strict';
import { keyboardOpen, keyboardOrientationKey, validKeyboardHeight, keyboardTargetHeight,
  prepareComposerFocus, focusComposer, trackViewport } from '../../web/js/viewport.js';

test('remembered keyboard heights use separate orientation keys, including square layouts', () => {
  assert.equal(keyboardOrientationKey(390, 844), 'iris.kb.portrait');
  assert.equal(keyboardOrientationKey(844, 390), 'iris.kb.landscape');
  assert.equal(keyboardOrientationKey(800, 800), 'iris.kb.portrait');
});

test('stored keyboard heights must be finite and between 100px and 70% of the screen', () => {
  for (const value of [null, undefined, '', ' ', 'garbage', '344px', 'NaN', 'Infinity', -200, 99.99, 590.81]) {
    assert.equal(validKeyboardHeight(value, 844), null, String(value));
  }
  assert.equal(validKeyboardHeight('344.5', 844), 344.5);
  assert.equal(validKeyboardHeight(100, 844), 100);
  assert.equal(validKeyboardHeight(590.8, 844), 590.8);
  assert.equal(validKeyboardHeight(280, 390), null);
});

test('pre-focus target subtracts the keyboard from layout height with screen-based validation', () => {
  assert.equal(keyboardTargetHeight(844, '344'), 500);
  assert.equal(keyboardTargetHeight(390, '210'), 180);
  assert.equal(keyboardTargetHeight(780, '344', 844), 436);
  assert.equal(keyboardTargetHeight(844, 99), null);
  assert.equal(keyboardTargetHeight(844, 600), null);
  assert.equal(keyboardTargetHeight(200, 250, 844), null);
});

test('phone keyboards substantially shrink the viewport in either orientation', () => {
  assert.equal(keyboardOpen(844, 500), true);
  assert.equal(keyboardOpen(390, 180), true);
  assert.equal(keyboardOpen(1024, 650), true);
});

test('browser chrome and small resizes are not keyboards', () => {
  assert.equal(keyboardOpen(844, 844), false);
  assert.equal(keyboardOpen(844, 750), false);
  assert.equal(keyboardOpen(844, 844 - 844 * .2), false);
  assert.equal(keyboardOpen(390, 270), false);
  assert.equal(keyboardOpen(844, 900), false);
});

test('pinch zoom and unavailable viewport sizes are not keyboards', () => {
  assert.equal(keyboardOpen(844, 422, 2), false);
  assert.equal(keyboardOpen(844, 500, 1.25), false);
  assert.equal(keyboardOpen(844, 500, 1.001), true);
  assert.equal(keyboardOpen(844, 0), false);
});

test('fractional viewport sizes cross the keyboard threshold without rounding', () => {
  assert.equal(keyboardOpen(844, 675.2), false);
  assert.equal(keyboardOpen(844, 675.19), true);
  assert.equal(keyboardOpen(390, 269.99), true);
});

function viewport(t, visual = true) {
  const win = new EventTarget();
  const doc = new EventTarget();
  const vv = visual ? Object.assign(new EventTarget(), { height: 844, offsetTop: 0, scale: 1 }) : null;
  const frames = new Map();
  const timers = new Map();
  const storage = new Map();
  const values = new Map();
  const classes = new Set();
  const writes = [];
  let layoutReads = 0;
  let scrollResets = 0;
  let layoutWidth = 390;
  let layoutHeight = 844;
  let nextTimer = 1;
  Object.assign(win, { visualViewport: vv, innerHeight: 844, scrollX: 0, scrollY: 0,
    localStorage: { getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value) },
    scrollTo(options) {
      assert.deepEqual(options, { left: 0, top: 0, behavior: 'instant' });
      scrollResets++;
      win.scrollX = win.scrollY = 0;
      if (vv) vv.offsetTop = 0;
    },
  });
  doc.documentElement = {
    get clientWidth() { layoutReads++; return layoutWidth; },
    get clientHeight() { layoutReads++; return layoutHeight; },
  };
  doc.activeElement = null;
  const root = {
    style: { setProperty(k, v) { values.set(k, v); writes.push([k, v]); } },
    classList: { toggle(k, yes) { yes ? classes.add(k) : classes.delete(k); } },
  };
  const globals = { window: win, document: doc,
    setTimeout(fn, ms) { const id = nextTimer++; timers.set(id, { fn, ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
    requestAnimationFrame(fn) { const id = frames.size + 1; frames.set(id, fn); return id; },
    cancelAnimationFrame(id) { frames.delete(id); },
  };
  const restore = [];
  for (const [key, value] of Object.entries(globals)) {
    const old = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    restore.push(() => old ? Object.defineProperty(globalThis, key, old) : delete globalThis[key]);
  }
  const stop = trackViewport(root);
  t.after(() => { stop(); restore.forEach((fn) => fn()); });
  return { win, doc, vv, frames, timers, storage, values, classes, writes, stop,
    composer: { closest: () => root, focus(options) { this.options = options; this.beforeFocus?.();
      doc.activeElement = { matches: () => true, type: 'textarea' };
      doc.dispatchEvent(new Event('focusin')); } },
    resizeLayout(w, h) { layoutWidth = w; layoutHeight = h; },
    runTimers(ms) { for (const [id, timer] of [...timers]) {
      if (timer.ms === ms && timers.has(id)) { timers.delete(id); timer.fn(); }
    } },
    get layoutReads() { return layoutReads; },
    get scrollResets() { return scrollResets; },
    focus(active = { matches: () => true, type: 'textarea' }) {
      doc.activeElement = active;
      doc.dispatchEvent(new Event('focusin'));
    },
  };
}

test('keyboard resizing and viewport panning correct the shell before another frame', (t) => {
  const v = viewport(t);
  v.focus();
  v.vv.height = 500;
  v.vv.offsetTop = 42.5;
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.values.get('--app-height'), '500px');
  assert.equal(v.values.get('--app-offset'), '42.5px');
  assert.equal(v.classes.has('keyboard-open'), true);
  const reads = v.layoutReads;
  v.vv.offsetTop = 17;
  v.vv.dispatchEvent(new Event('scroll'));
  assert.equal(v.values.get('--app-offset'), '17px');
  assert.equal(v.layoutReads, reads);
  assert.equal(v.frames.size, 0);
  const writes = v.writes.length;
  v.vv.dispatchEvent(new Event('scroll'));
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.writes.length, writes);
  // Some viewport changes arrive with a scroll event before resize.
  v.vv.height = 420;
  v.vv.dispatchEvent(new Event('scroll'));
  assert.equal(v.values.get('--app-height'), '420px');
  assert.equal(v.frames.size, 0);
});

test('document scrolling resets synchronously and uses the offset after the reset', (t) => {
  const v = viewport(t);
  v.vv.offsetTop = 42;
  v.vv.dispatchEvent(new Event('scroll'));
  const reads = v.layoutReads;
  v.win.scrollX = 4;
  v.win.scrollY = 100;
  v.win.dispatchEvent(new Event('scroll'));
  assert.equal(v.win.scrollY, 0);
  assert.equal(v.win.scrollX, 0);
  assert.equal(v.values.get('--app-offset'), '0px');
  assert.equal(v.scrollResets, 1);
  assert.equal(v.layoutReads, reads);
  assert.equal(v.frames.size, 0);
});

test('a scroll delivered before resize also synchronizes keyboard height', (t) => {
  const v = viewport(t);
  v.focus();
  v.vv.height = 500;
  v.vv.offsetTop = 32;
  v.vv.dispatchEvent(new Event('scroll'));
  assert.equal(v.values.get('--app-height'), '500px');
  assert.equal(v.values.get('--app-offset'), '32px');
  assert.equal(v.classes.has('keyboard-open'), true);
  assert.equal(v.frames.size, 0);
  v.vv.height = 844;
  v.vv.offsetTop = 0;
  v.win.dispatchEvent(new Event('scroll'));
  assert.equal(v.values.get('--app-height'), '844px');
  assert.equal(v.values.get('--app-offset'), '0px');
  assert.equal(v.classes.has('keyboard-open'), false);
});

test('pinch panning is untouched and keyboard dismissal restores navigation', (t) => {
  const v = viewport(t);
  v.focus();
  v.vv.height = 500;
  v.vv.dispatchEvent(new Event('resize'));
  v.vv.scale = 2;
  v.vv.offsetTop = 90;
  v.win.scrollY = 25;
  const writes = v.writes.length;
  v.vv.dispatchEvent(new Event('scroll'));
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.writes.length, writes);
  assert.equal(v.win.scrollY, 25);
  v.vv.scale = 1;
  v.doc.dispatchEvent(new Event('focusout'));
  v.doc.activeElement = null;
  v.vv.height = 844;
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.classes.has('keyboard-open'), false);
  assert.equal(v.values.get('--app-height'), '844px');
  assert.equal(v.win.scrollY, 0);
  v.stop();
  assert.equal(v.frames.size, 0);
  v.vv.offsetTop = 12;
  v.vv.dispatchEvent(new Event('scroll'));
  assert.equal(v.values.get('--app-offset'), '0px');
});

test('browsers without visualViewport still reset document scrolling immediately', (t) => {
  const v = viewport(t, false);
  v.focus();
  v.win.innerHeight = 500;
  v.win.dispatchEvent(new Event('resize'));
  assert.equal(v.values.get('--app-height'), '500px');
  assert.equal(v.classes.has('keyboard-open'), true);
  v.win.scrollY = 100;
  v.win.dispatchEvent(new Event('scroll'));
  assert.equal(v.win.scrollY, 0);
  assert.equal(v.frames.size, 0);
});

test('non-editable controls do not mistake browser resizing for a keyboard', (t) => {
  const v = viewport(t);
  v.vv.height = 500;
  for (const active of [
    { matches: () => true, type: 'checkbox' },
    { matches: () => true, type: 'file' },
    { matches: () => true, type: 'text', readOnly: true },
    { matches: () => true, type: 'text', disabled: true },
    { matches: () => false },
  ]) {
    v.focus(active);
    assert.equal(v.classes.has('keyboard-open'), false);
  }
  v.focus({ isContentEditable: true });
  assert.equal(v.classes.has('keyboard-open'), true);
});

test('blur defers only focus detection; pending blur never delays a pan correction', (t) => {
  const v = viewport(t);
  v.focus();
  v.vv.height = 500;
  v.vv.dispatchEvent(new Event('resize'));
  v.doc.dispatchEvent(new Event('focusout'));
  v.doc.dispatchEvent(new Event('focusout'));
  assert.equal(v.frames.size, 1);
  v.vv.offsetTop = 24;
  v.vv.dispatchEvent(new Event('scroll'));
  assert.equal(v.values.get('--app-offset'), '24px');
  assert.equal(v.frames.size, 1);
  v.doc.activeElement = null;
  // Keyboard is still animating closed, so keep navigation hidden.
  const [[id, callback]] = v.frames;
  v.frames.delete(id);
  callback();
  assert.equal(v.classes.has('keyboard-open'), true);
  v.vv.height = 844;
  v.vv.offsetTop = 0;
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.classes.has('keyboard-open'), false);
  assert.equal(v.values.get('--app-offset'), '0px');
});

test('composer pre-shrink is synchronous and survives focusin and unreported viewport animation', (t) => {
  const v = viewport(t);
  v.storage.set('iris.kb.portrait', '344');
  v.composer.beforeFocus = () => {
    assert.equal(v.values.get('--app-height'), '500px');
    assert.equal(v.classes.has('keyboard-open'), true);
    assert.equal(v.doc.activeElement, null);
  };
  focusComposer(v.composer);
  assert.deepEqual(v.composer.options, { preventScroll: true });
  v.vv.dispatchEvent(new Event('resize'));
  v.vv.dispatchEvent(new Event('scroll'));
  // Repeated touchstart/pointerdown must not postpone the deadline.
  const deadline = [...v.timers.keys()];
  prepareComposerFocus(v.composer);
  assert.deepEqual([...v.timers.keys()], deadline);
  assert.equal(v.values.get('--app-height'), '500px');
  assert.equal(v.frames.size, 0);
  v.vv.height = 480;
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.values.get('--app-height'), '480px');
  assert.equal(v.storage.get('iris.kb.portrait'), '344');
  v.runTimers(700);
  assert.equal(v.values.get('--app-height'), '480px');
  v.runTimers(150);
  assert.equal(v.storage.get('iris.kb.portrait'), '364');
});

test('hardware keyboards and rejected focus restore the layout after 700ms', (t) => {
  const v = viewport(t);
  v.storage.set('iris.kb.portrait', '344');
  for (const focused of [false, true]) {
    prepareComposerFocus(v.composer);
    if (focused) v.focus();
    assert.equal(v.values.get('--app-height'), '500px');
    v.runTimers(700);
    assert.equal(v.values.get('--app-height'), '844px');
    assert.equal(v.classes.has('keyboard-open'), false);
    assert.equal(v.storage.get('iris.kb.portrait'), '344');
  }
});

test('blur immediately cancels a predicted keyboard and disposal clears timers', (t) => {
  const v = viewport(t);
  v.storage.set('iris.kb.portrait', '344');
  focusComposer(v.composer);
  v.doc.dispatchEvent(new Event('focusout'));
  assert.equal(v.values.get('--app-height'), '844px');
  assert.equal(v.classes.has('keyboard-open'), false);
  assert.equal(v.timers.size, 0);
  prepareComposerFocus(v.composer);
  v.stop();
  assert.equal(v.timers.size, 0);
  assert.equal(v.frames.size, 0);
});

test('keyboard memory waits for the final resize and keeps orientations separate', (t) => {
  const v = viewport(t);
  v.focus();
  for (const height of [600, 540, 500]) {
    v.vv.height = height;
    v.vv.dispatchEvent(new Event('resize'));
  }
  assert.equal(v.storage.size, 0);
  assert.equal(v.timers.size, 1);
  v.runTimers(150);
  assert.equal(v.storage.get('iris.kb.portrait'), '344');
  v.resizeLayout(844, 390);
  v.vv.height = 390;
  v.vv.dispatchEvent(new Event('resize'));
  // A portrait value must not predict the landscape keyboard.
  prepareComposerFocus(v.composer);
  assert.equal(v.values.get('--app-height'), '390px');
  v.storage.set('iris.kb.landscape', '210');
  prepareComposerFocus(v.composer);
  assert.equal(v.values.get('--app-height'), '180px');
  v.vv.height = 170;
  v.vv.dispatchEvent(new Event('resize'));
  v.runTimers(150);
  assert.equal(v.storage.get('iris.kb.landscape'), '220');
  assert.equal(v.storage.get('iris.kb.portrait'), '344');
});

test('rotation cancels a pending prediction and pending keyboard memory', (t) => {
  const v = viewport(t);
  v.storage.set('iris.kb.portrait', '344');
  prepareComposerFocus(v.composer);
  v.resizeLayout(844, 390);
  v.vv.height = 390;
  v.vv.dispatchEvent(new Event('resize'));
  assert.equal(v.values.get('--app-height'), '390px');
  assert.equal(v.classes.has('keyboard-open'), false);
  assert.equal(v.timers.size, 0);
});

test('a blurred keyboard animating closed cannot replace the remembered open height', (t) => {
  const v = viewport(t);
  v.focus();
  v.vv.height = 500;
  v.vv.dispatchEvent(new Event('resize'));
  v.runTimers(150);
  assert.equal(v.storage.get('iris.kb.portrait'), '344');
  v.doc.dispatchEvent(new Event('focusout'));
  v.doc.activeElement = null;
  v.vv.height = 550;
  v.vv.dispatchEvent(new Event('resize'));
  v.runTimers(150);
  assert.equal(v.storage.get('iris.kb.portrait'), '344');
});

test('invalid or inaccessible storage and pinch zoom leave normal focus intact', (t) => {
  const v = viewport(t);
  for (const value of ['garbage', '99', '600']) {
    v.storage.set('iris.kb.portrait', value);
    focusComposer(v.composer);
    assert.equal(v.values.get('--app-height'), '844px');
    assert.equal(v.classes.has('keyboard-open'), false);
  }
  v.win.localStorage.getItem = () => { throw new Error('unavailable'); };
  assert.doesNotThrow(() => focusComposer(v.composer));
  v.win.localStorage.setItem = () => { throw new Error('quota'); };
  v.vv.height = 500;
  v.vv.dispatchEvent(new Event('resize'));
  assert.doesNotThrow(() => v.runTimers(150));
  v.vv.scale = 2;
  const writes = v.writes.length;
  prepareComposerFocus(v.composer);
  assert.equal(v.writes.length, writes);
});
