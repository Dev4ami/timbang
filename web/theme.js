// Theme toggle. The initial theme is already set by an inline script in each
// page's <head> (before the stylesheet, so there is no white flash); this file
// only wires the nav button and persists a manual choice.
//
// Precedence: a saved choice in localStorage wins; otherwise follow the OS via
// prefers-color-scheme. A stored value is the user overriding the OS on purpose,
// so it is never cleared here.
(function () {
  const root = document.documentElement;

  function apply(t) {
    root.dataset.theme = t;
  }

  window.addEventListener('DOMContentLoaded', () => {
    const btn = document.getElementById('tema');
    if (!btn) return;

    const sync = () => {
      btn.textContent = root.dataset.theme === 'dark' ? '☀ terang' : '☾ gelap';
    };
    sync();

    btn.addEventListener('click', () => {
      const next = root.dataset.theme === 'dark' ? 'light' : 'dark';
      localStorage.setItem('tema', next);
      apply(next);
      sync();
    });
  });
})();
