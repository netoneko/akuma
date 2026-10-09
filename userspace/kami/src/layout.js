// kami tui: the in-page change counter (see machine.rs, "layout mode").
//
// Returns "<document id>:<version>". The version moves on every DOM mutation,
// scroll, resize, subresource load and web-font load, so the machine can ask
// for a DOMSnapshot only when the page can have changed. The id is new for
// every document, so a navigation always reads as a change. Same-origin
// iframes are watched too (a consent dialog lives in one), picked up as they
// appear: every probe looks for new ones.
(() => {
  const w = window;
  if (!w.__kamiT) w.__kamiT = { v: 0, id: Math.random().toString(36).slice(2, 10) };
  const t = w.__kamiT;
  const bump = () => { t.v++; };
  const watch = (doc, win, depth) => {
    if (!doc || depth > 4) return;
    if (!doc.__kamiWatched) {
      doc.__kamiWatched = true;
      new MutationObserver(bump).observe(doc, {
        subtree: true, childList: true, characterData: true, attributes: true,
      });
      win.addEventListener('scroll', bump, true);
      win.addEventListener('resize', bump);
      win.addEventListener('load', bump, true);
      if (doc.fonts) doc.fonts.addEventListener('loadingdone', bump);
      if (depth > 0) bump();
    }
    for (const f of doc.querySelectorAll('iframe')) {
      try { watch(f.contentDocument, f.contentWindow, depth + 1); } catch (e) { /* cross-origin */ }
    }
  };
  watch(document, w, 0);
  return t.id + ':' + t.v;
})()
