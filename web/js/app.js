// Iris app shell: pairing, top bar, drawer, bottom tabs, routing, overlays.
import { html, render, useEffect, useRef, useState } from './h.js';
import { api, get, post, patch, del, setToken, hasToken, onUnauthorized, connectEvents, ApiError, seg, isId } from './api.js';
import {
  S, set, useStore, go, toast, handleEvent, loadConvs, mainId, isOwner, parseRoute,
} from './store.js';
import { Icon, Avatar, Img, Sheet, agentName, promptSheet, confirmSheet, openSheet, closeSheet, LetterTile } from './ui.js';
import { ChatScreen } from './chat.js';
import { FeedScreen, StoryViewer } from './feed.js';
import { IdeasScreen, GoalsScreen, LibraryScreen } from './lists.js';
import { Hub, Logins, Computer, Files, FileEditor, Folder } from './hub.js';
import { Connections, AddService, ConnectorPage, ConnectorForm, TrustedPeople } from './connect.js';
import { Settings, AiEngine, Devices, People, ActivityLog } from './settings.js';
import { isIOS, standalone } from './push.js';

const TABS = [
  ['chat', 'Chat', 'chat'],
  ['foryou', 'For you', 'sparkles'],
  ['ideas', 'Ideas', 'bulb'],
  ['goals', 'Goals', 'target'],
  ['library', 'Library', 'library'],
];

// ----------------------------------------------------------------- pairing

function deviceName() {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return 'iPhone';
  if (/iPad/.test(ua) || (isIOS() && !/iPhone/.test(ua))) return 'iPad';
  if (/Android/.test(ua)) return /Mobile/.test(ua) ? 'Android phone' : 'Android tablet';
  if (/Mac/.test(ua)) return 'Mac';
  if (/Windows/.test(ua)) return 'Windows PC';
  return 'Browser';
}

function Pair() {
  const [code, setCode] = useState('');
  const [err, setErr] = useState('');
  const [busy, setBusy] = useState(false);
  const ref = useRef();
  useEffect(() => { ref.current?.focus(); }, []);
  const onInput = (e) => {
    const raw = e.target.value.toUpperCase().replace(/[^0-9A-Z]/g, '').slice(0, 8);
    setCode(raw.length > 4 ? `${raw.slice(0, 4)}-${raw.slice(4)}` : raw);
    setErr('');
  };
  const submit = async (e) => {
    e.preventDefault();
    setBusy(true);
    try {
      const r = await post('/pair', { code, device_name: deviceName() });
      setToken(r.token);
      boot();
    } catch (x) {
      setErr(x.code === 'locked' ? 'Too many tries. Wait a few minutes, then get a new code.'
        : x.code === 'invalid_code' ? "That code didn't work. Codes work once and last 10 minutes."
        : x.message);
      setBusy(false);
    }
  };
  const ios = isIOS() && !standalone();
  return html`<div class="pair">
    <div class="pair-in">
      <div class="avatar" style="width:84px;height:84px"><div class="orb" /></div>
      <h1 class="serif">Pair this ${/iPhone|Android/.test(navigator.userAgent) ? 'phone' : 'device'}</h1>
      <p class="mu">Run <code>iris pair</code> on your server and type the code it shows.</p>
      <form onSubmit=${submit}>
        <input ref=${ref} class="code-in" value=${code} onInput=${onInput} placeholder="XXXX-XXXX"
          autocomplete="one-time-code" autocapitalize="characters" spellcheck=${false} aria-label="Pairing code" />
        ${err && html`<div class="err-line">${err}</div>`}
        <button class="b1 wide" disabled=${busy || code.replace('-', '').length < 8}>${busy ? 'Pairing…' : 'Continue'}</button>
      </form>
      ${ios && html`<p class="hint mu">Tip: tap Share, then <b>Add to Home Screen</b> so Iris opens like an app and can send you notifications.</p>`}
    </div>
  </div>`;
}

// ------------------------------------------------------------------ drawer

