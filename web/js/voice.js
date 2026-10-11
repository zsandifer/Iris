// Voice: replies as voice notes and voice input.
//
// Replies: the server speaks them with its local voice (POST /voice/speak).
// When the server has no voice set up, this device reads the text aloud
// (speechSynthesis) behind the same voice-note control, labeled as such.
//
// Input: recorded on the phone, converted to WAV here, and transcribed on
// the server (POST /voice/transcribe), so audio stays on your server. When
// the server cannot transcribe, the browser's own speech recognition is
// used if it has one.
import { html } from './h.js';
import { api } from './api.js';
import { S, set, useStore, toast } from './store.js';
import { Icon } from './ui.js';

export const voiceReplies = () => !!S.settings?.voice?.enabled;
export const serverVoice = () => !!S.settings?.voice?.tts;
export const serverListen = () => !!S.settings?.voice?.stt;
export const Speech = window.SpeechRecognition || window.webkitSpeechRecognition;
export const canRecord = () => !!(navigator.mediaDevices?.getUserMedia && window.MediaRecorder);
const synth = window.speechSynthesis;

// ------------------------------------------------------------- playback

// One audio element for every voice note. iOS only lets a page play sound
// after a tap, so the first tap (send, or play) unlocks it for later
// automatic playback.
let audio;
function player() {
  if (!audio) {
    audio = new Audio();
    audio.preload = 'auto';
    audio.addEventListener('timeupdate', () => tick(audio.currentTime * 1000));
    audio.addEventListener('ended', () => stopped());
    audio.addEventListener('pause', () => { if (S.voice?.mode === 'server') setVoice({ playing: false }); });
    audio.addEventListener('loadedmetadata', () => {
      if (Number.isFinite(audio.duration)) setVoice({ dur: Math.round(audio.duration * 1000) });
    });
  }
  return audio;
}
// A tiny silent WAV, as a blob URL (the app's CSP allows blob: media only).
let silent;
function silentUrl() {
  if (!silent) {
    const b64 = 'UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAIA+AAACABAAZGF0YQAAAAA=';
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    silent = URL.createObjectURL(new Blob([bytes], { type: 'audio/wav' }));
  }
  return silent;
}
let unlocked = false;
/** Call from a tap handler so later replies may play by themselves. */
export function unlockAudio() {
  if (unlocked) return;
  unlocked = true;
  const a = player();
  a.src = silentUrl();
  a.play().catch(() => { unlocked = false; });
  try { if (synth && !serverVoice()) synth.speak(new SpeechSynthesisUtterance('')); } catch { /* not supported */ }
}

function setVoice(patch) { set({ voice: { ...(S.voice || {}), ...patch } }); }
function tick(t) { if (S.voice?.playing) setVoice({ t }); }
function stopped() { setVoice({ playing: false, t: 0, id: null }); }

// A few recent voice notes stay in memory (least recently used first out;
// their object URLs are revoked unless one is playing).
const KEEP_NOTES = 6;
const notes = new Map(); // message id -> Promise<{url, dur}>
const durs = new Map();  // message id -> length in ms, once known
function serverNote(id) {
  if (notes.has(id)) {
    const p = notes.get(id);
    notes.delete(id);
    notes.set(id, p);
  } else {
    const p = api('POST', '/voice/speak', { message_id: id }, { raw: true, timeout: 180000 })
      .then(async (r) => {
        const n = { dur: Number(r.headers.get('x-voice-duration-ms')) || 0, url: URL.createObjectURL(await r.blob()) };
        durs.set(id, n.dur);
        set({});
        return n;
      });
    p.catch(() => notes.delete(id));
    notes.set(id, p);
    while (notes.size > KEEP_NOTES) {
      const [oldId, old] = notes.entries().next().value;
      notes.delete(oldId);
      old.then((n) => { if (!audio || audio.src !== n.url) URL.revokeObjectURL(n.url); }).catch(() => {});
    }
  }
  return notes.get(id);
}

/** Start fetching a reply's voice note (so it plays without a wait). */
export function prefetch(m) { if (serverVoice() && m?.id) serverNote(m.id).catch(() => {}); }

