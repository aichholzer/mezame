import { useSyncExternalStore } from 'react';

// Shared heartbeat so every component that cares about elapsed time
// re-renders on the same cadence. One interval for the whole app rather
// than one per component.

const INTERVAL_MS = 30_000;

let counter = 0;
const listeners = new Set<() => void>();
let timer: number | null = null;

const start = () => {
  if (timer !== null) {
    return;
  }
  timer = window.setInterval(() => {
    counter += 1;
    for (const l of listeners) {
      l();
    }
  }, INTERVAL_MS);
};

const stop = () => {
  if (timer !== null && listeners.size === 0) {
    clearInterval(timer);
    timer = null;
  }
};

const subscribe = (l: () => void) => {
  listeners.add(l);
  start();
  return () => {
    listeners.delete(l);
    stop();
  };
};

const getSnapshot = () => counter;

/** Returns a number that increments every 30 seconds, forcing a re-render. */
export const useTick = (): number => useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
