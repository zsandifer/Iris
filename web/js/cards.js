// Chat cards: agent cards (link, connect, info, result, idea, goal,
// feed_post) and server cards (action, engine_signin, engine_setup,
// connector_reconnect, approval).
import { html, useEffect, useState } from './h.js';
import { api, get, post, put, patch, seg, isId } from './api.js';
import {
  S, set, go, isOwner, toast, loadGoals, loadEngines, loginMark, applyLoginResponse, ensureConnectors,
} from './store.js';
import { Icon, Img, LetterTile, hostOf, safeUrl } from './ui.js';
import { plain } from './md.js';
import { ServiceSignIn, ServiceTile, labelOf } from './connect.js';

const apiPath = (p) => String(p || '').replace(/^\/api(?=\/)/, '');

function Busy({ children }) {
  return html`<div class="status shimmer">${children}</div>`;
}

function OpenLink({ url, label = 'Open', cls = 'pill' }) {
  url = safeUrl(url);
  if (!url) return null;
  return html`<a class=${cls} href=${url} target="_blank" rel="noopener noreferrer">${label} <${Icon} name="external" size=${14} /></a>`;
}

// ------------------------------------------------------------ agent cards

function LinkCard({ c }) {
  return html`<a class="lk" href=${safeUrl(c.url)} target="_blank" rel="noopener noreferrer">
    <${LetterTile} name=${hostOf(c.url) || c.title} size=${36} radius=${11} />
    <div class="grow"><b>${c.title}</b><small>${c.body ? plain(c.body, 90) : hostOf(c.url)}</small></div>
    <span class="go">${c.action_label || 'Open'}</span>
  </a>`;
}

function ResultCard({ c }) {
  return html`<div class="res">
    ${c.image_url ? html`<${Img} cls="res-im" src=${c.image_url} alt=${c.title || 'Photo'} />` : html`<div class="res-im res-blank" />`}
    <div class="res-tx"><b>${c.title}</b>${c.body && html`<small>${plain(c.body, 160)}</small>`}</div>
    ${safeUrl(c.url) && html`<a class="res-bk" href=${safeUrl(c.url)} target="_blank" rel="noopener noreferrer">${c.action_label || 'Open'}</a>`}
  </div>`;
}

const CATALOG = ['google', 'github', 'custom'];

function ConnectCard({ c }) {
  const service = String(c.service || c.title || '');
  const known = CATALOG.includes(service.toLowerCase()) ? service.toLowerCase() : null;
  useEffect(() => { if (known) ensureConnectors(); }, [known]);
  const con = known && known !== 'custom' && (S.connectors || []).find((x) => x.service === known);
  const ready = con && con.enabled && con.auth_state === 'ok';
  const url = !known && safeUrl(c.url);
  const label = c.action_label || 'Connect';
  const open = () => {
    if (con) go(`iris/connections/${con.id}`);
    else if (known) go(known === 'custom' ? 'iris/connections/new' : `iris/connections/add/${known}`);
    else go('iris/connections');
  };
  // A service Iris knows opens its setup here; for others the card's url
  // is the service's own connection page, with Connections one tap away
  // for the owner.
  return html`<div class="lk">
    ${known && known !== 'custom' ? html`<${ServiceTile} c=${{ service: known }} size=${36} />` : html`<${LetterTile} name=${service} size=${36} radius=${11} />`}
    <div class="grow"><b>${c.title}</b><small>${ready ? 'Connected' : c.body ? plain(c.body, 90) : url ? hostOf(url) : 'Connect it so I can help with it'}</small>
      ${url && isOwner() && html`<button class="lk-m" onClick=${open}>Connections ›</button>`}</div>
    ${ready ? html`<span class="ok">${'\u2713'}</span>`
      : url ? html`<a class="go" href=${url} target="_blank" rel="noopener noreferrer">${label}</a>`
      : html`<button class="go" onClick=${open}>${label}</button>`}
  </div>`;
}

function InfoCard({ c }) {
  return html`<div class="info-card"><b>${c.title}</b>${c.body && html`<p>${c.body}</p>`}
    ${c.url && html`<${OpenLink} url=${c.url} label=${c.action_label || 'Open'} cls="link" />`}</div>`;
}

function IdeaCard({ c }) {
  const [busy, setBusy] = useState(false);
  const start = async () => {
    if (!isId(c.ref_id)) return go('ideas');
    setBusy(true);
    try {
      const r = await post(`/ideas/${seg(c.ref_id)}/start`);
      go(`chat/${r.conversation.id}`);
    } catch (e) { toast(e.message); setBusy(false); }
  };
  return html`<div class="card-x"><span class="tag">Idea</span><b>${c.title}</b>
    ${c.body && html`<p>${c.body}</p>`}
    <div class="act"><button disabled=${busy} onClick=${start}>Do it</button>
    <button class="mu" onClick=${() => go('ideas')}>See ideas</button></div></div>`;
}

