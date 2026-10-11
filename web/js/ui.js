// Shared UI pieces: icons, avatar, images, rows, toggles, sheets, time.
import { html, useEffect, useState, useRef } from './h.js';
import { blobUrl } from './api.js';
import { S, set, back } from './store.js';

// Line icons (24px grid, drawn for Iris).
const P = {
  menu: 'M4 7h16M4 12h16M4 17h16',
  plus: 'M12 5v14M5 12h14',
  mic: 'M9 6a3 3 0 0 1 6 0v5a3 3 0 0 1-6 0zM5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21',
  send: 'M12 19V5M5.5 11.5 12 5l6.5 6.5',
  back: 'M15 5l-7 7 7 7',
  chev: 'M9 5l7 7-7 7',
  heart: 'M12 20s-7.5-4.6-7.5-10.2A4.3 4.3 0 0 1 12 7.2a4.3 4.3 0 0 1 7.5 2.6C19.5 15.4 12 20 12 20z',
  chat: 'M5 5h14a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2h-8l-5 4v-4H5a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2z',
  x: 'M6 6l12 12M18 6 6 18',
  search: 'M10.5 17a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM20 20l-4.8-4.8',
  check: 'M5 12.5l4.5 4.5L19 7',
  sparkles: 'M11 3l1.9 5.1L18 10l-5.1 1.9L11 17l-1.9-5.1L4 10l5.1-1.9zM18.5 14.5l.8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8z',
  bulb: 'M9.5 18h5M10.5 21h3M12 3a6 6 0 0 0-3.6 10.8c.7.6 1.1 1.3 1.1 2.2h5c0-.9.4-1.6 1.1-2.2A6 6 0 0 0 12 3z',
  target: 'M12 20.5a8.5 8.5 0 1 0 0-17 8.5 8.5 0 0 0 0 17zM12 16a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM12 12.5a.5.5 0 1 0 0-1 .5.5 0 0 0 0 1z',
  library: 'M5 4h3.5v16H5zM10.5 4H14v16h-3.5zM15.6 5.2l3.3-.9 3.6 14.9-3.3.9z',
  plug: 'M9 3v5M15 3v5M6.5 8h11v3.5a5.5 5.5 0 0 1-11 0zM12 17v4',
  lock: 'M6 11h12a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1v-8a1 1 0 0 1 1-1zM8 11V8a4 4 0 0 1 8 0v3',
  monitor: 'M5 4h14a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zM8.5 21h7M12 17v4',
  folder: 'M3 7.5A2.5 2.5 0 0 1 5.5 5h3.7l2 2h7.3A2.5 2.5 0 0 1 21 9.5v7a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 16.5z',
  file: 'M7 3h7l5 5v12a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1zM14 3v5h5M9 13h6M9 17h6',
  sliders: 'M4 6h9M17 6h3M4 12h3M11 12h9M4 18h11M19 18h1M15 4v4M9 10v4M17 16v4',
  bell: 'M6 16v-5a6 6 0 0 1 12 0v5l1.5 2h-15zM10 21h4',
  moon: 'M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z',
  user: 'M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4.5 21a7.5 7.5 0 0 1 15 0',
  users: 'M9 11.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7zM2.5 20a6.5 6.5 0 0 1 13 0M16 4.6a3.5 3.5 0 0 1 0 6.8M18 14a5.5 5.5 0 0 1 3.5 6',
  phone: 'M9 2.5h6a2 2 0 0 1 2 2v15a2 2 0 0 1-2 2H9a2 2 0 0 1-2-2v-15a2 2 0 0 1 2-2zM11 18h2',
  list: 'M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01',
  cpu: 'M8 6h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2zM10 10h4v4h-4zM9.5 3v3M14.5 3v3M9.5 18v3M14.5 18v3M3 9.5h3M3 14.5h3M18 9.5h3M18 14.5h3',
  image: 'M5 4.5h14a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-11a2 2 0 0 1 2-2zM9 11a1.8 1.8 0 1 0 0-3.6A1.8 1.8 0 0 0 9 11zM21 15.5l-5-5-9.5 9',
  copy: 'M10 8h9a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1V9a1 1 0 0 1 1-1zM15 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h4',
  external: 'M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
  trash: 'M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3',
  more: 'M5 12h.01M12 12h.01M19 12h.01',
  volume: 'M4 9.5v5h3.5L12 19V5L7.5 9.5zM16 8.5a5 5 0 0 1 0 7M18.5 6a8.5 8.5 0 0 1 0 12',
  key: 'M8 19a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM10.8 12.2 19 4M15.5 7.5l3 3',
  card: 'M5 5h14a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2zM3 10h18M7 15h4',
  refresh: 'M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7',
  pause: 'M9 5v14M15 5v14',
  play: 'M8 5.5v13l10.5-6.5z',
  pausefill: 'M7 5h3.5v14H7zM13.5 5H17v14h-3.5z',
  eye: 'M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12zM12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z',
  edit: 'M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4',
  globe: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM3 12h18M12 3c2.5 2.6 3.7 5.6 3.7 9s-1.2 6.4-3.7 9c-2.5-2.6-3.7-5.6-3.7-9S9.5 5.6 12 3z',
  hand: 'M8 13V5.5a1.5 1.5 0 0 1 3 0V11M11 10V4.5a1.5 1.5 0 0 1 3 0V11M14 10.5V6a1.5 1.5 0 0 1 3 0v8a7 7 0 0 1-7 7h-.5A5.5 5.5 0 0 1 5 18.2L3.3 14.6a1.5 1.5 0 0 1 2.6-1.5L8 16',
  stop: 'M8 7h8a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H8a1 1 0 0 1-1-1V8a1 1 0 0 1 1-1z',
  download: 'M12 4v11M7 10.5l5 5 5-5M5 20h14',
};

