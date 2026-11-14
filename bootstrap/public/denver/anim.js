(function () {
  var screen = document.getElementById('screen');
  var replay = document.getElementById('replay');
  var reduce = (window.matchMedia && matchMedia('(prefers-reduced-motion: reduce)').matches) || /[?&]fast\b/.test(location.search);
  // Each step: ['cmd', text] is typed at a prompt; ['out', text, delayMs] is printed as one line;
  // ['wait', ms] pauses. Lines wrapped in {ok}..{/} {hl}..{/} {p}..{/} are coloured.
  var script = [
    ['cmd', 'overlays/denver/build-kernel.sh'],
    ['out', '   Compiling akuma-jvm-kernel v0.1.0', 140],
    ['out', '   Compiling denver v0.1.0', 140],
    ['out', '    Finished `release` profile [optimized]', 220],
    ['out', '-rwxr-xr-x  2527744  out/denver', 80],
    ['cmd', 'overlays/denver/build-image.sh'],
    ['out', 'image ok', 420],
    ['out', '-rw-r--r--  671088640  out/denver-clojure.img', 80],
    ['cmd', 'overlays/denver/run.sh'],
    ['out', 'Denver/amd64 -- long mode reached', 200],
    ['out', '  machine: 3071 MiB RAM, 2 virtio-mmio slot(s)', 90],
    ['out', '[Block] Registered vda: 640 MB', 160],
    ['out', '  fs:   ext2 root on /dev/vda', 90],
    ['out', '[SmolNet] static IPv4 10.0.2.15/24 gw 10.0.2.2', 160],
    ['out', '[Denver] init=/opt/jdk/bin/java  clojure.main /opt/server.clj', 500],
    ['out', '{hl}listening on :8080{/}', 300],
    ['wait', 700],
    ['cmd', 'curl http://localhost:8080/', 'host'],
    ['out', 'Hello from Clojure 1.12.0 on Akuma/denver', 120],
    ['out', 'path:   /', 60],
    ['out', 'hit:    1', 60],
    ['out', 'java:   21.0.11 (amd64)', 60],
    ['out', 'cpus:   1', 60],
    ['out', 'heap:   742 MiB max', 60],
    ['wait', 600],
    ['cmd', 'curl http://localhost:8080/json', 'host'],
    ['out', '{"server":"clojure 1.12.0","hits":2,"java":"21.0.11","cpus":1}', 120],
    ['wait', 1800]
  ];
  var MAXLINES = 17, lines = [], gen = 0;
  function esc(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;'); }
  function color(s) {
    return esc(s).replace(/\{ok\}(.*?)\{\/\}/g, '<span class="ok">$1</span>')
                 .replace(/\{hl\}(.*?)\{\/\}/g, '<span class="hl">$1</span>');
  }
  function draw(partial) {
    var shown = lines.slice(-(MAXLINES - (partial === null ? 0 : 1)));
    screen.innerHTML = shown.map(function (l) { return l; }).join('\n') +
      (partial === null ? '' : '\n' + partial + '<span class="caret"></span>');
  }
  function sleep(ms) { return new Promise(function (r) { setTimeout(r, reduce ? 0 : ms); }); }
  async function play() {
    var my = ++gen; lines = []; draw('<span class="p">$</span> ');
    for (var i = 0; i < script.length; i++) {
      if (my !== gen) return;
      var st = script[i];
      if (st[0] === 'cmd') {
        var typed = '';
        for (var c = 0; c < st[1].length; c++) {
          typed += st[1][c];
          draw('<span class="p">$</span> ' + esc(typed));
          if (my !== gen) return;
          await sleep(28);
        }
        await sleep(250);
        lines.push('<span class="p">$</span> ' + esc(st[1])); draw('');
      } else if (st[0] === 'out') {
        await sleep(st[2] || 80); lines.push(color(st[1])); draw('');
      } else { await sleep(st[1]); }
    }
    draw('<span class="p">$</span> ');
  }
  replay.addEventListener('click', play);
  var started = false;
  // Start when the screen is mostly in view. A scroll check (not only IntersectionObserver) so it
  // also starts when the page is driven programmatically or printed.
  function check() {
    if (started) return;
    var r = screen.getBoundingClientRect(), h = window.innerHeight || 720;
    if (r.top < h * 0.85 && r.bottom > h * 0.15) { started = true; play().catch(function (e) { screen.textContent = 'ERR ' + e; }); }
  }
  window.addEventListener('scroll', check, { passive: true });
  window.addEventListener('resize', check);
  draw('<span class="p">$</span> ');
  check();
  // reduced motion (or ?fast): no animation to wait for, so play the transcript out at once
  if (reduce && !started) { started = true; play().catch(function (e) { screen.textContent = 'ERR ' + e; }); }
})();
