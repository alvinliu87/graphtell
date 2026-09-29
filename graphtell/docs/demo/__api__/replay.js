/* 静态 demo 的 API 回放：把 /api/* 请求改从预录的 JSON 读取。
   这样 GitHub Pages 这种纯静态托管上跑的仍是**真产品前端**，无需后端。 */
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
      // 写 / 查询类：优先按 body 精确匹配（例如不同的召回问句各返回各的结果）
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
    // 忽略查询参数的兜底：命中「同路径第一条」录制，保证页面照常渲染
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
