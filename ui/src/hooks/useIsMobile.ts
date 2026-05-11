import { useSyncExternalStore } from 'react';

// Single source of truth for "should this render in the mobile layout?"
// decisions that cannot be expressed in CSS alone (e.g. component
// branching, dynamic MIN_ROWS on the textarea).
//
// The query matches either:
//   - any viewport <= 767 px (phones in portrait), or
//   - a coarse-pointer viewport <= 1023 px (tablets held in portrait,
//     Chromebook in tablet mode).
//
// Desktop-with-touch at >= 1024 px is treated as desktop because the
// horizontal space is there. Mobile-with-mouse is vanishingly rare and
// handled as mobile only if the width is narrow, which is the same
// branch as a phone in portrait.
//
// Kept CSS-media-query-backed (not a resize observer) because
// matchMedia only fires at the breakpoint boundary, so listener count
// stays constant regardless of how fast the user drags a window edge.

const QUERY =
    '(max-width: 767.98px), (pointer: coarse) and (max-width: 1023.98px)';

const subscribe = (onChange: () => void): (() => void) => {
    if (typeof window === 'undefined') {
        return () => { };
    }
    const mql = window.matchMedia(QUERY);
    // `change` covers both directions (matches -> !matches and vice
    // versa). Single listener, single cleanup.
    mql.addEventListener('change', onChange);
    return () => mql.removeEventListener('change', onChange);
};

const getSnapshot = (): boolean => {
    if (typeof window === 'undefined') {
        return false;
    }
    return window.matchMedia(QUERY).matches;
};

// SSR snapshot is always `false`. Mezame is client-rendered so this
// only matters if the module is ever imported in a non-browser
// context; we still provide it because useSyncExternalStore requires
// one.
const getServerSnapshot = (): boolean => false;

export const useIsMobile = (): boolean =>
    useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);