/** Markdown to plain words for this device's voice (mirrors the server). */
export function speechText(md) {
  let code = false;
  const out = [];
  for (const raw of String(md || '').split('\n')) {
    const l = raw.trim();
    if (/^(```|~~~)/.test(l)) { if (!code) out.push('I put the code in the text.'); code = !code; continue; }
    if (code || !l || /^\|[\s|:-]+\|?$/.test(l)) continue;
    let s = l.replace(/^#+\s*|^>\s*|^[-*+]\s+(\[[ x]\]\s+)?|^\d{1,3}[.)]\s+/g, '')
      .replace(/!\[[^\]]*\]\([^)]*\)/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
      .replace(/https?:\/\/\S+/g, '(link)').replace(/[`*~]|(^|\W)_|_(\W|$)/g, '$1$2');
    if (l.startsWith('|')) s = s.split('|').map((c) => c.trim()).filter(Boolean).join(', ');
    s = s.trim();
    if (s) out.push(/[.!?:;,]$/.test(s) ? s : `${s}.`);
  }
  const text = out.join(' ');
  if (text.length <= 1500) return text;
  const cut = text.slice(0, 1500);
  const end = Math.max(cut.lastIndexOf('. '), cut.lastIndexOf('! '), cut.lastIndexOf('? '));
  return `${cut.slice(0, end > 500 ? end + 1 : cut.lastIndexOf(' '))} The rest is in the text.`;
}

const estimate = (text) => Math.max(1500, Math.round(text.split(/\s+/).length / 2.6 * 1000));

function stopAll() {
  if (audio && !audio.paused) audio.pause();
  if (synth?.speaking) synth.cancel();
}

// Bumped by every play or pause, so a note that finishes loading after the
// person paused (or picked another) never starts.
let playGen = 0;

/** Play (or pause) a reply's voice note. */
export async function play(m) {
  const v = S.voice || {};
  const my = ++playGen;
  if (v.id === m.id && v.playing) {
    stopAll();
    setVoice({ playing: false, loading: false });
    return;
  }
  stopAll();
  if (serverVoice()) {
    setVoice({ id: m.id, playing: true, loading: true, t: v.id === m.id ? v.t || 0 : 0, mode: 'server', dur: 0 });
    try {
      const n = await serverNote(m.id);
      if (playGen !== my || S.voice?.id !== m.id || !S.voice.playing) return;
      const a = player();
      if (a.src !== n.url) a.src = n.url;
      setVoice({ loading: false, dur: n.dur });
      await a.play();
    } catch (e) {
      if (playGen !== my) return;
      if (S.voice?.id === m.id) stopped();
      toast(e?.name === 'NotAllowedError' ? 'Tap play to hear it.' : e?.message || "I couldn't play that.");
    }
    return;
  }
  if (!synth) { toast("This device can't read replies aloud."); return; }
  const text = speechText(m.content);
  const u = new SpeechSynthesisUtterance(text);
  u.lang = navigator.language || 'en-US';
  const dur = estimate(text);
  const started = Date.now();
  const timer = setInterval(() => {
    if (S.voice?.id !== m.id || !S.voice.playing) { clearInterval(timer); return; }
    setVoice({ t: Math.min(dur, Date.now() - started) });
  }, 250);
  u.onend = () => { clearInterval(timer); if (S.voice?.id === m.id) stopped(); };
  u.onerror = () => { clearInterval(timer); if (S.voice?.id === m.id) stopped(); };
  setVoice({ id: m.id, playing: true, loading: false, t: 0, dur, mode: 'device' });
  synth.speak(u);
}

