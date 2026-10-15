// Plain-language schedule wording for Settings: "How often" choices for
// check-ins and time windows ("8 AM–10 PM").

/** "22:00" -> "10 PM", "07:30" -> "7:30 AM". */
export const fmt12 = (hhmm) => {
  const [h, m] = String(hhmm || '').split(':').map(Number);
  if (Number.isNaN(h)) return hhmm;
  return `${h % 12 || 12}${m ? ':' + String(m).padStart(2, '0') : ''} ${h < 12 ? 'AM' : 'PM'}`;
};

/** Check-in intervals offered in the app, in minutes. */
export const EVERY_PRESETS = [30, 60, 120, 240, 1440];

/** 60 -> "Every hour", 1440 -> "Once a day". */
export function everyLabel(mins) {
  const m = Number(mins);
  if (m === 1440) return 'Once a day';
  if (m === 60) return 'Every hour';
  if (m > 60 && m % 60 === 0) return `Every ${m / 60} hours`;
  return `Every ${m} minutes`;
}

/** The presets, plus the current value when it is not one of them. */
export function everyChoices(current) {
  const m = Number(current);
  const list = [...EVERY_PRESETS];
  if (Number.isInteger(m) && m > 0 && !list.includes(m)) list.push(m);
  return list.sort((a, b) => a - b);
}

/** "08:00", "22:00" -> "8 AM–10 PM"; the same start and end mean all day. */
export function betweenLabel(start, end) {
  if (!start || !end) return '';
  if (start === end) return 'All day';
  return `${fmt12(start)}–${fmt12(end)}`;
}
