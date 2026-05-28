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
    expect(timeAgo(NOW - MINUTE, NOW)).toBe('1 minute ago');
  });

  it('uses the singular wording for one minute', () => {
    expect(timeAgo(NOW - MINUTE, NOW)).toBe('1 minute ago');
  });

  it('reports the floor of minutes up to 59', () => {
    expect(timeAgo(NOW - 59 * MINUTE, NOW)).toBe('59 minutes ago');
    // Just under an hour still reads in minutes.
    expect(timeAgo(NOW - (HOUR - SECOND), NOW)).toBe('59 minutes ago');
  });

  it('flips to hours at exactly one hour, with proper plural', () => {
    expect(timeAgo(NOW - HOUR, NOW)).toBe('1 hour ago');
    expect(timeAgo(NOW - 13 * HOUR, NOW)).toBe('13 hours ago');
    expect(timeAgo(NOW - 23 * HOUR, NOW)).toBe('23 hours ago');
  });

  it('flips to days at exactly 24 hours, with proper plural', () => {
    expect(timeAgo(NOW - DAY, NOW)).toBe('1 day ago');
    expect(timeAgo(NOW - 7 * DAY, NOW)).toBe('7 days ago');
    // Up to and including 13 days is still reported in days.
    expect(timeAgo(NOW - 13 * DAY, NOW)).toBe('13 days ago');
  });

  it('flips to weeks at 14 days, with proper plural', () => {
    expect(timeAgo(NOW - 14 * DAY, NOW)).toBe('2 weeks ago');
    expect(timeAgo(NOW - 21 * DAY, NOW)).toBe('3 weeks ago');
    // Up to 8 weeks still reported in weeks (the boundary just
    // before the month threshold trips at ~60 days).
    expect(timeAgo(NOW - 56 * DAY, NOW)).toBe('8 weeks ago');
  });

  it('flips to months at 9 weeks (~63 days), with proper plural', () => {
    // The week branch caps at 8 weeks (56 days). The first day past
    // that, week count would round to 9, so the function falls
    // through to months. 63 / 30.44 ≈ 2 months.
    expect(timeAgo(NOW - 63 * DAY, NOW)).toBe('2 months ago');
    expect(timeAgo(NOW - 6 * 30 * DAY, NOW)).toBe('5 months ago');
    expect(timeAgo(NOW - 11 * 30 * DAY, NOW)).toBe('10 months ago');
  });

  it('flips to years at 12 months, with proper plural', () => {
    // 12 * 30.44 ≈ 365.28 days, just over the year threshold.
    expect(timeAgo(NOW - 366 * DAY, NOW)).toBe('1 year ago');
    expect(timeAgo(NOW - 2 * 366 * DAY, NOW)).toBe('2 years ago');
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
