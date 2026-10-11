// Chat: messages, live status phrases, receipts, cards, and the composer.
import { html, useEffect, useLayoutEffect, useRef, useState } from './h.js';
import { post, api } from './api.js';
import { S, set, useStore, loadMessages, loadOlder, sendMessage, toast, mainId, isOwner, loadMembers, hostAgent, loadEngines } from './store.js';
import { Icon, Img, Avatar, agentName, stamp, Spinner, openSheet, closeSheet } from './ui.js';
import { md } from './md.js';
import { Card } from './cards.js';
import { statusPhrase, receipt, seconds } from './status.js';
import { pushState, enablePush } from './push.js';
import {
  VoiceNote, voiceReplies, serverListen, canRecord, record, transcribe, unlockAudio, play, prefetch, Speech,
  MAX_RECORDING_MS,
} from './voice.js';

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
    ${ids.map((id) => html`<button class="photo" onClick=${() => set({ lightbox: `/api/attachments/${id}` })}>
      <${Img} src=${`/api/attachments/${id}`} alt="Photo" /></button>`)}
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
  const spoken = voiceReplies() && m.status === 'complete' && m.kind === 'text' && content.trim();
  return html`<${Receipt} meta=${m.meta} />
    ${spoken && html`<${VoiceNote} m=${m} />`}
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

function TogetherWelcome({ cid }) {
  const info = S.members[cid];
  const host = hostAgent(cid);
  const others = (info?.members || []).filter((x) => x.user_id !== S.me?.user?.id).map((x) => x.display_name);
  return html`<div class="welcome">
    <${Avatar} size=${64} agent=${host} />
    <h2 class="serif">${others.length ? `You and ${others.join(', ')}` : 'Together'}</h2>
    <p class="mu">${host?.name || agentName()} is here too. Everyone sees every message.
    Nobody's private memory or files are used in this chat.</p>
  </div>`;
}

export function ChatScreen({ cid }) {
  useStore();
  const scr = useRef();
  const stick = useRef(true);
  const prevHeight = useRef(0);
  const conv = (S.convs || []).find((c) => c.id === cid);
  const box = S.msgs[cid];
  const list = box?.list || [];
  const [members, setMembers] = useState(null);

  useEffect(() => {
    stick.current = true;
    if (!S.msgs[cid]) loadMessages(cid);
    post(`/conversations/${cid}/read`).catch(() => {});
  }, [cid]);

  const shared = conv && (conv.kind === 'group' || conv.member_count > 1);
  useEffect(() => {
    if (shared) {
      loadMembers(cid).then((r) => {
        setMembers(Object.fromEntries(r.members.map((x) => [x.user_id, x.display_name])));
      }).catch(() => {});
    } else setMembers(null);
  }, [cid, conv?.member_count, shared]);

  // Voice replies: fetch the voice note as soon as a reply is done, and
  // play it when it answers something sent from this device.
  // The newest text reply (cards Iris made during the turn come after it).
  const last = list.findLast((m) => m.role === 'assistant' && m.kind === 'text');
  useEffect(() => {
    if (!last || last.role !== 'assistant' || last.status !== 'complete' || last.kind !== 'text' || !voiceReplies()) return;
    prefetch(last);
    const auto = S.voiceAuto;
    if (auto && auto.cid === cid && auto.id === last.id) {
      set({ voiceAuto: null });
      if (document.visibilityState === 'visible') play(last);
    }
  }, [last?.id, last?.status]);

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
    if (lastId && document.visibilityState === 'visible') post(`/conversations/${cid}/read`).catch(() => {});
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
      ${!box ? html`<${Spinner} />` : !list.length && isMain ? html`<${Welcome} />` : !list.length && shared ? html`<${TogetherWelcome} cid=${cid} />` : null}
      ${isMain && box && !box.hasMore && html`<${NotifyPrompt} />`}
      ${rows}
    </div>
    <${Composer} cid=${cid} running=${running} name=${hostAgent(cid)?.name || agentName()} />
  </div>`;
}

// ---------------------------------------------------------------- composer

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

/** How voice input works here: recorded and transcribed on the server
 *  (private, works on every phone), else the browser's own recognition,
 *  else it needs setting up on the server. */
