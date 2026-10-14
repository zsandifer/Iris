// Routines: things Iris does on a schedule. The list (with a quick request
// box that asks Iris in the Main chat), a summary sheet per routine, the
// full editor (What / When / Rules / History), and the chat card for
// routines Iris proposes.
import { html, useEffect, useRef, useState } from './h.js';
import { api, get, post, patch, del, seg, isId } from './api.js';
import { S, useStore, go, toast, isOwner, mainId, sendMessage, loadRoutines } from './store.js';
import { Icon, Page, Spinner, Empty, Toggle, agentName, openSheet, closeSheet, confirmSheet } from './ui.js';

const DAYS = ['S', 'M', 'T', 'W', 'T', 'F', 'S'];
const DAY_NAMES = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
const MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
const NTH = [[1, 'first'], [2, 'second'], [3, 'third'], [4, 'fourth'], [-1, 'last']];
const WAITS = [[30, '30 minutes'], [60, '1 hour'], [120, '2 hours'], [240, '4 hours'], [480, '8 hours'], [1440, '24 hours']];
const RETRIES = [[0, "Don't retry"], [1, 'Retry once'], [2, 'Retry twice'], [3, 'Retry 3 times']];
const MISFIRES = [['skip', 'Skip it'], ['run_latest_in_window', 'Run it if under an hour late']];

const browserTz = () => { try { return Intl.DateTimeFormat().resolvedOptions().timeZone || ''; } catch { return ''; } };
const today = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
};
const shortDate = (ms) => new Date(ms).toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
const dateTime = (ms) => new Date(ms).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });

/** Plain words for a run's status (never a dot alone). */
export function runStatus(r) {
  if (r.status === 'ok') return ['ok', 'Done'];
  if (r.status === 'skipped') return ['mu', 'Skipped'];
  if (r.status === 'waiting') return ['warn', 'Waiting for your OK'];
  if (r.status === 'running') return ['ac', 'Running now'];
  if (r.status === 'failed' && r.retry_at) return ['warn', 'Trying again soon'];
  return ['bad', "Couldn't finish"];
}

function Ico({ r, size = 36 }) {
  return html`<div class="rtn-ico" style=${`width:${size}px;height:${size}px`}>${r.emoji || html`<${Icon} name="repeat" size=${18} />`}</div>`;
}

// -------------------------------------------------------------------- list

