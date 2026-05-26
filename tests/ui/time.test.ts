// Pure-formatter tests for `timeAgo` and `formatAbsolute`. Both take
// timestamps as `number` (ms since epoch) and return strings; no DOM
// involvement, no `Date.now()` dependence, so the tests are
// deterministic.

import { formatAbsolute, timeAgo } from '@/lib/time';

const SECOND = 1_000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

const NOW = Date.UTC(2026, 4, 26, 12, 0, 0); // May 26 2026 12:00:00 UTC

describe('timeAgo', () => {
  it('returns "just now" for the same instant', () => {
    expect(timeAgo(NOW, NOW)).toBe('just now');
  });

  it('returns "just now" inside the first minute', () => {
    expect(timeAgo(NOW - 30 * SECOND, NOW)).toBe('just now');
    // Right at the edge: 59s still "just now".
    expect(timeAgo(NOW - 59 * SECOND, NOW)).toBe('just now');
  });

  it('flips to minutes at exactly 60 seconds', () => {
    expect(timeAgo(NOW - MINUTE, NOW)).toBe('1 min ago');
  });

  it('uses the singular wording for one minute (no "1 mins")', () => {
    const text = timeAgo(NOW - MINUTE, NOW);
    expect(text).toBe('1 min ago');
    expect(text).not.toMatch(/mins/);
  });

  it('reports the floor of minutes up to 59', () => {
    expect(timeAgo(NOW - 59 * MINUTE, NOW)).toBe('59 min ago');
    // Just under an hour still reads in minutes.
    expect(timeAgo(NOW - (HOUR - SECOND), NOW)).toBe('59 min ago');
  });

  it('flips to hours at exactly one hour', () => {
    expect(timeAgo(NOW - HOUR, NOW)).toBe('1 h ago');
    expect(timeAgo(NOW - 23 * HOUR, NOW)).toBe('23 h ago');
  });

  it('flips to days at exactly 24 hours', () => {
    expect(timeAgo(NOW - DAY, NOW)).toBe('1 d ago');
    expect(timeAgo(NOW - 7 * DAY, NOW)).toBe('7 d ago');
  });

  it('clamps future timestamps to "just now" (no negative diff)', () => {
    expect(timeAgo(NOW + HOUR, NOW)).toBe('just now');
  });

  it('defaults the `now` argument to Date.now()', () => {
    // Don't assert exact wording; just confirm the call shape is
    // accepted. Useful regression against accidental signature change.
    const text = timeAgo(Date.now() - MINUTE);
    expect(typeof text).toBe('string');
    expect(text.length).toBeGreaterThan(0);
  });
});

describe('formatAbsolute', () => {
  // `toLocaleString` output depends on the runtime locale and time
  // zone, so we only assert that the formatter mentions the
  // information we asked for: year, month, day, plus hour/minute
  // separators. The exact string differs between Node and browsers.
  //
  // Building the timestamp via the local-time `Date` constructor
  // (rather than `Date.UTC`) keeps the displayed day stable across
  // host time zones; otherwise May 26 14:30 UTC slips forward or
  // back a day depending on where the test runs.

  const ts = new Date(2026, 4, 26, 14, 30, 45).getTime();

  it('includes the four-digit year', () => {
    expect(formatAbsolute(ts)).toMatch(/2026/);
  });

  it('includes the day of month', () => {
    expect(formatAbsolute(ts)).toMatch(/\b26\b/);
  });

  it('includes a short month token (May)', () => {
    // "May" happens to have a 3-letter form identical to its full
    // name, so this assertion holds regardless of "short" vs "long"
    // month naming on the host locale.
    expect(formatAbsolute(ts)).toMatch(/May/);
  });

  it('includes hour and minute separated by a colon', () => {
    expect(formatAbsolute(ts)).toMatch(/\d{1,2}:\d{2}/);
  });
});
