// Iris's computer: the live view of Iris's browser (chat peek, the
// computer screen, full-screen Watch), Take over / Hand back with taps,
// scrolling and typing forwarded to the page, browser setup, and Pause.
import { html, useEffect, useRef, useState } from './h.js';
import { get, post, put, wsSend } from './api.js';
import { S, set, useStore, go, toast, frames, loadBrowser } from './store.js';
import { Icon, Row, Group, Page, Spinner, Toggle, agentName, confirmSheet, stamp, hostOf } from './ui.js';
import { activityTitle, placeLabel } from './hub.js';

// ---------------------------------------------------------------- watching

// Each mounted view asks for a mode; the socket watches with the best one
// and stops when nothing is on screen (or the app is in the background).
const modes = new Map();
let sent = 'off';
let lastFrame = null;

function syncWatch(force = false) {
  const vals = [...modes.values()];
  let want = vals.includes('full') ? 'full' : vals.length ? 'peek' : 'off';
  if (document.visibilityState === 'hidden') want = 'off';
  if (want !== sent || force) {
    sent = want;
    wsSend({ type: 'browser.watch', mode: want });
  }
}

frames.subs.add((f) => { lastFrame = f; });
frames.onReady.add(() => { sent = 'off'; syncWatch(); });
document.addEventListener('visibilitychange', () => syncWatch());

/** The latest live-view frame while mounted (null until one arrives). */
function useFrame(mode, on = true) {
  const [f, setF] = useState(on ? lastFrame : null);
  useEffect(() => {
    if (!on) return undefined;
    const id = {};
    modes.set(id, mode);
    syncWatch();
    const sub = (ev) => setF(ev);
    frames.subs.add(sub);
    return () => {
      frames.subs.delete(sub);
      modes.delete(id);
      syncWatch();
    };
  }, [mode, on]);
  return on ? f : null;
}

const src = (f) => `data:image/jpeg;base64,${f.data}`;

// ------------------------------------------------------------- chat peek

