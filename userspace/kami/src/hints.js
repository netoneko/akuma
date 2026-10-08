// kami's in-page helper: link hints and the status line. Evaluated before each
// call (a navigation wipes it), so everything hangs off one guard. All results
// are plain numbers / "x,y,e" strings: kami scans CDP replies, not parses them.
if (!window.__kami) {
  const K = (window.__kami = { items: [], box: null });
  const MAX = 400;
  const SEL =
    'a[href],button,input:not([type=hidden]),select,textarea,summary,' +
    '[role=button],[role=link],[role=checkbox],[role=menuitem],[role=tab],' +
    '[role=option],[onclick],[tabindex]:not([tabindex="-1"]),[contenteditable=""],' +
    '[contenteditable=true]';
  const TEXTY = /^(text|search|email|url|tel|password|number|)$/i;

  // Top-most element at a point, through shadow roots.
  function hitAt(doc, x, y) {
    let e = doc.elementFromPoint(x, y);
    while (e && e.shadowRoot) {
      const inner = e.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === e) break;
      e = inner;
    }
    return e;
  }

  function editable(el) {
    if (el.isContentEditable) return true;
    const t = el.tagName;
    if (t === 'TEXTAREA') return true;
    if (t === 'INPUT') return TEXTY.test(el.getAttribute('type') || '');
    return false;
  }

  K.collect = function () {
    K.clear();
    const vw = innerWidth, vh = innerHeight, seen = new Set(), out = [];
    // Why candidates were turned away, for `__kami.why` when nothing is found.
    const st = (K.stats = { vw, vh, matched: 0, small: 0, off: 0, hidden: 0, dup: 0, url: location.href, state: document.readyState });
    function consider(el, ox, oy, doc) {
      if (out.length >= MAX) return;
      st.matched++;
      const r = el.getBoundingClientRect();
      if (r.width < 4 || r.height < 4) { st.small++; return; }
      const x0 = Math.max(ox + r.left, 0), y0 = Math.max(oy + r.top, 0);
      const x1 = Math.min(ox + r.right, vw), y1 = Math.min(oy + r.bottom, vh);
      if (x1 - x0 < 4 || y1 - y0 < 4) {
        st.off++;
        st.rect = [r.left, r.top, r.right, r.bottom, ox, oy].map(Math.round);
        return;
      }
      const x = (x0 + x1) / 2, y = (y0 + y1) / 2;
      const hit = hitAt(doc, x - ox, y - oy);
      if (!hit || !(hit === el || el.contains(hit) || hit.contains(el))) {
        st.hidden++;
        st.lastHit = hit ? hit.tagName : 'null';
        return;
      }
      const key = Math.round(x / 3) + ',' + Math.round(y / 3);
      if (seen.has(key)) { st.dup++; return; }
      seen.add(key);
      out.push({ el, x, y, lx: x0, ly: y0 });
    }
    function walk(root, ox, oy, doc) {
      let all;
      try { all = root.querySelectorAll('*'); } catch (e) { return; }
      for (const el of all) {
        if (el.matches(SEL)) {
          consider(el, ox, oy, doc);
        } else if (out.length < MAX && el.children.length === 0 || el.tagName === 'IMG') {
          // Script-handled "buttons": leaf nodes whose cursor says clickable,
          // and whose parent's does not (so a link's text is not a second hint).
          const p = el.parentElement;
          if (getComputedStyle(el).cursor === 'pointer' &&
              !(p && getComputedStyle(p).cursor === 'pointer')) consider(el, ox, oy, doc);
        }
        if (el.shadowRoot) walk(el.shadowRoot, ox, oy, doc);
        if (el.tagName === 'IFRAME') {
          try {
            const d = el.contentDocument;
            if (d) { const r = el.getBoundingClientRect(); walk(d, ox + r.left, oy + r.top, d); }
          } catch (e) { /* cross-origin: no hints inside */ }
        }
      }
    }
    walk(document, 0, 0, document);
    out.sort((a, b) => a.ly - b.ly || a.lx - b.lx);
    K.items = out;
    return out.length;
  };

  K.why = function () { return JSON.stringify(K.stats || {}); };

  K.draw = function (labels) {
    const box = document.createElement('div');
    box.style.cssText = 'position:fixed;left:0;top:0;width:0;height:0;z-index:2147483647;pointer-events:none';
    K.items.forEach((it, i) => {
      const d = document.createElement('div');
      d.textContent = labels[i].toUpperCase();
      d.style.cssText =
        'position:fixed;pointer-events:none;background:#ffd54a;color:#000;border:1px solid #b8860b;' +
        'border-radius:3px;padding:1px 3px;font:bold 14px/1.1 monospace;box-shadow:0 1px 3px #0008;' +
        'left:' + Math.min(it.lx, innerWidth - 40) + 'px;top:' + Math.min(it.ly, innerHeight - 20) + 'px';
      it.label = labels[i];
      it.tag = d;
      box.appendChild(d);
    });
    document.documentElement.appendChild(box);
    K.box = box;
    return K.items.length;
  };

  K.filter = function (prefix) {
    let n = 0;
    for (const it of K.items) {
      const on = it.label.startsWith(prefix);
      it.tag.style.display = on ? '' : 'none';
      if (on) n++;
    }
    return n;
  };

  // Drop the overlay and report where to click: "x,y,1" if it is a text field.
  K.click = function (i) {
    const it = K.items[i];
    K.clear();
    if (!it) return '';
    if (!it.el.isConnected) return '';
    return it.x + ',' + it.y + ',' + (editable(it.el) ? 1 : 0);
  };

  K.clear = function () {
    if (K.box) K.box.remove();
    K.box = null;
    K.items = [];
    return 0;
  };

  K.blur = function () {
    const a = document.activeElement;
    if (a && a.blur) a.blur();
    return 0;
  };
}
