// Connections: the small catalog (Google, GitHub, custom MCP server), the
// step-by-step setup guides, signing a service in through Iris's callback,
// each connection's rules ("Reads freely · asks before sending"), its
// actions, recent use, and the trusted people list.
import { html, useEffect, useState } from './h.js';
import { get, post, put, patch, del, seg, ApiError } from './api.js';
import { S, set, useStore, go, toast, loadConnectors } from './store.js';
import {
  Icon, Row, Group, Page, Spinner, Toggle, LetterTile, Empty,
  agentName, stamp, confirmSheet, openSheet, closeSheet,
} from './ui.js';

const BRAND = { google: '#4285f4', github: '#24292f' };

export function ServiceTile({ c, size = 30 }) {
  const svc = c.service || c.id;
  if (BRAND[svc]) {
    const name = svc === 'google' ? 'G' : 'GH';
    return html`<div class="tile" style=${`width:${size}px;height:${size}px;border-radius:9px;background:${BRAND[svc]};font-size:${svc === 'github' ? 11 : 14}px`}>${name}</div>`;
  }
  return html`<${LetterTile} name=${c.name || c.label} size=${size} />`;
}

/** "send_message" -> "Send message". */
export function humanize(tool) {
  const w = String(tool || '').replace(/([a-z0-9])([A-Z])/g, '$1 $2').split(/[^A-Za-z0-9]+/).filter(Boolean).join(' ').toLowerCase();
  return w ? w[0].toUpperCase() + w.slice(1) : tool;
}

export const labelOf = (c) => ({ google: 'Google', github: 'GitHub' }[c?.service] || c?.name || 'Connection');

export function statusOf(c) {
  if (!c.enabled) return 'Off';
  if (c.paused) return 'Paused';
  if (c.auth_state === 'needs_sign_in') return 'Needs sign-in';
  if (c.auth_state === 'needs_reconnect') return 'Sign in again';
  return c.rules;
}

const DECISION = {
  auto: 'Ran on its own',
  trusted: 'Sent to someone you trust',
  approved: 'You approved it',
  denied: 'You said no',
  expired: 'No answer in time',
  cancelled: 'Cancelled',
  blocked: 'Stopped by your rules',
  pending: 'Waiting for your OK',
};

export function callRow(it, showConnector) {
  const what = `${showConnector ? `${it.connector_name} · ` : ''}${humanize(it.tool)}`;
  const failed = it.outcome === 'error';
  const sub = [DECISION[it.decision] || it.decision, failed ? 'it failed' : null, it.summary || null].filter(Boolean).join(' · ');
  return html`<${Row} title=${what} sub=${sub} value=${stamp(it.created_at)} chevron=${false}
    onClick=${it.conversation_id ? () => go(`chat/${it.conversation_id}`) : undefined} />`;
}

// ------------------------------------------------------------- the list

export function Connections() {
  useStore();
  const [cat, setCat] = useState(null);
  useEffect(() => {
    loadConnectors().catch((e) => toast(e.message));
    get(`/connectors/catalog?origin=${encodeURIComponent(location.origin)}`).then(setCat).catch(() => {});
  }, []);
  const list = S.connectors || [];
  const services = (cat?.services || []).filter((s) => s.id !== 'custom' && !s.connector_id);
  return html`<${Page} title="Connections" sub=${`Services ${agentName()} can use for you. Or just ask: "help me set up Google".`}>
    ${!S.connectors ? html`<${Spinner} />` : html`
      <${Group} label="Connected" note=${list.length ? `${agentName()} asks you before sending, buying, deleting, or contacting someone new.` : null}>
        ${list.length ? list.map((c) => html`<${Row} icon=${html`<${ServiceTile} c=${c} />`} title=${labelOf(c)}
          sub=${statusOf(c)} onClick=${() => go(`iris/connections/${c.id}`)} />`)
          : html`<div class="r"><div class="rt mu">Nothing connected yet</div></div>`}
      <//>
      <${Group} label="Add a connection">
        ${services.map((s) => html`<${Row} icon=${html`<${ServiceTile} c=${s} />`} title=${s.label} sub=${s.description}
          onClick=${() => go(`iris/connections/add/${s.id}`)} />`)}
        <${Row} icon="plus" title="Custom (MCP server)" sub="For tinkerers: any MCP server, by web address or program" onClick=${() => go('iris/connections/new')} />
      <//>
      <${Group} note="Messages only to these people don't need your OK.">
        <${Row} icon="users" title="Trusted people" onClick=${() => go('iris/connections/trusted')} />
      <//>`}
  <//>`;
}