function GoalCard({ c }) {
  const goal = (S.goals || []).find((g) => g.id === c.ref_id);
  const confirm = async () => {
    try { await patch(`/goals/${seg(c.ref_id)}`, { suggested: false }); await loadGoals(); toast('Added to your goals'); } catch (e) { toast(e.message); }
  };
  return html`<div class="card-x"><span class="tag">${goal?.suggested ? 'Suggested goal' : 'Goal'}</span><b>${c.title}</b>
    ${c.body && html`<p>${c.body}</p>`}
    <div class="act">${goal?.suggested && html`<button onClick=${confirm}>Add goal</button>`}
    <button class=${goal?.suggested ? 'mu' : ''} onClick=${() => go('goals')}>See goals</button></div></div>`;
}

export function FeedPostCard({ c }) {
  return html`<div class="card-x post-card"><span class="tag">From your feed</span><b>${c.title}</b>
    ${c.body && html`<p>${plain(c.body, 220)}</p>`}
    ${c.url && html`<div class="act"><${OpenLink} url=${c.url} label="Visit" cls="link" /></div>`}</div>`;
}

// ----------------------------------------------------------- server cards

function ActionCard({ c, m }) {
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);
  const run = async () => {
    setBusy(true);
    try {
      await api(c.action.method, apiPath(c.action.path), c.action.body ?? undefined);
      setDone(true);
    } catch (e) { toast(e.message); }
    setBusy(false);
  };
  return html`<div class="card-x"><b>${c.title}</b>${c.body && html`<p>${c.body}</p>`}
    ${c.owner_only && !isOwner() ? html`<small class="mu">Only the owner can change this.</small>`
      : done ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> Done</div>`
      : html`<div class="act"><button disabled=${busy} onClick=${run}>${c.action_label || 'OK'}</button></div>`}
  </div>`;
}

/** The engine sign-in flow; used by Reconnect cards and AI engine settings. */
export function SignIn({ card, harness, flow, instructions, keyOptions }) {
  harness = harness || card?.harness;
  // A card is kept current by the server (message.updated); without one
  // (Settings), follow the live sign-in session (harness.login events).
  const st = { ...(card || S.logins[harness] || { state: 'needs_reconnect' }) };
  flow = card?.flow || flow || 'code_paste';
  if (!st.instructions && instructions) st.instructions = instructions;
  const [code, setCode] = useState('');
  const [busy, setBusy] = useState(false);
  const start = async () => {
    setBusy(true);
    try {
      const mark = loginMark(harness);
      const r = await post(`/harnesses/${seg(harness)}/login`);
      if (r.login) applyLoginResponse(harness, r.login, mark);
      else if (r.manual) toast(r.auth === 'ok' ? 'Signed in. Thanks!' : 'Still signed out. Follow the step above, then check again.');
    } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const sendCode = async (e) => {
    e.preventDefault();
    if (!code.trim()) return;
    setBusy(true);
    try {
      const mark = loginMark(harness);
      const r = await post(`/harnesses/${seg(harness)}/login/code`, { code: code.trim() });
      applyLoginResponse(harness, r.login, mark);
      setCode('');
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  const copy = () => navigator.clipboard?.writeText(st.user_code).then(() => toast('Code copied'));

  if (!isOwner()) return html`<small class="mu">Only the owner can reconnect this.</small>`;
  if (st.state === 'connected') return html`<div class="ok-line"><${Icon} name="check" size=${16} /> Connected</div>`;
  if (flow === 'api_key') {
    return html`<${ApiKeyForm} harness=${harness} options=${card?.key_options || keyOptions} />
      ${st.instructions && html`<details class="alt"><summary>Or set it up on the server</summary><div class="steps-box"><code>${st.instructions}</code></div>
        <div class="act"><button disabled=${busy} onClick=${start}>Check again</button></div></details>`}`;
  }
  if (flow === 'manual') {
    return html`${st.instructions && html`<div class="steps-box"><code>${st.instructions}</code></div>`}
      <div class="act"><button disabled=${busy} onClick=${start}>${card?.action_label || 'Check again'}</button></div>`;
  }
  switch (st.state) {
    case 'starting': return html`<${Busy}>Starting the sign-in…<//>`;
    case 'verifying': return html`<${Busy}>Checking with ${label(harness)}…<//>`;
    case 'waiting_for_code':
      return html`<ol class="steps-list">
          <li><span>Open the sign-in page and approve.</span> <${OpenLink} url=${st.url} label="Open sign-in" /></li>
          <li><span>Copy the code it shows you and paste it here.</span>
            <form class="code-row" onSubmit=${sendCode}>
              <input class="field" value=${code} placeholder="Paste code" autocomplete="one-time-code"
                autocapitalize="off" spellcheck=${false} onInput=${(e) => setCode(e.target.value)} />
              <button class="b1 sm" disabled=${busy || !code.trim()}>Connect</button>
            </form></li>
        </ol>${st.error && html`<div class="err-line">${st.error}</div>`}`;
    case 'waiting_for_approval':
      return html`<ol class="steps-list">
          <li><span>Open the sign-in page.</span> <${OpenLink} url=${st.url} label="Open sign-in" /></li>
          <li><span>Enter this code there:</span>
            <button class="big-code" onClick=${copy} aria-label="Copy code">${st.user_code} <${Icon} name="copy" size=${16} /></button></li>
        </ol><${Busy}>Waiting for you to approve…<//>`;
    case 'failed':
    case 'cancelled':
      return html`${st.error && html`<div class="err-line">${st.error}</div>`}
        <div class="act"><button disabled=${busy} onClick=${start}>Try again</button></div>`;
    default:
      return html`<div class="act"><button disabled=${busy} onClick=${start}>${card?.action_label || 'Reconnect'}</button></div>`;
  }
}

