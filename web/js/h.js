// Rendering helpers: Preact + htm (vendored, no build step).
import { h, render, Fragment } from '../vendor/preact.js';
import htm from '../vendor/htm.js';

export const html = htm.bind(h);
export { h, render, Fragment };
export {
  useState, useEffect, useLayoutEffect, useRef, useMemo, useCallback, useReducer,
} from '../vendor/hooks.js';