/** Small live thumbnail under Iris's status line while it browses. */
export function BrowserPeek() {
  useStore();
  const f = useFrame('peek');
  const b = S.browser || {};
  const host = hostOf(b.url);
  return html`<button class="peek" onClick=${() => go('iris/computer?view=watch')} aria-label=${`Watch ${agentName()}'s browser`}>
    <div class="peek-shot">${f && b.running !== false ? html`<img src=${src(f)} alt="" />` : html`<div class="peek-ph shimmer-bg" />`}</div>
    <div class="peek-t">
      <b>${agentName()}'s browser</b>
      <small>${host || 'Opening a page…'}</small>
      <span class="peek-go"><${Icon} name="eye" size=${14} /> Watch or take over</span>
    </div>
  </button>`;
}

// ------------------------------------------------------------- full view

const SPECIAL = new Set(['Enter', 'Backspace', 'Tab', 'Escape', 'Delete', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown']);
const PAD = '  ';

function input(ev) { wsSend({ type: 'browser.input', ...ev }); }

/** Full screen: Watch, or (in control) forward taps, drags and typing. */
function Viewer({ control }) {
  useStore();
  const b = S.browser || {};
  const f = useFrame('full', !!b.running);
  const img = useRef();
  const kb = useRef();
  const drag = useRef(null);
  const [url, setUrl] = useState('');
  const mine = b.controller === 'user' && b.controller_device_id === S.me?.device?.id;
  const driving = control && mine;
  const host = hostOf(b.url);
  // Leaving the view any way (back button too) hands control back, so the
  // agent is never left waiting on a screen nobody looks at.
  const drivingRef = useRef(false);
  drivingRef.current = mine;
  useEffect(() => () => {
    if (drivingRef.current) post('/browser/handback').catch(() => {});
  }, []);

  const takeOver = async () => {
    try { if (S.browser?.disconnected) setBrowserState((await post('/browser/start')).browser); setBrowserState((await post('/browser/takeover')).browser); go('iris/computer?view=control'); } catch (e) { toast(e.message); }
  };
  const handBack = async () => {
    try { setBrowserState((await post('/browser/handback')).browser); } catch (e) { toast(e.message); }
    go('iris/computer?view=watch');
  };
  const close = async () => {
    if (mine) await post('/browser/handback').catch(() => {});
    go('iris/computer');
  };
  const start = async () => {
    try { setBrowserState((await post('/browser/start')).browser); } catch (e) { toast(e.message); }
  };

  // Taps and drags on the screen (only while in control).
  const at = (e) => {
    const r = img.current.getBoundingClientRect();
    return [(e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height, r];
  };
  const down = (e) => {
    if (!driving || !img.current) return;
    e.preventDefault();
    img.current.setPointerCapture?.(e.pointerId);
    drag.current = { x: e.clientX, y: e.clientY, lx: e.clientX, ly: e.clientY, moved: false, t: 0 };
  };
  const move = (e) => {
    const d = drag.current;
    if (!d) return;
    if (Math.abs(e.clientX - d.x) + Math.abs(e.clientY - d.y) > 8) d.moved = true;
    if (!d.moved) return;
    const now = performance.now();
    if (now - d.t < 50) return;
    const [x, y, r] = at(e);
    const k = (f?.width || 412) / r.width;
    input({ kind: 'scroll', x, y, dx: (d.lx - e.clientX) * k, dy: (d.ly - e.clientY) * k });
    d.lx = e.clientX;
    d.ly = e.clientY;
    d.t = now;
  };
  const up = (e) => {
    const d = drag.current;
    drag.current = null;
    if (!d || d.moved) return;
    const [x, y] = at(e);
    if (x >= 0 && x <= 1 && y >= 0 && y <= 1) input({ kind: 'tap', x, y });
  };

  // Typing goes through a hidden field so the phone keyboard opens.
  const keyboard = () => {
    const el = kb.current;
    if (!el) return;
    el.value = PAD;
    el.focus();
  };
  const reset = (el) => { el.value = PAD; el.setSelectionRange(PAD.length, PAD.length); };
  const onKeyDown = (e) => {
    if (SPECIAL.has(e.key)) {
      e.preventDefault();
      input({ kind: 'key', key: e.key });
    }
  };
  const onBeforeInput = (e) => {
    const t = e.inputType || '';
    if (t === 'insertCompositionText') return;
    e.preventDefault();
    if (t === 'insertText' || t === 'insertReplacementText' || t === 'insertFromPaste') {
      const text = e.data ?? e.dataTransfer?.getData('text/plain') ?? '';
      if (text) input({ kind: 'text', text });
    } else if (t === 'deleteContentBackward') input({ kind: 'key', key: 'Backspace' });
    else if (t === 'deleteContentForward') input({ kind: 'key', key: 'Delete' });
    else if (t === 'insertLineBreak' || t === 'insertParagraph') input({ kind: 'key', key: 'Enter' });
  };
  const onComposition = (e) => {
    if (e.data) input({ kind: 'text', text: e.data });
    reset(e.target);
  };
  // Through the same ordered input stream as taps and typing.
  const go2 = (e) => {
    e.preventDefault();
    if (!url.trim()) return;
    input({ kind: 'navigate', url: url.trim() });
    setUrl('');
  };

  let status;
  if (!b.running) status = 'The browser is off';
  else if (driving) status = `You're in control · ${agentName()} is waiting`;
  else if (b.controller === 'user') status = 'Someone else has taken over';
  else if (b.busy) status = `${agentName()} is using it`;
  else status = 'Watching live';

  return html`<div class=${'viewer' + (driving ? ' driving' : '')}>
    <div class="vw-top">
      <button class="vw-btn" aria-label="Close" onClick=${close}><${Icon} name="x" size=${20} /></button>
      <div class="grow vw-title"><b>${host || `${agentName()}'s browser`}</b><small class=${b.busy && !driving ? 'shimmer' : ''}>${status}</small></div>
      ${b.running && (driving
        ? html`<button class="vw-pill on" onClick=${handBack}><${Icon} name="hand" size=${16} /> Hand back</button>`
        : html`<button class="vw-pill" onClick=${takeOver}><${Icon} name="hand" size=${16} /> Take over</button>`)}
    </div>
    <div class="vw-stage">
      ${!b.running ? html`<div class="vw-off"><${Icon} name="monitor" size=${30} /><b>The browser is off</b>
          <small>It opens when ${agentName()} needs it.</small><button class="b1" onClick=${start}>Open it now</button></div>`
        : f ? html`<img ref=${img} class="vw-img" src=${src(f)} alt=${`Live view of ${agentName()}'s browser`} draggable="false"
          onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerCancel=${() => { drag.current = null; }} />`
        : html`<${Spinner} />`}
    </div>
    ${driving && html`<div class="vw-bar">
      <button class="vw-btn" aria-label="Back" onClick=${() => input({ kind: 'back' })}><${Icon} name="back" size=${18} /></button>
      <form class="vw-url" onSubmit=${go2}><input type="text" inputmode="url" autocapitalize="off" autocomplete="off" spellcheck=${false}
        placeholder="Go to a website" value=${url} onInput=${(e) => setUrl(e.target.value)} aria-label="Web address" /></form>
      <button class="vw-btn" aria-label="Type" onClick=${keyboard}><${Icon} name="keyboard" size=${20} /></button>
      <input ref=${kb} class="vw-kb" aria-label="Type into the page" autocapitalize="off" autocomplete="off" autocorrect="off" spellcheck=${false}
        onKeyDown=${onKeyDown} onBeforeInput=${onBeforeInput} onCompositionEnd=${onComposition}
        onInput=${(e) => { if (!e.isComposing) reset(e.target); }} />
    </div>`}
  </div>`;
}

function setBrowserState(browser) {
  set({ browser: { ...(S.browser || {}), ...browser } });
}

// -------------------------------------------------------------- the page

/** A shell command to run on the server, selectable, with a Copy button. */
function Command({ text }) {
  const copy = async () => {
    try { await navigator.clipboard.writeText(text); toast('Copied'); }
    catch { toast('I couldn’t copy it. Select the command and copy it.'); }
  };
  return html`<div class="cmd-box">
    <pre class="mono" aria-label="Command to run on your server">${text}</pre>
    <button class="b2" type="button" onClick=${copy}><${Icon} name="copy" size=${15} /> Copy</button>
  </div>`;
}

/** Chrome's sandbox cannot start on this server: explain, show the exact
 * command that fixes it on this machine, and let the owner choose to run
 * without it instead. */
function NoSandbox({ b }) {
  // The status with the command for this machine (live updates carry
  // only the browser's own state).
  useEffect(() => { loadBrowser().catch(() => {}); }, []);
  const fix = b.sandbox_fix;
  const allow = async () => {
    const ok = await confirmSheet({
      title: 'Run the browser without its sandbox?',
      body: `The sandbox keeps a bad website from reaching the rest of your server. Without it, ${agentName()}'s browser still works, but a browser bug could expose your server. ${fix?.command ? 'The safer fix is the one-time command shown here.' : 'The safer fix is on the server (see "Iris\'s computer" in the self-hosting guide).'}`,
      yes: 'Run without it',
      danger: true,
    });
    if (!ok) return;
    try {
      setBrowserState((await post('/browser/sandbox', { allow_without: true })).browser);
      setBrowserState((await post('/browser/start')).browser);
    } catch (e) { toast(e.message); }
  };
  // After fixing the server: start again with the sandbox.
  const retry = async () => {
    try { setBrowserState((await post('/browser/start')).browser); } catch (e) { toast(e.message); }
    loadBrowser().catch(() => {});
  };
  const install = async () => {
    try { setBrowserState((await post('/browser/install')).browser); } catch (e) { toast(e.message); }
  };
  const installing = b.install?.state === 'running';
  let how = html`<small>This server blocks the sandbox Chrome uses to keep websites away from the rest of the system (common on Ubuntu). You can allow it on the server once, or run without it.</small>`;
  if (fix?.kind === 'apparmor') {
    how = html`<small>This server blocks the sandbox Chrome uses to keep websites away from the rest of the system (common on Ubuntu). Run this once on your server to allow it for this browser only, then tap Try again.</small>
      <${Command} text=${fix.command} />`;
  } else if (fix?.kind === 'setup') {
    how = html`<small>This server blocks the sandbox Chrome uses to keep websites away from the rest of the system (common on Ubuntu). The browser I found isn't protected well enough to allow it safely. Run this once on your server from the Iris source folder and follow what it prints${fix.install_browser ? '' : ' (then restart Iris)'}. Then set up my own browser here and tap Try again.</small>
      <${Command} text=${fix.command} />
      ${fix.install_browser && b.requires !== 'nodejs' && html`<button class="b2" disabled=${installing} onClick=${install}>${installing ? 'Setting up the browser…' : 'Set up the browser'}</button>`}
      ${b.install?.state === 'failed' && html`<small class="err-line">That didn't work: ${b.install.error}</small>`}`;
  }
  return html`<div class="screen">
    <${Icon} name="lock" size=${30} />
    <b>The browser's safety sandbox can't run here</b>
    ${how}
    <div class="row-btns"><button class="b2" onClick=${retry}>Try again</button>
      <button class="b1" onClick=${allow}>Run without the sandbox</button></div>
  </div>`;
}

function Setup({ b }) {
  const install = async () => {
    try { setBrowserState((await post('/browser/install')).browser); } catch (e) { toast(e.message); }
  };
  const op = b.install;
  if (op?.state === 'running') {
    return html`<div class="screen"><${Spinner} /><b>Setting up ${agentName()}'s browser…</b>
      <small>Downloading Chromium from its official source. This takes a minute or two.</small></div>`;
  }
  return html`<div class="screen">
    <${Icon} name="globe" size=${30} />
    <b>${agentName()} doesn't have a browser yet</b>
    ${b.requires === 'nodejs'
      ? html`<small>Setting it up needs Node.js on your server. Install Node.js there (or Chromium itself), then come back here.</small>`
      : html`<small>${agentName()} can browse, sign in, and fill forms for you in its own browser, and you can watch or take over any time.</small>
        <button class="b1" onClick=${install}>Set up the browser</button>`}
    ${op?.state === 'failed' && html`<small class="err-line">That didn't work: ${op.error}</small>`}
  </div>`;
}

function LiveScreen({ b }) {
  const f = useFrame('full');
  return html`<button class="screen live" onClick=${() => go('iris/computer?view=watch')} aria-label="Watch full screen">
    ${f ? html`<img src=${src(f)} alt=${`Live view of ${agentName()}'s browser`} />` : html`<${Spinner} />`}
    ${b.busy && html`<span class="live-dot">LIVE</span>`}
  </button>`;
}

export function Computer() {
  useStore();
  const view = S.route.query.get('view');
  const [act, setAct] = useState(null);
  const [safety, setSafety] = useState(null);
  useEffect(() => { get('/safety').then(setSafety).catch(() => {}); }, [S.settings?.paused]);
  useEffect(() => { loadBrowser().catch(() => {}); }, []);
  useEffect(() => { get('/activity?limit=8').then(setAct).catch(() => {}); }, []);
  if (view === 'watch' || view === 'control') return html`<${Viewer} control=${view === 'control'} />`;

  const b = S.browser;
  const paused = !!S.settings?.paused;
  const working = Object.keys(S.live).length > 0;
  const takeOver = async () => {
    try { if (S.browser?.disconnected) setBrowserState((await post('/browser/start')).browser); setBrowserState((await post('/browser/takeover')).browser); go('iris/computer?view=control'); } catch (e) { toast(e.message); }
  };
  const watch = async () => {
    if (!b?.running) {
      try { setBrowserState((await post('/browser/start')).browser); } catch (e) { toast(e.message); return; }
    }
    go('iris/computer?view=watch');
  };
  const pause = async (on) => {
    if (on && !(await confirmSheet({ title: `Pause ${agentName()}?`, body: `${agentName()} stops what it's doing, closes its browser, cancels approvals, stops connected services, and won't reply or run check-ins until you turn it back on.`, yes: 'Pause' }))) return;
    try { const safety = await put('/safety', { paused: on }); setSafety(safety); set({ settings: { ...S.settings, paused: safety.paused } }); toast(on ? `${agentName()} is paused` : `${agentName()} is back on`); } catch (e) { toast(e.message); }
  };
  const disconnectAll = async () => {
    if (!(await confirmSheet({ title: 'Disconnect everything?', body: `${agentName()} will stop using every connected service and its browser; waiting approvals are cancelled. Reconnect services in Connections or reopen the browser here.`, yes: 'Disconnect', danger: true }))) return;
    try {
      const { connectors } = await post('/safety/disconnect-all');
      set({ connectors });
      await loadBrowser();
      setSafety(await get('/safety'));
      toast('All connections are off');
    } catch (e) { toast(e.message); }
  };

  let screen;
  if (!b) screen = html`<div class="screen"><${Spinner} /></div>`;
  else if (paused) {
    screen = html`<div class="screen"><${Icon} name="pause" size=${30} /><b>${agentName()} is paused</b>
      <small>Nothing runs until you turn ${agentName()} back on below.</small></div>`;
  } else if (!b.available) screen = html`<${Setup} b=${b} />`;
  else if (b.sandbox_unavailable && !b.running) screen = html`<${NoSandbox} b=${b} />`;
  else if (b.running) screen = html`<${LiveScreen} b=${b} />`;
  else {
    screen = html`<div class="screen"><${Icon} name="monitor" size=${30} />
      <b>${working ? `${agentName()} is working` : `${agentName()} is idle`}</b>
      <small>${b.error ? `The browser couldn't start: ${b.error}` : b.disconnected ? 'The browser is disconnected. Open it below to reconnect.' : `The browser opens when ${agentName()} needs it. You can also open it to sign in to a site yourself.`}</small></div>`;
  }
  const usable = b?.available && !paused && !(b.sandbox_unavailable && !b.running);
  const host = hostOf(b?.url);
  const line = b?.running ? (b.controller === 'user' ? 'You have control' : b.busy ? `${agentName()} is browsing${host ? ` ${host}` : ''}` : host ? `Open: ${host}` : 'Open') : null;
  return html`<${Page} title=${`${agentName()}'s computer`} sub=${`${agentName()}'s own browser on your server`}>
    ${screen}
    ${line && html`<div class="live-line mu">${line}</div>`}
    ${usable && html`<div class="big2">
      <button onClick=${watch}><${Icon} name="eye" size=${16} /> Watch</button>
      <button class="p" onClick=${takeOver}><${Icon} name="hand" size=${16} /> Take over</button></div>`}
    ${usable && html`<div class="gnote">Take over to sign in to a site yourself or help ${agentName()} past a tricky step. ${agentName()} waits while you're in control. Sign-ins stay in its browser.</div>`}
    <${Group} label="Recently">
      ${!act ? html`<${Spinner} />` : act.items.length ? act.items.map((it) => html`<${Row} title=${activityTitle(it)} sub=${placeLabel(it)}
        value=${stamp(it.created_at)} chevron=${false} onClick=${it.conversation_kind === 'job' ? undefined : () => go(`chat/${it.conversation_id}`)} />`)
        : html`<div class="r"><div class="rt mu">Nothing yet</div></div>`}
      ${act?.items.length > 0 && html`<${Row} title="See all activity" accent onClick=${() => go('settings/activity')} />`}
    <//>
    <${Group} label="Safety" note=${`Pausing stops ${agentName()} right away. Disconnecting turns off every connection at once.`}>
      <${Row} title=${`Pause ${agentName()}`} sub=${paused ? 'Paused: not replying or browsing' : safety?.pending_approvals ? `${safety.pending_approvals} waiting for your OK` : 'Running'}
        right=${html`<${Toggle} on=${paused} onChange=${pause} label=${`Pause ${agentName()}`} />`} />
      ${b?.running && html`<${Row} title="Close the browser" sub="Sign-ins are kept" chevron=${false}
        onClick=${async () => { try { setBrowserState((await post('/browser/stop')).browser); } catch (e) { toast(e.message); } }} />`}
      <${Row} icon="users" title="Trusted people" sub="Messages to them don’t need your OK" onClick=${() => go('iris/connections/trusted')} />
      <${Row} title="Disconnect everything" danger chevron=${false} onClick=${disconnectAll} />
    <//>
  <//>`;
}
