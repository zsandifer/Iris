// Connections: the catalog by group (Google as its services, GitHub,
// one-tap services such as Notion, custom MCP servers), the step-by-step
// setup guides, signing a service in through Iris's callback, each
// service's permission screen (Allow / Ask / Off per action), recent use,
// and the trusted people list.
import { html, useEffect, useRef, useState } from './h.js';
import { get, post, put, patch, del, seg, ApiError } from './api.js';
import { S, set, useStore, go, toast, loadConnectors } from './store.js';
import {
  Icon, Row, Group, Page, Spinner, Toggle, LetterTile, Empty,
  agentName, stamp, confirmSheet, openSheet, closeSheet,
} from './ui.js';

import { ExternalLink } from './external-link.js';
import { googlePasteError } from './google-paste.js';
import {
  SERVICES, CATALOG_IDS, GOOGLE_PARTS, brandOf, topRows, rowAction, rowStatus, rowTarget, ruleFor, CHOICE_LABEL,
} from './catalog.js';

export { CATALOG_IDS };

export function ServiceTile({ c, size = 30 }) {
  const b = brandOf(c);
  if (b) {
    const font = Math.round(size * (b.mark.length > 2 ? 0.3 : b.mark.length > 1 ? 0.37 : 0.47));
    return html`<div class="tile brand" aria-hidden="true" style=${`width:${size}px;height:${size}px;border-radius:${Math.round(size * 0.3)}px;background:${b.bg};color:${b.fg || '#fff'};font-size:${font}px`}>${b.mark}</div>`;
  }
  return html`<${LetterTile} name=${c.name || c.label} size=${size} radius=${Math.round(size * 0.3)} />`;
}

/** "send_message" -> "Send message". */
export function humanize(tool) {
  const w = String(tool || '').replace(/([a-z0-9])([A-Z])/g, '$1 $2').split(/[^A-Za-z0-9]+/).filter(Boolean).join(' ').toLowerCase();
  return w ? w[0].toUpperCase() + w.slice(1) : tool;
}

export const labelOf = (c) => SERVICES[c?.service]?.name || c?.name || 'Connection';

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

function useCatalog() {
  const [cat, setCat] = useState(null);
  const load = () => get(`/connectors/catalog?origin=${encodeURIComponent(location.origin)}`).then(setCat).catch((e) => toast(e.message));
  // Reload when a connection changes (connected, signed out, turned off).
  useEffect(() => { load(); }, [S.connectors]);
  return cat;
}

function CatalogRow({ row }) {
  const action = rowAction(row);
  return html`<${Row} icon=${html`<${ServiceTile} c=${row} size=${31} />`} title=${row.label} sub=${rowStatus(row)}
    chevron=${false} onClick=${() => go(rowTarget(row))}
    right=${html`<span class=${'row-act' + (action === 'Connect' ? ' connect' : '')}>${action}</span>`} />`;
}

function GroupHead({ label, more, onMore }) {
  return html`<div class="cat-head"><span>${label}</span>
    ${more && html`<button class="see-all" onClick=${onMore}>See all<${Icon} name="chev" size=${14} /></button>`}</div>`;
}

export function Connections() {
  useStore();
  useEffect(() => { loadConnectors().catch((e) => toast(e.message)); }, []);
  const cat = useCatalog();
  const custom = (S.connectors || []).filter((c) => c.service === 'custom');
  return html`<${Page} title="Connections" sub=${`Services ${agentName()} can use for you. Or just ask: "help me set up Gmail".`}>
    ${!cat ? html`<${Spinner} />` : html`
      ${(cat.groups || []).map((g) => {
        const { rows, more } = topRows(g);
        return html`<${GroupHead} label=${g.label} more=${more} onMore=${() => go(`iris/connections/group/${g.id}`)} />
          <${Group}>${rows.map((r) => html`<${CatalogRow} row=${r} key=${r.service + (r.part || '')} />`)}<//>`;
      })}
      <${GroupHead} label="Custom" />
      <${Group}>
        ${custom.map((c) => html`<${Row} icon=${html`<${ServiceTile} c=${c} size=${31} />`} title=${c.name} sub=${statusOf(c) === c.rules ? null : statusOf(c)}
          chevron=${false} onClick=${() => go(`iris/connections/${c.id}`)} right=${html`<span class="row-act">View</span>`} />`)}
        <${Row} icon="plus" title="Custom connection" sub="Any MCP server, by web address or program" chevron=${false}
          onClick=${() => go('iris/connections/new')} right=${html`<span class="row-act connect">Connect</span>`} />
      <//>
      <${Group} note=${`${agentName()} asks you before sending, buying, deleting, or contacting someone new. Messages only to these people don't need your OK.`}>
        <${Row} icon="users" title="Trusted people" onClick=${() => go('iris/connections/trusted')} />
      <//>`}
  <//>`;
}

