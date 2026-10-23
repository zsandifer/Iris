// sheet-pull.js: pull a bottom sheet down to close it.
import test from 'node:test';
import assert from 'node:assert/strict';
import { pullToClose } from '../../web/js/sheet-pull.js';

test('pull a sheet down to close it, only from the top and not from a text box', () => {
  const el = { style: {}, classList: { add() {}, remove() {} } };
  let top = 0;
  let closed = 0;
  const h = pullToClose({ sheet: () => el, scroller: () => ({ scrollTop: top }), onClose: () => closed++ });
  const target = { closest: () => null };
  const touch = (y, x = 100) => ({ clientX: x, clientY: y });
  const drag = (to, opts = {}) => {
    h.onTouchStart({ target: opts.target || target, touches: [touch(100)] });
    h.onTouchMove({ touches: [touch(110)] });
    h.onTouchMove({ touches: [touch(to)] });
    h.onTouchEnd({ changedTouches: [touch(to)] });
  };
  drag(250);
  assert.equal(closed, 1);
  drag(160); // not far enough: springs back
  assert.equal(closed, 1);
  assert.equal(el.style.transform, '');
  top = 40; // content scrolled down: the pull scrolls instead
  drag(300);
  assert.equal(closed, 1);
  top = 0;
  drag(300, { target: { closest: () => ({}) } }); // started in a text box
  assert.equal(closed, 1);
  // Scrolled down at the start, reaching the top partway: still a scroll.
  top = 80;
  h.onTouchStart({ target, touches: [touch(100)] });
  h.onTouchMove({ touches: [touch(140)] });
  top = 0;
  h.onTouchMove({ touches: [touch(260)] });
  h.onTouchEnd({ changedTouches: [touch(260)] });
  assert.equal(closed, 1);
  // A document scrolled down inside the sheet: the pull scrolls it.
  const doc = { scrollTop: 250, parentElement: null, closest: () => null };
  drag(300, { target: doc });
  assert.equal(closed, 1);
  doc.scrollTop = 0;
  drag(300, { target: doc });
  assert.equal(closed, 2);
});
