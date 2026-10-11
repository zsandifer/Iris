// Chat: messages, live status phrases, receipts, cards, and the composer.
import { html, useEffect, useLayoutEffect, useRef, useState } from './h.js';
import { post, api, seg, isId } from './api.js';
import { S, set, useStore, loadMessages, loadOlder, sendMessage, toast, mainId, convFor, lookUpConv } from './store.js';
import { Icon, Img, Avatar, agentName, stamp, Spinner } from './ui.js';
import { md } from './md.js';
import { Card } from './cards.js';
import { statusPhrase, receipt, seconds } from './status.js';
import { pushState, enablePush } from './push.js';

const GAP = 30 * 60 * 1000;

export function StatusLine({ live }) {
  return html`<div class="status shimmer" aria-live="polite">${statusPhrase(live)}</div>`;
}

function Receipt({ meta }) {
  const [open, setOpen] = useState(false);
  const r = receipt(meta);
  if (!r) return null;
  const [summary, steps] = r;
  const dur = seconds(meta.duration_ms);
  return html`<div class="rcpt">
    <button class="rcpt-l" onClick=${() => setOpen(!open)} aria-expanded=${open ? 'true' : 'false'}>
      <${Icon} name="check" size=${14} /> ${summary}${dur && html` · ${dur}`} <span class=${'rc-chev' + (open ? ' open' : '')}>›</span>
    </button>
    ${open && html`<div class="rcpt-steps">${steps.map((s) => html`<div>${s}</div>`)}</div>`}
  </div>`;
}

function Photos({ ids, mine }) {
  return html`<div class=${'photos' + (mine ? ' mine' : '')}>
    ${ids.map((id) => html`<button class="photo" onClick=${() => set({ lightbox: `/api/attachments/${seg(id)}` })}>
      <${Img} src=${`/api/attachments/${seg(id)}`} alt="Photo" /></button>`)}
  </div>`;
}

function retryFor(list, m) {
  const i = list.findIndex((x) => x.id === m.id);
  for (let j = i - 1; j >= 0; j--) {
    const u = list[j];
    if (u.role === 'user') return u;
  }
  return null;
}

function Assistant({ m, list }) {
  const live = S.live[m.id];
  if (m.status === 'streaming') {
    const text = live?.text || '';
    const running = (live?.acts || []).some((a) => a.status === 'started');
    if (!text) return html`<${StatusLine} live=${live} />`;
    return html`<div class="b" dangerouslySetInnerHTML=${{ __html: md(text) }} />
      ${running && html`<${StatusLine} live=${live} />`}`;
  }
  const retry = async () => {
    const u = retryFor(list, m);
    if (!u) return;
    try { await sendMessage(m.conversation_id, u.content, u.attachments); } catch (e) { toast(e.message); }
  };
  const failed = m.status === 'error' || m.status === 'interrupted';
  let content = m.content;
  if (m.status === 'interrupted' && !content) content = 'I got interrupted before I could finish.';
  return html`<${Receipt} meta=${m.meta} />
    ${content ? html`<div class=${'b' + (failed ? ' b-err' : '')} dangerouslySetInnerHTML=${{ __html: md(content) }} />`
      : m.status === 'cancelled' ? html`<div class="note">Stopped.</div>` : null}
    ${failed && retryFor(list, m) && html`<button class="retry" onClick=${retry}><${Icon} name="refresh" size=${15} /> Try again</button>`}`;
}

function Message({ m, list, members }) {
  if (m.kind === 'card') return html`<${Card} m=${m} />`;
  if (m.role === 'user') {
    const mine = m.author_user_id === S.me?.user?.id;
    const who = !mine && members?.[m.author_user_id];
    return html`<div class=${'umsg' + (mine ? ' mine' : ' other')}>
      ${who && html`<div class="who">${who}</div>`}
      ${m.attachments?.length > 0 && html`<${Photos} ids=${m.attachments} mine=${mine} />`}
      ${m.content && html`<div class=${'b' + (mine ? ' u' : ' o')}>${m.content}</div>`}
    </div>`;
  }
  if (m.role === 'assistant') return html`<${Assistant} m=${m} list=${list} />`;
  return html`<div class="note">${m.content}</div>`;
}