/** Every service in one catalog group. */
export function CatalogGroup({ id }) {
  useStore();
  useEffect(() => { if (!S.connectors) loadConnectors().catch(() => {}); }, []);
  const cat = useCatalog();
  const g = cat?.groups?.find((x) => x.id === id);
  if (cat && !g) return html`<${Page} title="Connections"><${Empty} icon="plug" title="Nothing here">Pick a service from Connections.<//><//>`;
  return html`<${Page} title=${g?.label || 'Connections'}>
    ${!g ? html`<${Spinner} />` : html`<${Group}>${g.rows.map((r) => html`<${CatalogRow} row=${r} key=${r.service + (r.part || '')} />`)}<//>`}
  <//>`;
}

// ----------------------------------------------------------- signing in

/** Sign a connection in with the service's own page; finishes through
 * Iris's callback, or by pasting the address when Iris isn't on https. */
export function ServiceSignIn({ con, label, onDone, auto = false }) {
  useStore();
  const [flow, setFlow] = useState(null);
  const [client, setClient] = useState(null);
  const [busy, setBusy] = useState(false);
  const [url, setUrl] = useState('');
  const [cid, setCid] = useState('');
  const [secret, setSecret] = useState('');
  const live = (S.connectors || []).find((c) => c.id === con.id) || con;
  // Done when a sign-in newer than the one there when this flow started
  // lands (signing in again keeps the old grant until then).
  const signedInAt = live.auth_state === 'ok' && live.oauth?.connected ? live.oauth.connected_at || 1 : 0;
  useEffect(() => {
    if (flow && signedInAt && signedInAt !== flow.since) { setFlow(null); onDone?.(); }
  }, [signedInAt]);
  const start = async () => {
    setBusy(true);
    try {
      const r = await post(`/connectors/${seg(con.id)}/oauth/start`, { origin: location.origin });
      setFlow({ ...r, since: signedInAt });
      setClient(null);
    } catch (e) {
      if (e instanceof ApiError && e.code === 'needs_client') setClient(e.message);
      else toast(e.message);
    }
    setBusy(false);
  };
  // Right after one tap on Connect: go straight to the service's page link.
  useEffect(() => { if (auto) start(); }, []);
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
  if (client && con.service === 'google') return html`<button class="b1 wide" onClick=${() => go('iris/connections/add/google')}>Open Google setup</button>`;
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
        <${ExternalLink} url=${flow.authorize_url}>Open sign-in<//></li>
      ${flow.paste ? html`<li><span>Your browser will end on a page that doesn't load. That's expected: copy that page's whole address and paste it here.</span>
        <form class="code-row" onSubmit=${paste}>
          <input class="field" value=${url} placeholder="http://127.0.0.1…" autocapitalize="off" spellcheck=${false} onInput=${(e) => setUrl(e.target.value)} />
          <button class="b1 sm" disabled=${busy || !url.trim()}>Connect</button></form></li>`
      : html`<li><span>Come back here when it says connected.</span></li>`}
    </ol>
    ${!flow.paste && html`<div class="status shimmer">Waiting for you to approve…</div>`}
    <button class="lk-m" disabled=${busy} onClick=${start}>Start sign-in again</button>`;
}

// ------------------------------------------------------------- set-up guides

function Copy({ text }) {
  const copy = async () => {
    try { await navigator.clipboard.writeText(text); toast('Copied'); }
    catch { toast('I couldn’t copy it. Select the value and copy it.'); }
  };
  return html`<button class="copy-box mono" type="button" aria-label=${`Copy ${text}`} onClick=${copy}>
    <span>${text}</span><${Icon} name="copy" size=${15} /></button>`;
}

function Ext({ href, children }) {
  return html`<${ExternalLink} url=${href}>${children}<//>`;
}

// The Google APIs the setup turns on, one link each (for when the combined
// link misses some).
const GOOGLE_APIS = [['api_gmail', 'Gmail'], ['api_calendar', 'Calendar'], ['api_drive', 'Drive'], ['api_youtube', 'YouTube'],
  ['api_docs', 'Docs'], ['api_sheets', 'Sheets'], ['api_slides', 'Slides'], ['api_forms', 'Forms'], ['api_tasks', 'Tasks'], ['api_people', 'Contacts (People API)']];

