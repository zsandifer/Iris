// Pure chat timeline helpers: time headers, the owner's Delivered / Read
// receipt, and the new-message count on the jump-to-latest button. No
// DOM, so node tests import this directly. Times are in the device's time zone and locale.

/** A time header goes above a message after this long a gap. */
export const HEADER_GAP = 60 * 60 * 1000;
/** Show the jump-to-latest button this far (px) from the bottom. */
export const JUMP_AWAY = 300;
/** Within this distance (px) of the bottom the chat follows new messages. */
export const NEAR_BOTTOM = 90;

const DAY = 86400000;

/** Local calendar day as a whole number (daylight-saving safe). */
export function dayNumber(ms) {
  const d = new Date(ms);
  return Math.round(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate()) / DAY);
}

/** "Today", "Yesterday", a weekday within the last week, else a date. */
export function dayLabel(ms, now = Date.now(), locale = undefined) {
  const ago = dayNumber(now) - dayNumber(ms);
  if (ago === 0) return 'Today';
  if (ago === 1) return 'Yesterday';
  const d = new Date(ms);
  if (ago > 1 && ago < 7) return d.toLocaleDateString(locale, { weekday: 'long' });
  const opts = { weekday: 'short', month: 'short', day: 'numeric' };
  if (d.getFullYear() !== new Date(now).getFullYear()) opts.year = 'numeric';
  return d.toLocaleDateString(locale, opts);
}

/** "3:42 PM" (or "15:42", per the locale). */
export function timeLabel(ms, locale = undefined) {
  return new Date(ms).toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' });
}

/** Full date and time, for the tooltip on a message time. */
export function fullLabel(ms, locale = undefined) {
  return new Date(ms).toLocaleString(locale, { dateStyle: 'full', timeStyle: 'short' });
}

/**
 * Time headers for messages in display order (`ats` = created_at ms), as
 * in iMessage: one before the first message, and before any message that
 * comes more than HEADER_GAP after the one before it or on a new day.
 * Returns `{ day, time }` (such as `{ day: 'Today', time: '6:30 PM' }`) or
 * null per message.
 */
export function timeHeaders(ats, now = Date.now(), locale = undefined) {
  let last = null;
  return ats.map((at) => {
    const show = last == null || at - last > HEADER_GAP || dayNumber(at) !== dayNumber(last);
    last = at;
    return show ? { day: dayLabel(at, now, locale), time: timeLabel(at, locale) } : null;
  });
}

/**
 * The owner's receipt: under their most recent message, only while no
 * reply has finished after it. 'delivered' until the CLI picks the turn
 * up, then 'read' with the time (`meta.read_at`, or `readAt` from the
 * live turn event). Returns `{ id, state, at }` or null.
 */
export function receiptFor(list, meId, live = {}) {
  let i = -1;
  for (let j = list.length - 1; j >= 0; j--) {
    const m = list[j];
    if (m.role === 'user' && m.author_user_id === meId && m.kind !== 'system_note') { i = j; break; }
  }
  if (i < 0) return null;
  let readAt = null;
  for (const m of list.slice(i + 1)) {
    if (m.role !== 'assistant' || m.kind === 'card') continue;
    if (m.status !== 'streaming') return null; // answered (or failed): no receipt
    readAt = readAt ?? m.meta?.read_at ?? live[m.id]?.readAt ?? null;
  }
  return { id: list[i].id, state: readAt ? 'read' : 'delivered', at: readAt };
}

/**
 * How many messages after `seenId` (the newest message the reader had in
 * view) are not their own. 0 when `seenId` is no longer in the list.
 */
export function newSince(list, seenId, meId) {
  const i = list.findIndex((m) => m.id === seenId);
  if (i < 0) return 0;
  let n = 0;
  for (const m of list.slice(i + 1)) if (!(m.role === 'user' && m.author_user_id === meId)) n++;
  return n;
}

/** Badge text for a new-message count. */
export const badge = (n) => (n > 9 ? '9+' : String(n));