function NotifyPrompt() {
  const [st, setSt] = useState(null);
  useEffect(() => { pushState().then(setSt); }, []);
  if (st !== 'default' || localStorage.getItem('iris.notify.dismissed')) return null;
  const dismiss = () => { localStorage.setItem('iris.notify.dismissed', '1'); setSt('dismissed'); };
  const on = async () => {
    try { await enablePush(); toast('Notifications are on'); } catch (e) { toast(e.message); }
    dismiss();
  };
  return html`<div class="card-x notify-card">
    <div class="card-h"><${Icon} name="bell" size=${18} /><b>Want a ping when I reply?</b></div>
    <p>I'll notify you when I finish something, unless this chat is open.</p>
    <div class="act"><button onClick=${on}>Turn on</button><button class="mu" onClick=${dismiss}>Not now</button></div>
  </div>`;
}

function Welcome() {
  return html`<div class="welcome">
    <${Avatar} size=${72} />
    <h2 class="serif">Hi, I'm ${agentName()}.</h2>
    <p class="mu">Ask me anything, send a photo, or tell me what you'd like a hand with.
    I'll show you what I'm doing while I work.</p>
  </div>`;
}

export function ChatScreen({ cid }) {
  useStore();
  const scr = useRef();
  const stick = useRef(true);
  const prevHeight = useRef(0);
  const conv = convFor(cid);
  const box = S.msgs[cid];
  const list = box?.list || [];
  const [members, setMembers] = useState(null);

  useEffect(() => {
    stick.current = true;
    if (!isId(cid)) return;
    if (!S.msgs[cid]) loadMessages(cid);
    post(`/conversations/${seg(cid)}/read`).catch(() => {});
  }, [cid]);

  // Opened by URL but not in the list: it may be archived (for the menu).
  const known = !!conv;
  useEffect(() => { if (isId(cid) && !known) lookUpConv(cid); }, [cid, known, !!S.convs]);

  useEffect(() => {
    if (conv && conv.member_count > 1) {
      api('GET', `/conversations/${seg(cid)}/members`).then((r) => {
        setMembers(Object.fromEntries(r.members.map((x) => [x.user_id, x.display_name])));
      }).catch(() => {});
    } else setMembers(null);
  }, [cid, conv?.member_count]);

  // Keep the newest message in view unless the user scrolled up.
  useLayoutEffect(() => {
    const el = scr.current;
    if (!el) return;
    if (prevHeight.current && el.scrollTop < 80 && el.scrollHeight > prevHeight.current && !stick.current) {
      el.scrollTop += el.scrollHeight - prevHeight.current;
    } else if (stick.current) {
      el.scrollTop = el.scrollHeight;
    }
    prevHeight.current = el.scrollHeight;
  });

  const lastId = list.at(-1)?.id;
  useEffect(() => {
    if (lastId && isId(cid) && document.visibilityState === 'visible') post(`/conversations/${seg(cid)}/read`).catch(() => {});
  }, [lastId]);

  const onScroll = (e) => {
    const el = e.currentTarget;
    stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 90;
    if (el.scrollTop < 60 && box?.hasMore && !box.loading) {
      prevHeight.current = el.scrollHeight;
      loadOlder(cid).catch(() => {});
    }
  };

  const running = list.some((m) => m.status === 'streaming');
  let prev = 0;
  const rows = [];
  for (const m of list) {
    if (!prev || m.created_at - prev > GAP) rows.push(html`<div class="ts" key=${'t' + m.id}>${stamp(m.created_at)}</div>`);
    prev = m.created_at;
    rows.push(html`<div class="row" key=${m.id}><${Message} m=${m} list=${list} members=${members} /></div>`);
  }
  const isMain = cid === mainId();
  return html`<div class="chat">
    <div class="scr" ref=${scr} onScroll=${onScroll}>
      ${box?.loading && box?.hasMore && html`<${Spinner} />`}
      ${!box ? html`<${Spinner} />` : !list.length && isMain ? html`<${Welcome} />` : null}
      ${isMain && box && !box.hasMore && html`<${NotifyPrompt} />`}
      ${rows}
    </div>
    <${Composer} cid=${cid} running=${running} />
  </div>`;
}

