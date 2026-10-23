// Chat: messages, live status phrases, receipts, cards, and the composer.
import { html, useEffect, useLayoutEffect, useRef, useState } from './h.js';
import { post, put, api, seg, isId } from './api.js';
import { S, set, useStore, loadMessages, loadOlder, sendMessage, toast, mainId, isOwner, convFor, lookUpConv, peekShown } from './store.js';
import { Icon, Img, Avatar, agentName, Spinner, clock } from './ui.js';
import { timeHeaders, receiptFor, fullLabel, newSince, badge, JUMP_AWAY, NEAR_BOTTOM } from './chat-time.js';
import { openPhoto } from './photo.js';
import { md } from './md.js';
import { Card } from './cards.js';
import { statusPhrase, receipt, seconds } from './status.js';
import { pushState, enablePush } from './push.js';
import { BrowserPeek } from './computer.js';
import { focusComposer, prepareComposerFocus } from './viewport.js';
import { NotedLine } from './memory.js';

/** Typing dots, with what the agent is doing. */
function Typing({ live, text }) {
  const phrase = text || statusPhrase(live);
  const line = phrase === 'Thinking…' ? `${agentName()} is thinking…` : phrase;
  return html`<div class="typing" aria-live="polite"><span class="dots" aria-hidden="true"><i /><i /><i /></span><span>${line}</span></div>`;
}

