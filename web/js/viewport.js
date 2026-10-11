// Keep the shell inside the visible area when a phone keyboard pans or
// shrinks the viewport. Browser chrome and pinch zoom are not keyboards.
export function keyboardOpen(layoutHeight, visibleHeight, scale = 1) {
  return Math.abs(scale - 1) < .01 && visibleHeight > 0
    && layoutHeight - visibleHeight > Math.max(120, layoutHeight * .2);
}

export function keyboardOrientationKey(width, height) {
  return `iris.kb.${width > height ? 'landscape' : 'portrait'}`;
}

export function validKeyboardHeight(value, screenHeight) {
  const height = Number(value);
  return Number.isFinite(height) && height >= 100 && height <= screenHeight * .7
    ? height : null;
}

export function keyboardTargetHeight(layoutHeight, rememberedHeight, screenHeight = layoutHeight) {
  const height = validKeyboardHeight(rememberedHeight, screenHeight);
  return height !== null && height < layoutHeight ? layoutHeight - height : null;
}

const composers = new WeakMap();

// Both native gestures and programmatic focus must move the field above
// the future keyboard before iOS decides whether it needs to pan.
export function prepareComposerFocus(el) {
  if (!el || el.disabled || el.readOnly) return;
  composers.get(el.closest('#app'))?.();
}

export function focusComposer(el) {
  if (!el) return;
  prepareComposerFocus(el);
  el.focus({ preventScroll: true });
}

export function trackViewport(root) {
  const vv = window.visualViewport;
  let frame = 0;
  let width = 0;
  let fullHeight = 0;
  let open = false;
  let appHeight;
  let appOffset;
  let prediction = null;
  let predictionTimer = 0;
  let rememberTimer = 0;
  const zoomed = () => vv && Math.abs(vv.scale - 1) >= .01;
  const orientationKey = () => keyboardOrientationKey(width, fullHeight);
  const screenHeight = () => {
    const screen = window.screen;
    if (!screen?.width || !screen.height) return fullHeight;
    return width > fullHeight ? Math.min(screen.width, screen.height) : Math.max(screen.width, screen.height);
  };
  const clearPrediction = () => {
    clearTimeout(predictionTimer);
    predictionTimer = 0;
    prediction = null;
  };
  const remember = (height) => {
    clearTimeout(rememberTimer);
    const key = orientationKey();
    const keyboardHeight = validKeyboardHeight(fullHeight - height, screenHeight());
    if (!vv || keyboardHeight === null) return;
    // Do not learn an intermediate animation frame. Each resize restarts
    // this short settling period; storage failures must never affect focus.
    rememberTimer = setTimeout(() => {
      if (zoomed() || !open || key !== orientationKey() || vv.height !== height) return;
      try { window.localStorage.setItem(key, String(keyboardHeight)); } catch { /* storage unavailable */ }
    }, 150);
  };
  const correctPosition = () => {
    if (zoomed()) return;
    // Reset document scrolling before reading offsetTop: the reset can
    // itself move the visual viewport. Never wait a frame to correct a pan.
    if (window.scrollX || window.scrollY) window.scrollTo({ left: 0, top: 0, behavior: 'instant' });
    const offset = vv?.offsetTop || 0;
    if (offset !== appOffset) {
      root.style.setProperty('--app-offset', `${offset}px`);
      appOffset = offset;
    }
  };
  const update = () => {
    // Leave native pinch zoom alone, including its viewport panning.
    if (zoomed()) return;
    const layout = document.documentElement;
    // Read layout once, before writing styles. Panning with an unchanged
    // viewport height only needs the compositor offset.
    const layoutWidth = layout.clientWidth;
    const layoutHeight = layout.clientHeight;
    const height = vv?.height || window.innerHeight;
    const active = document.activeElement;
    const editing = active?.isContentEditable || (active?.matches('input, textarea')
      && !active.disabled && !active.readOnly
      && !/^(button|checkbox|color|file|hidden|radio|range|reset|submit)$/.test(active.type));
    if (width !== layoutWidth) clearPrediction();
    if (width !== layoutWidth || (!editing && !open)) fullHeight = layoutHeight;
    width = layoutWidth;
    fullHeight = Math.max(fullHeight, layoutHeight);
    const nextOpen = !!(editing || open) && keyboardOpen(fullHeight, height);
    if (nextOpen) clearPrediction();
    if (nextOpen && editing) {
      remember(height);
    } else {
      clearTimeout(rememberTimer);
    }
    // focusin and early scroll/resize events can still report the full
    // viewport during iOS's animation. Keep the pre-focus shrink until an
    // actual keyboard resize, blur, or the hardware-keyboard timeout.
    const targetHeight = prediction ?? height;
    const targetOpen = prediction !== null || nextOpen;
    correctPosition();
    if (targetHeight !== appHeight) {
      root.style.setProperty('--app-height', `${targetHeight}px`);
      appHeight = targetHeight;
    }
    if (targetOpen !== open) root.classList.toggle('keyboard-open', targetOpen);
    open = targetOpen;
  };
  const prepare = () => {
    if (!vv || zoomed()) return;
    // Refresh layout/orientation synchronously, including after rotation.
    update();
    if (keyboardOpen(fullHeight, vv.height, vv.scale) || prediction !== null) return;
    let stored;
    try { stored = window.localStorage.getItem(orientationKey()); } catch { return; }
    const target = keyboardTargetHeight(fullHeight, stored, screenHeight());
    if (target === null) return;
    prediction = target;
    update();
    predictionTimer = setTimeout(() => {
      clearPrediction();
      update();
    }, 700);
  };
  composers.set(root, prepare);
  const onScroll = () => {
    if ((vv?.height || window.innerHeight) !== appHeight) update();
    else correctPosition();
  };
  // activeElement still names the departing input during focusout.
  const afterBlur = (e) => {
    // Focus moving straight into another text field (e.g. into the
    // composer) keeps the keyboard up, so keep the prediction.
    const next = e?.relatedTarget;
    const staysEditing = !!next && (next.isContentEditable || next.matches?.('input, textarea'));
    if (prediction !== null && !staysEditing) {
      clearPrediction();
      update();
    }
    clearTimeout(rememberTimer);
    if (!frame) frame = requestAnimationFrame(() => { frame = 0; update(); });
  };
  vv?.addEventListener('resize', update);
  vv?.addEventListener('scroll', onScroll);
  window.addEventListener('resize', update);
  window.addEventListener('scroll', onScroll);
  document.addEventListener('focusin', update);
  document.addEventListener('focusout', afterBlur);
  update();
  return () => {
    composers.delete(root);
    clearPrediction();
    clearTimeout(rememberTimer);
    cancelAnimationFrame(frame);
    vv?.removeEventListener('resize', update);
    vv?.removeEventListener('scroll', onScroll);
    window.removeEventListener('resize', update);
    window.removeEventListener('scroll', onScroll);
    document.removeEventListener('focusin', update);
    document.removeEventListener('focusout', afterBlur);
  };
}
