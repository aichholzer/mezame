import { useCallback, useEffect, useState } from 'react';

// Sidebar resize, with persistence and clamping.
//
// Width is stored in pixels (not rem) so the runtime drag math is
// trivial: pageX delta maps directly to width delta. The persisted
// value lives in `localStorage` under a stable key so the choice
// survives reloads on the same browser; this is intentionally
// per-browser, not per-session, because the right width depends on
// the display, not on the workspace.

const STORAGE_KEY = 'mezame.sidebar.width';

/** Clamps. 12 rem is the minimum readable label width (16 chars at
 * the current font size); 30 rem is the most we let the sidebar
 * consume of the viewport on a normal desktop, beyond which the
 * chat pane gets pinched. */
export const SIDEBAR_MIN_PX = 192;
export const SIDEBAR_MAX_PX = 480;

/** Default. A pinch wider than the original 16 rem so there is room
 * for the new larger MEZAME wordmark without it crowding the edges. */
export const SIDEBAR_DEFAULT_PX = 272;

const clamp = (value: number): number =>
  Math.max(SIDEBAR_MIN_PX, Math.min(SIDEBAR_MAX_PX, value));

const readPersisted = (): number => {
  if (typeof window === 'undefined') {
    return SIDEBAR_DEFAULT_PX;
  }
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (raw === null) {
      return SIDEBAR_DEFAULT_PX;
    }
    const parsed = Number(raw);
    if (!Number.isFinite(parsed)) {
      return SIDEBAR_DEFAULT_PX;
    }
    return clamp(parsed);
  } catch {
    return SIDEBAR_DEFAULT_PX;
  }
};

const writePersisted = (value: number) => {
  if (typeof window === 'undefined') {
    return;
  }
  try {
    window.localStorage.setItem(STORAGE_KEY, String(value));
  } catch {
    // Storage full or disabled (private browsing, quota); the in-memory
    // state still drives the layout for the rest of the session.
  }
};

export type SidebarWidthState = {
  /** Current width in pixels, already clamped. */
  width: number;
  /** Begin a drag. The handler attaches its own document listeners
   * so the resize keeps tracking even when the cursor leaves the
   * narrow handle strip. */
  beginDrag: (e: React.PointerEvent) => void;
  /** True while a drag is in flight, so the consumer can disable
   * transitions and apply a resize cursor cleanly. */
  dragging: boolean;
};

export const useSidebarWidth = (): SidebarWidthState => {
  const [width, setWidth] = useState<number>(readPersisted);
  const [dragging, setDragging] = useState(false);

  const beginDrag = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault();
      const startX = e.clientX;
      const startWidth = width;
      setDragging(true);

      const onMove = (ev: PointerEvent) => {
        setWidth(clamp(startWidth + (ev.clientX - startX)));
      };
      const onUp = () => {
        setDragging(false);
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
        window.removeEventListener('pointercancel', onUp);
      };
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
      window.addEventListener('pointercancel', onUp);
    },
    [width]
  );

  // Persist on every settled width. Debouncing is unnecessary because
  // the write only happens when `dragging` flips back to false.
  useEffect(() => {
    if (!dragging) {
      writePersisted(width);
      return;
    }
    // Hold the resize cursor across the whole document while the
    // drag is in flight. Without this, leaving the narrow handle
    // strip mid-drag flickers back to the default cursor every time
    // the pointer crosses another element.
    const previousCursor = document.body.style.cursor;
    const previousUserSelect = document.body.style.userSelect;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    return () => {
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousUserSelect;
    };
  }, [dragging, width]);

  // Mirror the live width onto a root CSS variable so other regions
  // (the main chat column) can reserve matching left padding without
  // prop-drilling. The sidebar floats on `position: fixed` at desktop
  // widths so it does not push the main column itself; the variable
  // is the bridge that keeps the two columns visually aligned. The
  // value is wrapped with `px` for direct use in calc() expressions.
  useEffect(() => {
    if (typeof document === 'undefined') {
      return;
    }
    document.documentElement.style.setProperty('--mz-sidebar-width', `${width}px`);
  }, [width]);

  return { width, beginDrag, dragging };
};
