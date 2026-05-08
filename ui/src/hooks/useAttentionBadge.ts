import { useEffect } from 'react';
import { useOkiro } from '@/hooks/useOkiro';

// Paints a numeric badge onto the favicon and prefixes the document
// title with `(N)` when one or more background sessions have attention.
// Attention is set by the store on permission request, turn completion,
// or error; the active session never gets it, so the count is always
// "things waiting for the user elsewhere".

const FAVICON_URL = '/favicon.png';
const ICON_SIZE = 64;
const BADGE_COLOUR = '#ef4444'; // Tailwind red-500.
const BADGE_TEXT_COLOUR = '#ffffff';

// Cache the decoded base image so we do not re-fetch on every repaint.
let baseImage: Promise<HTMLImageElement> | null = null;

const loadBase = (): Promise<HTMLImageElement> => {
  if (baseImage) {
    return baseImage;
  }
  baseImage = new Promise((resolve, reject) => {
    const img = new Image();
    img.crossOrigin = 'anonymous';
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error('favicon load failed'));
    img.src = FAVICON_URL;
  });
  return baseImage;
};

const ensureLink = (): HTMLLinkElement => {
  let link = document.querySelector<HTMLLinkElement>('link[rel="icon"]');
  if (!link) {
    link = document.createElement('link');
    link.rel = 'icon';
    link.type = 'image/png';
    document.head.appendChild(link);
  }
  return link;
};

/** Remember the initial icon so we can restore it when count hits zero
 * without re-running the canvas pipeline. */
let baseHref: string | null = null;

const paintBadge = async (count: number) => {
  const link = ensureLink();
  if (baseHref === null) {
    baseHref = link.href || FAVICON_URL;
  }
  if (count <= 0) {
    if (link.href !== baseHref) {
      link.href = baseHref;
    }
    return;
  }
  try {
    const img = await loadBase();
    const canvas = document.createElement('canvas');
    canvas.width = ICON_SIZE;
    canvas.height = ICON_SIZE;
    const ctx = canvas.getContext('2d');
    if (!ctx) {
      return;
    }
    ctx.drawImage(img, 0, 0, ICON_SIZE, ICON_SIZE);

    // Badge: bottom-right circle, roughly 45% of the icon diameter.
    const label = count > 9 ? '9+' : String(count);
    const radius = ICON_SIZE * 0.28;
    const cx = ICON_SIZE - radius - 2;
    const cy = ICON_SIZE - radius - 2;

    ctx.fillStyle = BADGE_COLOUR;
    ctx.beginPath();
    ctx.arc(cx, cy, radius, 0, Math.PI * 2);
    ctx.fill();

    ctx.fillStyle = BADGE_TEXT_COLOUR;
    ctx.font = `bold ${Math.round(radius * 1.2)}px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif`;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(label, cx, cy + 1);

    link.href = canvas.toDataURL('image/png');
  } catch {
    // Favicon painting is best-effort; silently fall back to the base
    // icon if anything goes wrong (image load blocked, canvas tainted,
    // etc.).
  }
};

const BASE_TITLE = 'Okiro!';

const paintTitle = (count: number) => {
  document.title = count > 0 ? `(${count}) ${BASE_TITLE}` : BASE_TITLE;
};

/**
 * Mount once at the app root. Subscribes to the session store and
 * keeps both the favicon badge and the document title in sync with
 * the number of background sessions needing attention.
 */
export const useAttentionBadge = () => {
  const { sessions } = useOkiro();
  const count = sessions.filter((s) => s.attention !== null).length;

  useEffect(() => {
    void paintBadge(count);
    paintTitle(count);
  }, [count]);
};
