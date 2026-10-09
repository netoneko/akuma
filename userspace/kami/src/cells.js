// kami tui: lay the page out in terminal cells.
//
// One monospace face at one size for all text, so every character Chromium
// places is one cell wide and its line boxes wrap at the terminal's width:
// the snapshot's x coordinates then divide into columns exactly. Bold, italic
// and colour survive; only face and size are forced. Idempotent (it runs from
// addScriptToEvaluateOnNewDocument and again before every probe, since a page
// can drop its own <head>). `kami tui --page-fonts` turns it off.
(() => {
  const id = '__kami_tui_css';
  const put = () => {
    if (document.getElementById(id) || !document.documentElement) return;
    const s = document.createElement('style');
    s.id = id;
    s.textContent =
      '*:not(svg):not(svg *){font-family:monospace!important;font-size:16px!important;' +
      'letter-spacing:0!important;word-spacing:0!important}';
    (document.head || document.documentElement).appendChild(s);
  };
  put();
  if (!document.documentElement) document.addEventListener('readystatechange', put);
})();
