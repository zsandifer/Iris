// One viewer for all photo surfaces. Kept outside #app so the whole app
// can be inert and the overlay is independent of the shell's transforms.
import { html, render, useLayoutEffect, useRef, useState } from './h.js';
import { holdBlob } from './api.js';
import { clampPan, zoomAt, doubleTapTarget } from './photo-math.js';

let active = null;
let nextId = 0;
export const photoIsOpen = () => !!active;

export function openPhoto(src, alt = '', options = {}) {
  if (!src || active) return;
  const host = document.createElement('div');
  document.body.append(host);
  const session = { host, focus: options.focus || document.activeElement, id: `${Date.now()}-${++nextId}` };
  active = session;
  history.pushState({ ...history.state, irisPhoto: session.id }, '');
  document.dispatchEvent(new Event('iris-photo-change'));
  render(html`<${PhotoViewer} src=${src} alt=${alt} session=${session} action=${options.action} />`, host);
}

function PhotoViewer({ src, alt, session, action }) {
  const overlay = useRef(null), layer = useRef(null), image = useRef(null), close = useRef(null);
  const [url, setUrl] = useState(src.startsWith('/api/') ? null : src);
  const [failed, setFailed] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const controller = useRef(null);

  useLayoutEffect(() => {
    let blob;
    let live = true, closing = false, requested = false, timer = 0, frame = 0;
    let afterClose;
    let state = { scale: 1, x: 0, y: 0 }, width = 0, height = 0;
    let center = { x: 0, y: 0 };
    let points = new Map(), baseline = null, tap = null, lastTap = null;
    const siblings = [...document.body.children].filter((el) => el !== session.host);
    const inerts = siblings.map((el) => el.inert);
    siblings.forEach((el) => { el.inert = true; });
    document.body.classList.add('photo-open');
    close.current.focus({ preventScroll: true });
    if (src.startsWith('/api/')) {
      blob = holdBlob(src);
      blob.url.then((u) => live && setUrl(u)).catch(() => live && setFailed(true));
    }
    const paint = () => {
      frame = 0;
      image.current.style.transform = `translate3d(${state.x}px, ${state.y}px, 0) scale(${state.scale})`;
    };
    const update = (next) => {
      state = clampPan(next, width, height);
      if (!frame) frame = requestAnimationFrame(paint);
    };
    const fit = () => {
      const img = image.current;
      if (!img.naturalWidth || !img.naturalHeight) return;
      const rect = overlay.current.querySelector('.photo-stage').getBoundingClientRect();
      center = { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
      const ratio = Math.min(rect.width / img.naturalWidth, rect.height / img.naturalHeight, 1);
      width = img.naturalWidth * ratio;
      height = img.naturalHeight * ratio;
      layer.current.style.width = `${width}px`;
      layer.current.style.height = `${height}px`;
      points.clear(); baseline = tap = lastTap = null;
      update(state);
    };
    const finish = () => {
      render(null, session.host);
      session.host.remove();
      active = null;
      document.dispatchEvent(new Event('iris-photo-change'));
      if (session.focus?.isConnected) session.focus.focus({ preventScroll: true });
      afterClose?.();
    };
    const dismiss = () => {
      if (closing) return;
      closing = true;
      setLeaving(true);
      timer = setTimeout(finish, matchMedia('(prefers-reduced-motion: reduce)').matches ? 0 : 160);
    };
    const requestClose = (next) => {
      if (requested || closing) return;
      requested = true;
      afterClose = next;
      if (history.state?.irisPhoto === session.id) history.back();
      else dismiss();
    };
    const pop = () => { if (history.state?.irisPhoto !== session.id) dismiss(); };
    const key = (e) => {
      if (e.key === 'Escape') { e.preventDefault(); e.stopImmediatePropagation(); requestClose(); }
      if (e.key === 'Tab') {
        const buttons = [...overlay.current.querySelectorAll('button')];
        const i = buttons.indexOf(document.activeElement);
        e.preventDefault();
        buttons[(i + (e.shiftKey ? -1 : 1) + buttons.length) % buttons.length].focus();
      }
    };
    const local = (p) => ({ x: p.x - center.x, y: p.y - center.y });
    const geometry = () => {
      const [a, b] = [...points.values()];
      return b ? { point: local({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 }),
        distance: Math.hypot(b.x - a.x, b.y - a.y) } : { point: local(a), distance: 0 };
    };
    const rebase = () => { baseline = points.size ? { ...geometry(), state: { ...state } } : null; };
    const start = () => {
      if (closing) return;
      if (points.size === 1) tap = { ...[...points.values()][0], time: performance.now() };
      else { tap = lastTap = null; }
      rebase();
    };
    const move = () => {
      if (!baseline || !points.size || closing) return;
      const g = geometry();
      if (tap && Math.hypot(g.point.x - baseline.point.x, g.point.y - baseline.point.y) > 8) tap = lastTap = null;
      if (points.size > 1) {
        update(zoomAt(baseline.state, baseline.state.scale * g.distance / (baseline.distance || 1), baseline.point, g.point));
      } else if (state.scale > 1) {
        update({ ...state, x: baseline.state.x + g.point.x - baseline.point.x,
          y: baseline.state.y + g.point.y - baseline.point.y });
      }
    };
    const end = (cancelled) => {
      if (!points.size && tap && !cancelled && performance.now() - tap.time < 300) {
        if (lastTap && tap.time - lastTap.time < 320 && Math.hypot(tap.x - lastTap.x, tap.y - lastTap.y) < 30) {
          update(doubleTapTarget(state, local(tap), width, height));
          lastTap = null;
        } else lastTap = tap;
      }
      tap = null;
      if (cancelled) lastTap = null;
      rebase();
    };
    // Native touch listeners run before document's blockZoom. preventDefault
    // stops browser gestures, but never stops delivery of our touch data.
    const touches = (e) => {
      points = new Map([...e.touches].map((t) => [t.identifier, { x: t.clientX, y: t.clientY }]));
      e.preventDefault();
    };
    const touchStart = (e) => { touches(e); start(); };
    const touchMove = (e) => { touches(e); move(); };
    const touchEnd = (e) => { touches(e); end(e.type === 'touchcancel'); };
    const pointerDown = (e) => {
      if (e.pointerType === 'touch' || e.button !== 0) return;
      e.preventDefault();
      layer.current.setPointerCapture(e.pointerId);
      points.set(e.pointerId, { x: e.clientX, y: e.clientY }); start();
    };
    const pointerMove = (e) => {
      if (!points.has(e.pointerId) || e.pointerType === 'touch') return;
      points.set(e.pointerId, { x: e.clientX, y: e.clientY }); move();
    };
    const pointerEnd = (e) => {
      if (e.pointerType === 'touch' || !points.has(e.pointerId)) return;
      points.delete(e.pointerId); end(e.type !== 'pointerup');
    };
    const el = layer.current;
    const handlers = { touchstart: touchStart, touchmove: touchMove, touchend: touchEnd, touchcancel: touchEnd,
      pointerdown: pointerDown, pointermove: pointerMove, pointerup: pointerEnd, pointercancel: pointerEnd,
      lostpointercapture: pointerEnd };
    for (const [name, fn] of Object.entries(handlers)) el.addEventListener(name, fn, { passive: false });
    const resize = new ResizeObserver(fit);
    resize.observe(overlay.current.querySelector('.photo-stage'));
    window.addEventListener('popstate', pop);
    document.addEventListener('keydown', key, true);
    controller.current = { fit, requestClose };
    return () => {
      live = false;
      clearTimeout(timer); cancelAnimationFrame(frame);
      blob?.release(); resize.disconnect();
      window.removeEventListener('popstate', pop);
      document.removeEventListener('keydown', key, true);
      for (const [name, fn] of Object.entries(handlers)) el.removeEventListener(name, fn);
      siblings.forEach((el, i) => { el.inert = inerts[i]; });
      document.body.classList.remove('photo-open');
    };
  }, []);

  return html`<div ref=${overlay} class=${'photo-viewer' + (leaving ? ' leaving' : '')}
    role="dialog" aria-modal="true" aria-label="Photo viewer"
    onClick=${(e) => { if (!e.target.closest('.photo-layer, button')) controller.current.requestClose(); }}>
    <div class="photo-stage">
      <div ref=${layer} class="photo-layer"><img ref=${image} src=${url || undefined} alt=${alt} draggable="false"
        referrerpolicy="no-referrer" onLoad=${() => controller.current.fit()} onError=${() => setFailed(true)} /></div>
      ${failed && html`<p class="photo-error" role="status">I couldn't load this photo.</p>`}
    </div>
    <button ref=${close} class="photo-close" aria-label="Close photo" onClick=${() => controller.current.requestClose()}>×</button>
    ${action && html`<button class="b2 photo-action" onClick=${() => controller.current.requestClose(action.onClick)}>${action.label}</button>`}
  </div>`;
}