function Drawer() {
  const [q, setQ] = useState('');
  const convs = S.convs || [];
  const cid = currentChat();
  const match = (c) => !q.trim() || title(c).toLowerCase().includes(q.trim().toLowerCase());
  const chats = convs.filter((c) => c.kind !== 'group' && c.member_count <= 1).filter(match);
  const together = convs.filter((c) => c.kind === 'group' || c.member_count > 1).filter(match);
  const newSide = async () => {
    try { const r = await post('/conversations', { kind: 'side', title: '' }); go(`chat/${r.conversation.id}`); } catch (e) { toast(e.message); }
  };
  const newTogether = async () => {
    try {
      const { users } = await get('/users');
      const others = users.filter((u) => !u.is_owner);
      if (!others.length) { go('settings/people'); toast('Add a person first'); return; }
      openSheet(() => html`<div class="sheet-body"><h3>Chat together</h3><p class="mu">Pick who to chat with. ${agentName()} joins too.</p>
        <div class="grp">${others.map((u) => html`<div class="r tap" onClick=${async () => {
          closeSheet();
          try { const r = await post('/conversations', { kind: 'group', title: `You and ${u.display_name}`, member_ids: [u.id] }); go(`chat/${r.conversation.id}`); } catch (e) { toast(e.message); }
        }}><${LetterTile} name=${u.display_name} radius=${15} /><div class="rt">${u.display_name}</div></div>`)}</div></div>`);
    } catch (e) { toast(e.message); }
  };
  const Item = ({ c, icon }) => html`<button class=${'it' + (c.id === cid && S.route.tab === 'chat' ? ' on' : '')} onClick=${() => go(`chat/${c.id}`)}>
    <${Icon} name=${icon} size=${18} /><span>${title(c)}</span></button>`;
  return html`<div class=${'scrim' + (S.drawer ? ' on' : '')} onClick=${() => set({ drawer: false })} />
    <nav class=${'drw' + (S.drawer ? ' on' : '')} aria-hidden=${S.drawer ? 'false' : 'true'}>
      <label class="srch"><${Icon} name="search" size=${17} /><input value=${q} placeholder="Search chats" onInput=${(e) => setQ(e.target.value)} /></label>
      <div class="drw-scroll">
        <div class="sm">Chats</div>
        ${chats.map((c) => html`<${Item} c=${c} icon=${c.kind === 'main' ? 'chat' : c.seed?.type === 'feed_post' ? 'sparkles' : c.seed?.type === 'idea' ? 'bulb' : 'chat'} />`)}
        <button class="it ac" onClick=${newSide}><${Icon} name="plus" size=${18} /><span>New side chat</span></button>
        ${(together.length > 0 || isOwner()) && html`<div class="sm">Together</div>`}
        ${together.map((c) => html`<${Item} c=${c} icon="users" />`)}
        ${isOwner() && html`<button class="it ac" onClick=${newTogether}><${Icon} name="plus" size=${18} /><span>New together chat</span></button>`}
      </div>
      <button class="profile" onClick=${() => go('iris')}>
        <${Avatar} size=${38} /><div class="grow"><b>${agentName()}</b><small>Connections, logins, settings</small></div>
        <${Icon} name="chev" size=${16} />
      </button>
    </nav>`;
}

const title = (c) => (c.kind === 'main' ? 'Main chat' : c.title || (c.kind === 'group' ? 'Together' : 'New chat'));

// Route ids go into API paths: anything but a server id opens the main chat.
function currentChat() {
  const r = S.route;
  return r.tab === 'chat' && isId(r.parts[1]) ? r.parts[1] : mainId();
}

const okSeg = (s) => !!s && s !== '.' && s !== '..';

// ------------------------------------------------------------------ top bar