export function Icon({ name, size = 22, fill = false, cls = '' }) {
  return html`<svg class=${'ic ' + cls} width=${size} height=${size} viewBox="0 0 24 24" aria-hidden="true"
    fill=${fill ? 'currentColor' : 'none'} stroke="currentColor" stroke-width="1.8"
    stroke-linecap="round" stroke-linejoin="round"><path d=${P[name] || P.more} /></svg>`;
}

/** Image from a URL; `/api/...` paths are fetched with the device token. */
export function Img({ src, alt = '', cls = '', onLoad, onError, ...rest }) {
  const [url, setUrl] = useState(src && !src.startsWith('/api/') ? src : null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let live = true;
    setFailed(false);
    if (!src) setUrl(null);
    else if (src.startsWith('/api/')) blobUrl(src).then((u) => live && setUrl(u)).catch(() => live && setFailed(true));
    else setUrl(src);
    return () => { live = false; };
  }, [src]);
  if (!url || failed) return html`<div class=${'img-ph ' + cls} />`;
  return html`<img class=${cls} src=${url} alt=${alt} loading="lazy" decoding="async"
    referrerpolicy="no-referrer" onLoad=${onLoad}
    onError=${() => { setFailed(true); onError?.(); }} ...${rest} />`;
}

/** Iris's avatar: the uploaded picture, or the built-in placeholder.
 *  `agent` shows another person's Iris (the host of a Together chat). */
export function Avatar({ size = 36, agent }) {
  const a = agent || S.settings;
  const src = a?.avatar_custom ? a.avatar_url : null;
  return html`<div class="avatar" style=${`width:${size}px;height:${size}px`}>
    ${src ? html`<${Img} src=${src} alt="" />` : html`<div class="orb" />`}
  </div>`;
}

export function agentName() {
  return S.settings?.agent_name || 'Iris';
}

export function Toggle({ on, onChange, disabled = false, label }) {
  return html`<button class=${'tog' + (on ? ' on' : '')} role="switch" aria-checked=${on ? 'true' : 'false'}
    aria-label=${label} disabled=${disabled}
    onClick=${(e) => { e.stopPropagation(); if (!disabled) onChange(!on); }} />`;
}

/** A settings-style row. */
export function Row({ icon, iconBg, title, sub, value, onClick, right, danger = false, accent = false, chevron }) {
  const showChev = chevron ?? !!onClick;
  return html`<div class=${'r' + (onClick ? ' tap' : '') + (danger ? ' danger' : '') + (accent ? ' accent' : '')}
    onClick=${onClick} role=${onClick ? 'button' : undefined} tabindex=${onClick ? 0 : undefined}>
    ${icon && html`<div class="ri" style=${iconBg ? `background:${iconBg}` : ''}>${typeof icon === 'string' ? html`<${Icon} name=${icon} size=${17} />` : icon}</div>`}
    <div class="rt">${title}${sub && html`<small>${sub}</small>`}</div>
    ${value != null && html`<span class="rv">${value}</span>`}
    ${right}
    ${showChev && html`<${Icon} name="chev" size=${16} cls="rc" />`}
  </div>`;
}

export function Group({ label, children, note }) {
  return html`${label && html`<div class="gl">${label}</div>`}
    <div class="grp">${children}</div>
    ${note && html`<div class="gnote">${note}</div>`}`;
}

/** Full-screen sub page with a back button and a serif title. */
export function Page({ title, sub, children, onBack, right }) {
  const ref = useRef();
  useEffect(() => { ref.current && (ref.current.scrollTop = 0); }, [title]);
  return html`<div class="page" ref=${ref}>
    <div class="page-top">
      <button class="btn" aria-label="Back" onClick=${onBack || (() => back())}><${Icon} name="back" /></button>
      <div class="grow" />
      ${right}
    </div>
    <h1 class="ttl">${title}</h1>
    ${sub && html`<div class="sub">${sub}</div>`}
    ${children}
  </div>`;
}