/** Add an API key for an engine (Gemini CLI, an OpenCode provider, or a
 * Claude/OpenAI key instead of the subscription). Checked right away with
 * one tiny request through the engine itself. */
export function ApiKeyForm({ harness, options = [], onDone }) {
  const [env, setEnv] = useState(options[0]?.env || '');
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState(null);
  const opt = options.find((o) => o.env === env) || options[0];
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    setMsg(null);
    try {
      const r = await put(`/harnesses/${seg(harness)}/keys/${seg(opt.env)}`, { value: value.trim() });
      setValue('');
      if (r.auth === 'ok') { setMsg(['ok', 'Connected. I can use it now.']); onDone?.(); }
      else if (r.auth === 'auth_required') setMsg(['err', "That key didn't work, so I didn't keep it. Check it and try again."]);
      else setMsg(['ok', "Saved. I'll use it from your next message."]);
      get('/harnesses').then((x) => set({ harnesses: x.harnesses })).catch(() => {});
    } catch (err) { setMsg(['err', err.message]); }
    setBusy(false);
  };
  if (!opt) return null;
  return html`<form class="form key-form" onSubmit=${save}>
    ${options.length > 1 && html`<label class="lbl">Provider</label>
      <select class="field" value=${env} onChange=${(e) => setEnv(e.target.value)}>${options.map((o) => html`<option value=${o.env}>${o.label}</option>`)}</select>`}
    <div class="key-hint"><span>Get a key from ${opt.label}</span> <${OpenLink} url=${opt.url} label="Open" /></div>
    <div class="code-row"><input class="field" type="password" value=${value} placeholder="Paste the key" autocomplete="off"
      autocapitalize="off" spellcheck=${false} onInput=${(e) => setValue(e.target.value)} aria-label="API key" />
      <button class="b1 sm" disabled=${busy || !value.trim()}>${busy ? 'Checking…' : 'Save'}</button></div>
    ${msg && html`<div class=${msg[0] === 'ok' ? 'ok-line' : 'err-line'}>${msg[1]}</div>`}
    <small class="mu">Stored encrypted on your server and passed only to the engine.</small>
  </form>`;
}

function label(h) {
  return { claude: 'Claude', codex: 'ChatGPT', gemini: 'Google', opencode: 'OpenCode' }[h] || 'the engine';
}

function SigninCard({ c }) {
  const done = c.state === 'connected';
  return html`<div class=${'card-x signin' + (done ? ' done' : '')}>
    <div class="card-h"><${LetterTile} name=${c.harness} size=${30} radius=${9} /><b>${c.title}</b></div>
    ${c.body && html`<p>${c.body}</p>`}
    <${SignIn} card=${c} />
  </div>`;
}