function chatMenu(c) {
  const rename = async () => {
    closeSheet();
    const t = await promptSheet({ title: 'Rename chat', value: c.title });
    if (t == null) return;
    try { await patch(`/conversations/${seg(c.id)}`, { title: t.trim() }); loadConvs(); } catch (e) { toast(e.message); }
  };
  const archive = async () => {
    closeSheet();
    try { await patch(`/conversations/${seg(c.id)}`, { archived: true }); await loadConvs(); go('chat'); } catch (e) { toast(e.message); }
  };
  const remove = async () => {
    closeSheet();
    if (!(await confirmSheet({ title: 'Delete this chat?', body: 'It is deleted for everyone in it.', yes: 'Delete', danger: true }))) return;
    try { await del(`/conversations/${seg(c.id)}`); await loadConvs(); go('chat'); } catch (e) { toast(e.message); }
  };
  openSheet(() => html`<div class="sheet-body"><h3>${title(c)}</h3><div class="grp">
    <div class="r tap" onClick=${rename}><div class="rt">Rename</div></div>
    <div class="r tap" onClick=${archive}><div class="rt">Archive</div></div>
    <div class="r tap danger" onClick=${remove}><div class="rt">Delete</div></div></div></div>`);
}

function TopBar() {
  const cid = currentChat();
  const conv = (S.convs || []).find((c) => c.id === cid);
  const busy = Object.keys(S.live).length > 0;
  let line = 'here for you';
  if (!S.online) line = 'reconnecting…';
  else if (S.settings?.paused) line = 'paused';
  else if (S.route.tab === 'chat' && conv && conv.kind !== 'main') line = title(conv);
  else if (busy) line = 'working on it';
  return html`<header class="top">
    <button class="btn" aria-label="Menu" onClick=${() => set({ drawer: true })}><${Icon} name="menu" /></button>
    <button class="who-btn" onClick=${() => go('iris')} aria-label=${`${agentName()} profile`}><${Avatar} size=${36} /></button>
    <div class="grow"><div class="nm">${agentName()}</div><div class=${'st' + (!S.online || S.settings?.paused ? ' warn' : '')}>${line}</div></div>
    ${S.route.tab === 'chat' && conv && conv.kind !== 'main' && html`<button class="btn" aria-label="Chat options" onClick=${() => chatMenu(conv)}><${Icon} name="more" cls="ic-more" /></button>`}
  </header>`;
}

function Tabs() {
  return html`<nav class="nav" aria-label="Sections">${TABS.map(([k, label, icon]) => html`
    <button class=${S.route.tab === k ? 'on' : ''} aria-current=${S.route.tab === k ? 'page' : undefined}
      onClick=${() => go(k === 'chat' ? `chat/${mainId()}` : k)}>
      <${Icon} name=${icon} size=${23} /><span>${label}</span></button>`)}</nav>`;
}

function Lightbox() {
  if (!S.lightbox) return null;
  const close = () => set({ lightbox: null, lightboxChat: null });
  return html`<div class="lightbox" onClick=${close}>
    <button class="x" aria-label="Close"><${Icon} name="x" size=${26} /></button>
    <${Img} src=${S.lightbox} alt="" />
    ${S.lightboxChat && html`<button class="b2 lb-go" onClick=${(e) => { e.stopPropagation(); const c = S.lightboxChat; close(); go(`chat/${c}`); }}>Open chat</button>`}
  </div>`;
}

// -------------------------------------------------------------------- root

