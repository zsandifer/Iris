// Pull a bottom sheet down to close it. No DOM access of its own, so node
// tests import it directly.

/**
 * Touch handlers that let a sheet be pulled down to close: the drag starts
 * only when it is mostly downward and the sheet's content is scrolled to
 * the top, follows the finger, and closes past `distance` px (else springs
 * back). `sheet()` and `scroller()` return elements; drags that start in a
 * text box are left alone.
 */
export function pullToClose({ sheet, scroller, onClose, distance = 110 }) {
  let d = null;
  const reset = (el) => { el.classList.remove('dragging'); el.style.transform = ''; };
  return {
    onTouchStart: (e) => {
      const t = e.touches[0];
      d = e.touches.length === 1 && !e.target.closest?.('input, textarea, select, [contenteditable]')
        ? { x: t.clientX, y: t.clientY, on: false } : null;
    },
    onTouchMove: (e) => {
      if (!d) return;
      const t = e.touches[0];
      const dx = t.clientX - d.x;
      const dy = t.clientY - d.y;
      if (!d.on) {
        if (dy > 8 && dy > Math.abs(dx) && (scroller()?.scrollTop || 0) <= 0) d.on = true;
        else if (Math.abs(dx) > 10 || dy < -8) { d = null; return; } else return;
      }
      const el = sheet();
      if (!el) return;
      el.classList.add('dragging');
      el.style.transform = `translateY(${Math.max(0, dy)}px)`;
    },
    onTouchEnd: (e) => {
      const was = d;
      d = null;
      const el = sheet();
      if (!was?.on || !el) return;
      reset(el);
      if (e.changedTouches[0].clientY - was.y > distance) onClose();
    },
    onTouchCancel: () => {
      const el = sheet();
      d = null;
      if (el) reset(el);
    },
  };
}
