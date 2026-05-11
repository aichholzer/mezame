import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from '@/App';
import 'highlight.js/styles/github-dark.css';
import 'katex/dist/katex.min.css';
import '@/index.css';

const container = document.getElementById('root');
if (!container) {
  throw new Error('#root not found');
}

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>
);

// PWA installability hook. Registration is deferred to `load` so it
// does not compete with the first render; a failure here does not
// break the app (no offline story depends on it), so we swallow.
// See `ui/public/sw.js` for scope and intent.
if ('serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js').catch(() => {
      // Intentionally silent: the worker exists only to satisfy
      // Chrome's install criteria and there is nothing to retry.
    });
  });
}
