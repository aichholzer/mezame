import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from '@/App';
import { ErrorBoundary } from '@/components/ErrorBoundary';
import { bootTheme } from '@/hooks/useTheme';
// Both families ship inside the bundle, from Fontsource. Nothing is
// fetched from Google: a page load used to send every viewer's address to
// fonts.googleapis.com and fonts.gstatic.com, and the two origins stood in
// the way of a same-origin content security policy.
import '@fontsource-variable/jetbrains-mono/index.css';
import '@fontsource-variable/jetbrains-mono/wght-italic.css';
import '@fontsource/gugi/latin-400.css';
import 'highlight.js/styles/github-dark.css';
import 'katex/dist/katex.min.css';
import '@/index.css';

const container = document.getElementById('root');
if (!container) {
  throw new Error('#root not found');
}

// Apply the persisted theme before the first paint so the app never
// flashes light then snaps to dark once settings hydrate.
bootTheme();

createRoot(container).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>
);

// PWA installability hook. Registration is deferred to `load` so it
// does not compete with the first render. A failure is swallowed; no
// offline story depends on it. See `ui/public/sw.js` for scope and
// intent.
if ('serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js').catch(() => {
      // Intentionally silent: the worker exists only to satisfy
      // Chrome's install criteria and there is nothing to retry.
    });
  });
}