const fmtTime = (ms) => {
  const s = Math.max(0, Math.round((ms || 0) / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};

// A stable, pleasant waveform per message (decoration only).
function bars(id, n = 28) {
  let h = 7;
  const out = [];
  for (let i = 0; i < n; i++) {
    h = (h * 31 + (id.charCodeAt(i % id.length) || 7)) % 997;
    out.push(0.28 + ((h % 70) / 100) * (0.55 + 0.45 * Math.sin((i / n) * Math.PI)));
  }
  return out;
}

/** The voice-note control shown above a reply's text. */
export function VoiceNote({ m }) {
  useStore();
  const v = S.voice || {};
  const active = v.id === m.id;
  const playing = active && v.playing;
  const device = !serverVoice();
  // Only the newest reply is made ahead of time (see prefetch); older ones
  // are made when played, so opening a long chat costs the server nothing.
  // Until a note's real length is known, show an estimate from the text.
  const known = device ? null : durs.get(m.id);
  const total = (active && v.dur) || known || estimate(speechText(m.content));
  const progress = active && total ? Math.min(1, (v.t || 0) / total) : 0;
  const shape = bars(m.id);
  return html`<div class=${'vn' + (playing ? ' on' : '')}>
    <button class="vn-play" aria-label=${playing ? 'Pause voice note' : 'Play voice note'}
      onClick=${() => { unlockAudio(); play(m); }}>
      ${active && v.loading ? html`<span class="vn-spin" />` : html`<${Icon} name=${playing ? 'pausefill' : 'play'} size=${16} fill />`}
    </button>
    <div class="vn-bars" aria-hidden="true">${shape.map((b, i) => html`<i style=${`height:${Math.round(b * 100)}%`}
      class=${i / shape.length < progress ? 'past' : ''} />`)}</div>
    <span class="vn-t">${fmtTime(active && v.t ? v.t : total)}</span>
  </div>
  ${device && html`<div class="vn-lbl">Read aloud by this device</div>`}`;
}

// ---------------------------------------------------------------- input

/** Decode a recording and re-encode it as 16-bit mono WAV. */
export async function toWav(blob) {
  const AC = window.AudioContext || window.webkitAudioContext;
  const ctx = new AC();
  let decoded;
  try {
    decoded = await ctx.decodeAudioData(await blob.arrayBuffer());
  } finally {
    ctx.close?.();
  }
  let rendered = decoded;
  for (const rate of [16000, 22050, 44100]) {
    try {
      const off = new OfflineAudioContext(1, Math.max(1, Math.ceil(decoded.duration * rate)), rate);
      const src = off.createBufferSource();
      src.buffer = decoded;
      src.connect(off.destination);
      src.start();
      rendered = await off.startRendering();
      break;
    } catch { /* this browser does not resample to that rate */ }
  }
  const pcm = rendered.getChannelData(0);
  const rate = rendered.sampleRate;
  const buf = new ArrayBuffer(44 + pcm.length * 2);
  const d = new DataView(buf);
  const str = (o, s) => { for (let i = 0; i < s.length; i++) d.setUint8(o + i, s.charCodeAt(i)); };
  str(0, 'RIFF'); d.setUint32(4, 36 + pcm.length * 2, true); str(8, 'WAVE');
  str(12, 'fmt '); d.setUint32(16, 16, true); d.setUint16(20, 1, true); d.setUint16(22, 1, true);
  d.setUint32(24, rate, true); d.setUint32(28, rate * 2, true); d.setUint16(32, 2, true); d.setUint16(34, 16, true);
  str(36, 'data'); d.setUint32(40, pcm.length * 2, true);
  for (let i = 0; i < pcm.length; i++) {
    const x = Math.max(-1, Math.min(1, pcm[i]));
    d.setInt16(44 + i * 2, x < 0 ? x * 0x8000 : x * 0x7fff, true);
  }
  return new Blob([buf], { type: 'audio/wav' });
}

/** Start recording from the microphone. Resolves to a handle with
 *  `stop()` (resolves to the recording) and `cancel()`. */
export async function record() {
  const stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true } });
  const rec = new MediaRecorder(stream);
  const chunks = [];
  rec.ondataavailable = (e) => { if (e.data?.size) chunks.push(e.data); };
  const done = new Promise((resolve) => { rec.onstop = () => resolve(new Blob(chunks, { type: rec.mimeType || chunks[0]?.type || 'audio/webm' })); });
  const release = () => stream.getTracks().forEach((t) => t.stop());
  rec.start(250);
  return {
    started: Date.now(),
    async stop() { if (rec.state !== 'inactive') rec.stop(); const b = await done; release(); return b; },
    cancel() { if (rec.state !== 'inactive') rec.stop(); release(); },
  };
}

/** Transcribe a recording on the server. */
export async function transcribe(blob) {
  const wav = await toWav(blob);
  const r = await api('POST', '/voice/transcribe', wav, { timeout: 180000 });
  return (r.text || '').trim();
}
