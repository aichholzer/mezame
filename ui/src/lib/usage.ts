// Formatting for the token counts a turn reports on `prompt_done`. The
// footer under the last agent bubble shows the short form; the exact
// counts sit in its `title`.

import type { Usage } from '@/types';

const SHORT_FROM = 10_000;

/** `9,999` stays exact; from 10,000 on the count is shortened to one
 * decimal with a `k` suffix (`10.0k`, `12.3k`). */
export const formatCount = (n: number): string => {
  if (n >= SHORT_FROM) {
    return `${(n / 1000).toFixed(1)}k`;
  }
  return n.toLocaleString('en-US');
};

/** `<input> in · <output> out · <cacheRead> cached`, adding
 * ` · <cacheWrite> written` only when a cache write happened. */
export const formatUsage = (u: Usage): string => {
  const parts = [
    `${formatCount(u.input)} in`,
    `${formatCount(u.output)} out`,
    `${formatCount(u.cacheRead)} cached`
  ];
  if (u.cacheWrite > 0) {
    parts.push(`${formatCount(u.cacheWrite)} written`);
  }
  return parts.join(' · ');
};

/** The exact counts, for the footer's `title`. */
export const exactUsage = (u: Usage): string =>
  `${u.input.toLocaleString('en-US')} input · ${u.output.toLocaleString('en-US')} output · ` +
  `${u.cacheRead.toLocaleString('en-US')} cache read · ${u.cacheWrite.toLocaleString('en-US')} cache write`;
