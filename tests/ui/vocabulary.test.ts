// The words that left with the agent process must stay gone from the UI
// source, comments included: rustdoc-style drift in a comment is how the
// server's `unix.rs` came to describe a spawn that no longer existed.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import { describeRejection } from '@/lib/attachments';

const walk = (dir: string): string[] =>
  readdirSync(dir).flatMap((name) => {
    const full = path.join(dir, name);
    return statSync(full).isDirectory() ? walk(full) : [full];
  });

describe('UI vocabulary', () => {
  it('names no removed agent concept anywhere under ui/src', () => {
    const root = path.resolve(__dirname, '../../ui/src');
    const stale = /\bKiro\b|\bACP\b|\bMCP\b|parse_kiro_history|kiro_default|ModeModelSelectors/;
    const offenders: string[] = [];
    for (const file of walk(root)) {
      if (!/\.(ts|tsx|css)$/.test(file)) {
        continue;
      }
      const hit = readFileSync(file, 'utf8').match(stale);
      if (hit) {
        offenders.push(`${path.relative(root, file)}: ${hit[0]}`);
      }
    }
    expect(offenders).toEqual([]);
  });

  it('describes a rejected attachment in terms of the session', () => {
    expect(describeRejection({ kind: 'image-not-supported' })).toBe(
      'This session does not accept images.'
    );
    expect(describeRejection({ kind: 'embed-not-supported' })).toBe(
      'This session does not accept embedded files.'
    );
  });
});
