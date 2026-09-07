// The token-count formatters behind the usage footer (requirement 11.3,
// 11.5): exact below 10,000, one decimal and `k` from 10,000 on, and the
// `written` part only when a cache write happened.

import { exactUsage, formatCount, formatUsage } from '@/lib/usage';

describe('formatCount', () => {
  it('keeps 9,999 exact with a thousands separator', () => {
    expect(formatCount(9_999)).toBe('9,999');
  });

  it('shortens 10,000 to one decimal and a k', () => {
    expect(formatCount(10_000)).toBe('10.0k');
  });

  it('rounds 12,345 to 12.3k', () => {
    expect(formatCount(12_345)).toBe('12.3k');
  });

  it('leaves small counts alone', () => {
    expect(formatCount(0)).toBe('0');
    expect(formatCount(65)).toBe('65');
  });
});

describe('formatUsage', () => {
  it('joins in, out and cached with a middle dot when nothing was written', () => {
    expect(formatUsage({ input: 65, output: 4, cacheRead: 0, cacheWrite: 0 })).toBe(
      '65 in · 4 out · 0 cached'
    );
  });

  it('adds the written part when a cache write happened', () => {
    expect(
      formatUsage({ input: 1_200, output: 340, cacheRead: 12_345, cacheWrite: 10_000 })
    ).toBe('1,200 in · 340 out · 12.3k cached · 10.0k written');
  });
});

describe('exactUsage', () => {
  it('spells every count out in full for the title', () => {
    expect(exactUsage({ input: 12_345, output: 4, cacheRead: 10_000, cacheWrite: 0 })).toBe(
      '12,345 input · 4 output · 10,000 cache read · 0 cache write'
    );
  });
});
