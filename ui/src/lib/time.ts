/** Human-friendly elapsed time. Outputs the singular form for `1 X
 * ago`, plural otherwise, with units stepping up at conventional
 * thresholds:
 *
 *   < 1 min     → "just now"
 *   < 1 h       → "N minutes ago"
 *   < 24 h      → "N hours ago"
 *   < 14 d      → "N days ago"
 *   < ~2 mo     → "N weeks ago"
 *   < 12 mo     → "N months ago"
 *   else        → "N years ago"
 *
 * Future timestamps clamp to "just now". Months and years use the
 * standard 30.44- and 365.25-day approximations rather than calendar
 * arithmetic; it is good enough for chat timeline labels and avoids
 * dragging in a date library.
 */
export const timeAgo = (ts: number, now: number = Date.now()): string => {
  const diff = Math.max(0, now - ts);
  const s = Math.floor(diff / 1000);
  if (s < 60) {
    return 'just now';
  }
  const m = Math.floor(s / 60);
  if (m < 60) {
    return plural(m, 'minute');
  }
  const h = Math.floor(m / 60);
  if (h < 24) {
    return plural(h, 'hour');
  }
  const d = Math.floor(h / 24);
  if (d < 14) {
    return plural(d, 'day');
  }
  const w = Math.floor(d / 7);
  // Bump to months once the week count would hit 9 (~63 days).
  // Eight weeks would round oddly to "8 weeks" right before flipping
  // to "2 months"; capping at the 8-week mark keeps the transition
  // clean and the months branch picks up at 9 weeks.
  if (w < 9) {
    return plural(w, 'week');
  }
  const mo = Math.floor(d / 30.44);
  if (mo < 12) {
    return plural(mo, 'month');
  }
  const y = Math.floor(d / 365.25);
  return plural(y, 'year');
};

const plural = (count: number, unit: string): string =>
  `${count} ${unit}${count === 1 ? '' : 's'} ago`;

/** Absolute timestamp for tooltip display. Locale-aware, short. */
export const formatAbsolute = (ts: number): string => {
  const d = new Date(ts);
  return d.toLocaleString(undefined, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit'
  });
};