/** Delivered / Read under the owner's most recent message. */
function OwnerReceipt({ r }) {
  return html`<div class="own-rcpt" role="status"><b>${r.state === 'read' ? 'Read' : 'Delivered'}</b>${r.at ? ` ${clock(r.at)}` : ''}</div>`;
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
    ${ids.map((id) => html`<button class="photo" onClick=${() => openPhoto(`/api/attachments/${seg(id)}`, 'Photo')}>
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

/** The live browser peek under an assistant message (`peekShown`). A
 * finished turn often ends by asking the owner to take over, so its peek
 * stays a while; this re-renders when that minimum time is up (the server
 * drops it when they take over or the browser closes). */
function Peek({ m, list }) {
  const [, tick] = useState(0);
  const until = S.browsing[m.id]?.until;
  const left = until == null ? 0 : until - Date.now();
  useEffect(() => {
    if (left <= 0) return undefined;
    const t = setTimeout(() => tick((x) => x + 1), left + 50);
    return () => clearTimeout(t);
  }, [until]);
  return peekShown(m.id, list) ? html`<${BrowserPeek} />` : null;
}

function peekFor(m, list) {
  return S.browsing[m.id] ? html`<${Peek} m=${m} list=${list} />` : null;
}

/** Under a message the agent sent on its own (a check-in): was it worth
 * sending? The verdict is the scorecard the next check-in learns from. */
function Verdict({ m }) {
  if (m.meta?.initiated_by !== 'heartbeat' || !isOwner()) return null;
  const v = m.meta.proactive_outcome;
  const mark = async (outcome) => {
    try { await post(`/proactive/${seg(m.id)}/outcome`, { outcome }); } catch (e) { toast(e.message); }
  };
  if (v) return html`<div class="verdict done">${v === 'useful' ? 'Marked useful' : 'Marked not needed. I’ll send fewer like this.'}</div>`;
  return html`<div class="verdict" role="group" aria-label="Was this worth sending?">
    <span class="mu">Worth sending?</span>
    <button onClick=${() => mark('useful')}>Yes</button>
    <button onClick=${() => mark('not_needed')}>Not needed</button>
  </div>`;
}

function Assistant({ m, list }) {
  const live = S.live[m.id];
  const peek = peekFor(m, list);
  if (m.status === 'streaming') {
    const text = live?.text || '';
    const running = (live?.acts || []).some((a) => a.status === 'started');
    const waiting = list.some((x) => x.card?.type === 'approval' && x.card.turn_message_id === m.id && x.card.state === 'pending');
    // Until the CLI picks the turn up, the owner's receipt says Delivered.
    const read = !!(m.meta?.read_at || live?.readAt || text || live?.acts?.length);
    if (waiting) return html`${text && html`<div class="b" dangerouslySetInnerHTML=${{ __html: md(text) }} />`}<${Typing} text="Waiting for your OK…" />${peek}`;
    if (!text) return html`${read && html`<${Typing} live=${live} />`}${peek}`;
    return html`<div class="b" dangerouslySetInnerHTML=${{ __html: md(text) }} />
      ${running && html`<${Typing} live=${live} />`}${peek}`;
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
    <${NotedLine} m=${m} />
    <${Verdict} m=${m} />
    ${peek}
    ${failed && m.meta?.error?.kind !== 'paused' && retryFor(list, m) && html`<button class="retry" onClick=${retry}><${Icon} name="refresh" size=${15} /> Try again</button>`}`;
}

function Message({ m, list, members, composing }) {
  if (m.kind === 'system_note') return html`<div class="system-note" role="status">${m.content}</div>`;
  if (m.kind === 'card') return html`<${Card} m=${m} inactive=${composing} />`;
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

/** Round button above the composer that scrolls to the newest message,
 * with a count of messages that arrived while scrolled up. */
function JumpButton({ count, onJump }) {
  const label = count ? `Jump to latest, ${count} new message${count === 1 ? '' : 's'}` : 'Jump to latest';
  return html`<button class="jump" aria-label=${label} onClick=${onJump}>
    <${Icon} name="down" size=${20} />${count > 0 && html`<span class="jump-n" aria-hidden="true">${badge(count)}</span>`}
  </button>`;
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

/** Shown while the owner has paused Iris. */
function PausedBanner() {
  if (!S.settings?.paused) return null;
  const resume = async () => {
    try { await put('/safety', { paused: false }); toast(`${agentName()} is back`); } catch (e) { toast(e.message); }
  };
  return html`<div class="paused-bar" role="status"><${Icon} name="pause" size=${15} />
    <span class="grow">${agentName()} is paused and won't reply or do anything until you resume.</span>
    ${isOwner() && html`<button class="b1 sm" onClick=${resume}>Resume</button>`}</div>`;
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
  // Where the reader is: 'bottom' (following new messages), 'near', or
  // 'far' (JUMP_AWAY or more up: the jump button shows).
  const [pos, setPos] = useState('bottom');
  // The newest message the reader had in view, for the new-message count.
  const seen = useRef(null);
  // A tap on the jump button is smooth-scrolling down.
  const jumping = useRef(false);
  const jumpTimer = useRef(0);
  useEffect(() => () => clearTimeout(jumpTimer.current), []);
  const conv = convFor(cid);
  const box = S.msgs[cid];
  const list = box?.list || [];
  const [members, setMembers] = useState(null);
  const [composing, setComposing] = useState(false);
  const firstNewest = useRef(null);

  useEffect(() => {
    stick.current = true;
    jumping.current = false;
    setPos('bottom');
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

  // Keep the newest message in view unless the user scrolled up (or a
  // jump is gliding there: setting scrollTop would cut it short).
  useLayoutEffect(() => {
    const el = scr.current;
    if (!el) return;
    if (prevHeight.current && el.scrollTop < 80 && el.scrollHeight > prevHeight.current && !stick.current) {
      el.scrollTop += el.scrollHeight - prevHeight.current;
    } else if (stick.current && !jumping.current) {
      el.scrollTop = el.scrollHeight;
    }
    prevHeight.current = el.scrollHeight;
    // A growing reply moves the bottom away without a scroll event.
    if (!stick.current && !jumping.current) {
      setPos(el.scrollHeight - el.scrollTop - el.clientHeight >= JUMP_AWAY ? 'far' : 'near');
    }
  });

  // Resizing the viewport or a growing composer does not render this
  // component. Follow the bottom through those changes, too.
  useLayoutEffect(() => {
    const el = scr.current;
    const observer = new ResizeObserver(() => {
      if (stick.current && !jumping.current) el.scrollTop = el.scrollHeight;
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const lastId = list.at(-1)?.id;
  useEffect(() => {
    if (lastId && isId(cid) && document.visibilityState === 'visible') post(`/conversations/${seg(cid)}/read`).catch(() => {});
  }, [lastId]);

  const onScroll = (e) => {
    const el = e.currentTarget;
    const away = el.scrollHeight - el.scrollTop - el.clientHeight;
    if (jumping.current) {
      // Keep following the bottom until the smooth scroll gets there.
      if (away >= NEAR_BOTTOM) return;
      jumping.current = false;
    }
    stick.current = away < NEAR_BOTTOM;
    if (stick.current) seen.current = list.at(-1)?.id ?? null;
    setPos(stick.current ? 'bottom' : away >= JUMP_AWAY ? 'far' : 'near');
    if (el.scrollTop < 60 && box?.hasMore && !box.loading) {
      prevHeight.current = el.scrollHeight;
      loadOlder(cid).catch(() => {});
    }
  };

  // Like native chat apps: tapping the conversation or dragging it puts
  // the keyboard away. Taps on controls still do their own thing first.
  const typing = () => {
    const a = document.activeElement;
    return a && a !== document.body && a.closest?.('.composer') ? a : null;
  };
  const dismissOnTap = (e) => {
    if (!e.target.closest?.('a, button, input, textarea, select, [role="button"]')) typing()?.blur();
  };
  const dismissOnDrag = () => typing()?.blur();
  // Touching, wheeling, or keys in the conversation stop following a jump.
  const interrupt = () => { jumping.current = false; };
  // A jump that has not arrived in time (content kept growing, or the
  // browser never reported the end) finishes at the bottom.
  const settle = () => {
    const el = scr.current;
    if (!jumping.current || !el) return;
    jumping.current = false;
    if (stick.current) el.scrollTop = el.scrollHeight;
  };

  const jump = () => {
    const el = scr.current;
    if (!el) return;
    const smooth = !matchMedia('(prefers-reduced-motion: reduce)').matches;
    stick.current = true;
    jumping.current = smooth;
    seen.current = list.at(-1)?.id ?? null;
    setPos('bottom');
    el.scrollTo({ top: el.scrollHeight, behavior: smooth ? 'smooth' : 'auto' });
    // Not every browser reports the end of a smooth scroll.
    clearTimeout(jumpTimer.current);
    if (smooth) jumpTimer.current = setTimeout(settle, 1500);
  };

  const running = list.some((m) => m.status === 'streaming');
  // Approval cards belong before the reply of the turn that asked.
  const asks = {};
  for (const m of list) {
    const t = m.card?.type === 'approval' && m.card.turn_message_id;
    if (t && list.some((x) => x.id === t)) (asks[t] ||= []).push(m);
  }
  const held = new Set(Object.values(asks).flat().map((m) => m.id));
  const shown = [];
  for (const m of list) {
    if (held.has(m.id)) continue;
    shown.push(...(asks[m.id] || []), m);
  }
  // A time header after an hour's gap (or a new day), and the owner's
  // receipt under their latest message.
  const heads = timeHeaders(shown.map((m) => m.created_at));
  const rcpt = receiptFor(list, S.me?.user?.id, S.live);
  // New messages pop in; ones already there when the chat opened (or
  // older pages loaded later) don't.
  if (firstNewest.current == null && box) firstNewest.current = list.at(-1)?.created_at ?? 0;
  const rows = [];
  shown.forEach((m, i) => {
    const h = heads[i];
    if (h) rows.push(html`<div class="ts" role="separator" aria-label=${`${h.day} ${h.time}`} key=${'d' + m.id}><b>${h.day}</b> ${h.time}</div>`);
    const fresh = firstNewest.current != null && m.created_at > firstNewest.current;
    rows.push(html`<div class=${'row' + (fresh ? ' pop' : '')} key=${m.id} title=${fullLabel(m.created_at)}><${Message} m=${m} list=${list} members=${members} composing=${composing} />
      ${rcpt?.id === m.id && html`<${OwnerReceipt} r=${rcpt} />`}</div>`);
  });

  // New messages from others while scrolled up: count them on the button.
  if (stick.current) seen.current = lastId ?? null;
  const fresh = stick.current ? 0 : newSince(list, seen.current, S.me?.user?.id);
  const showJump = !stick.current && (pos === 'far' || fresh > 0);
  const isMain = cid === mainId();
  return html`<div class=${'chat' + (S.settings?.paused ? ' paused' : '')}>
    <${PausedBanner} />
    <div class="scr" ref=${scr} onScroll=${onScroll} onClick=${dismissOnTap} onTouchMove=${dismissOnDrag}
      onPointerDown=${interrupt} onTouchStart=${interrupt} onWheel=${interrupt} onKeyDown=${interrupt}>
      ${box?.loading && box?.hasMore && html`<${Spinner} />`}
      ${!box ? html`<${Spinner} />` : !list.length && isMain ? html`<${Welcome} />` : null}
      ${isMain && box && !box.hasMore && html`<${NotifyPrompt} />`}
      ${rows}
    </div>
    <${Composer} cid=${cid} running=${running}
      jump=${showJump && html`<${JumpButton} count=${fresh} onJump=${jump} />`} onFocus=${() => { stick.current = true; setComposing(true); }} onBlur=${() => setComposing(false)} />
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

function Composer({ cid, running, jump, onFocus, onBlur }) {
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
      setTimeout(() => focusComposer(ta.current), 50);
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

  const onComposerPointerDown = (e) => {
    const el = ta.current;
    if (e.isPrimary === false || e.button > 0 || !el
      || e.target.closest('button, input, a')) return;
    if (document.activeElement === el) prepareComposerFocus(el);
    else focusComposer(el);
    // Leave the default gesture intact for native caret placement,
    // selection, and scrolling within a multiline draft.
  };

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
    ${jump}
    ${unsent.map((u) => html`<button class="note" onClick=${() => retry(u)}>Not sent, tap to retry: ${u.t || 'photo'}</button>`)}
    ${atts.length > 0 && html`<div class="att-row">${atts.map((a) => html`<div class="att" key=${a.key}>
      <${Img} src=${a.preview} alt="Photo to send" />${a.uploading && html`<div class="att-busy"><${Spinner} /></div>`}
      <button class="att-x" aria-label="Remove photo" onClick=${() => { URL.revokeObjectURL(a.preview); setAtts((cur) => cur.filter((x) => x.key !== a.key)); }}><${Icon} name="x" size=${12} /></button>
    </div>`)}</div>`}
    <div class=${'inp' + (rec ? ' listening' : '')} onPointerDown=${onComposerPointerDown} onTouchStart=${onComposerPointerDown}>
      <button class="pl" aria-label="Add photos" onClick=${() => file.current.click()} disabled=${atts.length >= MAX_PHOTOS}><${Icon} name="plus" size=${20} /></button>
      <input ref=${file} type="file" accept="image/*" multiple hidden tabindex="-1" onChange=${pick} />
      <textarea ref=${ta} rows="1" value=${text} placeholder=${rec ? 'Listening…' : `Talk to ${agentName()}…`}
        aria-label="Message" enterkeyhint=${touch ? 'enter' : 'send'}
        onFocus=${onFocus} onBlur=${onBlur}
        onInput=${(e) => setText(e.target.value)}
        onKeyDown=${(e) => { if (e.key === 'Enter' && !e.shiftKey && !touch && !e.isComposing) { e.preventDefault(); send(); } }} />
      ${action}
    </div>
  </div>`;
}