function GoogleSetup({ entry, con }) {
  useStore();
  const [setup, setSetup] = useState(null);
  const [paste, setPaste] = useState('');
  const [email, setEmail] = useState('');
  const [project, setProject] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef(false);
  const revision = useRef(0);
  const reload = async () => {
    if (pending.current) return;
    const mark = ++revision.current;
    try { const r = await get('/connectors/google/setup'); if (revision.current === mark) setSetup(r); }
    catch (e) { if (revision.current === mark) setError(e.message); }
  };
  useEffect(() => { reload(); }, [S.connectors]);
  useEffect(() => {
    const visible = () => { if (!document.hidden) { reload(); loadConnectors().catch(() => {}); } };
    window.addEventListener('focus', visible);
    document.addEventListener('visibilitychange', visible);
    return () => { window.removeEventListener('focus', visible); document.removeEventListener('visibilitychange', visible); };
  }, []);
  const p = setup?.progress;
  useEffect(() => { setProject(p?.project_id || ''); }, [p?.project_id]);
  useEffect(() => { if (p?.step !== 6 || p?.phase !== 'setup') setPaste(''); }, [p?.step, p?.phase]);
  const advance = async (step, sign_in = false, project_id) => {
    pending.current = true; revision.current++; setBusy(true); setError('');
    try {
      const r = await put('/connectors/google/setup', { step, sign_in, project_id });
      setSetup(r);
      if (project_id !== undefined) setProject(r.progress.project_id || '');
    }
    catch (e) { setError(e.message); }
    pending.current = false; setBusy(false);
  };
  const save = async (e) => {
    e.preventDefault(); pending.current = true; revision.current++; setBusy(true); setError('');
    try {
      setSetup(await post('/connectors/google/setup/client', { paste }));
      setPaste('');
      await loadConnectors();
    } catch (e) { setError(e.message); }
    pending.current = false; setBusy(false);
  };
  const back = () => {
    if (busy) { go('iris/connections', true); return; }
    if (!p || p.phase === 'connected' || (p.step === 1 && p.phase === 'setup')) go('iris/connections', true);
    else advance(!setup.redirect_uri && p.step >= 4 ? 4 : p.phase === 'sign_in' ? 6 : p.step - 1);
  };
  const report = async (code) => {
    pending.current = true; revision.current++; setBusy(true); setError('');
    try { setSetup(await post('/connectors/google/setup/error', { code })); }
    catch (e) { setError(e.message); }
    pending.current = false; setBusy(false);
  };
  const titles = ['Create a project', 'Turn on Google services', 'Set up your sign-in screen', 'Create your login key', 'Keep yourself signed in', 'Paste your login key'];
  const title = p?.phase === 'connected' ? 'Google connected' : p?.phase === 'sign_in' ? 'Sign in with Google' : titles[(p?.step || 1) - 1];
  const validation = paste ? googlePasteError(paste) : null;
  const pages = setup?.pages;
  const open = (key, label = 'Open') => html`<${ExternalLink} url=${pages[key]} cls="b1 wide">${label}<//>`;
  const projectField = html`<label class="lbl" for="google-project">Project ID</label>
    <input id="google-project" class="field mono" value=${project} placeholder="example-project-123456"
      autocomplete="off" autocapitalize="off" spellcheck=${false} aria-describedby="google-project-help"
      onInput=${(e) => { setProject(e.target.value); setError(''); }} />
    <small id="google-project-help" class="mu">Paste the ID, or the Google Console page’s address.</small>`;
  return html`<${Page} title=${title} sub="One setup for Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks, Contacts and YouTube read-only." onBack=${back}>
    ${error && html`<p class="err-line" role="alert">${error}</p>`}
    ${!p ? html`<${Spinner} />${error && html`<button class="b2 wide" onClick=${reload}>Try again</button>`}` : html`
      <div class="wizard-progress" aria-label=${p.phase === 'setup' ? `Step ${p.step} of 6` : p.phase === 'sign_in' ? 'Setup complete. Sign in with Google.' : 'Connected'}>
        ${titles.map((t, i) => html`<span class=${i + 1 <= p.step ? 'on' : ''} aria-current=${p.phase === 'setup' && i + 1 === p.step ? 'step' : undefined} title=${t}></span`)}
        <small>${p.phase === 'setup' ? `Step ${p.step} of 6` : p.phase === 'sign_in' ? 'Last thing: sign in' : 'Connected ✓'}</small>
      </div>
      ${p.error && html`<div class="warn-box" role="alert">${p.error.message}</div>`}
      ${entry.missing?.length > 0 && p.phase === 'sign_in' && html`<div class="warn-box">I need the Google helper installed on my server before I can use this connection. Ask whoever runs your Iris server to install uv.</div>`}
      ${p.phase === 'connected' && p.more_access ? html`<div class="ok-line"><${Icon} name="check" /> Connected ✓</div>
        <p>Google works as before. I can now also use Docs, Sheets, Slides, Forms, Tasks, Contacts and Gmail filters.</p>
        <p>First turn on the new services in your Google project (it's fine that some are already on):</p>${open('apis', 'Turn on Google services')}
        <p>Then sign in again and leave every box checked.</p>
        ${con ? html`<${ServiceSignIn} con=${con} label="Google" onDone=${reload} />` : html`<${Spinner} />`}`
      : p.phase === 'connected' ? html`<div class="ok-line"><${Icon} name="check" /> Connected ✓</div><p>Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks, Contacts and YouTube read-only are ready. ${agentName()} uses your approval rules for Google actions.</p>
        <button class="b1 wide" onClick=${() => go('iris/connections', true)}>Done</button>
        <button class="b2 wide gap" disabled=${busy} onClick=${() => advance(6)}>Replace login key</button>`
      : p.phase === 'sign_in' ? html`<p>Google asks your permission so I can use every service with your approval rules.</p>
        <p>“Google hasn't verified this app” is expected for your own private app. Tap <b>Advanced → Go to Iris</b>, then approve access and leave every box checked.</p>
        ${!setup.redirect_uri ? html`<div class="warn-box">I need my server’s HTTPS address set first.</div><button class="b1 wide" onClick=${() => advance(4)}>Set up HTTPS</button>` : con ? html`<${ServiceSignIn} con=${con} label="Google" onDone=${reload} />` : html`<${Spinner} />`}
        <details class="alt"><summary>Google showed a problem</summary><p>If Google stopped before returning here, choose what its page says:</p>
          ${[['redirect_uri_mismatch', 'Callback address doesn’t match'], ['access_denied', 'Access denied'], ['invalid_client', 'Invalid login key'], ['unauthorized_client', 'Wrong kind of login key']].map(([code, label]) => html`<button class="b2 wide gap" disabled=${busy} onClick=${() => report(code)}>${label}</button>`)}
        </details>`
      : html`<div class="google-step" key=${p.step}>
        ${p.step >= 2 && p.step <= 5 && p.project_id && html`<p>You should see <b class="mono">${p.project_id}</b> on Google’s page. If it shows a different project, tap the project picker at the top and choose it.</p>`}
        ${p.step === 1 && html`<p>A project gives your private Iris app a home in Google.</p>
          <p>The name field may say <b>My Project 12345</b>. Set it to <b>Iris</b> before tapping <b>Create</b>; either name works.</p>
          <${Copy} text="Iris" />${open('project')}
          <p>After tapping <b>Create</b>, copy the <b>Project ID</b> shown under the project name and paste it here.</p>
          <p>Google uses this ID to choose your project, not the name.</p>
          <form class="form" onSubmit=${(e) => { e.preventDefault(); advance(2, false, project); }}>
            ${projectField}<button class="b1 wide gap" disabled=${busy || !project.trim()}>${busy ? 'Saving…' : 'Save Project ID and continue'}</button>
          </form>`}
        ${p.step === 2 && html`<p>Turning these on lets Iris use your mail, calendar, files, documents, spreadsheets, slides, forms, tasks, contacts and YouTube subscriptions.</p>
          <p>Turn on all ten services together.</p>${open('apis')}
          <details class="alt"><summary>Didn't turn on all ten?</summary><p>Open each one and tap <b>Enable</b>:</p>
            ${GOOGLE_APIS.map(([k, l]) => html`<${ExternalLink} url=${pages[k]} cls="b2 wide gap">${l}<//>`)}</details>`}
        ${p.step === 3 && html`<p>This tells Google what to show when you sign in to your own app.</p>
          <p>On <b>Branding</b>, fill all three required fields:</p>
          <ul>
            <li><b>App name:</b> Iris</li>
            <li><b>User support email:</b> pick your Gmail.</li>
            <li><b>Developer contact information:</b> your Gmail. This is at the very bottom of the Branding page and easy to miss.</li>
          </ul>
          ${setup.redirect_uri && html`<p>Under <b>App domain</b>, Google needs two links before it lets you publish. Iris serves both:</p>
            <p><b>Application home page:</b></p><${Copy} text=${setup.redirect_uri.replace(/\/api\/oauth\/callback$/, '/')} />
            <p><b>Application privacy policy link:</b></p><${Copy} text=${setup.redirect_uri.replace(/\/api\/oauth\/callback$/, '/privacy')} />`}
          <p>Leave the logo and terms of service empty. A logo makes Google require a review.</p>
          ${setup.authorized_domain ? html`<p>Under <b>Authorized domains</b>, add this domain:</p>
            <${Copy} text=${setup.authorized_domain} />
            <p>Domain only — no https://, no port, no path.</p>`
            : html`<div class="warn-box">I need my server’s HTTPS address set first to show the authorized domain.</div>
              <a class="pill" href="#/iris/connections/google-help">How to set up HTTPS</a>`}
          <p>Then tap <b>Save</b> at the bottom of Branding.</p>
          <${Copy} text="Iris" /><label class="lbl" for="google-email">Your Google email (for copying)</label>
          <input id="google-email" class="field" type="email" value=${email} autocomplete="email" onInput=${(e) => setEmail(e.target.value)} />
          ${email.trim() && html`<${Copy} text=${email.trim()} />`}${open('branding', 'Open Branding')}
          <p>Then on Audience, choose <b>External</b> and add your own Google account as a <b>test user</b>.</p>
          <${Copy} text="External" />${open('audience', 'Open Audience')}`}
        ${p.step === 4 && html`<p>A login key lets Google send your sign-in back to this Iris server.</p>
          ${!setup.redirect_uri ? html`<div class="warn-box">I need my server’s <code>public_url</code> set to its HTTPS address first. Set it in config.toml and restart Iris, then come back here.</div>
            <a class="pill" href="#/iris/connections/google-help">How to set up HTTPS</a>`
          : html`<p>Choose <b>Web application</b>. Under <b>Authorized redirect URIs</b>, add this exact address. Save, then download the client JSON or copy the ID and secret for step 6.</p>
            <${Copy} text="Web application" /><${Copy} text=${setup.redirect_uri} />${open('client')}
            <small class="mu">On older Google screens this is <b>APIs &amp; Services → Credentials → Create credentials → OAuth client ID</b>.</small>`}`}
        ${p.step === 5 && html`<p>Publishing prevents Google from signing you out every 7 days.</p>
          <p>On Audience, tap <b>Publish app</b> and confirm that it says <b>In production</b>. Your app is still for your own private use.</p>
          <p>If <b>Publish app</b> is greyed out, Google says to complete Branding: go back to step 3 and make sure all three fields, the two App domain links, and the <b>Authorized domains</b> entry are filled in, then <b>Save</b>.</p>
          <p>When you sign in, Google will say <b>“Google hasn't verified this app”</b>. That’s expected because this is your own app: tap <b>Advanced → Go to Iris</b>.</p>${open('audience')}`}
        ${p.step === 6 && html`<p>I keep your login key encrypted so Google actions go through your approval rules.</p>
          <p>Paste the whole downloaded JSON, or the client ID and secret together, in any order. Labels and line breaks are fine.</p>
          <form class="form" onSubmit=${save}><label class="lbl" for="google-paste">Your login key</label>
            <textarea id="google-paste" class="field mono" rows="6" value=${paste} autocomplete="off" autocapitalize="off" spellcheck=${false} aria-describedby="google-paste-feedback"
              onInput=${(e) => { setPaste(e.target.value); setError(''); }} />
            <small id="google-paste-feedback" class=${validation ? 'err-line' : 'mu'} role="status">${validation || (paste ? 'I found both parts. Ready to save.' : 'Your key stays here, never in chat.')}</small>
            <button class="b1 wide gap" disabled=${busy || !paste || !!validation}>${busy ? 'Saving…' : 'Done — save key'}</button>
          </form>
          ${con?.oauth?.client_configured && html`<button class="b2 wide gap" disabled=${busy} onClick=${() => advance(6, true)}>Use saved key</button>`}`}
        ${p.step > 1 && html`<details class="alt"><summary>${p.project_id ? 'Change Project ID' : 'Add Project ID to open the right project'}</summary>
          <form class="form" onSubmit=${(e) => { e.preventDefault(); advance(p.step, false, project); }}>
            ${projectField}<button class="b2 wide gap" disabled=${busy || !project.trim()}>${busy ? 'Saving…' : 'Save Project ID'}</button>
          </form></details>`}
        ${p.step > 1 && p.step < 6 && html`<button class="b1 wide gap" disabled=${busy || (p.step === 4 && !setup.redirect_uri)} onClick=${() => advance(p.step + 1)}>${busy ? 'Saving…' : 'Done'}</button>`}
      </div>`}
      <button class="b2 wide gap" onClick=${back}>Back</button>
      <small class="mu">Your place is saved. You can leave and come back anytime.</small>
    `}
  <//>`;
}

