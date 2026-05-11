// Minimum viable service worker for PWA installability.
//
// Intentionally does nothing beyond existing: no caching, no offline
// shell, no background sync. If the network is down, the app fails
// to load, same as a normal browser tab would.
//
// This file exists because Chrome's install criteria require a
// registered service worker with a fetch handler. Safari does not
// need one but does no harm. If we ever want a proper offline story,
// that work lives here; for now, keep the surface area tiny.

self.addEventListener('install', () => self.skipWaiting());

self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('fetch', () => {
    // Pass-through. Not calling respondWith() lets the browser handle
    // the request normally. The single listener is what satisfies
    // Chrome's "has a fetch handler" install check.
});
