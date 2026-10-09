// kami tui: the in-page change counter (see machine.rs, "layout mode").
//
// Returns "<document id>:<version>". The version moves on every DOM mutation,
// scroll, resize, subresource load and web-font load, so the machine can ask
// for a DOMSnapshot only when the page can have changed. The id is new for
// every document, so a navigation always reads as a change.
(() => {
  const w = window;
  if (!w.__kamiT) {
    const t = (w.__kamiT = { v: 0, id: Math.random().toString(36).slice(2, 10) });
    const bump = () => { t.v++; };
    new MutationObserver(bump).observe(document, {
      subtree: true, childList: true, characterData: true, attributes: true,
    });
    addEventListener('scroll', bump, true);
    addEventListener('resize', bump);
    addEventListener('load', bump, true);
    if (document.fonts) document.fonts.addEventListener('loadingdone', bump);
  }
  return w.__kamiT.id + ':' + w.__kamiT.v;
})()