// ----------------------------------------------------------- signing in

/** Sign a connection in with the service's own page; finishes through
 * Iris's callback, or by pasting the address when Iris isn't on https. */
export function ServiceSignIn({ con, label, onDone }) {
  useStore();
  const [flow, setFlow] = useState(null);
  const [client, setClient] = useState(null);
  const [busy, setBusy] = useState(false);
  const [url, setUrl] = useState('');
  const [cid, setCid] = useState('');
  const [secret, setSecret] = useState('');
  const live = (S.connectors || []).find((c) => c.id === con.id) || con;
  const connected = live.auth_state === 'ok' && live.oauth?.connected;
  useEffect(() => { if (connected && flow) { setFlow(null); onDone?.(); } }, [connected]);
  const start = async () => {
    setBusy(true);
    try {
      setFlow(await post(`/connectors/${seg(con.id)}/oauth/start`, { origin: location.origin }));
      setClient(null);
    } catch (e) {
      if (e instanceof ApiError && e.code === 'needs_client') setClient(e.message);
      else toast(e.message);
    }
    setBusy(false);
  };
  const paste = async (e) => {
    e.preventDefault();
    setBusy(true);
    try {
      await post('/oauth/complete', { url: url.trim() });
      await loadConnectors();
      setFlow(null);
      toast(`${label} is connected`);
      onDone?.();
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  const saveClient = async (e) => {
    e.preventDefault();
    setBusy(true);
    try {
      await post(`/connectors/${seg(con.id)}/oauth/client`, { client_id: cid.trim(), client_secret: secret.trim() || undefined });
      await start();
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  if (client) {
    return html`<form class="form" onSubmit=${saveClient}>
      <div class="lock">${client}</div>
      <label class="lbl">Client ID</label><input class="field mono" value=${cid} required autocapitalize="off" spellcheck=${false} onInput=${(e) => setCid(e.target.value)} />
      <label class="lbl">Client secret (if it has one)</label><input class="field mono" type="password" value=${secret} autocomplete="off" onInput=${(e) => setSecret(e.target.value)} />
      <button class="b1 wide" disabled=${busy}>Save and sign in</button></form>`;
  }
  if (!flow) {
    return html`<button class="b1 wide" disabled=${busy} onClick=${start}>${busy ? 'Starting…' : `Sign in with ${label}`}</button>`;
  }
  return html`<ol class="steps-list">
      <li><span>Open ${label}'s sign-in page and approve.</span>
        <a class="pill" href=${flow.authorize_url} target="_blank" rel="noopener noreferrer">Open sign-in <${Icon} name="external" size=${14} /></a></li>
      ${flow.paste ? html`<li><span>Your browser will end on a page that doesn't load. That's expected: copy that page's whole address and paste it here.</span>
        <form class="code-row" onSubmit=${paste}>
          <input class="field" value=${url} placeholder="http://127.0.0.1…" autocapitalize="off" spellcheck=${false} onInput=${(e) => setUrl(e.target.value)} />
          <button class="b1 sm" disabled=${busy || !url.trim()}>Connect</button></form></li>`
      : html`<li><span>Come back here when it says connected.</span></li>`}
    </ol>
    ${!flow.paste && html`<div class="status shimmer">Waiting for you to approve…</div>`}`;
}

// ------------------------------------------------------------- set-up guides

function Copy({ text }) {
  return html`<button class="copy-box mono" onClick=${() => navigator.clipboard?.writeText(text).then(() => toast('Copied'))}>
    <span>${text}</span><${Icon} name="copy" size=${15} /></button>`;
}

function Ext({ href, children }) {
  return html`<a class="pill" href=${href} target="_blank" rel="noopener noreferrer">${children} <${Icon} name="external" size=${13} /></a>`;
}

function useCatalog() {
  const [cat, setCat] = useState(null);
  useEffect(() => { get(`/connectors/catalog?origin=${encodeURIComponent(location.origin)}`).then(setCat).catch((e) => toast(e.message)); }, []);
  return cat;
}

function GoogleSetup({ entry, con }) {
  const [f, setF] = useState({ client_id: '', client_secret: '' });
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(con);
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    try {
      const r = await post('/connectors/catalog/google', f);
      setSaved(r.connector);
      loadConnectors();
      toast('Saved. Now sign in with Google.');
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  const done = saved?.oauth?.connected && saved?.auth_state === 'ok';
  return html`
    <div class="lock"><${Icon} name="lock" size=${15} /> Google's own sign-in, with your own Google Cloud project: your mail and files go only between this server and Google. It takes about five minutes, once.</div>
    ${entry.missing?.length > 0 && html`<div class="warn-box">Your server needs <b>uv</b> to run the Google connector (an open-source Google Workspace MCP server). Install it once on the server: <code>curl -LsSf https://astral.sh/uv/install.sh | sh</code></div>`}
    <ol class="guide">
      <li><b>Create a Google Cloud project</b> (any name, like "Iris").<div><${Ext} href="https://console.cloud.google.com/projectcreate">Open Google Cloud<//></div></li>
      <li><b>Turn on Gmail, Calendar, and Drive</b> for that project. Tap each and press Enable.
        <div class="pill-row"><${Ext} href="https://console.cloud.google.com/apis/library/gmail.googleapis.com">Gmail API<//>
        <${Ext} href="https://console.cloud.google.com/apis/library/calendar-json.googleapis.com">Calendar API<//>
        <${Ext} href="https://console.cloud.google.com/apis/library/drive.googleapis.com">Drive API<//></div></li>
      <li><b>Set up the sign-in screen.</b> Choose <i>External</i>, fill in the app name and your email, then under Audience add your own Google address as a test user.
        <div><${Ext} href="https://console.cloud.google.com/auth/overview">Open sign-in setup<//></div>
        <small class="mu">Tip: while the app is in testing, Google signs you out every 7 days. Publishing it (Audience, Publish app) keeps you signed in; Google will warn that it's unverified, which is fine for your own use.</small></li>
      <li><b>Create an OAuth client</b> of type <i>Web application</i>, and add this as an <i>Authorized redirect URI</i>:
        <${Copy} text=${entry.redirect_uri} />
        <div><${Ext} href="https://console.cloud.google.com/auth/clients/create">Create client<//></div>
        ${!entry.redirect_direct && html`<small class="mu">Iris isn't on a secure (https) address, so after you approve, Google sends you to a page that won't load. That's expected: you'll copy its address back into the app.</small>`}</li>
      <li><b>Paste the Client ID and Client secret</b> Google shows you.
        <form class="form" onSubmit=${save}>
          <input class="field mono" placeholder=${saved?.oauth?.client_configured ? 'Saved (paste to replace)' : '….apps.googleusercontent.com'} value=${f.client_id} autocapitalize="off" spellcheck=${false}
            onInput=${(e) => setF({ ...f, client_id: e.target.value })} />
          <input class="field mono gap" type="password" placeholder=${saved?.oauth?.client_configured ? 'Saved (paste to replace)' : 'Client secret'} value=${f.client_secret} autocomplete="off"
            onInput=${(e) => setF({ ...f, client_secret: e.target.value })} />
          <button class="b2 wide gap" disabled=${busy || (!f.client_id && !saved)}>Save</button>
        </form></li>
      <li><b>Sign in with Google.</b>
        ${done ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> Connected${saved.oauth.account ? ` as ${saved.oauth.account}` : ''}</div>`
          : saved?.oauth?.client_configured ? html`<${ServiceSignIn} con=${saved} label="Google" onDone=${() => { loadConnectors(); go(`iris/connections/${saved.id}`); }} />`
          : html`<small class="mu">Save the client first.</small>`}</li>
    </ol>`;
}

function GitHubSetup({ entry, con }) {
  const [mode, setMode] = useState(con?.oauth ? 'oauth' : 'token');
  const [f, setF] = useState({ token: '', client_id: '', client_secret: '' });
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(con);
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    try {
      const body = mode === 'token' ? { token: f.token } : { client_id: f.client_id, client_secret: f.client_secret };
      const r = await post('/connectors/catalog/github', body);
      setSaved(r.connector);
      await loadConnectors();
      if (!r.needs_sign_in) { toast('GitHub is connected'); go(`iris/connections/${r.connector.id}`); }
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  return html`
    <div class="seg">${[['token', 'Access token'], ['oauth', 'Sign in (OAuth app)']].map(([k, l]) => html`<button class=${mode === k ? 'on' : ''} onClick=${() => setMode(k)}>${l}</button>`)}</div>
    ${mode === 'token' ? html`<ol class="guide">
      <li><b>Create a fine-grained personal access token.</b> Pick the repositories ${agentName()} may use and the permissions you want (for example Issues and Pull requests: read and write).
        <div><${Ext} href="https://github.com/settings/personal-access-tokens/new">Open GitHub<//></div></li>
      <li><b>Paste the token.</b> It's stored encrypted on your server and only ever sent to GitHub.
        <form class="form" onSubmit=${save}><input class="field mono" type="password" placeholder="github_pat_…" value=${f.token} autocomplete="off" onInput=${(e) => setF({ ...f, token: e.target.value })} />
        <button class="b1 wide gap" disabled=${busy || !f.token.trim()}>Connect</button></form></li></ol>`
    : html`<ol class="guide">
      <li><b>Create an OAuth app</b> on GitHub, with this <i>Authorization callback URL</i>:
        <${Copy} text=${entry.redirect_uri} /><div><${Ext} href="https://github.com/settings/applications/new">New OAuth app<//></div></li>
      <li><b>Paste its Client ID and a new client secret.</b>
        <form class="form" onSubmit=${save}>
          <input class="field mono" placeholder="Client ID" value=${f.client_id} autocapitalize="off" spellcheck=${false} onInput=${(e) => setF({ ...f, client_id: e.target.value })} />
          <input class="field mono gap" type="password" placeholder="Client secret" value=${f.client_secret} autocomplete="off" onInput=${(e) => setF({ ...f, client_secret: e.target.value })} />
          <button class="b2 wide gap" disabled=${busy || !f.client_id.trim()}>Save</button></form></li>
      <li><b>Sign in with GitHub.</b>
        ${saved?.oauth?.client_configured ? html`<${ServiceSignIn} con=${saved} label="GitHub" onDone=${() => go(`iris/connections/${saved.id}`)} />`
          : html`<small class="mu">Save the app first.</small>`}</li></ol>`}`;
}

export function AddService({ service }) {
  useStore();
  const cat = useCatalog();
  useEffect(() => { if (!S.connectors) loadConnectors().catch(() => {}); }, []);
  if (service === 'custom') return html`<${ConnectorForm} id="new" />`;
  const entry = cat?.services?.find((s) => s.id === service);
  const con = (S.connectors || []).find((c) => c.service === service);
  if (cat && !entry) return html`<${Page} title="Connect"><${Empty} icon="plug" title="Unknown service">Pick one from Connections.<//><//>`;
  return html`<${Page} title=${`Connect ${entry?.label || ''}`} sub=${entry?.description}>
    ${!cat ? html`<${Spinner} />` : service === 'google' ? html`<${GoogleSetup} entry=${entry} con=${con} />`
      : html`<${GitHubSetup} entry=${entry} con=${con} />`}
  <//>`;
}

// ------------------------------------------------------------ one connection

const MODES = [
  ['standard', 'Reads freely, asks before sending, buying, or deleting'],
  ['careful', 'Reads freely, asks before any change'],
  ['read_only', 'Only reads, never changes anything'],
  ['ask_always', 'Asks every time'],
];
const EFFECT = { runs: 'Runs on its own', asks: 'Asks you first', off: 'Off', 'asks unless trusted': 'Asks unless to someone you trust' };

function ToolSheet({ con, tool, onSaved }) {
  const choose = async (rule) => {
    closeSheet();
    try {
      const r = await put(`/connectors/${seg(con.id)}/policy`, { tools: { [tool.name]: rule } });
      onSaved(r.connector);
    } catch (e) { toast(e.message); }
  };
  const opts = [[null, 'Follow the rules above'], ['allow', 'Always run on its own'], ['ask', 'Always ask me'], ['block', 'Never']];
  return html`<div class="sheet-body"><h3>${tool.label}</h3>${tool.description && html`<p class="mu">${tool.description}</p>`}
    <div class="grp">${opts.map(([v, l]) => html`<div class="r tap" onClick=${() => choose(v)}><div class="rt">${l}</div>
      <div class=${'rad' + ((tool.rule || null) === v ? ' on' : '')} /></div>`)}</div>
    ${tool.category === 'purchase' && html`<p class="gnote">Purchases and payments always ask, whatever you pick.</p>`}</div>`;
}

export function ConnectorPage({ id }) {
  useStore();
  const [tools, setTools] = useState(null);
  const [recent, setRecent] = useState(null);
  const con = (S.connectors || []).find((c) => c.id === id);
  const loadTools = () => get(`/connectors/${seg(id)}/tools`).then(setTools).catch((e) => setTools({ tools: [], error: e.message }));
  useEffect(() => {
    if (!S.connectors) loadConnectors().catch(() => {});
    get(`/connectors/activity?connector_id=${encodeURIComponent(id)}&limit=8`).then((r) => setRecent(r.items)).catch(() => setRecent([]));
  }, [id]);
  useEffect(() => { if (con?.enabled && con.auth_state === 'ok') loadTools(); }, [id, con?.enabled, con?.auth_state, con?.updated_at]);
  if (!con) return html`<${Page} title="Connection">${S.connectors ? html`<${Empty} icon="plug" title="Not found">That connection is gone.<//>` : html`<${Spinner} />`}<//>`;
  const label = labelOf(con);
  const replace = (c) => set({ connectors: S.connectors.map((x) => (x.id === c.id ? c : x)) });
  const toggle = async (on) => {
    try { replace((await post(`/connectors/${seg(id)}/${on ? 'connect' : 'disconnect'}`)).connector); } catch (e) { toast(e.message); }
  };
  const resume = async () => {
    try { replace((await post(`/connectors/${seg(id)}/resume`)).connector); toast(`${label} is back on`); } catch (e) { toast(e.message); }
  };
  const mode = async (m) => {
    try { replace((await put(`/connectors/${seg(id)}/policy`, { mode: m })).connector); loadTools(); } catch (e) { toast(e.message); }
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Remove ${label}?`, body: `${agentName()} won't be able to use it anymore. Its saved secrets and sign-in are deleted from this server.`, yes: 'Remove', danger: true }))) return;
    try { await del(`/connectors/${seg(id)}`); await loadConnectors(); go('iris/connections'); } catch (e) { toast(e.message); }
  };
  const needsSignIn = con.oauth && con.auth_state !== 'ok';
  return html`<${Page} title=${label} sub=${con.oauth?.account ? `Connected as ${con.oauth.account}` : con.description || 'A connected service'}>
    ${con.paused && html`<div class="warn-box"><b>Paused.</b> I stopped using ${label} because ${con.paused_reason}. Check the recent activity below, then resume it if everything looks right.
      <div class="act"><button class="b1 sm" onClick=${resume}>Resume</button></div></div>`}
    ${needsSignIn && con.enabled && html`<div class="card-x wide-card"><b>${con.auth_state === 'needs_reconnect' ? `Sign in to ${label} again` : `Sign in to ${label}`}</b>
      <p>${con.auth_state === 'needs_reconnect' ? 'The sign-in expired or was revoked.' : 'One more step: approve access on its own page.'}</p>
      <${ServiceSignIn} con=${con} label=${label} onDone=${() => loadConnectors()} /></div>`}
    <${Group}><${Row} title="On" sub="Off means I don't use it at all" right=${html`<${Toggle} on=${con.enabled} onChange=${toggle} label="Connection on" />`} /><//>
    <${Group} label="What I can do on my own" note="Purchases and payments always ask. Messages only to your trusted people don't.">
      ${MODES.map(([m, l]) => html`<div class="r tap" onClick=${() => mode(m)}><div class="rt">${l}</div><div class=${'rad' + (con.policy?.mode === m ? ' on' : '')} /></div>`)}
    <//>
    ${con.enabled && !needsSignIn && html`<${Group} label="Actions" note=${tools?.error ? null : 'Tap one to change what it does.'}>
      ${!tools ? html`<div class="r"><div class="rt mu">Starting ${label}…</div></div>`
        : tools.error ? html`<div class="r"><div class="rt">${tools.error}${tools.detail && html`<small class="mono">${tools.detail}</small>`}</div></div>`
        : tools.tools.map((t) => html`<${Row} title=${t.label} sub=${EFFECT[t.effect] || t.effect} value=${t.rule ? 'custom' : ''}
            onClick=${() => openSheet(() => html`<${ToolSheet} con=${con} tool=${t} onSaved=${(c) => { replace(c); loadTools(); }} />`)} />`)}
    <//>`}
    <${Group} label="Recently">
      ${!recent ? html`<${Spinner} />` : recent.length ? recent.map((it) => callRow(it, false)) : html`<div class="r"><div class="rt mu">Not used yet</div></div>`}
    <//>
    <${Group}>
      <${Row} title="Advanced settings" sub=${con.transport === 'stdio' ? 'Program, arguments, secrets' : 'Web address, headers'} onClick=${() => go(`iris/connections/${id}/edit`)} />
      <${Row} title="Remove connection" danger onClick=${remove} chevron=${false} />
    <//>
  <//>`;
}

// ---------------------------------------------------- custom / advanced form

function KeyValues({ label, keys, value, onChange, placeholder }) {
  return html`<label class="lbl">${label}</label>
    <textarea class="field mono" rows="3" value=${value} placeholder=${placeholder}
      onInput=${(e) => onChange(e.target.value)} />
    ${keys?.length > 0 && html`<small class="mu">Saved: ${keys.join(', ')} (values hidden). Leave empty to keep them.</small>`}`;
}

const parseKV = (text) => Object.fromEntries(text.split('\n').map((l) => l.trim()).filter(Boolean).map((l) => {
  const i = l.search(/[=:]/);
  return i < 0 ? [l, ''] : [l.slice(0, i).trim(), l.slice(i + 1).trim()];
}));

export function ConnectorForm({ id }) {
  useStore();
  const isNew = id === 'new';
  const c = isNew ? null : (S.connectors || []).find((k) => k.id === id);
  const [f, setF] = useState({ name: '', description: '', transport: 'http', auth: 'oauth', command: '', args: '', url: '', env: '', headers: '' });
  const [busy, setBusy] = useState(false);
  useEffect(() => { if (!isNew && !S.connectors) loadConnectors().catch(() => {}); }, []);
  useEffect(() => {
    if (c) setF({ name: c.name, description: c.description, transport: c.transport, auth: c.oauth ? 'oauth' : 'none', command: c.command || '', args: (c.args || []).join('\n'), url: c.url || '', env: '', headers: '' });
  }, [c?.id]);
  const up = (k) => (v) => setF({ ...f, [k]: v });
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    const body = { description: f.description };
    if (f.transport === 'stdio') {
      body.command = f.command.trim();
      body.args = f.args.split('\n').map((a) => a.trim()).filter(Boolean);
      if (f.env.trim() || isNew) body.env = parseKV(f.env);
    } else {
      body.url = f.url.trim();
      if (f.headers.trim() || (isNew && f.auth !== 'oauth')) body.headers = parseKV(f.headers);
    }
    try {
      if (isNew) {
        const r = await post('/connectors', { ...body, name: f.name.trim(), transport: f.transport, auth: f.transport === 'http' ? f.auth : 'none' });
        await loadConnectors();
        toast('Added');
        go(`iris/connections/${r.connector.id}`);
      } else {
        await patch(`/connectors/${seg(id)}`, body);
        await loadConnectors();
        toast('Saved');
        go(`iris/connections/${id}`);
      }
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  if (!isNew && !c) return html`<${Page} title="Connection"><${Spinner} /><//>`;
  return html`<${Page} title=${isNew ? 'Custom connection' : `${labelOf(c)}: advanced`}
    sub=${isNew ? 'Add any MCP server. It runs through Iris, which keeps its secrets encrypted on your server and asks you before risky actions.' : 'For tinkerers. Secrets are never shown again.'}>
    <form class="form" onSubmit=${save}>
      ${isNew && html`<label class="lbl">Name</label>
        <input class="field" value=${f.name} placeholder="e.g. calendar" pattern="[A-Za-z0-9_\\-]{1,48}" required onInput=${(e) => up('name')(e.target.value)} />
        <label class="lbl">How it runs</label>
        <div class="seg">${['http', 'stdio'].map((t) => html`<button type="button" class=${f.transport === t ? 'on' : ''} onClick=${() => up('transport')(t)}>${t === 'stdio' ? 'Program on the server' : 'Web address'}</button>`)}</div>`}
      <label class="lbl">Description</label>
      <input class="field" value=${f.description} placeholder="What it's for" onInput=${(e) => up('description')(e.target.value)} />
      ${f.transport === 'stdio' ? html`
        <label class="lbl">Command</label>
        <input class="field mono" value=${f.command} placeholder="npx" required onInput=${(e) => up('command')(e.target.value)} />
        <label class="lbl">Arguments (one per line)</label>
        <textarea class="field mono" rows="3" value=${f.args} placeholder=${'-y\nsome-mcp-server'} onInput=${(e) => up('args')(e.target.value)} />
        <${KeyValues} label="Secrets (KEY=value, one per line)" keys=${c?.env_keys} value=${f.env} onChange=${up('env')} placeholder="API_TOKEN=..." />`
      : html`
        <label class="lbl">Web address</label>
        <input class="field mono" value=${f.url} placeholder="https://..." required onInput=${(e) => up('url')(e.target.value)} />
        ${isNew && html`<label class="lbl">Signing in</label>
          <div class="seg">${[['oauth', 'Sign in on its page'], ['none', 'Key or none']].map(([k, l]) => html`<button type="button" class=${f.auth === k ? 'on' : ''} onClick=${() => up('auth')(k)}>${l}</button>`)}</div>
          ${f.auth === 'oauth' && html`<small class="mu">For servers that follow the MCP sign-in standard. You'll sign in on the service's own page next.</small>`}`}
        ${(f.auth !== 'oauth' || !isNew) && html`<${KeyValues} label="Headers (Name: value, one per line)" keys=${c?.header_keys} value=${f.headers} onChange=${up('headers')} placeholder="Authorization: Bearer ..." />`}`}
      <button class="b1 wide" disabled=${busy}>${isNew ? 'Add connection' : 'Save'}</button>
    </form>
  <//>`;
}

// ---------------------------------------------------------- trusted people

export function TrustedPeople() {
  const [list, setList] = useState(null);
  const [v, setV] = useState('');
  useEffect(() => { get('/safety').then((s) => setList(s.trusted_recipients)).catch((e) => toast(e.message)); }, []);
  const save = async (next) => {
    try { setList((await put('/safety', { trusted_recipients: next })).trusted_recipients); } catch (e) { toast(e.message); }
  };
  const add = async (e) => {
    e.preventDefault();
    if (!v.trim()) return;
    await save([...(list || []), v.trim()]);
    setV('');
  };
  return html`<${Page} title="Trusted people" sub=${`Messages and emails only to these people go out without asking you first. Everything else that sends still waits for your OK.`}>
    ${!list ? html`<${Spinner} />` : html`<${Group}>
      ${list.map((r) => html`<${Row} icon=${html`<${LetterTile} name=${r} radius=${15} />`} title=${r} chevron=${false}
        right=${html`<button class="icon-btn" aria-label=${`Remove ${r}`} onClick=${() => save(list.filter((x) => x !== r))}><${Icon} name="trash" size=${16} /></button>`} />`)}
      ${!list.length && html`<div class="r"><div class="rt mu">No one yet</div></div>`}
    <//>
    <form class="code-row" onSubmit=${add}><input class="field" value=${v} placeholder="Email address or phone number" autocapitalize="off" inputmode="email" onInput=${(e) => setV(e.target.value)} />
      <button class="b1 sm" disabled=${!v.trim()}>Add</button></form>`}
  <//>`;
}

// ------------------------------------------------------------- activity log

export function ServiceActivity() {
  const [items, setItems] = useState(null);
  const [more, setMore] = useState(false);
  const load = async (before) => {
    const r = await get(`/connectors/activity?limit=50${before ? `&before=${before}` : ''}`);
    setItems((cur) => (before ? [...(cur || []), ...r.items] : r.items));
    setMore(r.has_more);
  };
  useEffect(() => { load().catch((e) => toast(e.message)); }, []);
  if (!items) return html`<${Spinner} />`;
  if (!items.length) return html`<${Empty} icon="plug" title="Nothing yet">Every time I use a connected service, it shows up here, with whether it ran on its own or you approved it.<//>`;
  return html`<${Group}>${items.map((it) => callRow(it, true))}<//>
    ${more && html`<button class="b2 wide" onClick=${() => load(items.at(-1).created_at)}>Show more</button>`}`;
}