// ---------------------------------------------------------------- composer

const Speech = window.SpeechRecognition || window.webkitSpeechRecognition;
const touch = matchMedia('(pointer: coarse)').matches;

/** Downscale big photos before upload (keeps uploads fast on cellular). */
async function shrink(file) {
  if (!/^image\/(jpeg|png|webp|heic|heif)$/.test(file.type) || file.size < 1.5e6) return file;
  try {
    const bmp = await createImageBitmap(file);
    const scale = Math.min(1, 2048 / Math.max(bmp.width, bmp.height));
    const c = document.createElement('canvas');
    c.width = Math.round(bmp.width * scale);
    c.height = Math.round(bmp.height * scale);
    c.getContext('2d').drawImage(bmp, 0, 0, c.width, c.height);
    return await new Promise((r) => c.toBlob((b) => r(b || file), 'image/jpeg', 0.86));
  } catch {
    return file;
  }
}

const MAX_PHOTOS = 4; // per message (server limit)
const unpreview = (list) => list.forEach((a) => a.preview && URL.revokeObjectURL(a.preview));

function Composer({ cid, running }) {
  const [text, setText] = useState('');
  const [atts, setAtts] = useState([]);
  const [rec, setRec] = useState(null);
  const [unsent, setUnsent] = useState([]);
  const textNow = useRef('');
  textNow.current = text;
  const ta = useRef();
  const file = useRef();
  // Photo previews are object URLs: revoke each when its photo is sent,
  // removed, fails to upload, or the composer goes away.
  const mine = useRef({ atts: [], mounted: true });
  mine.current.atts = atts;
  useEffect(() => () => {
    mine.current.mounted = false;
    unpreview(mine.current.atts);
  }, []);

  useEffect(() => {
    const d = S.drafts[cid];
    if (d) {
      setText(d);
      set({ drafts: { ...S.drafts, [cid]: undefined } });
      setTimeout(() => ta.current?.focus(), 50);
    }
  }, [cid]);

  useLayoutEffect(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, 132) + 'px';
  }, [text]);

  const pick = async (e) => {
    const files = [...e.target.files].slice(0, MAX_PHOTOS - atts.length);
    e.target.value = '';
    // The loop awaits; stop once the composer is gone (its cleanup already
    // ran, so revoke what this iteration made itself).
    for (const f of files) {
      if (!mine.current.mounted) break;
      const key = Math.random().toString(36).slice(2);
      const preview = URL.createObjectURL(f);
      setAtts((a) => [...a, { key, preview, uploading: true }]);
      try {
        const blob = await shrink(f);
        if (!mine.current.mounted) { URL.revokeObjectURL(preview); break; }
        const r = await api('POST', '/attachments', blob);
        if (!mine.current.mounted) { URL.revokeObjectURL(preview); break; }
        setAtts((a) => a.map((x) => (x.key === key ? { ...x, id: r.attachment.id, uploading: false } : x)));
      } catch (err) {
        if (!mine.current.mounted) { URL.revokeObjectURL(preview); break; }
        toast(err.status === 415 ? 'I can only take photos (JPEG, PNG, GIF, or WebP).' : err.message);
        setAtts((a) => a.filter((x) => x.key !== key));
        URL.revokeObjectURL(preview);
      }
    }
  };

  const ready = atts.every((a) => !a.uploading);
  const canSend = (text.trim() || atts.length) && ready;

  const send = async () => {
    if (!canSend) return;
    const t = text.trim();
    const sent = atts;
    rec?.stop();
    setText('');
    setAtts([]);
    try {
      await sendMessage(cid, t, sent.map((a) => a.id));
      unpreview(sent);
    } catch (e) {
      toast(e.message);
      if (!mine.current.mounted) { unpreview(sent); return; }
      // Put the text back only if the composer is still empty (or still
      // holds it); a newer draft stays, and the failed one waits for a tap.
      const cur = textNow.current.trim();
      if (!cur || cur === t) setText(t);
      else if (t || sent.length) { unpreview(sent); setUnsent((u) => [...u, { t, ids: sent.map((a) => a.id) }]); return; }
      setAtts((cur) => {
        const all = [...sent, ...cur];
        unpreview(all.slice(MAX_PHOTOS));
        return all.slice(0, MAX_PHOTOS);
      });
    }
  };

  const mic = () => {
    if (rec) { rec.stop(); return; }
    const r = new Speech();
    r.lang = navigator.language || 'en-US';
    r.interimResults = true;
    r.continuous = false;
    const base = text ? text.replace(/\s*$/, ' ') : '';
    r.onresult = (e) => {
      let t = '';
      for (const res of e.results) t += res[0].transcript;
      setText(base + t);
    };
    r.onerror = (e) => { if (e.error !== 'aborted' && e.error !== 'no-speech') toast("I couldn't hear that. Check microphone access."); };
    r.onend = () => setRec(null);
    try { r.start(); setRec(r); } catch { toast("Voice input isn't available here."); }
  };

  const stop = () => post(`/conversations/${seg(cid)}/cancel`).catch((e) => toast(e.message));

  let action;
  if (canSend) action = html`<button class="send" aria-label="Send" onClick=${send}><${Icon} name="send" size=${20} /></button>`;
  else if (rec) action = html`<button class="send rec" aria-label="Stop listening" onClick=${mic}><${Icon} name="stop" size=${18} fill /></button>`;
  else if (running && !text.trim()) action = html`<button class="send stop" aria-label="Stop" onClick=${stop}><${Icon} name="stop" size=${18} fill /></button>`;
  else if (Speech && !text.trim()) action = html`<button class="send" aria-label="Talk" onClick=${mic}><${Icon} name="mic" size=${20} /></button>`;
  else action = html`<button class="send" aria-label="Send" disabled><${Icon} name="send" size=${20} /></button>`;

  const retry = async (u) => {
    setUnsent((l) => l.filter((x) => x !== u));
    try { await sendMessage(cid, u.t, u.ids); } catch (e) {
      toast(e.message);
      if (mine.current.mounted) setUnsent((l) => [...l, u]);
    }
  };

  return html`<div class="composer">
    ${unsent.map((u) => html`<button class="note" onClick=${() => retry(u)}>Not sent, tap to retry: ${u.t || 'photo'}</button>`)}
    ${atts.length > 0 && html`<div class="att-row">${atts.map((a) => html`<div class="att" key=${a.key}>
      <img src=${a.preview} alt="" />${a.uploading && html`<div class="att-busy"><${Spinner} /></div>`}
      <button class="att-x" aria-label="Remove photo" onClick=${() => { URL.revokeObjectURL(a.preview); setAtts((cur) => cur.filter((x) => x.key !== a.key)); }}><${Icon} name="x" size=${12} /></button>
    </div>`)}</div>`}
    <div class=${'inp' + (rec ? ' listening' : '')}>
      <button class="pl" aria-label="Add photos" onClick=${() => file.current.click()} disabled=${atts.length >= MAX_PHOTOS}><${Icon} name="plus" size=${20} /></button>
      <input ref=${file} type="file" accept="image/*" multiple hidden onChange=${pick} />
      <textarea ref=${ta} rows="1" value=${text} placeholder=${rec ? 'Listening…' : `Talk to ${agentName()}…`}
        aria-label="Message" enterkeyhint=${touch ? 'enter' : 'send'}
        onInput=${(e) => setText(e.target.value)}
        onKeyDown=${(e) => { if (e.key === 'Enter' && !e.shiftKey && !touch && !e.isComposing) { e.preventDefault(); send(); } }} />
      ${action}
    </div>
  </div>`;
}
