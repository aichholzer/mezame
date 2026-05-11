import { useEffect, useSyncExternalStore } from 'react';

// Writes the current virtual-keyboard occluded height to the
// `--mz-kb-inset` CSS custom property on `document.documentElement`
// AND exposes the same value to React via `useKeyboardInsetValue()`.
//
// CSS consumers (composer bottom offset, log pane bottom padding)
// pick it up via `calc(... + var(--mz-kb-inset))` with zero re-
// render cost. React consumers that need to react to keyboard
// open/close (e.g. re-pinning the log scroll to the bottom when the
// keyboard appears) subscribe via the exported hook.
//
// How the math works: when iOS or Android opens the virtual keyboard,
// `visualViewport.height` shrinks by the keyboard's height while
// `window.innerHeight` stays at the layout-viewport height. The delta
// is the keyboard's occluded region; subtracting `offsetTop` accounts
// for any part of the layout that has scrolled into the safe area
// above the visual viewport.
//
// Fallback behaviour: when `window.visualViewport` is missing (old
// Firefox Android, SSR), the side effect is a no-op, `--mz-kb-inset`
// stays at its `:root` default of `0px`, and `useKeyboardInsetValue`
// keeps returning 0.

// Module-level store of the current inset, shared by every subscriber.
// Writes come from the single `useKeyboardInset()` side effect;
// React readers subscribe via `useSyncExternalStore`.
let currentInset = 0;
const listeners = new Set<() => void>();

const setInset = (next: number) => {
    if (next === currentInset) {
        return;
    }
    currentInset = next;
    for (const l of listeners) {
        l();
    }
};

const subscribe = (onChange: () => void): (() => void) => {
    listeners.add(onChange);
    return () => listeners.delete(onChange);
};

export const useKeyboardInset = (): void => {
    useEffect(() => {
        if (typeof window === 'undefined') {
            return;
        }
        const vv = window.visualViewport;
        if (!vv) {
            return;
        }

        const root = document.documentElement;
        const update = () => {
            const inset = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
            root.style.setProperty('--mz-kb-inset', `${inset}px`);
            setInset(inset);
        };

        vv.addEventListener('resize', update);
        vv.addEventListener('scroll', update);
        update();

        return () => {
            vv.removeEventListener('resize', update);
            vv.removeEventListener('scroll', update);
            // Clear the inline override on unmount so the `:root` default
            // takes over again; prevents a stale value if the hook ever
            // unmounts mid-session.
            root.style.removeProperty('--mz-kb-inset');
            setInset(0);
        };
    }, []);
};

/** Read-side hook for components that need to react (not just style)
 * to keyboard open/close. Returns the current inset in pixels;
 * subscribes to updates via `useSyncExternalStore`. */
export const useKeyboardInsetValue = (): number =>
    useSyncExternalStore(
        subscribe,
        () => currentInset,
        () => 0
    );
