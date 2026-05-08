/** Human-friendly elapsed time. Matches the legacy UI wording. */
export const timeAgo = (ts: number, now: number = Date.now()): string => {
  const diff = Math.max(0, now - ts);
  const s = Math.floor(diff / 1000);
  if (s < 60) {
    return 'just now';
  }
  const m = Math.floor(s / 60);
  if (m < 60) {
    return `${m} min ago`;
  }
  const h = Math.floor(m / 60);
  if (h < 24) {
    return `${h} h ago`;
  }
  const d = Math.floor(h / 24);
  return `${d} d ago`;
};

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
