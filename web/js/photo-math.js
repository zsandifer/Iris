// Coordinates are relative to the center of the fitted photo, in CSS pixels.
export const clampScale = (scale) => Math.min(4, Math.max(1, scale));

export function clampPan({ scale, x, y }, width, height) {
  scale = clampScale(scale);
  if (scale === 1) return { scale, x: 0, y: 0 };
  const limitX = width * (scale - 1) / 2;
  const limitY = height * (scale - 1) / 2;
  return { scale, x: Math.min(limitX, Math.max(-limitX, x)),
    y: Math.min(limitY, Math.max(-limitY, y)) };
}

// Keep the image point under `from` beneath `to`, even as the midpoint moves.
export function zoomAt(state, scale, from, to = from) {
  scale = clampScale(scale);
  const ratio = scale / state.scale;
  return { scale, x: to.x - (from.x - state.x) * ratio,
    y: to.y - (from.y - state.y) * ratio };
}

export function doubleTapTarget(state, point, width, height) {
  return state.scale > 1 ? { scale: 1, x: 0, y: 0 }
    : clampPan(zoomAt(state, 2.5, point), width, height);
}
