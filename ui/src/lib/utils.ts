import { type ClassValue, clsx } from 'clsx';
import { twMerge } from 'tailwind-merge';

export const cn = (...inputs: ClassValue[]): string => twMerge(clsx(inputs));

/** True when running on macOS. Used to pick between the Cmd and Ctrl
 * modifier in keyboard-shortcut affordances. SSR-safe: returns false
 * when there is no `navigator`. `navigator.platform` is deprecated but
 * remains the most reliable synchronous signal across browsers; fall
 * back to the UA string when it is empty. */
export const isMac = (): boolean => {
  if (typeof navigator === 'undefined') {
    return false;
  }
  const probe = navigator.platform || navigator.userAgent || '';
  return /mac/i.test(probe);
};
