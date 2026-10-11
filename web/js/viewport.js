// Keep the shell inside the visible area when a phone keyboard pans or
// shrinks the viewport. Browser chrome and pinch zoom are not keyboards.
export function keyboardOpen(layoutHeight, visibleHeight, scale = 1) {
  return Math.abs(scale - 1) < .01 && visibleHeight > 0
    && layoutHeight - visibleHeight > Math.max(120, layoutHeight * .2);
}

export function trackViewport(root) {
  const vv = window.visualViewport;
  let frame = 0;
  let width = 0;
  let fullHeight = 0;
  let open = false;
  const update = () => {
    frame = 0;
    // Leave native pinch zoom alone, including its viewport panning.
    if (vv && Math.abs(vv.scale - 1) >= .01) return;
    const layout = document.documentElement;
    const height = vv?.height || window.innerHeight;
    const offset = vv?.offsetTop || 0;
    const active = document.activeElement;
    const editing = active?.isContentEditable || (active?.matches('input, textarea')
      && !active.disabled && !active.readOnly
      && !/^(button|checkbox|color|file|hidden|radio|range|reset|submit)$/.test(active.type));
    if (width !== layout.clientWidth || (!editing && !open)) fullHeight = layout.clientHeight;
    width = layout.clientWidth;
    fullHeight = Math.max(fullHeight, layout.clientHeight);
    open = !!(editing || open) && keyboardOpen(fullHeight, height);
    root.style.setProperty('--app-height', `${height}px`);
    root.style.setProperty('--app-offset', `${offset}px`);
    root.classList.toggle('keyboard-open', open);
    // iOS may scroll the document while bringing a focused input into view.
    if (window.scrollX || window.scrollY || editing) window.scrollTo(0, 0);
  };
  const schedule = () => { if (!frame) frame = requestAnimationFrame(update); };
  vv?.addEventListener('resize', schedule);
  vv?.addEventListener('scroll', schedule);
  window.addEventListener('resize', schedule);
  window.addEventListener('scroll', schedule);
  document.addEventListener('focusin', schedule);
  document.addEventListener('focusout', schedule);
  update();
  return () => {
    cancelAnimationFrame(frame);
    vv?.removeEventListener('resize', schedule);
    vv?.removeEventListener('scroll', schedule);
    window.removeEventListener('resize', schedule);
    window.removeEventListener('scroll', schedule);
    document.removeEventListener('focusin', schedule);
    document.removeEventListener('focusout', schedule);
  };
}