function micMode() {
  if (serverListen() && canRecord()) return 'server';
  if (Speech) return 'browser';
  if (canRecord()) return 'setup';
  return null;
}

function VoiceSetupSheet() {
  useStore();
  const e = (S.engines?.engines || []).find((x) => x.name === 'stt');
  const busy = e?.operation?.state === 'running';
  const install = async () => {
    try { await api('POST', '/engines/stt/install'); toast('Setting up voice input on your server…'); closeSheet(); } catch (err) { toast(err.message); }
  };
  return html`<div class="sheet-body"><h3>Voice input</h3>
    <p class="mu">${isOwner()
      ? `To talk instead of type, your server needs speech recognition. It runs on your server with an open-source model (Whisper), so your voice never leaves it.${e?.download_mb ? ` It's a one-time ${e.download_mb} MB download.` : ''}`
      : 'Voice input needs speech recognition on your server. Ask the person who runs your Iris to turn on voice input in Settings.'}</p>
    <div class="btns"><button class="b2" onClick=${closeSheet}>Not now</button>
      ${isOwner() && html`<button class="b1" disabled=${busy} onClick=${install}>${busy ? 'Setting up…' : 'Set it up'}</button>`}</div></div>`;
}

function Composer({ cid, running, name }) {
  const [text, setText] = useState('');
  const [atts, setAtts] = useState([]);
  const [rec, setRec] = useState(null);       // browser recognition
  const [recording, setRecording] = useState(null); // server recording handle
  const [secs, setSecs] = useState(0);
  const [hearing, setHearing] = useState(false); // transcribing
  const ta = useRef();
  const file = useRef();
  const textRef = useRef('');
  textRef.current = text;
  const recRef = useRef(null);
  recRef.current = recording;
  const mounted = useRef(true);
  const starting = useRef(false);
  useEffect(() => () => { mounted.current = false; }, []);

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

  // Recording timer; stop recording when leaving the chat.
  useEffect(() => {
    if (!recording) return undefined;
    const t = setInterval(() => {
      setSecs(Math.floor((Date.now() - recording.started) / 1000));
      // At the limit, stop and transcribe what was said so far.
      if (Date.now() - recording.started >= MAX_RECORDING_MS && recRef.current === recording) listenServer();
    }, 250);
    return () => clearInterval(t);
  }, [recording]);
  useEffect(() => () => recRef.current?.cancel(), []);

  const pick = async (e) => {
    const files = [...e.target.files].slice(0, 4 - atts.length);
    e.target.value = '';
    for (const f of files) {
      const key = Math.random().toString(36).slice(2);
      const preview = URL.createObjectURL(f);
      setAtts((a) => [...a, { key, preview, uploading: true }]);
      try {
        const blob = await shrink(f);
        const r = await api('POST', '/attachments', blob);
        setAtts((a) => a.map((x) => (x.key === key ? { ...x, id: r.attachment.id, uploading: false } : x)));
      } catch (err) {
        toast(err.status === 415 ? 'I can only take photos (JPEG, PNG, GIF, or WebP).' : err.message);
        setAtts((a) => a.filter((x) => x.key !== key));
      }
    }
  };

  const ready = atts.every((a) => !a.uploading);
  const canSend = (text.trim() || atts.length) && ready && !recording && !hearing;

  const send = async () => {
    if (!canSend) return;
    const t = text.trim();
    const ids = atts.map((a) => a.id);
    rec?.stop();
    setText('');
    setAtts([]);
    if (voiceReplies()) unlockAudio();
    try {
      const r = await sendMessage(cid, t, ids);
      if (voiceReplies()) {
        // Play this reply when it is done (it may already be).
        const id = r.assistant_message.id;
        const done = S.msgs[cid]?.list.find((x) => x.id === id && x.status === 'complete');
        if (done) play(done);
        else set({ voiceAuto: { cid, id } });
      }
    } catch (e) { toast(e.message); setText(t); }
  };

  const listenBrowser = () => {
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

  const listenServer = async () => {
    if (recording) {
      const handle = recording;
      recRef.current = null;
      setRecording(null);
      setHearing(true);
      try {
        const blob = await handle.stop();
        if (Date.now() - handle.started < 600) { toast('Hold on a little longer, then tap stop.'); return; }
        const heard = await transcribe(blob);
        if (!heard) toast("I didn't catch anything. Try again a little closer to the mic.");
        else {
          const cur = textRef.current;
          setText(cur ? `${cur.replace(/\s*$/, ' ')}${heard}` : heard);
          setTimeout(() => ta.current?.focus(), 30);
        }
      } catch (e) {
        toast(e.code === 'voice_unavailable' ? 'Voice input was turned off on the server.' : e.message || "I couldn't hear that.");
      } finally {
        setHearing(false);
      }
      return;
    }
    if (starting.current) return;
    starting.current = true;
    unlockAudio();
    try {
      setSecs(0);
      const handle = await record();
      // The chat closed while the microphone was starting: let it go.
      if (!mounted.current) handle.cancel();
      else setRecording(handle);
    } catch (e) {
      if (mounted.current) toast(e?.name === 'NotAllowedError' ? 'Allow microphone access to talk to me.' : "I can't use the microphone here.");
    } finally {
      starting.current = false;
    }
  };

  const mic = () => {
    const mode = micMode();
    if (mode === 'server') listenServer();
    else if (mode === 'browser') listenBrowser();
    else if (mode === 'setup') {
      if (isOwner()) loadEngines().catch(() => {});
      openSheet(() => html`<${VoiceSetupSheet} />`);
    }
  };

  const stop = () => post(`/conversations/${cid}/cancel`).catch((e) => toast(e.message));

  let action;
  if (recording) action = html`<button class="send rec" aria-label="Stop recording" onClick=${listenServer}><${Icon} name="stop" size=${18} fill /></button>`;
  else if (hearing) action = html`<button class="send" aria-label="Transcribing" disabled><span class="vn-spin light" /></button>`;
  else if (canSend) action = html`<button class="send" aria-label="Send" onClick=${send}><${Icon} name="send" size=${20} /></button>`;
  else if (rec) action = html`<button class="send rec" aria-label="Stop listening" onClick=${mic}><${Icon} name="stop" size=${18} fill /></button>`;
  else if (running && !text.trim()) action = html`<button class="send stop" aria-label="Stop" onClick=${stop}><${Icon} name="stop" size=${18} fill /></button>`;
  else if (micMode() && !text.trim()) action = html`<button class="send" aria-label="Talk" onClick=${mic}><${Icon} name="mic" size=${20} /></button>`;
  else action = html`<button class="send" aria-label="Send" disabled><${Icon} name="send" size=${20} /></button>`;

  const listening = rec || recording;
  const placeholder = recording ? `Listening… ${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`
    : hearing ? 'Writing down what you said…' : rec ? 'Listening…' : `Talk to ${name}…`;
  return html`<div class="composer">
    ${atts.length > 0 && html`<div class="att-row">${atts.map((a) => html`<div class="att" key=${a.key}>
      <img src=${a.preview} alt="" />${a.uploading && html`<div class="att-busy"><${Spinner} /></div>`}
      <button class="att-x" aria-label="Remove photo" onClick=${() => setAtts(atts.filter((x) => x.key !== a.key))}><${Icon} name="x" size=${12} /></button>
    </div>`)}</div>`}
    <div class=${'inp' + (listening ? ' listening' : '')}>
      <button class="pl" aria-label="Add photos" onClick=${() => file.current.click()} disabled=${atts.length >= 4 || !!recording}><${Icon} name="plus" size=${20} /></button>
      <input ref=${file} type="file" accept="image/*" multiple hidden onChange=${pick} />
      ${recording ? html`<div class="rec-line" aria-live="polite"><span class="rec-dot" />${placeholder}</div>`
      : html`<textarea ref=${ta} rows="1" value=${text} placeholder=${placeholder}
        aria-label="Message" enterkeyhint=${touch ? 'enter' : 'send'} disabled=${hearing}
        onInput=${(e) => setText(e.target.value)}
        onKeyDown=${(e) => { if (e.key === 'Enter' && !e.shiftKey && !touch && !e.isComposing) { e.preventDefault(); send(); } }} />`}
      ${action}
    </div>
  </div>`;
}
