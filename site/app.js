// Flip to false when the Windows installer ships.
const WINDOWS_COMING_SOON = true;

(function () {
  const tabs = [].slice.call(document.querySelectorAll('[role=tab]'));
  const panels = { unix: document.getElementById('panel-unix'), win: document.getElementById('panel-win') };

  function select(os, focus) {
    tabs.forEach(function (t) {
      const on = t.dataset.os === os;
      t.setAttribute('aria-selected', on);
      t.tabIndex = on ? 0 : -1;
      if (on && focus) t.focus();
    });
    panels.unix.hidden = os !== 'unix';
    panels.win.hidden = os !== 'win';
  }

  if (!WINDOWS_COMING_SOON) {
    document.querySelectorAll('.pill, .soon-note').forEach(function (e) { e.remove(); });
    const c = document.querySelector('[data-coming-soon]');
    if (c) c.removeAttribute('data-coming-soon');
  }

  tabs.forEach(function (t, i) {
    t.addEventListener('click', function () { select(t.dataset.os); });
    t.addEventListener('keydown', function (e) {
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
        e.preventDefault();
        select(tabs[(i + 1) % tabs.length].dataset.os, true);
      }
    });
  });

  // Default to the visitor's OS; Linux/macOS is the fallback.
  const ua = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || navigator.userAgent || '';
  select(/win/i.test(ua) ? 'win' : 'unix');

  document.querySelectorAll('.copy').forEach(function (btn) {
    btn.hidden = false;
    btn.addEventListener('click', async function () {
      const code = document.getElementById(btn.dataset.target);
      try {
        await navigator.clipboard.writeText(code.textContent.trim());
      } catch (e) {
        const r = document.createRange();
        r.selectNodeContents(code);
        const s = getSelection();
        s.removeAllRanges();
        s.addRange(r);
        try { document.execCommand('copy'); } catch (e2) { return; }
      }
      btn.textContent = 'Copied!';
      btn.classList.add('done');
      setTimeout(function () { btn.textContent = 'Copy'; btn.classList.remove('done'); }, 2000);
    });
  });
})();