export function GoogleHttpsHelp() {
  return html`<${Page} title="Give Iris an HTTPS address" sub="Google needs a secure address to send your sign-in back to Iris.">
    <p>If you use Tailscale, turn on MagicDNS and HTTPS certificates in its admin console. On the Iris server run:</p>
    <${Copy} text="tailscale serve --bg 7420" />
    <p>Use your Iris port if it differs from 7420. Copy the HTTPS address it prints into <code>public_url</code> in your Iris config.toml, then restart Iris. A private HTTPS reverse proxy works too.</p>
    <p>Open Iris at that HTTPS address on your phone, then return to Google setup.</p>
    <${ExternalLink} url="https://tailscale.com/kb/1312/serve">Open Tailscale’s guide<//>
    <a class="b1 wide gap" href="#/iris/connections/add/google">Back to Google setup</a>
  <//>`;
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

/** A service with its own hosted server: one Connect button, then approve
 * on the service's page. Iris registers itself, so there is nothing to
 * create or paste. */
function OneTapSetup({ entry, con }) {
  useStore();
  const [saved, setSaved] = useState(con || null);
  const [fresh, setFresh] = useState(false);
  const [busy, setBusy] = useState(false);
  const live = (S.connectors || []).find((c) => c.id === saved?.id) || saved;
  const open = (c) => go(`iris/connections/${c.id}`, true);
  const connect = async () => {
    setBusy(true);
    try {
      const r = await post(`/connectors/catalog/${seg(entry.id)}`, {});
      await loadConnectors();
      if (!r.needs_sign_in) { open(r.connector); return; }
      setFresh(true);
      setSaved(r.connector);
    } catch (e) { toast(e.message); }
    setBusy(false);
  };
  if (live?.auth_state === 'ok') {
    return html`<div class="ok-line"><${Icon} name="check" /> Connected ✓</div>
      <button class="b1 wide gap" onClick=${() => open(live)}>Done</button>`;
  }
  return html`<p>You'll approve access on ${entry.label}'s own page. Nothing to create or paste.</p>
    <p class="mu">${agentName()} reads freely and asks you before changing, sending, or deleting anything. You can change that later.</p>
    ${live ? html`<${ServiceSignIn} con=${live} label=${entry.label} auto=${fresh}
        onDone=${() => { toast(`${entry.label} is connected`); open(live); }} />`
      : html`<button class="b1 wide" disabled=${busy} onClick=${connect}>${busy ? 'Connecting…' : 'Connect'}</button>`}`;
}

export function AddService({ service }) {
  useStore();
  const cat = useCatalog();
  useEffect(() => { if (!S.connectors) loadConnectors().catch(() => {}); }, []);
  if (service === 'custom') return html`<${ConnectorForm} id="new" />`;
  const entry = cat?.services?.find((s) => s.id === service);
  const con = (S.connectors || []).find((c) => c.service === service);
  if (cat && !entry) return html`<${Page} title="Connect"><${Empty} icon="plug" title="Unknown service">Pick one from Connections.<//><//>`;
  if (service === 'google' && entry) return html`<${GoogleSetup} entry=${entry} con=${con} />`;
  return html`<${Page} title=${`Connect ${entry?.label || ''}`} sub=${entry?.description}>
    ${!cat ? html`<${Spinner} />` : entry.setup === 'oauth_sign_in' ? html`<${OneTapSetup} entry=${entry} con=${con} />` : html`<${GitHubSetup} entry=${entry} con=${con} />`}
  <//>`;
}

// ------------------------------------------------------------ one connection

const MODES = [
  ['standard', 'Reads freely, asks before sending, buying, or deleting'],
  ['careful', 'Reads freely, asks before any change'],
  ['read_only', 'Only reads, never changes anything'],
  ['ask_always', 'Asks every time'],
];

function ModeSheet({ con, onSaved }) {
  const choose = async (m) => {
    closeSheet();
    try { onSaved((await put(`/connectors/${seg(con.id)}/policy`, { mode: m })).connector); } catch (e) { toast(e.message); }
  };
  return html`<div class="sheet-body"><h3>Starting rules</h3>
    <p class="mu">What each action does until you choose Allow, Ask or Off for it. Purchases and payments always ask.</p>
    <div class="grp">${MODES.map(([m, l]) => html`<div class="r tap" role="button" tabindex="0" onClick=${() => choose(m)}><div class="rt">${l}</div>
      <div class=${'rad' + (con.policy?.mode === m ? ' on' : '')} /></div>`)}</div></div>`;
}

/** Allow / Ask / Off for one action (a native picker, styled quietly). */
function PermissionRow({ a, onChoose, disabled }) {
  return html`<div class="r perm">
    <div class="rt">${a.label}${a.note && html`<small>${a.note}</small>`}</div>
    <label class="perm-pick">
      <span>${CHOICE_LABEL[a.value] || a.value}</span>
      <svg class="ic" width="14" height="14" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2"
        stroke-linecap="round" stroke-linejoin="round"><path d="M8 9l4-4 4 4M8 15l4 4 4-4" /></svg>
      <select aria-label=${`${a.label}: ${CHOICE_LABEL[a.value] || a.value}`} value=${a.value} disabled=${disabled}
        onChange=${(e) => onChoose(a, e.target.value)}>
        ${a.choices.map((c) => html`<option value=${c}>${CHOICE_LABEL[c]}</option>`)}
      </select>
    </label>
  </div>`;
}

function PermissionSections({ perms, con, onSaved, disabled }) {
  const choose = async (a, choice) => {
    try {
      const r = await put(`/connectors/${seg(con.id)}/policy`, { tools: { [a.id]: ruleFor(choice, a.default) } });
      onSaved(r.connector);
    } catch (e) { toast(e.message); }
  };
  return perms.sections.map((sec) => html`<${Group} label=${sec.label}>
    ${sec.actions.map((a) => html`<${PermissionRow} a=${a} key=${a.id} disabled=${disabled} onChoose=${choose} />`)}<//>`);
}

const cap = (t) => (t ? t[0].toUpperCase() + t.slice(1) : t);

function Hero({ c, title, sub }) {
  return html`<div class="svc-hero"><${ServiceTile} c=${c} size=${64} /><h1 class="ttl">${title}</h1>${sub && html`<div class="sub">${sub}</div>`}</div>`;
}

/**
 * A service's permission screen. `part` is a Google service (Gmail,
 * Calendar...); without it, the whole connection: for Google its
 * services, otherwise the tools its server lists.
 */
export function ConnectorPage({ id, part = null }) {
  useStore();
  const [perms, setPerms] = useState(null);
  const [recent, setRecent] = useState(null);
  const con = (S.connectors || []).find((c) => c.id === id);
  const google = con?.service === 'google';
  const svc = part && GOOGLE_PARTS[part] ? part : null;
  const loadPerms = () => get(`/connectors/${seg(id)}/permissions${svc ? `?service=${encodeURIComponent(svc)}` : ''}`)
    .then(setPerms).catch((e) => setPerms({ sections: [], error: e.message }));
  useEffect(() => {
    if (!S.connectors) loadConnectors().catch(() => {});
    get(`/connectors/activity?connector_id=${encodeURIComponent(id)}&limit=8`).then((r) => setRecent(r.items)).catch(() => setRecent([]));
  }, [id]);
  const ready = con && con.enabled && con.auth_state === 'ok';
  // A Google service's actions need no running server; other lists do.
  useEffect(() => { if (con && (svc || ready)) loadPerms(); }, [id, svc, con?.enabled, con?.auth_state, con?.updated_at]);
  if (!con) return html`<${Page} title="Connection">${S.connectors ? html`<${Empty} icon="plug" title="Not found">That connection is gone.<//>` : html`<${Spinner} />`}<//>`;
  if (part && (!google || !svc)) return html`<${Page} title="Connection"><${Empty} icon="plug" title="Not found">That service isn't part of this connection.<//><//>`;

  const label = svc ? GOOGLE_PARTS[svc].name : labelOf(con);
  const replace = (c) => { set({ connectors: S.connectors.map((x) => (x.id === c.id ? c : x)) }); };
  const saved = (c) => { replace(c); loadPerms(); };
  const needsSignIn = con.oauth && con.auth_state !== 'ok';
  const account = perms?.account || con.oauth?.account;
  const serviceOn = svc ? !(con.policy?.off || []).includes(svc) : con.enabled;
  const toggle = async (on) => {
    try {
      if (svc) saved((await put(`/connectors/${seg(id)}/policy`, { services: { [svc]: on } })).connector);
      else replace((await post(`/connectors/${seg(id)}/${on ? 'connect' : 'disconnect'}`)).connector);
    } catch (e) { toast(e.message); }
  };
  const resume = async () => {
    try { replace((await post(`/connectors/${seg(id)}/resume`)).connector); toast(`${labelOf(con)} is back on`); } catch (e) { toast(e.message); }
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Remove ${labelOf(con)}?`, body: `${agentName()} won't be able to use it anymore. Its saved secrets, sign-in and rules are deleted from this server.`, yes: 'Remove', danger: true }))) return;
    try { await del(`/connectors/${seg(id)}`); await loadConnectors(); go('iris/connections', true); } catch (e) { toast(e.message); }
  };
  // Disconnect signs out (keeping the rules) where there is a sign-in;
  // otherwise it removes the connection.
  const disconnect = async () => {
    if (!con.oauth) { remove(); return; }
    const body = google
      ? `This signs ${agentName()} out of Google, so Gmail, Calendar, Drive and every other Google service stop working until you connect again. Your choices here are kept.`
      : `This signs ${agentName()} out of ${label}. Your choices here are kept, so connecting again is one sign-in.`;
    if (!(await confirmSheet({ title: `Disconnect ${google ? 'Google' : label}?`, body, yes: 'Disconnect', danger: true }))) return;
    try { replace((await post(`/connectors/${seg(id)}/oauth/sign-out`)).connector); toast(`${google ? 'Google' : label} is disconnected`); } catch (e) { toast(e.message); }
  };
  const signInSheet = () => openSheet(() => html`<div class="sheet-body"><h3>Sign in to ${labelOf(con)}</h3>
    <p class="mu">Choose the account on ${labelOf(con)}'s page. It replaces the one signed in now.</p>
    <${ServiceSignIn} con=${con} label=${labelOf(con)} onDone=${closeSheet} /></div>`);
  const sub = svc ? perms?.service?.description || '' : con.description || 'A connected service';
  const tileFor = svc ? { service: 'google', part: svc } : con;
  const parts = google && !svc ? Object.entries(GOOGLE_PARTS) : [];
  const otherTools = !svc && perms?.sections?.length > 0;

  return html`<${Page} title=${null}>
    <${Hero} c=${tileFor} title=${label} sub=${sub} />
    ${con.paused && html`<div class="warn-box"><b>Paused.</b> I stopped using ${labelOf(con)} because ${con.paused_reason}. Check the recent activity below, then resume it if everything looks right.
      <div class="act"><button class="b1 sm" onClick=${resume}>Resume</button></div></div>`}
    ${!needsSignIn && con.enabled && con.oauth?.needs_more_access && html`<div class="card-x wide-card"><b>Google can do more now</b>
      <p>Sign in again to add Docs, Sheets, Slides, Forms, Tasks, Contacts and Gmail filters. Everything you use today keeps working until then.</p>
      <div class="act"><button onClick=${() => go('iris/connections/add/google')}>Add Google services</button></div></div>`}
    ${needsSignIn && con.enabled && html`<div class="card-x wide-card"><b>${con.auth_state === 'needs_reconnect' ? `Sign in to ${labelOf(con)} again` : `Connect ${labelOf(con)}`}</b>
      <p>${con.auth_state === 'needs_reconnect' ? 'The sign-in expired or was revoked.' : `Approve access on ${labelOf(con)}'s own page.`}</p>
      <${ServiceSignIn} con=${con} label=${labelOf(con)} onDone=${() => loadConnectors()} /></div>`}

    ${!needsSignIn && html`<${Group}><${Row} title="Disconnect" danger chevron=${false} onClick=${disconnect} /><//>`}

    ${con.oauth && !needsSignIn && html`<${Group}>
      ${account && html`<${Row} icon=${html`<${LetterTile} name=${account} size=${31} radius=${16} />`} title=${account} chevron=${false} />`}
      <${Row} icon="refresh" title=${account ? 'Use another account' : 'Sign in again'} accent chevron=${false} onClick=${signInSheet} />
    <//>`}

    <${Group} note=${svc === 'youtube' ? `${agentName()} reads your subscriptions and likes to find stories for For you. It never changes anything on YouTube.` : null}>
      <${Row} title=${svc ? `Use ${label}` : 'On'} sub=${serviceOn ? null : `${agentName()} doesn't use ${label} at all`}
        right=${html`<${Toggle} on=${serviceOn} onChange=${toggle} label=${svc ? `Use ${label}` : 'Connection on'} />`} />
    <//>

    ${parts.length > 0 && html`<${Group} label="Services">
      ${parts.map(([p, b]) => html`<${Row} icon=${html`<${ServiceTile} c=${{ service: 'google', part: p }} size=${31} />`} title=${b.name}
        sub=${(con.policy?.off || []).includes(p) ? 'Off' : null} onClick=${() => go(`iris/connections/${id}/${p}`)} />`)}<//>`}

    ${(svc || ready) && html`${!perms ? html`<${Group}><div class="r"><div class="rt mu">${svc ? 'Loading…' : `Starting ${label}…`}</div></div><//>`
      : perms.error ? (google ? null : html`<${Group} label="Permissions"><div class="r"><div class="rt">${cap(perms.error)}${perms.detail && html`<small class="mono">${perms.detail}</small>`}</div></div><//>`)
      : html`${google && otherTools && html`<div class="gl">Other actions</div>`}
        <${PermissionSections} perms=${perms} con=${con} onSaved=${saved} disabled=${!serviceOn} />
        ${!perms.sections.length && !google && html`<${Group}><div class="r"><div class="rt mu">${label} didn't list any actions.</div></div><//>`}
        ${perms.sections.length > 0 && html`<div class="gnote">Allow runs on its own. Ask waits for your OK. Off hides it from ${agentName()}. Purchases and payments always ask.</div>`}`}`}

    ${!svc && html`<${Group} label="Recently">
      ${!recent ? html`<${Spinner} />` : recent.length ? recent.map((it) => callRow(it, false)) : html`<div class="r"><div class="rt mu">Not used yet</div></div>`}
    <//>`}

    ${svc ? html`<${Group}>
        <${Row} icon=${html`<${ServiceTile} c=${con} size=${31} />`} title="Google connection" sub="All services, recent use, and settings" onClick=${() => go(`iris/connections/${id}`)} />
      <//>`
    : html`<${Group} label="Settings">
        <${Row} title="Starting rules" sub=${con.rules} onClick=${() => openSheet(() => html`<${ModeSheet} con=${con} onSaved=${saved} />`)} />
        ${con.oauth && html`<${Row} title="Sign-in app" sub=${con.oauth.client_configured ? (con.oauth.client_source === 'dcr' ? 'Registered by Iris automatically' : 'Your own OAuth client') : 'Not set up yet'}
          onClick=${() => (con.service === 'custom' ? openSheet(() => html`<${ClientSheet} con=${con} />`) : go(`iris/connections/add/${con.service}`))} />`}
        <${Row} title="Advanced settings" sub=${con.transport === 'stdio' ? 'Program, arguments, secrets' : 'Web address, headers'} onClick=${() => go(`iris/connections/${id}/edit`)} />
        <${Row} title="Remove connection" danger onClick=${remove} chevron=${false} />
      <//>`}
  <//>`;
}

/** Replace the OAuth client of a custom connection (for services that
 * don't let apps register themselves, or a client that stopped working). */
function ClientSheet({ con }) {
  const [cid, setCid] = useState('');
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    try {
      await post(`/connectors/${seg(con.id)}/oauth/client`, { client_id: cid.trim(), client_secret: secret.trim() || undefined });
      await loadConnectors();
      closeSheet();
      toast('Saved. Sign in again to use it.');
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  return html`<form class="sheet-body form" onSubmit=${save}><h3>Sign-in app</h3>
    <p class="mu">The Client ID (and secret) of an OAuth app you created on the service. Its callback address must be ${con.oauth?.redirect_uri || 'the one shown when you sign in'}.</p>
    <label class="lbl">Client ID</label><input class="field mono" required value=${cid} autocapitalize="off" spellcheck=${false} onInput=${(e) => setCid(e.target.value)} />
    <label class="lbl">Client secret (if it has one)</label><input class="field mono" type="password" autocomplete="off" value=${secret} onInput=${(e) => setSecret(e.target.value)} />
    <button class="b1 wide" disabled=${busy || !cid.trim()}>Save</button></form>`;
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
    <form class="code-row plain gap" onSubmit=${add}><input class="field" value=${v} placeholder="Email address or phone number" autocapitalize="off" inputmode="email" onInput=${(e) => setV(e.target.value)} />
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
