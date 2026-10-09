// kami tui: lay the page out in terminal cells.
//
// One monospace face at one size for all text, so every character Chromium
// places is one cell wide and its line boxes wrap at the terminal's width:
// the snapshot's x coordinates then divide into columns exactly. Bold, italic
// and colour survive; only face and size are forced. Idempotent (it runs from
// addScriptToEvaluateOnNewDocument and again before every probe, since a page
// can drop its own <head>). `kami tui --page-fonts` turns it off.
//
// Same-origin iframes get it too: a script-built `about:blank` frame (Tumblr's
// cookie dialog) never runs a new-document script of its own.
(() => {
  const id = '__kami_tui_css';
  const css =
    '*:not(svg):not(svg *){font-family:monospace!important;font-size:16px!important;' +
    'letter-spacing:0!important;word-spacing:0!important}';
  const put = (doc, depth) => {
    if (!doc || !doc.documentElement || depth > 4) return;
    if (!doc.getElementById(id)) {
      const s = doc.createElement('style');
      s.id = id;
      s.textContent = css;
      (doc.head || doc.documentElement).appendChild(s);
    }
    for (const f of doc.querySelectorAll('iframe')) {
      let inner = null;
      try { inner = f.contentDocument; } catch (e) { /* cross-origin */ }
      put(inner, depth + 1);
    }
  };
  put(document, 0);
  if (!document.documentElement) document.addEventListener('readystatechange', () => put(document, 0));
})();
