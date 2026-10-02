/* API replay for the static demo: serve /api/* requests from pre-recorded JSON.
   This way pure static hosting like GitHub Pages still runs the **real product frontend**, with no backend needed. */
(() => {
  const IDX_URL = new URL('./index.json', document.currentScript.src).href;
  let idx = null;
  let pending = null;

  const norm = (u) => {
    try {
      const url = new URL(u, location.href);
      const sp = new URLSearchParams(url.search);
      sp.sort();
      const q = sp.toString();
      return url.pathname + (q ? '?' + q : '');
    } catch (e) { return String(u); }
  };

  const jsonResponse = (data) =>
    new Response(JSON.stringify(data), { status: 200, headers: { 'Content-Type': 'application/json' } });

  const loadIdx = async () => {
    if (idx) return idx;
    if (!pending) pending = fetch(IDX_URL).then(r => r.json()).catch(() => ({}));
    idx = await pending;
    return idx;
  };

  const orig = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const req = (input instanceof Request) ? input : new Request(input, init);
    if (req.url.indexOf('/api/') === -1) return orig(input, init);
    const table = await loadIdx();
    const method = (req.method || 'GET').toUpperCase();
    const key = method + ' ' + norm(req.url);
    const base = method + ' ' + new URL(req.url, location.href).pathname;
    let file = null;
    if (method !== 'GET') {
      // Write / query class: prefer exact body match (e.g. different recall queries each return their own result)
      let bodyKey = null;
      try {
        const text = await req.clone().text();
        const obj = JSON.parse(text || '{}');
        bodyKey = key + '|' + JSON.stringify(obj, Object.keys(obj).sort());
      } catch (e) { /* body 不是 JSON，忽略 */ }
      file = (bodyKey && table[bodyKey]) || table[key + '|*'] || table[key];
    } else {
      file = table[key];
    }
    // Fallback ignoring query params: hit the "first recording of the same path" so the page still renders normally
    if (!file) file = table[base];
    if (!file) {
      console.warn('[demo] 未录制的请求（静态 demo 无后端）:', method, norm(req.url));
      return jsonResponse({});
    }
    try {
      const data = await (await fetch(new URL(file, IDX_URL).href)).json();
      return jsonResponse(data);
    } catch (e) {
      return jsonResponse({});
    }
  };
})();