function SetupCard({ c }) {
  const engines = S.engines?.engines || [];
  const install = async (o) => {
    try {
      await api(o.action.method, apiPath(o.action.path));
      loadEngines().catch(() => {});
    } catch (e) { toast(e.message); }
  };
  if (isOwner() && !S.engines) loadEngines().catch(() => {});
  return html`<div class="card-x"><b>${c.title}</b>${c.body && html`<p>${c.body}</p>`}
    ${c.state === 'done' ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> All set</div>`
      : html`<div class="opt-list">${(c.options || []).map((o) => {
        const e = engines.find((x) => x.name === o.harness);
        const running = e?.operation?.state === 'running';
        return html`<div class="opt"><${LetterTile} name=${o.harness} size=${28} radius=${8} />
          <span class="grow">${o.label}</span>
          ${o.installed || e?.installed ? html`<small class="mu">Installed</small>`
            : o.requires ? html`<small class="mu">Needs Node.js</small>`
            : running ? html`<small class="shimmer">Installing…</small>`
            : isOwner() ? html`<button class="go" onClick=${() => install(o)}>Install</button>` : null}
        </div>`;
      })}</div>`}
  </div>`;
}

function ConnectorReconnectCard({ c }) {
  useEffect(() => { ensureConnectors(); }, []);
  const con = (S.connectors || []).find((x) => x.id === c.connector_id);
  const done = c.state === 'connected';
  return html`<div class=${'card-x' + (done ? ' done' : '')}>
    <div class="card-h">${con ? html`<${ServiceTile} c=${con} />` : html`<${LetterTile} name=${c.service || c.title} size=${30} radius=${9} />`}<b>${c.title}</b></div>
    ${c.body && html`<p>${c.body}</p>`}
    ${done ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> Connected</div>`
      : !isOwner() ? html`<small class="mu">Only the owner can reconnect this.</small>`
      : c.auth === 'oauth' && con ? html`<${ServiceSignIn} con=${con} label=${labelOf(con)} />`
      : isId(c.connector_id) ? html`<div class="act"><button onClick=${() => go(`iris/connections/${c.connector_id}${c.auth === 'oauth' ? '' : '/edit'}`)}>${c.action_label || 'Reconnect'}</button></div>`
      : null}
  </div>`;
}

function Countdown({ until }) {
  const [, tick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => tick((x) => x + 1), 1000);
    return () => clearInterval(t);
  }, []);
  const s = Math.max(0, Math.round((until - Date.now()) / 1000));
  return html`<span>${s >= 60 ? `${Math.ceil(s / 60)} min` : `${s}s`} left</span>`;
}

const APPROVAL_DONE = {
  approved: 'You approved this',
  denied: "You said no, so I didn't do it.",
  expired: "No answer in time, so I didn't do it.",
  cancelled: 'Cancelled. Nothing was done.',
};

function ApprovalCard({ c, m }) {
  const [busy, setBusy] = useState(false);
  const [raw, setRaw] = useState(false);
  const mine = c.approver_user_id === S.me?.user?.id;
  const decide = async (approve) => {
    setBusy(true);
    try { await post(`/messages/${seg(m.id)}/decision`, { approve }); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const p = c.preview || {};
  const pending = c.state === 'pending';
  return html`<div class=${'ap' + (!pending ? ' decided' : '')}>
    <div class="ap-h"><${Icon} name="lock" size=${14} /> Needs your OK
      ${pending && c.expires_at && html`<span class="ap-left"><${Countdown} until=${c.expires_at} /></span>`}</div>
    ${c.reason && html`<div class="ap-why">${c.reason}</div>`}
    <div class="ap-em"><b>${c.title}</b>
      ${p.to && html`<div><span class="mu">To</span> ${p.to}</div>`}
      ${p.subject && html`<div><span class="mu">Subject</span> ${p.subject}</div>`}
      ${(p.fields || []).map((f) => html`<div><span class="mu">${f.label}</span> ${f.value}</div>`)}
      ${p.body && html`<div class="ap-body">${p.body}</div>`}</div>
    ${c.details && html`<button class="ap-raw-btn" onClick=${() => setRaw(!raw)} aria-expanded=${raw ? 'true' : 'false'}>${raw ? 'Hide' : 'Show'} the exact request</button>
      ${raw && html`<pre class="ap-raw">${c.details}</pre>`}`}
    ${c.state === 'approved' ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> ${APPROVAL_DONE.approved}</div>`
      : !pending ? html`<div class="mu small">${APPROVAL_DONE[c.state] || 'Decided.'}</div>`
      : mine ? html`<div class="ap-bt"><button disabled=${busy} onClick=${() => decide(false)}>Deny</button>
          <button class="y" disabled=${busy} onClick=${() => decide(true)}>${c.action_label || 'Approve'}</button></div>`
      : html`<small class="mu">Waiting for the person this is for.</small>`}
  </div>`;
}

const CARDS = {
  link: LinkCard,
  result: ResultCard,
  connect: ConnectCard,
  info: InfoCard,
  idea: IdeaCard,
  goal: GoalCard,
  feed_post: FeedPostCard,
  action: ActionCard,
  engine_signin: SigninCard,
  engine_setup: SetupCard,
  connector_reconnect: ConnectorReconnectCard,
  approval: ApprovalCard,
};

export function Card({ m, inactive = false }) {
  const c = m.card || {};
  let C = CARDS[c.type] || InfoCard;
  if (c.type === 'link' && c.image_url) C = ResultCard;
  // Sign-in cards can contain inputs. Keep them out of iOS's form
  // navigation while the composer has focus; scrolling stays on .scr.
  const signin = c.type === 'engine_signin' || c.type === 'connector_reconnect';
  return html`<div class="card-wrap" inert=${inactive && signin}><${C} c=${c} m=${m} /></div>`;
}