export function RoutinesScreen() {
  useStore();
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  useEffect(() => { loadRoutines().catch((e) => toast(e.message)); }, []);
  const list = S.routines;
  const ask = async (e) => {
    e.preventDefault();
    const t = text.trim();
    if (!t || busy) return;
    setBusy(true);
    try {
      await sendMessage(mainId(), `Set up a routine: ${t}`);
      setText('');
      go(`chat/${mainId()}`);
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  const waiting = (list || []).filter((r) => r.state === 'pending_approval' || r.pending_change || r.open_run?.status === 'waiting');
  const settled = (list || []).filter((r) => r.state !== 'pending_approval');
  const repeating = settled.filter((r) => r.schedule_kind !== 'once');
  const once = settled.filter((r) => r.schedule_kind === 'once');
  return html`<${Page} title="Routines" sub="Things I do for you on a schedule"
      right=${html`<button class="btn" aria-label="New routine" onClick=${() => go('iris/routines/new')}><${Icon} name="plus" /></button>`}>
    <form class="rtn-cmp" onSubmit=${ask}>
      <input value=${text} placeholder="Remind me to… / Every Monday…" aria-label="Ask for a routine"
        onInput=${(e) => setText(e.target.value)} enterkeyhint="send" />
      <button class="go" aria-label="Ask" disabled=${busy || !text.trim()}><${Icon} name="send" size=${18} /></button>
    </form>
    ${!list ? html`<${Spinner} />` : !list.length ? html`<${Empty} icon="repeat" title="No routines yet">
        Tell me something you'd like done regularly, like "order dinner every Saturday" or "remind me about rent on the 1st", and I'll set it up for your OK.<//>`
      : html`
      ${waiting.length > 0 && html`<div class="gl">Waiting for you</div>${waiting.map((r) => html`<${WaitingRow} r=${r} key=${'w' + r.id} />`)}`}
      ${repeating.length > 0 && html`<div class="gl">Repeating</div>${repeating.map((r) => html`<${RoutineRow} r=${r} key=${r.id} />`)}`}
      ${once.length > 0 && html`<div class="gl">Once</div>${once.map((r) => html`<${RoutineRow} r=${r} key=${r.id} />`)}`}`}
    <button class="b2 wide" onClick=${() => go('iris/routines/new')}><${Icon} name="plus" size=${17} /> New routine</button>
  <//>`;
}

async function decideChange(changeId, approve, body) {
  const r = await post(`/routines/changes/${seg(changeId)}/${approve ? 'approve' : 'decline'}`, body);
  loadRoutines().catch(() => {});
  return r;
}

function WaitingRow({ r }) {
  const [busy, setBusy] = useState(false);
  const c = r.pending_change;
  const act = async (yes) => {
    setBusy(true);
    try { await decideChange(c.id, yes); toast(yes ? (c.action === 'delete' ? 'Deleted' : c.action === 'create' ? 'Set up' : 'Updated') : 'Declined'); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  if (c) {
    const [no, yes] = c.action === 'create' ? ['Decline', 'Set it up'] : c.action === 'delete' ? ['Keep it', 'Delete'] : ['Decline', 'Apply'];
    return html`<div class="rtn-row proposed">
      <div class="rtn-line"><${Ico} r=${r} /><div class="grow"><b>${r.title}</b>
        <small>${c.action === 'create' ? r.schedule_text : c.action === 'delete' ? 'I suggested deleting this' : c.summary}</small></div></div>
      <div class="rtn-act">
        ${c.action === 'create' && html`<button class="b2 sm" onClick=${() => go(`iris/routines/${r.id}`)}>Edit</button>`}
        <button class="b2 sm" disabled=${busy} onClick=${() => act(false)}>${no}</button>
        <button class=${'b1 sm' + (c.action === 'delete' ? ' danger' : '')} disabled=${busy} onClick=${() => act(true)}>${yes}</button></div>
    </div>`;
  }
  return html`<div class="rtn-row tap" onClick=${() => go(`chat/${mainId()}`)} role="button" tabindex="0">
    <div class="rtn-line"><${Ico} r=${r} /><div class="grow"><b>${r.title}</b><small>Ready and waiting for your OK in the chat</small></div>
      <${Icon} name="chev" size=${16} cls="rc" /></div></div>`;
}

function Pills({ r }) {
  const last = r.last_run;
  const lastText = last && (last.status === 'ok' ? `Last: ${last.summary || 'done'} ✓`
    : last.status === 'skipped' ? 'Last: skipped' : "Last: couldn't finish");
  return html`<div class="rtn-pills">
    ${r.open_run?.status === 'running' && html`<span class="rpill a">Running now</span>`}
    ${r.state === 'paused' ? html`<span class="rpill w">Paused</span>`
      : r.state === 'done' ? html`<span class="rpill">Finished</span>`
      : r.next_run_text && html`<span class="rpill a">Next: ${r.next_run_text}</span>`}
    ${lastText && html`<span class=${'rpill' + (last.status === 'failed' ? ' b' : '')}>${lastText}</span>`}
  </div>`;
}

function RoutineRow({ r }) {
  const toggle = async (on) => {
    try { await post(`/routines/${seg(r.id)}/${on ? 'resume' : 'pause'}`); await loadRoutines(); } catch (e) { toast(e.message); }
  };
  return html`<div class="rtn-row tap" role="button" tabindex="0" onClick=${(e) => { if (!e.target.closest('button')) summarySheet(r); }}>
    <div class="rtn-line"><${Ico} r=${r} />
      <div class="grow"><b>${r.title}</b><small>${r.schedule_text}${r.ask_before_acting ? ' · asks first' : ''}</small></div>
      ${r.state !== 'done' && html`<${Toggle} on=${r.state === 'active'} label=${r.state === 'active' ? 'Pause' : 'Resume'} onChange=${toggle} />`}
    </div>
    <${Pills} r=${r} />
  </div>`;
}

function endsText(r) {
  if (r.end_at) return `On ${r.end_at}`;
  if (r.max_runs) return `After ${r.max_runs} run${r.max_runs === 1 ? '' : 's'}`;
  return r.schedule_kind === 'once' ? 'After it runs' : 'Never';
}

function SummarySheet({ id }) {
  useStore();
  const [data, setData] = useState(null);
  const reload = () => get(`/routines/${seg(id)}`).then((x) => setData(x.routine)).catch((e) => toast(e.message));
  useEffect(() => { reload(); }, [id, S.routines]);
  const r = data || (S.routines || []).find((x) => x.id === id);
  if (!r) return html`<div class="sheet-body"><${Spinner} /></div>`;
  const runNow = async () => {
    try { await post(`/routines/${seg(id)}/run`); toast("On it. I'll post the result in the chat."); reload(); } catch (e) { toast(e.message); }
  };
  const pause = async () => {
    try { await post(`/routines/${seg(id)}/${r.state === 'paused' ? 'resume' : 'pause'}`); loadRoutines(); reload(); } catch (e) { toast(e.message); }
  };
  return html`<div class="sheet-body tall">
    <div class="rtn-line"><${Ico} r=${r} /><h3 class="grow">${r.title}</h3></div>
    <p class="mu rtn-instr">${r.instructions}</p>
    <div class="rtn-fld"><span>Repeats</span><i>${r.schedule_text}</i></div>
    <div class="rtn-fld"><span>Time zone</span><i>${r.tz}</i></div>
    <div class="rtn-fld"><span>Before acting</span><i>${r.ask_before_acting ? 'Ask me' : "Don't ask"}</i></div>
    <div class="rtn-fld"><span>Ends</span><i>${endsText(r)}</i></div>
    ${r.runs?.length > 0 && html`<div class="gl">History</div>${r.runs.slice(0, 3).map((x) => html`<${HistoryItem} run=${x} key=${x.id} />`)}`}
    <div class="btns">
      <button class="b2" onClick=${() => { closeSheet(); go(`iris/routines/${id}`); }}>Edit</button>
      <button class="b2" onClick=${runNow} disabled=${!!r.open_run}>Run now</button>
      ${r.state !== 'done' && html`<button class="b2" onClick=${pause}>${r.state === 'paused' ? 'Resume' : 'Pause'}</button>`}
    </div>
  </div>`;
}

function summarySheet(r) {
  openSheet(() => html`<${SummarySheet} id=${r.id} />`);
}

function HistoryItem({ run }) {
  const [cls, label] = runStatus(run);
  const detail = run.status === 'ok' ? run.summary : run.reason || run.summary;
  return html`<div class="rtn-hist">
    <div class="rtn-hist-h"><span>${shortDate(run.scheduled_for)}${run.manual ? ' · run by you' : ''}</span>
      <span class=${'rtn-st ' + cls}>${label}</span></div>
    ${detail && html`<small class="mu">${detail}</small>`}
    ${run.approved_at && html`<small class="mu">You approved at ${dateTime(run.approved_at)}</small>`}
  </div>`;
}

// ------------------------------------------------------------------ editor

function blankDraft(tz) {
  return {
    title: '', emoji: '', instructions: '', schedule_kind: 'rule', run_at: null,
    rule: { freq: 'weekly', days: [new Date().getDay()], every: 1, time: '09:00' },
    cron_expr: null, tz: tz || browserTz() || 'UTC', starts_at: null, end_at: null, max_runs: null,
    ask_before_acting: true, notify_when_done: true, approval_timeout_mins: 120,
    misfire: 'run_latest_in_window', max_retries: 2,
  };
}

const SPEC_KEYS = ['title', 'emoji', 'instructions', 'schedule_kind', 'run_at', 'rule', 'cron_expr', 'tz', 'starts_at',
  'end_at', 'max_runs', 'ask_before_acting', 'notify_when_done', 'approval_timeout_mins', 'misfire', 'max_retries'];
const pick = (r) => Object.fromEntries(SPEC_KEYS.map((k) => [k, r[k] ?? null]));

export function RoutineEditor({ id }) {
  useStore();
  const fresh = id === 'new';
  const [routine, setRoutine] = useState(null);
  const [draft, setDraft] = useState(null);
  const [docs, setDocs] = useState([]);
  const [tab, setTab] = useState('what');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(null);
  const load = async () => {
    if (fresh) {
      let tz = browserTz();
      try { const l = await get('/routines'); tz = tz || l.default_tz; } catch { /* default */ }
      setDraft(blankDraft(tz));
      return;
    }
    try {
      const { routine: r } = await get(`/routines/${seg(id)}`);
      setRoutine(r);
      setDraft((d) => d || pick(r));
      setDocs(r.docs || []);
    } catch (e) { setErr(e.message); }
  };
  useEffect(() => { load(); }, [id]);
  if (err) return html`<${Page} title="Routine"><${Empty} icon="repeat" title="I couldn't open this routine">${err}<//><//>`;
  if (!draft) return html`<${Page}><${Spinner} /><//>`;
  const up = (patchObj) => setDraft((d) => ({ ...d, ...patchObj }));
  const pending = routine?.state === 'pending_approval' && routine.pending_change?.action === 'create';
  // Existing routines get document changes right away; new or proposed ones on save.
  const liveDocs = routine && !pending;
  const save = async () => {
    setBusy(true);
    const body = { ...draft, doc_ids: docs.map((d) => d.id) };
    try {
      if (fresh) {
        const r = await post('/routines', body);
        toast('Routine saved');
        await loadRoutines();
        go(`iris/routines/${r.routine.id}`, true);
      } else if (pending) {
        await decideChange(routine.pending_change.id, true, { routine: body });
        toast('Set up');
        load();
        setDraft(null);
        setTimeout(load, 0);
      } else {
        const r = await patch(`/routines/${seg(id)}`, body);
        setRoutine(r.routine);
        setDraft(pick(r.routine));
        toast('Saved');
        loadRoutines().catch(() => {});
      }
    } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const tabs = [['what', 'What'], ['when', 'When'], ['rules', 'Rules'], ...(fresh ? [] : [['history', 'History']])];
  return html`<${Page} onBack=${() => go('iris/routines')}
      right=${html`<button class="b1 sm" disabled=${busy} onClick=${save}>${pending ? 'Set it up' : 'Save'}</button>`}>
    <div class="rtn-line rtn-head"><${Ico} r=${draft} size=${40} /><h1 class="ttl grow">${draft.title || (fresh ? 'New routine' : 'Routine')}</h1></div>
    ${pending && html`<div class="rtn-note">I suggested this routine. Check it over, then tap Set it up.</div>`}
    <div class="seg rtn-tabs" role="tablist">${tabs.map(([k, label]) => html`<button role="tab" aria-selected=${tab === k ? 'true' : 'false'}
      class=${tab === k ? 'on' : ''} onClick=${() => setTab(k)}>${label}</button>`)}</div>
    ${tab === 'what' && html`<${WhatTab} draft=${draft} up=${up} docs=${docs} setDocs=${setDocs} routineId=${liveDocs ? id : null} />`}
    ${tab === 'when' && html`<${WhenTab} draft=${draft} up=${up} />`}
    ${tab === 'rules' && html`<${RulesTab} draft=${draft} up=${up} routine=${routine} />`}
    ${tab === 'history' && routine && html`<${HistoryTab} routine=${routine} reload=${load} />`}
  <//>`;
}

function WhatTab({ draft, up, docs, setDocs, routineId }) {
  const file = useRef();
  const [uploading, setUploading] = useState(false);
  const upload = async (e) => {
    const f = e.target.files?.[0];
    e.target.value = '';
    if (!f) return;
    setUploading(true);
    try {
      const { doc } = await api('POST', `/routines/docs?name=${encodeURIComponent(f.name)}`, f);
      const next = [...docs, doc];
      if (routineId) await patch(`/routines/${seg(routineId)}`, { doc_ids: next.map((d) => d.id) });
      setDocs(next);
    } catch (err) { toast(err.message); }
    setUploading(false);
  };
  const remove = async (d) => {
    try {
      if (routineId) await del(`/routines/${seg(routineId)}/docs/${seg(d.id)}`);
      setDocs(docs.filter((x) => x.id !== d.id));
    } catch (err) { toast(err.message); }
  };
  return html`<div class="form">
    <div class="rtn-name"><div><label class="lbl" for="rt-emoji">Icon</label>
      <input id="rt-emoji" class="field rtn-emoji" value=${draft.emoji} maxlength="8" placeholder="🔁" onInput=${(e) => up({ emoji: e.target.value })} /></div>
      <div class="grow"><label class="lbl" for="rt-title">Name</label>
      <input id="rt-title" class="field" value=${draft.title} placeholder="Saturday dinner order" onInput=${(e) => up({ title: e.target.value })} /></div></div>
    <label class="lbl" for="rt-instr">Instructions</label>
    <textarea id="rt-instr" class="field rtn-text" rows="6" value=${draft.instructions}
      placeholder="What should I do each time? For example: find a Thai place that's open, build my usual order, and check with me before ordering."
      onInput=${(e) => up({ instructions: e.target.value })} />
    <div class="gl">Reference documents</div>
    ${docs.length > 0 && html`<div class="grp">${docs.map((d) => html`<div class="r" key=${d.id}>
      <div class="ri"><${Icon} name="file" size=${17} /></div>
      <div class="rt">${d.title}<small>${d.chars < 1000 ? `${d.chars} characters` : `${Math.round(d.chars / 1000)}k characters`} · in your Library · read every run</small></div>
      <button class="icon-btn" aria-label=${`Remove ${d.title}`} onClick=${() => remove(d)}><${Icon} name="x" size=${16} /></button></div>`)}</div>`}
    <input ref=${file} type="file" hidden accept=".md,.markdown,.txt,text/markdown,text/plain" onChange=${upload} />
    <button class="b2 wide" disabled=${uploading} onClick=${() => file.current?.click()}><${Icon} name="upload" size=${17} /> ${uploading ? 'Uploading…' : 'Upload'}</button>
    <div class="gnote">Markdown or text files, up to ${Math.round((S.routinesInfo?.max_doc_bytes || 10485760) / 1048576)} MB. Saved to your Library. I read them on every run.</div>
  </div>`;
}

function usePreview(draft) {
  const [out, setOut] = useState(null);
  const key = JSON.stringify([draft.schedule_kind, draft.run_at, draft.rule, draft.cron_expr, draft.tz, draft.starts_at, draft.end_at, draft.max_runs]);
  useEffect(() => {
    let live = true;
    const t = setTimeout(async () => {
      try {
        const r = await post('/routines/preview', { ...draft, count: 3 });
        if (live) setOut({ ok: r });
      } catch (e) { if (live) setOut({ err: e.message }); }
    }, 300);
    return () => { live = false; clearTimeout(t); };
  }, [key]);
  return out;
}

function Seg({ value, options, onChange, label }) {
  return html`<div class="seg seg-gap" role="radiogroup" aria-label=${label}>${options.map(([v, l]) => html`<button type="button" role="radio"
    aria-checked=${value === v ? 'true' : 'false'} class=${value === v ? 'on' : ''} onClick=${() => onChange(v)}>${l}</button>`)}</div>`;
}

function Fld({ label, children }) {
  return html`<label class="rtn-fld"><span>${label}</span>${children}</label>`;
}

function WhenTab({ draft, up }) {
  const preview = usePreview(draft);
  const rule = draft.rule || blankDraft().rule;
  const setRule = (p) => up({ rule: { ...rule, ...p } });
  const kind = draft.schedule_kind;
  const setKind = (k) => {
    if (k === 'once' && !draft.run_at) up({ schedule_kind: k, run_at: `${today()}T${rule.time || '09:00'}` });
    else if (k === 'cron' && !draft.cron_expr) {
      const [h, m] = (rule.time || '09:00').split(':').map(Number);
      up({ schedule_kind: k, cron_expr: `${m} ${h} * * *` });
    } else up({ schedule_kind: k, rule: draft.rule || rule });
  };
  const setFreq = (freq) => {
    const d = new Date();
    const base = { freq, every: 1, time: rule.time || '09:00', anchor: rule.anchor };
    if (freq === 'weekly') base.days = rule.days?.length ? rule.days : [d.getDay()];
    if (freq === 'monthly') base.day = d.getDate();
    if (freq === 'yearly') { base.month = d.getMonth() + 1; base.day = d.getDate(); }
    up({ rule: base });
  };
  const unit = { daily: 'day', weekly: 'week', monthly: 'month', yearly: 'year' }[rule.freq];
  const [date, time] = (draft.run_at || `${today()}T09:00`).split('T');
  const zones = typeof Intl.supportedValuesOf === 'function' ? Intl.supportedValuesOf('timeZone') : [];
  const ends = draft.end_at ? 'date' : draft.max_runs ? 'count' : 'never';
  return html`<div class="form">
    <${Seg} label="Kind of schedule" value=${kind} onChange=${setKind} options=${[['rule', 'Repeats'], ['once', 'Once'], ['cron', 'Custom']]} />
    ${kind === 'rule' && html`
      <${Seg} label="How often" value=${rule.freq} onChange=${setFreq} options=${[['daily', 'Day'], ['weekly', 'Week'], ['monthly', 'Month'], ['yearly', 'Year']]} />
      <div class="rtn-every">Every <input class="field rtn-num" type="number" min="1" max="365" value=${rule.every || 1}
        aria-label="Interval" onInput=${(e) => setRule({ every: Math.max(1, parseInt(e.target.value, 10) || 1) })} /> ${unit}${(rule.every || 1) > 1 ? 's' : ''}${rule.freq === 'weekly' ? ' on' : ''}</div>
      ${rule.freq === 'weekly' && html`<div class="rtn-days" role="group" aria-label="Days of the week">${DAYS.map((l, i) => {
        const on = (rule.days || []).includes(i);
        return html`<button type="button" class=${on ? 'on' : ''} aria-pressed=${on ? 'true' : 'false'} aria-label=${DAY_NAMES[i]}
          onClick=${() => setRule({ days: on ? rule.days.filter((x) => x !== i) : [...(rule.days || []), i].sort() })}>${l}</button>`;
      })}</div>`}
      ${rule.freq === 'monthly' && html`
        <${Seg} label="Monthly on" value=${rule.nth ? 'nth' : 'day'} options=${[['day', 'On a day'], ['nth', 'On a weekday']]}
          onChange=${(v) => setRule(v === 'nth' ? { day: undefined, nth: 1, weekday: new Date().getDay() } : { nth: undefined, weekday: undefined, day: new Date().getDate() })} />
        ${rule.nth ? html`<div class="two">
            <select class="field" aria-label="Which" value=${rule.nth} onChange=${(e) => setRule({ nth: Number(e.target.value) })}>${NTH.map(([v, l]) => html`<option value=${v}>The ${l}</option>`)}</select>
            <select class="field" aria-label="Weekday" value=${rule.weekday} onChange=${(e) => setRule({ weekday: Number(e.target.value) })}>${DAY_NAMES.map((l, i) => html`<option value=${i}>${l}</option>`)}</select></div>`
          : html`<select class="field" aria-label="Day of the month" value=${rule.day} onChange=${(e) => setRule({ day: Number(e.target.value) })}>
              ${Array.from({ length: 31 }, (_, i) => html`<option value=${i + 1}>Day ${i + 1}${i >= 28 ? ' (or the last day)' : ''}</option>`)}</select>`}`}
      ${rule.freq === 'yearly' && html`<div class="two">
        <select class="field" aria-label="Month" value=${rule.month} onChange=${(e) => setRule({ month: Number(e.target.value) })}>${MONTHS.map((l, i) => html`<option value=${i + 1}>${l}</option>`)}</select>
        <select class="field" aria-label="Day" value=${rule.day} onChange=${(e) => setRule({ day: Number(e.target.value) })}>${Array.from({ length: 31 }, (_, i) => html`<option value=${i + 1}>${i + 1}</option>`)}</select></div>`}
      <${Fld} label="Time"><input class="field rtn-in" type="time" value=${rule.time} onInput=${(e) => setRule({ time: e.target.value })} /><//>`}
    ${kind === 'once' && html`
      <${Fld} label="Date"><input class="field rtn-in" type="date" value=${date} onInput=${(e) => up({ run_at: `${e.target.value}T${time}` })} /><//>
      <${Fld} label="Time"><input class="field rtn-in" type="time" value=${time} onInput=${(e) => up({ run_at: `${date}T${e.target.value}` })} /><//>`}
    ${kind === 'cron' && html`
      <input class="field mono" value=${draft.cron_expr || ''} aria-label="Cron pattern" placeholder="30 17 * * 6" autocapitalize="off" spellcheck=${false}
        onInput=${(e) => up({ cron_expr: e.target.value })} />
      <div class="gnote">Minute, hour, day of month, month, day of week.${preview?.ok?.cron_text ? html` <b>“${preview.ok.cron_text}”</b>` : ''}</div>`}
    <${Fld} label="Time zone"><input class="field rtn-in" list="rtn-zones" value=${draft.tz} onChange=${(e) => up({ tz: e.target.value.trim() })} />
      <datalist id="rtn-zones">${zones.map((z) => html`<option value=${z} />`)}</datalist><//>
    ${kind !== 'once' && html`
      <${Fld} label="Starts"><input class="field rtn-in" type="date" value=${draft.starts_at || ''} onInput=${(e) => up({ starts_at: e.target.value || null })} /><//>
      <div class="lbl">Ends</div>
      <${Seg} label="Ends" value=${ends} options=${[['never', 'Never'], ['date', 'On a date'], ['count', 'After']]}
        onChange=${(v) => up(v === 'never' ? { end_at: null, max_runs: null } : v === 'date' ? { end_at: today(), max_runs: null } : { end_at: null, max_runs: 5 })} />
      ${ends === 'date' && html`<${Fld} label="Last day"><input class="field rtn-in" type="date" value=${draft.end_at} onInput=${(e) => up({ end_at: e.target.value || null })} /><//>`}
      ${ends === 'count' && html`<div class="rtn-every">After <input class="field rtn-num" type="number" min="1" max="1000" value=${draft.max_runs}
        aria-label="Number of runs" onInput=${(e) => up({ max_runs: Math.max(1, parseInt(e.target.value, 10) || 1) })} /> runs</div>`}`}
    <div class="card rtn-next"><small class="mu">${kind === 'once' ? 'Runs' : 'Next 3 runs'}</small>
      ${!preview ? html`<div class="mu">…</div>` : preview.err ? html`<div class="err-line">${preview.err}</div>`
        : preview.ok.runs.length ? html`<div>${preview.ok.runs.map((r) => r.text).join(' · ')}</div>`
        : html`<div class="mu">No upcoming runs with these settings.</div>`}</div>
  </div>`;
}

function Select({ value, options, onChange, label }) {
  return html`<select class="field rtn-sel" aria-label=${label} value=${value} onChange=${(e) => onChange(e.target.value)}>
    ${options.map(([v, l]) => html`<option value=${v}>${l}</option>`)}</select>`;
}

function RulesTab({ draft, up, routine }) {
  const created = routine && `Created by ${routine.created_by === 'iris' ? agentName() : 'you'} on ${new Date(routine.created_at).toLocaleDateString(undefined, { month: 'short', day: 'numeric' })}${routine.created_by === 'iris' && routine.state !== 'pending_approval' ? ' · you approved it' : ''}`;
  return html`<div class="rtn-rules">
    <div class="rtn-fld"><span>Ask before acting<small>I prepare everything, then check with you before buying, ordering, or sending.</small></span>
      <${Toggle} on=${draft.ask_before_acting} label="Ask before acting" onChange=${(v) => up({ ask_before_acting: v })} /></div>
    ${draft.ask_before_acting && html`<div class="rtn-fld"><span>Wait for my answer</span>
      <${Select} label="Wait for my answer" value=${draft.approval_timeout_mins} onChange=${(v) => up({ approval_timeout_mins: Number(v) })}
        options=${WAITS.some(([m]) => m === draft.approval_timeout_mins) ? WAITS : [...WAITS, [draft.approval_timeout_mins, `${draft.approval_timeout_mins} minutes`]]} /></div>`}
    <div class="rtn-fld"><span>Tell me when done<small>A notification with the result. It's always in your Main chat.</small></span>
      <${Toggle} on=${draft.notify_when_done} label="Tell me when done" onChange=${(v) => up({ notify_when_done: v })} /></div>
    <div class="rtn-fld"><span>Results go to</span><i>Main chat</i></div>
    <div class="rtn-fld"><span>If I was offline</span>
      <${Select} label="If I was offline" value=${draft.misfire} onChange=${(v) => up({ misfire: v })} options=${MISFIRES} /></div>
    <div class="rtn-fld"><span>If it fails</span>
      <${Select} label="If it fails" value=${draft.max_retries} onChange=${(v) => up({ max_retries: Number(v) })} options=${RETRIES} /></div>
    <div class="gnote">For routines that buy or send things, keep "Ask before acting" on and skip runs I missed while offline.</div>
    ${created && html`<div class="gnote">${created}</div>`}
  </div>`;
}

function HistoryTab({ routine, reload }) {
  const runNow = async () => {
    try { await post(`/routines/${seg(routine.id)}/run`); toast("On it. I'll post the result in the chat."); reload(); } catch (e) { toast(e.message); }
  };
  const pause = async () => {
    try { await post(`/routines/${seg(routine.id)}/${routine.state === 'paused' ? 'resume' : 'pause'}`); reload(); loadRoutines().catch(() => {}); } catch (e) { toast(e.message); }
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Delete “${routine.title}”?`, body: 'It stops for good and its history is removed. Its documents stay in your Library.', yes: 'Delete', danger: true }))) return;
    try { await del(`/routines/${seg(routine.id)}`); await loadRoutines(); go('iris/routines', true); } catch (e) { toast(e.message); }
  };
  const runs = routine.runs || [];
  return html`<div>
    ${runs.length ? html`<div class="grp rtn-hist-list">${runs.map((x) => html`<${HistoryItem} run=${x} key=${x.id} />`)}</div>`
      : html`<p class="mu">No runs yet.${routine.next_run_text ? ` The first one is ${routine.next_run_text}.` : ''}</p>`}
    <div class="btns">
      ${routine.state !== 'pending_approval' && html`<button class="b2" disabled=${!!routine.open_run} onClick=${runNow}>Run now</button>`}
      ${['active', 'paused'].includes(routine.state) && html`<button class="b2" onClick=${pause}>${routine.state === 'paused' ? 'Resume' : 'Pause'}</button>`}
      <button class="b2 danger-text" onClick=${remove}>Delete</button>
    </div>
  </div>`;
}

// ------------------------------------------------------------- chat card

const DONE = { create: 'Set up. It runs on schedule now.', update: 'Updated.', delete: 'Deleted.' };

/** A routine Iris proposed (or a change to one): nothing happens until the
 * owner taps. */
export function RoutineCard({ c }) {
  const [busy, setBusy] = useState(false);
  const mine = c.approver_user_id === S.me?.user?.id;
  const decide = async (yes) => {
    setBusy(true);
    try { await decideChange(c.change_id, yes); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const tag = c.action === 'create' ? 'New routine' : c.action === 'delete' ? 'Delete a routine' : 'Change a routine';
  const [no, yes] = c.action === 'create' ? ['Edit', 'Set it up'] : c.action === 'delete' ? ['Keep it', 'Delete'] : ['Decline', 'Apply'];
  return html`<div class=${'card-x rtn-card' + (c.state === 'pending' ? '' : ' done')}>
    <span class="tag">${tag}</span>
    <div class="rtn-line"><div class="rtn-ico">${c.emoji || html`<${Icon} name="repeat" size=${18} />`}</div>
      <div class="grow"><b>${c.title}</b><small class="mu">${c.action === 'update' ? c.body : c.schedule}</small></div></div>
    ${c.action !== 'delete' && html`<div class="rtn-pills">${c.asks_first && html`<span class="rpill a">Asks before acting</span>`}
      ${c.next && c.action === 'create' && html`<span class="rpill">Next: ${c.next}</span>`}</div>`}
    ${c.action === 'delete' && html`<p>${c.body}</p>`}
    ${c.state === 'approved' ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> ${DONE[c.action]}</div>`
      : c.state === 'superseded' ? html`<div class="mu small">Replaced by a newer suggestion.</div>`
      : c.state === 'cancelled' ? html`<div class="mu small">No longer needed.</div>`
      : c.state !== 'pending' ? html`<div class="mu small">Decided.</div>`
      : !mine || !isOwner() ? html`<small class="mu">Waiting for the owner.</small>`
      : html`<div class="bt2">
          <button class="b2 sm" disabled=${busy} onClick=${() => (c.action === 'create' && isId(c.routine_id) ? go(`iris/routines/${c.routine_id}`) : decide(false))}>${no}</button>
          <button class=${'b1 sm' + (c.action === 'delete' ? ' danger' : '')} disabled=${busy} onClick=${() => decide(true)}>${yes}</button></div>`}
    ${c.state === 'pending' && c.action === 'create' && mine && html`<button class="lk-m" disabled=${busy} onClick=${() => decide(false)}>No thanks</button>`}
  </div>`;
}

export function routinesSub() {
  const list = S.routines;
  if (!list) return ' ';
  const on = list.filter((r) => r.state === 'active').length;
  const waiting = list.filter((r) => r.state === 'pending_approval' || r.pending_change || r.open_run?.status === 'waiting').length;
  if (waiting) return `${waiting} waiting for you`;
  return on ? `${on} active` : 'Nothing scheduled yet';
}