function Screen() {
  const { parts, tab } = S.route;
  const owner = isOwner();
  switch (tab) {
    case 'chat': {
      const cid = currentChat();
      return html`<${ChatScreen} cid=${cid} key=${cid} />`;
    }
    case 'foryou': return html`<${FeedScreen} />`;
    case 'ideas': return html`<${IdeasScreen} />`;
    case 'goals': return html`<${GoalsScreen} />`;
    case 'library': return html`<${LibraryScreen} />`;
    case 'iris': {
      const [, sub, a, ...rest] = parts;
      if (!sub) return html`<${Hub} />`;
      if (!owner) return html`<${Settings} />`;
      if (sub === 'connections') {
        if (a === 'add' && ['google', 'github', 'custom'].includes(rest[0])) return html`<${AddService} service=${rest[0]} key=${rest[0]} />`;
        if (a === 'new') return html`<${ConnectorForm} id="new" key="new" />`;
        if (a === 'trusted') return html`<${TrustedPeople} />`;
        if (isId(a) && rest[0] === 'edit') return html`<${ConnectorForm} id=${a} key=${`${a}-edit`} />`;
        if (isId(a)) return html`<${ConnectorPage} id=${a} key=${a} />`;
        return html`<${Connections} />`;
      }
      if (sub === 'logins') return html`<${Logins} />`;
      if (sub === 'computer') return html`<${Computer} />`;
      if (sub === 'files') {
        if (!okSeg(a) || (a === 'system' && !okSeg(rest.join('/')))) return html`<${Files} />`;
        if (a === 'system') return html`<${FileEditor} area="system" path=${rest.join('/')} key=${rest.join('/')} />`;
        if (S.route.query.get('edit')) return html`<${FileEditor} area=${a} path=${rest.join('/')} key=${rest.join('/')} />`;
        return html`<${Folder} area=${a} path=${rest.join('/')} key=${a + rest.join('/')} />`;
      }
      return html`<${Hub} />`;
    }
    case 'settings': {
      const sub = parts[1];
      if (sub === 'ai' && owner) return html`<${AiEngine} />`;
      if (sub === 'devices') return html`<${Devices} />`;
      if (sub === 'people' && owner) return html`<${People} />`;
      if (sub === 'activity') return html`<${ActivityLog} />`;
      return html`<${Settings} />`;
    }
    default: return html`<${ChatScreen} cid=${mainId()} key=${mainId()} />`;
  }
}

function App() {
  useStore();
  if (!S.me) {
    return S.bootError ? html`<div class="pair"><div class="pair-in"><div class="avatar" style="width:72px;height:72px"><div class="orb" /></div>
      <h1 class="serif">Can't reach Iris</h1><p class="mu">${S.bootError}</p>
      <button class="b1" onClick=${() => location.reload()}>Try again</button></div></div>`
      : hasToken() ? html`<div class="boot"><div class="avatar" style="width:64px;height:64px"><div class="orb pulse" /></div></div>` : html`<${Pair} />`;
  }
  const tabbed = TABS.some(([k]) => k === S.route.tab);
  return html`<div class=${'app' + (tabbed ? ' tabbed' : ' paged') + (S.route.tab === 'chat' ? ' is-chat' : '')}>
    ${tabbed && html`<${TopBar} />`}
    <main class="main"><${Screen} /></main>
    ${tabbed && html`<${Tabs} />`}
    <${Drawer} />
    <${StoryViewer} />
    <${Lightbox} />
    <${Sheet} />
    ${S.toast && html`<div class="toast" role="status">${S.toast}</div>`}
  </div>`;
}

// -------------------------------------------------------------------- boot

let events;
async function boot() {
  set({ bootError: null });
  if (!hasToken()) { set({ me: null }); return; }
  try {
    const me = await api('GET', '/me', undefined, { timeout: 8000 });
    set({ me });
    await loadConvs();
    events?.close();
    events = connectEvents(handleEvent, (online) => set({ online }));
    if (!location.hash || location.hash === '#/' || location.hash === '#') history.replaceState(null, '', `#/chat/${me.main_conversation_id}`);
    set({ route: { ...S.route, ...parseRoute() } });
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) return;
    set({ bootError: e.message });
  }
}

window.addEventListener('online', () => { if (!S.me && hasToken()) boot(); });

onUnauthorized(() => {
  events?.close();
  setToken('');
  set({ me: null, convs: null, msgs: {}, live: {} });
});

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').catch(() => {});
  navigator.serviceWorker.addEventListener('message', (e) => {
    if (e.data?.type === 'open' && e.data.url) location.hash = e.data.url.replace(/^\/?#?/, '#');
  });
}

render(html`<${App} />`, document.getElementById('app'));
boot();
