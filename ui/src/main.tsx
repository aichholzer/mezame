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