export function openSheet(render) { set({ sheet: render }); }
export function closeSheet() { set({ sheet: null }); }

export function Sheet() {
  if (!S.sheet) return null;
  return html`<div class="sheet-wrap" onClick=${closeSheet}>
    <div class="sheet" onClick=${(e) => e.stopPropagation()} role="dialog" aria-modal="true">
      <div class="grab" />
      ${S.sheet()}
    </div>
  </div>`;
}

/** Ask a yes/no question in a sheet. */
export function confirmSheet({ title, body, yes = 'OK', danger = false }) {
  return new Promise((resolve) => {
    const done = (v) => { closeSheet(); resolve(v); };
    openSheet(() => html`<div class="sheet-body">
      <h3>${title}</h3>${body && html`<p class="mu">${body}</p>`}
      <div class="btns">
        <button class="b2" onClick=${() => done(false)}>Cancel</button>
        <button class=${'b1' + (danger ? ' danger' : '')} onClick=${() => done(true)}>${yes}</button>
      </div></div>`);
  });
}

/** Ask for one line of text in a sheet. */
export function promptSheet({ title, body, value = '', placeholder = '', yes = 'Save', type = 'text' }) {
  return new Promise((resolve) => {
    const done = (v) => { closeSheet(); resolve(v); };
    const Body = () => {
      const [v, setV] = useState(value);
      const ref = useRef();
      useEffect(() => { setTimeout(() => ref.current?.focus(), 50); }, []);
      return html`<form class="sheet-body" onSubmit=${(e) => { e.preventDefault(); done(v); }}>
        <h3>${title}</h3>${body && html`<p class="mu">${body}</p>`}
        <input class="field" ref=${ref} type=${type} value=${v} placeholder=${placeholder}
          onInput=${(e) => setV(e.target.value)} />
        <div class="btns">
          <button type="button" class="b2" onClick=${() => done(null)}>Cancel</button>
          <button class="b1" type="submit">${yes}</button>
        </div></form>`;
    };
    openSheet(() => html`<${Body} />`);
  });
}

export function Empty({ icon = 'sparkles', title, children }) {
  return html`<div class="empty"><div class="empty-ic"><${Icon} name=${icon} size=${26} /></div>
    <b>${title}</b><div class="mu">${children}</div></div>`;
}

export function Spinner() { return html`<div class="spin" aria-label="Loading" />`; }

// -------------------------------------------------------------------- time

const pad = (n) => String(n).padStart(2, '0');
export function clock(ms) {
  const d = new Date(ms);
  let h = d.getHours();
  const am = h < 12 ? 'AM' : 'PM';
  h = h % 12 || 12;
  return `${h}:${pad(d.getMinutes())} ${am}`;
}

export function dayLabel(ms) {
  const d = new Date(ms);
  const today = new Date();
  const y = new Date(today);
  y.setDate(today.getDate() - 1);
  if (d.toDateString() === today.toDateString()) return '';
  if (d.toDateString() === y.toDateString()) return 'Yesterday';
  const opts = { weekday: 'short', month: 'short', day: 'numeric' };
  if (d.getFullYear() !== today.getFullYear()) opts.year = 'numeric';
  return d.toLocaleDateString(undefined, opts);
}

export function stamp(ms) {
  const day = dayLabel(ms);
  return day ? `${day} ${clock(ms)}` : clock(ms);
}

export function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return 'now';
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  if (s < 86400 * 7) return `${Math.floor(s / 86400)}d`;
  return new Date(ms).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

/** "just now", "5m ago", "3d ago", or "on Sep 11". */
export function since(ms) {
  const a = ago(ms);
  if (a === 'now') return 'just now';
  return /\d[mhd]$/.test(a) ? `${a} ago` : `on ${a}`;
}

/** The URL if it is http(s), else undefined (never a javascript: link). */
export function safeUrl(url) {
  return /^https?:\/\//i.test(String(url || '')) ? url : undefined;
}

export function hostOf(url) {
  try { return new URL(url).hostname.replace(/^www\./, ''); } catch { return ''; }
}

/** Stable pleasant color for a name (letter tiles). */
export function hue(s) {
  let h = 0;
  for (const c of String(s)) h = (h * 31 + c.charCodeAt(0)) % 360;
  return `hsl(${h} 55% 55%)`;
}

export function LetterTile({ name, size = 30, radius = 9 }) {
  const letter = (String(name || '?').trim()[0] || '?').toUpperCase();
  return html`<div class="tile" style=${`width:${size}px;height:${size}px;border-radius:${radius}px;background:${hue(name)}`}>${letter}</div>`;
}
