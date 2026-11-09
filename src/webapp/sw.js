// Arr's service worker. It only shows notifications and opens the app when one is tapped;
// it caches nothing and handles no fetches.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

self.addEventListener('push', (e) => {
  let m = {};
  try { m = e.data ? e.data.json() : {}; } catch { /* show the plain notification */ }
  e.waitUntil(self.registration.showNotification(String(m.title || 'Arr'), {
    body: String(m.body || ''),
    icon: '/app/icons/icon-192.png',
    badge: '/app/icons/icon-192.png',
    data: { url: typeof m.url === 'string' && m.url[0] === '/' ? m.url : '/' },
  }));
});

self.addEventListener('notificationclick', (e) => {
  e.notification.close();
  const url = new URL((e.notification.data && e.notification.data.url) || '/', self.location.origin).href;
  e.waitUntil((async () => {
    const open = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const c of open) {
      if (new URL(c.url).origin === self.location.origin && 'focus' in c) {
        await c.focus();
        if ('navigate' in c) { try { await c.navigate(url); } catch { /* the page is not ours any more */ } }
        return;
      }
    }
    await self.clients.openWindow(url);
  })());
});
