import test from 'node:test';
import assert from 'node:assert/strict';
import { clampScale, clampPan, zoomAt, doubleTapTarget } from '../../web/js/photo-math.js';

test('zoom is bounded to 1–4 and pan resets at native size', () => {
  assert.equal(clampScale(.3), 1);
  assert.equal(clampScale(8), 4);
  assert.deepEqual(clampPan({ scale: .5, x: 80, y: -70 }, 300, 200), { scale: 1, x: 0, y: 0 });
});

test('pan keeps both edges covering the fitted photo window', () => {
  assert.deepEqual(clampPan({ scale: 2.5, x: 1000, y: -1000 }, 300, 200), { scale: 2.5, x: 225, y: -150 });
  for (const [width, height] of [[300, 200], [100, 600], [600, 100]]) {
    for (const scale of [1, 1.2, 2.5, 4, 9]) {
      for (const x of [-9999, -10, 0, 10, 9999]) {
        const p = clampPan({ scale, x, y: -x }, width, height);
        assert.ok(p.x - width * p.scale / 2 <= -width / 2);
        assert.ok(p.x + width * p.scale / 2 >= width / 2);
        assert.ok(p.y - height * p.scale / 2 <= -height / 2);
        assert.ok(p.y + height * p.scale / 2 >= height / 2);
      }
    }
  }
});

test('pinch preserves the image point at a moving midpoint, with existing pan', () => {
  const state = { scale: 2, x: -40, y: 20 };
  const from = { x: 60, y: -30 }, to = { x: 90, y: 10 };
  const next = zoomAt(state, 3, from, to);
  assert.deepEqual(next, { scale: 3, x: -60, y: 85 });
  assert.equal((from.x - state.x) / state.scale, (to.x - next.x) / next.scale);
  assert.equal((from.y - state.y) / state.scale, (to.y - next.y) / next.scale);
  assert.deepEqual(zoomAt(next, 2, to, from), state);
});

test('zoom uses the bounded scale when anchoring at the pinch midpoint', () => {
  assert.deepEqual(zoomAt({ scale: 1, x: 0, y: 0 }, 10, { x: 30, y: 20 }), { scale: 4, x: -90, y: -60 });
});

test('double tap targets 2.5× at the tap point and toggles any zoom back to center', () => {
  const native = { scale: 1, x: 0, y: 0 };
  assert.deepEqual(doubleTapTarget(native, { x: 50, y: -20 }, 300, 200), { scale: 2.5, x: -75, y: 30 });
  assert.deepEqual(doubleTapTarget(native, { x: 999, y: -999 }, 300, 200), { scale: 2.5, x: -225, y: 150 });
  assert.deepEqual(doubleTapTarget({ scale: 1.01, x: 20, y: 10 }, { x: 100, y: 70 }, 300, 200), native);
});
