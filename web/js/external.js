// Sign-in belongs in the owner's real browser, including from an iOS PWA.
export function externalUrl(url, env = {}) {
  let parsed;
  try { parsed = new URL(url); } catch { return null; }
  if (!['https:', 'http:'].includes(parsed.protocol) || parsed.username || parsed.password) return null;
  const nav = globalThis.navigator || {};
  const ua = env.userAgent ?? nav.userAgent ?? '';
  const platform = env.platform ?? nav.platform ?? '';
  const touches = env.maxTouchPoints ?? nav.maxTouchPoints ?? 0;
  const ios = /iPad|iPhone|iPod/.test(ua) || (platform === 'MacIntel' && touches > 1);
  const standalone = env.standalone ?? (nav.standalone === true || globalThis.matchMedia?.('(display-mode: standalone)').matches === true);
  return ios && standalone ? parsed.href.replace(/^https?:/, (scheme) => `x-safari-${scheme}`) : parsed.href;
}

export function openExternal(url) {
  const href = externalUrl(url);
  if (!href) return;
  if (href.startsWith('x-safari-')) globalThis.location.assign(href);
  else globalThis.open(href, '_blank', 'noopener,noreferrer');
}
