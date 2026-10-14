(() => {
  "use strict";
  const WS = ["what", "who", "when", "where", "why", "how"];
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;"}[c]));
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const p2 = (n) => String(n).padStart(2, "0");
  const hms = (d) => `${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}`;
  const when = (ms) => { if (!ms) return "—"; const d = new Date(ms); return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}, ${hms(d)}`; };
  const short = (ms) => { if (!ms) return "—"; const d = new Date(ms); return `${d.getDate()} ${MONTHS[d.getMonth()]}, ${p2(d.getHours())}:${p2(d.getMinutes())}`; };
  const fmt = (n) => n == null ? "—" : Number(n).toLocaleString("en-US");
  const kfmt = (n) => n == null ? "—" : n >= 1e6 ? (n / 1e6).toFixed(2) + "M" : n >= 1e3 ? (n / 1e3).toFixed(1) + "k" : String(n);
  // A memory share is usually tiny: show two significant digits, not "0%".
  const pct = (x) => x == null ? "—" : x === 0 ? "0%" : x >= 0.1 ? (x * 100).toFixed(0) + "%" : (x * 100).toPrecision(2) + "%";
  const usd = (x) => x ? "$" + x.toFixed(2) : "";
  const cap = (s) => s ? s[0].toUpperCase() + s.slice(1) : "";
  let key = ""; try { key = sessionStorage.getItem("mnemo_key") || ""; } catch {}

  // X-Mnemo-UI: with a browser session, the server only accepts writes that carry it (CSRF).
  async function api(path, body, method) {
    const r = await fetch(path, {method: method || (body ? "POST" : "GET"), credentials: "same-origin",
      headers: {"content-type": "application/json", "x-mnemo-ui": "1", ...(key ? {authorization: "Bearer " + key} : {})}, body: body ? JSON.stringify(body) : undefined});
    if (r.status === 401) { showLanding(); throw new Error("needs sign-in"); }
    const j = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(j.error?.message || `The server answered ${r.status}`);
    return j;
  }
  const table = (cols, rows, empty = "Nothing yet.") => rows.length ? `<table><thead><tr>${cols.map((c) => `<th class="${c[2] || ""}">${c[0]}</th>`).join("")}</tr></thead><tbody>${rows.map((r) => `<tr>${cols.map((c) => `<td class="${c[2] || ""}">${c[1](r)}</td>`).join("")}</tr>`).join("")}</tbody></table>` : `<p class="muted">${empty}</p>`;

  // "" follows the system theme; theme.js sets data-theme before the first paint.
  const THEMES = [["", "i-auto", "System theme"], ["light", "i-sun", "Light theme"], ["dark", "i-moon", "Dark theme"]];
  let theme = document.documentElement.dataset.theme || "";
  function paintTheme() {
    const [, icon, label] = THEMES.find((t) => t[0] === theme);
    $("theme").innerHTML = `<svg class="ic"><use href="#${icon}"/></svg><span>${label}</span>`;
  }
  $("theme").onclick = () => {
    theme = THEMES[(THEMES.findIndex((t) => t[0] === theme) + 1) % THEMES.length][0];
    if (theme) document.documentElement.dataset.theme = theme; else delete document.documentElement.dataset.theme;
    try { theme ? localStorage.setItem("mnemo_theme", theme) : localStorage.removeItem("mnemo_theme"); } catch {}
    paintTheme(); if (view === "usage") loadCost();
  };

  let me = null, authCfg = {};
  function showLanding(msg) {
    $("shell").hidden = true; $("landing").hidden = false;
    $("keyerr").textContent = msg || "";
    $("sso").hidden = $("sso-top").hidden = !authCfg.oidc;
    const keys = authCfg.keys !== false;
    $("usekey").hidden = !keys || !authCfg.oidc;
    $("keyform").hidden = !keys || !!authCfg.oidc;
    $("cmd").textContent = `mnemo login ${location.origin}`;
  }
  $("usekey").onclick = () => { $("keyform").hidden = false; $("usekey").hidden = true; $("key").focus(); };
  $("keyform").onsubmit = (e) => { e.preventDefault(); key = $("key").value.trim(); try { sessionStorage.setItem("mnemo_key", key); } catch {} boot(); };
  $("logout").onclick = async () => {
    if (me?.via === "session") { try { await api("auth/logout", {}, "POST"); } catch {} }
    key = ""; try { sessionStorage.removeItem("mnemo_key"); } catch {}
    location.hash = ""; location.reload();
  };

  // Delegated: a trusted front end serves only the memory, under its own login.
  const delegated = () => me?.via === "delegated";
  async function boot() {
    // whoami first: a delegating front end does not pass auth/* through.
    try { me = await api("v1/whoami"); }
    catch (e) {
      try { authCfg = await (await fetch("auth/config")).json(); } catch {}
      showLanding(key ? "That key was not accepted. Check it or sign in with SSO." : e.message === "needs sign-in" ? "" : `mnemo did not answer: ${e.message}. Try again in a moment.`); return;
    }
    if (!delegated()) { try { authCfg = await (await fetch("auth/config")).json(); } catch {} }
    $("landing").hidden = true; $("shell").hidden = false;
    $("navaccount").hidden = $("logout").hidden = delegated();
    const name = me.name || me.email || me.subject || "?";
    $("whoname").textContent = name + (me.kind === "admin" ? " (admin)" : "");
    $("whoname").title = [me.email, me.subject].filter(Boolean).join("\n");
    $("avatar").textContent = name.split(/[\s@.]+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join("");
    $("navadmin").hidden = me.kind !== "admin";
    for (const a of document.querySelectorAll("[data-keys]")) a.hidden = authCfg.keys === false;
    $("csubject").hidden = me.kind !== "admin";
    $("kbd").textContent = /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘K" : "Ctrl K";
    paintTheme(); route(); refreshStats(); setInterval(refreshStats, 60_000);
    const q0 = new URLSearchParams(location.search).get("q");
    if (q0) { $("q").value = q0; $("ask").requestSubmit(); }
  }

  const CRUMBS = {search: ["Memory", "Search"], map: ["Memory", "Map"], usage: ["Account", "Usage"], connections: ["Account", "Connections"], keys: ["Account", "Keys"],
    "admin/overview": ["Admin", "Status"], "admin/people": ["Admin", "People"], "admin/sessions": ["Admin", "Sessions"], "admin/keys": ["Admin", "All keys"], "admin/forget": ["Admin", "Delete a person"]};
  let view = "map", globeLoaded = false;
  window.addEventListener("hashchange", () => me && route());
  function route() {
    let v = location.hash.slice(1) || "map";
    if (v === "admin") v = "admin/overview";
    if (!CRUMBS[v] || (v.startsWith("admin/") && me.kind !== "admin")) v = "map";
    if (delegated() && v !== "search" && v !== "map") v = "map";
    view = v;
    for (const a of document.querySelectorAll(".side .nav")) a.classList.toggle("on", a.dataset.v === v);
    $("crumbs").innerHTML = `${CRUMBS[v][0]} / <b>${CRUMBS[v][1]}</b>`;
    $("vsearch").hidden = v !== "search"; $("vmap").hidden = v !== "map"; $("vusage").hidden = v !== "usage"; $("vkeys").hidden = v !== "keys"; $("vconn").hidden = v !== "connections"; $("vadmin").hidden = !v.startsWith("admin/");
    $("side").classList.remove("open");
    if (v === "map") { if (!globeLoaded) { globeLoaded = true; startMap(); } requestAnimationFrame(resize); }
    if (v === "search" && !lastQuery) searchEmpty();
    if (v === "usage") loadCost();
    if (v === "keys") loadKeys();
    if (v === "connections") loadConnections();
    if (v.startsWith("admin/")) loadAdmin(v.slice(6));
  }
  $("menu").onclick = () => $("side").classList.toggle("open");
  document.addEventListener("click", (e) => { if ($("side").classList.contains("open") && !$("side").contains(e.target) && !$("menu").contains(e.target)) $("side").classList.remove("open"); });
  document.addEventListener("keydown", (e) => {
    if ((e.key === "k" && (e.metaKey || e.ctrlKey)) || (e.key === "/" && !/INPUT|TEXTAREA|SELECT/.test(document.activeElement?.tagName))) { e.preventDefault(); $("q").focus(); $("q").select(); }
    if (e.key === "Escape" && !$("drawer").hidden) closeDrawer();
  });

  async function refreshStats() {
    try {
      const st = await api("v1/stats");
      const live = st.live ?? st.globe?.memories ?? 0;
      $("nmem").textContent = live ? kfmt(live) : "";
      const d = st.distill || {};
      $("distill").hidden = !d.paused;
      const what = {model: "The extraction model", embedder: "The embedding model", judge: "The judge"}[d.waiting_for] || "A model";
      $("distillwhy").textContent = `${what} is not responding. New sessions are stored and will be distilled when it is back.`;
      return st;
    } catch { return null; }
  }

  function sixW(m) {
    const names = (roles) => (m.links || []).filter((l) => roles.includes(l.role) && l.subrole !== "context");
    return {who: names(["who"]), where: names(["where", "about"]), context: (m.links || []).filter((l) => l.subrole === "context"),
      when: m.occurred_from ? when(m.occurred_from) + (m.occurred_to && m.occurred_to - m.occurred_from > 60_000 ? " to " + when(m.occurred_to) : "") : ""};
  }
  function state(m) {
    if (m.valid_to != null && Date.now() >= m.valid_to) return m.superseded_by ? ["old", "Replaced"] : ["old", "No longer true"];
    return ["ok", "Current"];
  }
  const entPill = (l) => `<i class="pill ent" data-ent="${esc(l.entity)}">${esc(l.name || l.entity)}</i>`;
  function row(m, sel) {
    const [cls, label] = state(m), ents = (m.links || []).filter((l) => l.subrole !== "context").slice(0, 3);
    return `<button type="button" class="res ${cls === "old" ? "old" : ""} ${sel ? "sel" : ""}" data-mem="${esc(m.id)}"><span class="t">${esc(m.what)}</span><span class="pill ${cls}">${label}</span>
      <span class="s"><span>${esc(cap(m.kind))}</span><span>${short(m.occurred_from || m.known_at)}</span>${ents.map(entPill).join("")}</span></button>`;
  }
  function detailHTML(m, history, asked) {
    const six = sixW(m), [cls, label] = state(m);
    const f = (k, v) => v ? `<dt>${cap(k)}</dt><dd class="${k === asked ? "ask" : ""}">${v}</dd>` : "";
    const missing = ["who", "why", "how"].filter((k) => !(k === "who" ? six.who.length : m[k]));
    const recorded = !m.occurred_from || Math.abs(m.known_at - m.occurred_from) > 60_000;
    const judged = m.judge && m.judge !== "heuristic";
    const conf = m.confidence != null ? `<div class="sec"><h3>Confidence</h3>${judged
      ? `<div class="meter"><progress max="1" value="${Number(m.confidence) || 0}"></progress>${(Number(m.confidence) || 0).toFixed(2)}, judged by ${esc(m.judge)}</div>`
      : `<p class="muted">Not judged by a model: it was stored with the default score when the judge did not answer.</p>`}</div>` : "";
    const hist = (history || []).length ? `<div class="sec"><h3>History</h3>${history.map((h) => `<button type="button" class="ver ${h.live ? "" : "old"} ${h.id === m.id ? "this" : ""}" data-mem="${esc(h.id)}"><i></i><span>${esc(h.what)}</span><time>${when(h.valid_from)}</time></button>`).join("")}</div>` : "";
    const srcs = (m.source_episodes || []).slice(0, 6);
    return `<div class="facts"><span class="pill ${cls}">${label}</span><span>${esc(cap(m.kind))}</span></div>
      <h2>${esc(m.what)}</h2>
      <dl class="fields">${f("who", six.who.map(entPill).join(""))}${f("when", esc(six.when))}${recorded ? f("recorded", esc(when(m.known_at))) : ""}${f("where", six.where.map(entPill).join(""))}
        ${f("why", esc(m.why || ""))}${f("how", esc(m.how || ""))}${six.context.length ? `<dt>Context</dt><dd class="none">${six.context.map(entPill).join("")}</dd>` : ""}</dl>
      ${missing.length ? `<p class="muted gap">The conversation it came from did not say ${missing.map((k) => k === "who" ? "who was involved" : k).join(missing.length > 2 ? ", " : " or ").replace(/, ([^,]*)$/, " or $1")}.</p>` : ""}
      ${conf}${hist}
      ${srcs.length ? `<div class="sec"><h3>Sources</h3><div class="src">${srcs.map((e) => `<button type="button" data-ep="${esc(e)}">${esc(e.length > 14 ? e.slice(0, 12) + "…" : e)}</button>`).join("")}</div><div id="eptext"></div></div>` : ""}`;
  }
  async function openMemory(id, target, asked) {
    try {
      const r = await api("v1/memories/" + encodeURIComponent(id));
      target.innerHTML = `<div class="detail">${detailHTML(r.memory, r.history, asked)}</div>`;
      return r.memory;
    } catch (e) { target.innerHTML = `<div class="detail"><p class="err">${esc(e.message)}</p></div>`; return null; }
  }
  function wire(container, onMem) {
    container.addEventListener("click", async (e) => {
      const ep = e.target.closest("[data-ep]");
      if (ep) {
        const box = container.querySelector("#eptext");
        try { const r = await api("v1/episodes/" + encodeURIComponent(ep.dataset.ep) + "?around=0"); const x = (r.episodes || []).find((y) => y.episode_id === ep.dataset.ep) || (r.episodes || [])[0] || {}; if (box) box.innerHTML = `<div class="raw"><small>${when(x.occurred_at)} ${esc(x.role || "")}</small>${esc(x.text || "")}</div>`; }
        catch (err) { if (box) box.innerHTML = `<p class="err">${esc(err.message)}</p>`; }
        return;
      }
      const ent = e.target.closest("[data-ent]");
      if (ent) { e.stopPropagation(); showEntity(ent.dataset.ent); return; }
      const mem = e.target.closest("[data-mem]");
      if (mem) onMem(mem.dataset.mem, mem);
    });
  }

  let lastQuery = "", lastW = "";
  function searchEmpty() {
    $("qtitle").textContent = "Search"; $("parsed").innerHTML = "";
    const ex = ["Why did we choose ClickHouse?", "Who decided the deploy process?", "What happened to the login last week?"];
    $("vsearch").querySelector(".split").classList.add("solo");
    $("results").innerHTML = `<div class="empty"><h2>Ask in your own words</h2><p>mnemo reads what you ask for (why, who, when, where, how) and which things you mention, and answers from your memories.</p><ul>${ex.map((q) => `<li><button type="button" data-try="${esc(q)}">${esc(q)}</button></li>`).join("")}</ul></div>`;
    $("detail").innerHTML = "";
  }
  $("results").addEventListener("click", (e) => { const t = e.target.closest("[data-try]"); if (t) { $("q").value = t.dataset.try; $("ask").requestSubmit(); } });
  wire($("results"), async (id, el) => {
    for (const b of $("results").querySelectorAll(".res")) b.classList.toggle("sel", b === el);
    const m = await openMemory(id, $("detail"), lastW);
    if (m) { hi = {ents: new Set((m.links || []).map((l) => l.entity)), mems: new Set([id]), query: new Set()}; schedule(); }
  });
  wire($("detail"), async (id) => { await openMemory(id, $("detail"), lastW); });
  $("ask").onsubmit = async (e) => {
    e.preventDefault(); const q = $("q").value.trim(); if (!q) return;
    lastQuery = q; if (location.hash !== "#search") location.hash = "search";
    $("qtitle").textContent = q; $("parsed").innerHTML = `<span>Searching…</span>`;
    try {
      const r = await api("v1/recall", {query: q, limit: 12, budget_tokens: 2000});
      lastW = r.parsed.w || "";
      const q2 = new Set((r.parsed.entities || []).map((x) => x.id));
      hi = {ents: new Set([...q2, ...r.items.flatMap((m) => (m.links || []).map((l) => l.entity))]), mems: new Set(r.items.map((m) => m.id)), query: q2, label: q};
      $("parsed").innerHTML = (lastW ? `<span class="pill acc">Asks ${esc(lastW)}</span>` : "") + (r.parsed.entities || []).map((x) => entPill({entity: x.id, name: x.name, kind: x.kind})).join("")
        + `<span>${r.items.length} ${r.items.length === 1 ? "memory" : "memories"} in ${r.fast_ms.toFixed(1)} ms</span>`;
      const raw = (r.raw || []).map((x) => `<div class="raw"><small>Original record, ${when(x.occurred_at)} ${esc(x.role || "")}</small><div class="more">${esc(x.text)}</div></div>`).join("");
      $("results").innerHTML = r.items.length || raw
        ? r.items.map((m, i) => row(m, i === 0)).join("") + (raw ? `<h3>From the original conversations</h3>${raw}` : "")
        : `<div class="empty"><h2>Nothing you can see answers this yet</h2><p>Try other words, or name the system or person it is about.</p></div>`;
      $("detail").innerHTML = "";
      $("vsearch").querySelector(".split").classList.toggle("solo", !r.items.length);
      if (r.items[0]) openMemory(r.items[0].id, $("detail"), lastW);
      paintMapQuery(); schedule();
    } catch (err) { $("results").innerHTML = `<div class="empty"><p class="err">${esc(err.message)}</p></div>`; $("parsed").innerHTML = ""; }
  };

  $("creload").onclick = () => loadCost();
  $("cdays").onchange = () => loadCost();
  async function loadCost() {
    const q = new URLSearchParams({days: $("cdays").value}); if ($("csubject").value.trim()) q.set("subject", $("csubject").value.trim());
    let r; try { r = await api("v1/cost?" + q); } catch (e) { $("ctiles").innerHTML = `<p class="err">${esc(e.message)}</p>`; return; }
    const inj = r.injected, val = r.value, ses = r.sessions, bg = r.background;
    const tiles = [
      ["Useful when injected", pct(val.useful_rate), `${fmt(val.useful_blocks)} of ${fmt(val.judged_blocks)} judged blocks` + (val.unjudged_blocks ? `, ${fmt(val.unjudged_blocks)} not judged` : "")],
      ["Injected", kfmt(inj.tokens) + " tokens", `${fmt(inj.blocks)} blocks ${usd(inj.usd)}`],
      ["Share of session input", pct(ses.memory_share), `${kfmt(ses.input + ses.cache_read + ses.cache_creation)} tokens in ${fmt(ses.turns)} turns`],
      ["Tokens per useful block", kfmt(val.tokens_per_useful_block), "Injected divided by the blocks that helped"],
      ["Background work", kfmt(bg.tokens) + " tokens", usd(bg.usd) || "Distilling, judging and embedding"],
      ["Saved by not repeating", kfmt(inj.repeated_tokens_saved) + " tokens", `${fmt(inj.repeated_skipped)} memories already in context`],
      ["Muted", fmt(inj.muted_skipped), "Skipped because they never helped"],
      ["Budget stops", fmt(inj.budget_skips), "Sessions that reached their cap"],
    ];
    $("ctiles").innerHTML = tiles.map(([k, v, sub]) => `<div class="kpi"><span>${k}</span><b>${esc(v)}</b><small>${esc(sub)}</small></div>`).join("");
    drawBars(r.daily);
    $("csurf").innerHTML = table([["Surface", (x) => esc(x[0])], ["Blocks", (x) => fmt(x[1].blocks), "n"], ["Tokens", (x) => fmt(x[1].tokens), "n"]], Object.entries(inj.by_surface || {}));
    $("cbg").innerHTML = table([["Kind", (x) => esc(x[0])], ["Calls", (x) => fmt(x[1].calls) + (x[1].failed ? ` <span class="warn">(${x[1].failed} failed)</span>` : ""), "n"], ["Prompt", (x) => fmt(x[1].prompt), "n"], ["Cached", (x) => fmt(x[1].cached), "n"], ["Output", (x) => fmt(x[1].completion), "n"], ["Cost", (x) => usd(x[1].usd) || "—", "n"], ["Models", (x) => esc((x[1].models || []).join(", "))]], Object.entries(bg.by_kind || {}));
    $("cmems").innerHTML = table([["Memory", (m) => esc(m.what || m.id) + (m.muted ? ` <span class="pill warn">Muted</span>` : "")], ["Injected", (m) => fmt(m.injected), "n"], ["Judged", (m) => fmt(m.judged), "n"], ["Used", (m) => fmt(m.used), "n"]], r.memories || []);
  }
  // viewBox = real pixels, so text keeps its size instead of stretching with the screen.
  let lastDays = [];
  const axis = (v) => v >= 1e6 ? +(v / 1e6).toFixed(1) + "M" : v >= 1e3 ? +(v / 1e3).toFixed(1) + "k" : String(v);
  function drawBars(days) {
    lastDays = days || [];
    const svg = $("cbars"), w = Math.max(320, Math.round(svg.clientWidth || 800)), h = 200;
    const left = 48, right = 8, top = 12, bottom = 26, ph = h - top - bottom, n = lastDays.length || 1, slot = (w - left - right) / n;
    const bw = Math.max(4, Math.min(44, slot * .62));
    const raw = Math.max(1, ...lastDays.map((d) => d.injected_tokens + d.background_tokens));
    // A round scale: 1, 2 or 5 times a power of ten, with three lines.
    const step = (() => { const t = raw / 2, p = 10 ** Math.floor(Math.log10(t)); return [1, 2, 5, 10].map((m) => m * p).find((v) => v >= t); })();
    const max = step * 2;
    const cs = getComputedStyle(document.documentElement), c1 = cs.getPropertyValue("--accent").trim(), c2 = cs.getPropertyValue("--k-system").trim(), cl = cs.getPropertyValue("--muted").trim(), cg = cs.getPropertyValue("--line").trim();
    const font = 'font-family="Geist, system-ui, sans-serif" font-size="12"';
    const y = (v) => top + ph - (v / max) * ph;
    svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
    let out = "";
    for (const v of [0, step, max]) out += `<line x1="${left}" x2="${w - right}" y1="${y(v)}" y2="${y(v)}" stroke="${cg}"${v ? ' stroke-dasharray="3 4"' : ""}/><text x="${left - 8}" y="${y(v) + 4}" text-anchor="end" fill="${cl}" ${font}>${axis(v)}</text>`;
    const every = Math.ceil(n / Math.max(1, Math.floor((w - left) / 64)));
    lastDays.forEach((d, i) => {
      const cx = left + slot * (i + .5), x = cx - bw / 2, h1 = (d.injected_tokens / max) * ph, h2 = (d.background_tokens / max) * ph;
      if (h1 > 0) out += `<rect x="${x}" y="${top + ph - h1}" width="${bw}" height="${h1}" fill="${c1}" rx="2"><title>${esc(d.day)}: ${fmt(d.injected_tokens)} tokens injected</title></rect>`;
      if (h2 > 0) out += `<rect x="${x}" y="${top + ph - h1 - h2}" width="${bw}" height="${h2}" fill="${c2}" rx="2"><title>${esc(d.day)}: ${fmt(d.background_tokens)} tokens of background work</title></rect>`;
      if (i % every === 0) { const [yy, mm, dd] = String(d.day).split("-").map(Number); out += `<text x="${cx}" y="${h - 8}" text-anchor="middle" fill="${cl}" ${font}>${dd ? `${dd} ${MONTHS[mm - 1]}` : esc(d.day)}</text>`; }
    });
    svg.innerHTML = out;
    $("clegend").innerHTML = `<span><i class="sw-accent"></i>Injected into sessions</span><span><i class="sw-system"></i>Background work</span>`;
  }
  window.addEventListener("resize", () => { if (view === "usage" && lastDays.length) drawBars(lastDays); });

  // Metadata clients (CIMD) have a URL as their id: name them by its domain.
  const APPS = {"claude-mnemo": "Claude.ai", "mnemo-cli": "Claude Code (mnemo login)"};
  const HOSTS = {"grok.com": "Grok", "claude.ai": "Claude.ai", "chatgpt.com": "ChatGPT", "x.ai": "Grok"};
  function appName(id) {
    if (APPS[id]) return APPS[id];
    try { const h = new URL(id).hostname.replace(/^www\./, ""); return HOSTS[h] || h; } catch { return id; }
  }
  const appCell = (c) => `<b>${esc(appName(c.client))}</b><br><span class="muted">${esc(c.client)}</span>`;
  const agentCell = (c) => `<span class="muted">${esc((c.agent || "—").slice(0, 70))}</span>`;
  const stateCell = (c) => c.blocked ? `<span class="pill warn">Disconnected</span>` : `<span class="pill ok">Connected</span>`;
  async function loadConnections() {
    let r; try { r = await api("v1/me/connections"); } catch (e) { $("capps").innerHTML = `<p class="err">${esc(e.message)}</p>`; return; }
    $("capps").innerHTML = table([["App", appCell], ["Last used", (c) => when(c.last_ms)], ["First seen", (c) => when(c.first_ms)], ["Client", agentCell], ["State", stateCell],
      ["", (c) => `<button class="btn" type="button" data-client="${esc(c.client)}" data-block="${c.blocked ? "0" : "1"}">${c.blocked ? "Allow again" : "Disconnect"}</button>`]],
      r.apps, "No app has connected yet. Claude Code, Claude.ai and Grok appear here after their first sign-in.");
    $("csess").innerHTML = table([["Signed in", (s) => when(s.created_ms)], ["Expires", (s) => when(s.expires_ms)], ["", (s) => s.current ? `<span class="pill acc">This browser</span>` : ""],
      ["", (s) => `<button class="btn" type="button" data-session="${esc(s.id)}">Sign out</button>`]], r.sessions, "No browser sessions.");
  }
  $("vconn").addEventListener("click", async (e) => {
    const b = e.target.closest("button"); if (!b) return;
    try {
      if (b.dataset.client != null) {
        const block = b.dataset.block === "1";
        if (block && !confirm(`Disconnect ${appName(b.dataset.client)}? It stops working until you allow it again.`)) return;
        await api("v1/me/connections", {client: b.dataset.client, blocked: block});
      } else if (b.dataset.session) {
        await api("v1/me/sessions/" + encodeURIComponent(b.dataset.session), null, "DELETE");
        if (b.closest("tr")?.textContent.includes("This browser")) { location.reload(); return; }
      }
      loadConnections();
    } catch (err) { $("capps").insertAdjacentHTML("afterbegin", `<p class="err">${esc(err.message)}</p>`); }
  });

  $("kform").onsubmit = async (e) => {
    e.preventDefault();
    try {
      const r = await api("v1/me/keys", {name: $("kname").value, days: Number($("kdays").value)});
      $("knew").innerHTML = `<p class="ok">Key created. Copy it now: it is not shown again.</p><div class="secret">${esc(r.key)}</div><p class="muted">For Claude Code: <code>export MNEMO_URL=${esc(location.origin)} MNEMO_KEY=${esc(r.key)}</code></p>`;
      $("kname").value = ""; loadKeys(false);
    } catch (err) { $("knew").innerHTML = `<p class="err">${esc(err.message)}</p>`; }
  };
  async function loadKeys(clear = true) {
    if (clear) $("knew").innerHTML = "";
    $("kform").hidden = me.via !== "session";
    if ($("kform").hidden) $("kwhy").textContent = "Sign in with SSO to create your own keys.";
    let r; try { r = await api("v1/me/keys"); } catch (e) { $("klist").innerHTML = `<p class="err">${esc(e.message)}</p>`; return; }
    $("klist").innerHTML = table([["Name", (k) => esc(k.name)], ["Created", (k) => when(k.created_ms)], ["Expires", (k) => k.expires_ms ? when(k.expires_ms) : "Never"], ["Last used", (k) => when(k.last_used_ms)],
      ["", (k) => k.revoked ? `<span class="pill old">Revoked</span>` : `<button class="btn" type="button" data-revoke="${esc(k.id)}">Revoke</button>`]], r.keys, "You have no keys yet.");
    for (const b of $("klist").querySelectorAll("[data-revoke]")) b.onclick = async () => { if (confirm("Revoke this key? Anything using it stops working.")) { await api("v1/me/keys/" + b.dataset.revoke, null, "DELETE"); loadKeys(false); } };
  }

  async function loadAdmin(sub) {
    const body = $("abody"); body.innerHTML = `<p class="muted">Loading…</p>`;
    const head = (t, extra = "") => `<div class="vhead"><h1>${t}</h1>${extra}</div>`;
    try {
      if (sub === "overview") {
        const r = await api("v1/admin/overview"), c = r.config || {}, st = r.stats || {};
        const kv = (o) => `<table>${Object.entries(o).map(([k, v]) => `<tr><th>${esc(k)}</th><td>${esc(typeof v === "object" && v !== null ? JSON.stringify(v) : v)}</td></tr>`).join("")}</table>`;
        body.innerHTML = head("Status") + `<div class="kpis">${[["Current memories", fmt(st.live)], ["Entities", fmt(st.entities)], ["Raw episodes", fmt(st.episodes)], ["Threads to distill", fmt(st.pending_threads)], ["Muted", fmt(r.muted)]].map(([k, v]) => `<div class="kpi"><span>${k}</span><b>${v}</b></div>`).join("")}</div>
          <div class="panel"><h2>Vectors</h2><p class="muted">Search by meaning uses one vector per memory. They are recalculated by themselves when the embedding model changes; recalculate them by hand if the same model now runs on another server.</p><button class="btn" type="button" id="reembed">Recalculate all vectors</button> <span id="reembedout" class="muted"></span></div>
          <div class="panel"><h2>Writer</h2>${r.writer ? `${esc(r.writer.instance)}: ${r.writer.alive ? `<span class="pill ok">Alive</span>` : `<span class="pill warn">Lease expired</span>`} until ${when(r.writer.until_ms)}` : "—"}</div>
          <div class="panel"><h2>Configuration, from the environment (version ${esc(r.version)})</h2>${kv(c)}</div>`;
        $("reembed").onclick = async () => {
          if (!confirm("Recalculate the vectors of every memory? Search by meaning may be incomplete for a few minutes.")) return;
          try { const x = await api("v1/admin/reembed", {}); $("reembedout").textContent = `Scheduled for ${fmt(x.memories)} memories.`; } catch (err) { $("reembedout").textContent = err.message; }
        };
      } else if (sub === "keys") {
        const r = await api("v1/keys");
        body.innerHTML = head("All keys") + `<div class="panel"><h2>New key</h2><form class="row" id="akform"><input id="akname" placeholder="Name"><select id="akkind"><option>user</option><option>service</option><option>admin</option></select><input id="aksub" placeholder="Subject (email or account)"><select id="akdays"><option value="">No expiry</option><option value="90">90 days</option><option value="365">1 year</option></select><button class="btn pri">Create key</button></form><div id="aknew"></div></div>
          <div class="panel">${table([["Name", (k) => esc(k.name)], ["Kind", (k) => esc(k.kind)], ["Subject", (k) => esc(k.subject)], ["Created by", (k) => esc(k.created_by || "—")], ["Expires", (k) => k.expires_ms ? when(k.expires_ms) : "Never"], ["Last used", (k) => when(k.last_used_ms)],
            ["", (k) => k.revoked ? `<span class="pill old">Revoked</span>` : `<button class="btn" type="button" data-revoke="${esc(k.id)}">Revoke</button>`]], r.keys)}</div>`;
        $("akform").onsubmit = async (e) => {
          e.preventDefault();
          try {
            const k = await api("v1/keys", {name: $("akname").value, kind: $("akkind").value, subject: $("aksub").value, days: $("akdays").value ? Number($("akdays").value) : undefined});
            $("aknew").innerHTML = `<p class="ok">Key created. Copy it now: it is not shown again.</p><div class="secret">${esc(k.key)}</div>`;
          } catch (err) { $("aknew").innerHTML = `<p class="err">${esc(err.message)}</p>`; }
        };
        for (const b of body.querySelectorAll("[data-revoke]")) b.onclick = async () => { if (confirm("Revoke this key? Anything using it stops working.")) { await api("v1/keys/" + b.dataset.revoke, null, "DELETE"); loadAdmin("keys"); } };
      } else if (sub === "people") {
        const r = await api("v1/admin/people");
        body.innerHTML = head("People") + `<div class="panel"><p class="muted">Each memory belongs to one person. As an admin you see counts, never their content.</p>${table([["Who", (p) => p.name || p.email ? `${esc(p.name || p.email)}${p.name && p.email ? ` <span class="muted">${esc(p.email)}</span>` : ""}` : `<span class="muted">${esc(p.subject)}</span>`], ["Account", (p) => `<span class="muted">${esc(p.subject)}</span>`], ["Role", (p) => esc(p.kind)], ["Last seen", (p) => p.last_ms ? when(p.last_ms) : "—"], ["Current memories", (p) => fmt(p.live), "n"], ["All", (p) => fmt(p.total), "n"]], r.people)}</div>`;
      } else if (sub === "sessions") {
        const [r, c] = await Promise.all([api("v1/admin/sessions"), api("v1/admin/connections")]);
        const who = (x) => x.name || x.email ? `${esc(x.name || x.email)}${x.name && x.email ? ` <span class="muted">${esc(x.email)}</span>` : ""}` : `<span class="muted">${esc(x.subject)}</span>`;
        body.innerHTML = head("Sessions") + `<div class="panel"><h2>Apps</h2><p class="muted">Apps each person connected with OAuth. Disconnecting one stops only that app for that person.</p>${table([["Who", who], ["App", appCell], ["Last used", (x) => when(x.last_ms)], ["Client", agentCell], ["State", stateCell],
            ["", (x) => `<button class="btn" type="button" data-sub="${esc(x.subject)}" data-client="${esc(x.client)}" data-block="${x.blocked ? "0" : "1"}">${x.blocked ? "Allow again" : "Disconnect"}</button>`]], c.apps, "No app has connected yet.")}</div>
          <div class="panel"><h2>Browsers</h2><p class="muted">Browsers signed in with SSO.</p>${table([["Who", (x) => esc(x.name ? `${x.name} (${x.email})` : x.subject)], ["Kind", (x) => esc(x.kind)], ["Since", (x) => when(x.created_ms)], ["Until", (x) => when(x.expires_ms)], ["", (x) => `<button class="btn" type="button" data-kill="${esc(x.id)}">Sign out</button>`]], r.sessions)}</div>`;
        for (const b of body.querySelectorAll("[data-kill]")) b.onclick = async () => { await api("v1/admin/sessions/" + b.dataset.kill, null, "DELETE"); loadAdmin("sessions"); };
        for (const b of body.querySelectorAll("[data-client]")) b.onclick = async () => {
          const block = b.dataset.block === "1";
          if (block && !confirm(`Disconnect ${appName(b.dataset.client)} for this person?`)) return;
          try { await api("v1/admin/connections", {subject: b.dataset.sub, client: b.dataset.client, blocked: block}); loadAdmin("sessions"); } catch (err) { body.insertAdjacentHTML("afterbegin", `<p class="err">${esc(err.message)}</p>`); }
        };
      } else if (sub === "forget") {
        body.innerHTML = head("Delete a person") + `<div class="panel"><p>This deletes their memories with all history, their personal entities, raw episodes and measurements, and revokes their keys. It cannot be undone.</p>
          <form class="row" id="pform"><input id="psub" placeholder="Subject (email or account)"><input id="pconfirm" placeholder="Type it again to confirm"><button class="btn danger">Delete everything</button></form><div id="pout"></div></div>`;
        $("pform").onsubmit = async (e) => {
          e.preventDefault(); const s = $("psub").value.trim();
          if (!s || s !== $("pconfirm").value.trim()) { $("pout").innerHTML = `<p class="err">The two fields must match.</p>`; return; }
          try { const r = await api("v1/owners/" + encodeURIComponent(s), null, "DELETE"); $("pout").innerHTML = `<p class="ok">Deleted: ${esc(JSON.stringify(r))}</p>`; } catch (err) { $("pout").innerHTML = `<p class="err">${esc(err.message)}</p>`; }
        };
      }
    } catch (e) { body.innerHTML = `<p class="err">${esc(e.message)}</p>`; }
  }

  // The server picks each entity's palette index (`k`), always the same for the same thing.
  const ENT_PALETTE = ["#a894ff", "#f07aa8", "#4fd1b8", "#f0b65a", "#7ba4ff", "#a5b1c6", "#6fd27a", "#e58ad0", "#5fc3e4", "#d4a373"];
  const MEM_KINDS = ["fact", "event", "decision", "procedure", "state", "preference"];
  const MEM_COLORS = ["#7ba4ff", "#4fd1b8", "#f0b65a", "#a894ff", "#f07aa8", "#6fd27a"];
  let colors = {e: [], m: []}, hi = null;
  const cv = $("globe"), ctx = cv.getContext("2d");
  let W = 0, H = 0, dpr = 1;
  const view0 = {cx: .5, cy: .5, scale: 0};
  const tiles = new Map(), pending = new Set();
  // Dark space in both themes: the light tones only glow on it.
  const SPACE = "#03040a";
  function readColors() {
    const cs = getComputedStyle(document.documentElement);
    colors = {accent: "#ffd166", mono: cs.getPropertyValue("--mono").trim(), e: ENT_PALETTE, m: MEM_COLORS};
  }
  const ecol = (k) => colors.e[k] || colors.e[0];
  const mcol = (k) => colors.m[k] || colors.m[0];
  const rgba = (hex, a) => { const n = parseInt(hex.slice(1), 16); return `rgba(${n >> 16},${(n >> 8) & 255},${n & 255},${a})`; };
  const vw = view0;
  const zoomLevel = () => Math.max(0, Math.min(22, Math.floor(Math.log2(vw.scale / 256))));
  const toScreen = (x, y) => [(x - vw.cx) * vw.scale + W / 2, (y - vw.cy) * vw.scale + H / 2];
  const toWorld = (sx, sy) => [(sx - W / 2) / vw.scale + vw.cx, (sy - H / 2) / vw.scale + vw.cy];
  function visibleTiles(z) {
    const n = 2 ** z, [x0, y0] = toWorld(0, 0), [x1, y1] = toWorld(W, H), out = [];
    for (let x = Math.max(0, Math.floor(x0 * n)); x <= Math.min(n - 1, Math.floor(x1 * n)); x++)
      for (let y = Math.max(0, Math.floor(y0 * n)); y <= Math.min(n - 1, Math.floor(y1 * n)); y++) out.push([z, x, y]);
    return out;
  }
  async function fetchTile(z, x, y) {
    const k = `${z}/${x}/${y}`; if (tiles.has(k) || pending.has(k) || pending.size > 24) return;
    pending.add(k);
    try { tiles.set(k, await api(`v1/galaxy?z=${z}&x=${x}&y=${y}`)); } catch {} finally { pending.delete(k); schedule(); }
  }
  // While this zoom's tile loads, the nearest ancestor's tile is drawn: never a blank map.
  function tileFor(z, x, y) {
    for (let zz = z; zz >= 0; zz--) { const s = 2 ** (z - zz), t = tiles.get(`${zz}/${Math.floor(x / s)}/${Math.floor(y / s)}`); if (t) return t; }
    return null;
  }
  let raf = 0, screenPts = [], hover = null;
  const schedule = () => { if (!raf) raf = requestAnimationFrame(() => { raf = 0; draw(); }); };
  const onScreen = (sx, sy, m) => sx > -m && sy > -m && sx < W + m && sy < H + m;
  function draw() {
    if (!W || $("vmap").hidden) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0); ctx.globalCompositeOperation = "source-over"; ctx.globalAlpha = 1;
    ctx.fillStyle = SPACE; ctx.fillRect(0, 0, W, H);
    const z = zoomLevel(), seen = new Set(); let total = 0; screenPts = [];
    for (const [tz, tx, ty] of visibleTiles(z)) {
      if (!tiles.has(`${tz}/${tx}/${ty}`)) fetchTile(tz, tx, ty);
      const t = tileFor(tz, tx, ty); if (t) { const key = `${t.z}/${t.x}/${t.y}`; if (!seen.has(key)) { seen.add(key); total += t.total; } }
    }
    const loaded = [...seen].map((k) => tiles.get(k)).filter(Boolean);
    const ents = new Map(), routes = new Map();
    for (const t of loaded) { for (const e of t.entities || []) ents.set(e.id, e); for (const r of t.routes || []) routes.set(r.id, r); }
    const near = Math.min(1, Math.log2(vw.scale / 600 + 1) / 4);
    const hiOn = (id) => hi?.ents.has(id);
    ctx.globalCompositeOperation = "lighter";
    for (const e of ents.values()) {
      if (!e.root) continue;
      const [sx, sy] = toScreen(e.x, e.y), r = Math.max(28, .028 * vw.scale) * (1 + Math.log10(1 + e.n) * .25);
      if (!onScreen(sx, sy, r)) continue;
      const g = ctx.createRadialGradient(sx, sy, 0, sx, sy, r), col = ecol(e.k);
      g.addColorStop(0, rgba(col, .16 * (1 - near * .6))); g.addColorStop(.5, rgba(col, .05)); g.addColorStop(1, rgba(col, 0));
      ctx.fillStyle = g; ctx.beginPath(); ctx.arc(sx, sy, r, 0, Math.PI * 2); ctx.fill();
    }
    // Routes thin and faint: the stars lead the map, not the lines.
    const far = z < 3;
    ctx.globalCompositeOperation = "source-over";
    const top = [...routes.values()].sort((a, b) => b.w - a.w).slice(0, far ? 90 : 300);
    const wmax = Math.max(1, ...top.map((r) => r.w));
    for (const r of top) {
      const [ax, ay] = toScreen(r.a[0], r.a[1]), [bx, by] = toScreen(r.b[0], r.b[1]);
      if (!onScreen(ax, ay, 3000) && !onScreen(bx, by, 3000)) continue;
      const lit = !hi || hiOn(r.ea) || hiOn(r.eb), rel = Math.log(1 + r.w) / Math.log(1 + wmax);
      const alpha = (.06 + rel * .34) * (lit ? 1 : .2) * (hi && lit ? 2 : 1);
      const g = ctx.createLinearGradient(ax, ay, bx, by);
      g.addColorStop(0, rgba(ecol(r.ka), alpha)); g.addColorStop(.5, `rgba(220,228,255,${alpha * .8})`); g.addColorStop(1, rgba(ecol(r.kb), alpha));
      ctx.strokeStyle = g; ctx.lineWidth = (far ? .5 : .4) + rel * (far ? 1.6 : 1.1);
      ctx.beginPath(); ctx.moveTo(ax, ay);
      if (far) { const mx = (ax + bx) / 2, my = (ay + by) / 2, dx = bx - ax, dy = by - ay; ctx.quadraticCurveTo(mx - dy * .12, my + dx * .12, bx, by); }
      else ctx.lineTo(bx, by);
      ctx.stroke();
    }
    ctx.globalCompositeOperation = "lighter";
    if (!far) {
      ctx.strokeStyle = "rgba(200,210,255,.10)"; ctx.lineWidth = .7; ctx.beginPath();
      for (const e of ents.values()) { if (!e.p) continue; const [sx, sy] = toScreen(e.x, e.y), [qx, qy] = toScreen(e.p[0], e.p[1]); if (!onScreen(sx, sy, 400) && !onScreen(qx, qy, 400)) continue; ctx.moveTo(sx, sy); ctx.lineTo(qx, qy); }
      ctx.stroke();
    }
    for (const t of loaded) {
      if (t.cells) {
        const cpx = Math.max(2, t.cell * vw.scale);
        for (const c of t.cells) {
          const [sx, sy] = toScreen(c.x, c.y); if (!onScreen(sx, sy, cpx)) continue;
          const col = mcol(c.k), dots = Math.min(Math.ceil(Math.log2(1 + c.n) * 1.6), Math.max(1, Math.floor(cpx * cpx / 8)));
          ctx.fillStyle = rgba(col, Math.min(.55, .12 + Math.log10(1 + c.n) * .12));
          let seed = Math.floor(c.x * 1e7) ^ Math.floor(c.y * 1e7) * 31;
          for (let d = 0; d < dots; d++) {
            seed = (seed * 1103515245 + 12345) & 0x7fffffff; const ox = (seed % 1000) / 1000 - .5;
            seed = (seed * 1103515245 + 12345) & 0x7fffffff; const oy = (seed % 1000) / 1000 - .5;
            ctx.fillRect(sx + ox * cpx, sy + oy * cpx, 1.3, 1.3);
          }
        }
      }
      for (const p of t.points || []) {
        const [sx, sy] = toScreen(p.x, p.y); if (!onScreen(sx, sy, 6)) continue;
        const on = hi?.mems.has(p.id), r = on ? 2.6 : Math.min(2, .9 + near * 1.1);
        ctx.fillStyle = on ? "rgba(255,240,200,1)" : rgba(mcol(p.k), hi ? .15 : (p.live ? .55 : .2));
        ctx.fillRect(sx - r / 2, sy - r / 2, r, r); screenPts.push(["m", p.id, sx, sy, null]);
      }
    }
    const list = [...ents.values()].sort((a, b) => b.n - a.n);
    for (const e of list) {
      const [sx, sy] = toScreen(e.x, e.y); if (!onScreen(sx, sy, 40)) continue;
      const on = hiOn(e.id), dim = hi && !on ? .3 : 1;
      const size = e.root ? (e.region_n || e.n) : e.n;
      const core = Math.min(e.root ? 5 : 3, (e.root ? 1.4 : 1) + Math.log2(1 + size) * (e.root ? .3 : .12 + .22 * near)), halo = core * (e.root ? 6 : 4.5);
      const g = ctx.createRadialGradient(sx, sy, 0, sx, sy, halo), col = ecol(e.k);
      g.addColorStop(0, rgba(col, .9 * dim)); g.addColorStop(.25, rgba(col, .35 * dim)); g.addColorStop(1, rgba(col, 0));
      ctx.fillStyle = g; ctx.beginPath(); ctx.arc(sx, sy, halo, 0, Math.PI * 2); ctx.fill();
      // Small white core: from afar regions are large, and a large core would blow out to white.
      ctx.fillStyle = `rgba(255,255,255,${(e.root ? .7 : .9) * dim})`; ctx.beginPath(); ctx.arc(sx, sy, Math.min(2.4, core * .5), 0, Math.PI * 2); ctx.fill();
      if (on) { ctx.strokeStyle = colors.accent; ctx.lineWidth = 1.5; ctx.beginPath(); ctx.arc(sx, sy, halo * .55, 0, Math.PI * 2); ctx.stroke(); }
      screenPts.push(["e", e.id, sx, sy, e]);
    }
    ctx.globalCompositeOperation = "source-over";
    const taken = [];
    const label = (text, sx, sy, font, color, spacing) => {
      ctx.font = font; ctx.letterSpacing = spacing; const w = ctx.measureText(text).width, box = [sx - w / 2, sy - 9, w, 16];
      if (taken.some((b) => box[0] < b[0] + b[2] && b[0] < box[0] + box[2] && box[1] < b[1] + b[3] && b[1] < box[1] + box[3])) return;
      taken.push(box); ctx.fillStyle = "rgba(3,4,10,.55)"; ctx.fillText(text, sx - w / 2 + 1, sy + 4); ctx.fillStyle = color; ctx.fillText(text, sx - w / 2, sy + 3);
    };
    const regions = list.filter((e) => e.root).sort((a, b) => (b.region_n || 0) - (a.region_n || 0));
    const maxRegions = z < 2 ? 16 : z < 4 ? 40 : 400;
    let shown = 0;
    for (const e of regions) {
      if (z >= 9 || shown >= maxRegions) break;
      const [sx, sy] = toScreen(e.x, e.y); if (!onScreen(sx, sy, 0)) continue;
      const before = taken.length;
      label(e.name.toUpperCase(), sx, sy - 16 - near * 6, `600 ${11 + Math.round((1 - near) * 2)}px ${colors.mono}`, "rgba(230,235,255,.85)", "2px");
      if (taken.length > before) shown++;
    }
    const systems = list.filter((e) => !e.root && (hiOn(e.id) || vw.scale > 2600)).slice(0, 60);
    for (const e of systems) {
      const [sx, sy] = toScreen(e.x, e.y); if (!onScreen(sx, sy, 0)) continue;
      label(e.name, sx, sy + 13, `500 10.5px ${colors.mono}`, "rgba(200,210,235,.75)", "0px");
    }
    ctx.letterSpacing = "0px";
    if (hover) {
      const [hx, hy] = [hover[2], hover[3]], text = hover[0] === "e" ? `${hover[4].name}, ${hover[4].n.toLocaleString("en-US")} memories` : "Memory: click to open";
      ctx.font = `500 11.5px ${colors.mono}`; const w = ctx.measureText(text).width, bx = Math.min(W - w - 18, hx + 12), by = Math.max(6, hy - 30);
      ctx.fillStyle = "rgba(12,14,28,.92)"; ctx.strokeStyle = "rgba(160,170,255,.35)"; ctx.fillRect(bx, by, w + 14, 22); ctx.strokeRect(bx, by, w + 14, 22);
      ctx.fillStyle = "#e8eaff"; ctx.fillText(text, bx + 7, by + 15);
    }
    const empty = seen.size > 0 && total === 0 && ents.size === 0;
    $("overlay").hidden = !empty && seen.size > 0; $("overlay").textContent = empty ? "No memories yet. They appear once conversations reach mnemo and are distilled." : "Loading…";
    $("mapinfo").textContent = `Zoom ${z}, ${total.toLocaleString("en-US")} memories, ${routes.size.toLocaleString("en-US")} routes in view`;
    drawMini();
  }
  const mcv = $("minimap"), mctx = mcv.getContext("2d");
  function drawMini() {
    const w = mcv.clientWidth, h = mcv.clientHeight; if (!w) return;
    mcv.width = Math.round(w * dpr); mcv.height = Math.round(h * dpr); mctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    mctx.fillStyle = SPACE; mctx.fillRect(0, 0, w, h);
    const s = Math.min(w, h) * .92, ox = (w - s) / 2, oy = (h - s) / 2, mx = (x) => ox + x * s, my = (y) => oy + y * s;
    const root = tiles.get("0/0/0");
    if (!root) fetchTile(0, 0, 0);
    for (const e of root?.entities || []) { mctx.fillStyle = rgba(ecol(e.k), e.root ? .85 : .45); mctx.beginPath(); mctx.arc(mx(e.x), my(e.y), e.root ? 2.6 : 1.2, 0, Math.PI * 2); mctx.fill(); }
    const [x0, y0] = toWorld(0, 0), [x1, y1] = toWorld(W, H);
    mctx.strokeStyle = colors.accent; mctx.lineWidth = 1.2; mctx.strokeRect(mx(x0), my(y0), mx(x1) - mx(x0), my(y1) - my(y0));
    mcv._m = {ox, oy, s};
  }
  mcv.onclick = (ev) => { const m = mcv._m; if (!m) return; const r = mcv.getBoundingClientRect(); vw.cx = (ev.clientX - r.left - m.ox) / m.s; vw.cy = (ev.clientY - r.top - m.oy) / m.s; schedule(); };
  let mapAt = null;
  function fit() {
    if (mapAt) { vw.cx = mapAt.x; vw.cy = mapAt.y; vw.scale = 256 * 2 ** mapAt.z; mapAt = null; return; }
    vw.cx = .5; vw.cy = .5; vw.scale = Math.min(W, H) / .96;
  }
  function resize() {
    if ($("vmap").hidden) return;
    const first = !W; W = cv.clientWidth; H = cv.clientHeight; dpr = Math.min(devicePixelRatio || 1, 2);
    if (!W || !H) return;
    cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr); if (first || !vw.scale) fit(); draw();
  }
  function zoomAt(sx, sy, f) {
    const [wx, wy] = toWorld(sx, sy);
    vw.scale = Math.max(Math.min(W, H) / 1.2, Math.min(256 * 2 ** 22, vw.scale * f));
    vw.cx = wx - (sx - W / 2) / vw.scale; vw.cy = wy - (sy - H / 2) / vw.scale; schedule();
  }
  function pickPt(x, y) {
    let best = null, bd = 120;
    for (const pt of screenPts) { const d = (pt[2] - x) ** 2 + (pt[3] - y) ** 2; if (d < bd || (d === bd && pt[0] === "e")) { bd = d; best = pt; } }
    return best;
  }
  const ptrs = new Map(); let drag = null, pinch = 0;
  const local = (ev) => { const r = cv.getBoundingClientRect(); return {x: ev.clientX - r.left, y: ev.clientY - r.top}; };
  cv.onpointerdown = (ev) => { const p = local(ev); ptrs.set(ev.pointerId, p); cv.setPointerCapture(ev.pointerId); drag = {...p, sx: p.x, sy: p.y, moved: false};
    if (ptrs.size === 2) { const [a, b] = [...ptrs.values()]; pinch = Math.hypot(a.x - b.x, a.y - b.y); } };
  cv.onpointermove = (ev) => {
    const p = local(ev);
    if (ptrs.size === 2 && ptrs.has(ev.pointerId)) {
      ptrs.set(ev.pointerId, p); const [a, b] = [...ptrs.values()], d = Math.hypot(a.x - b.x, a.y - b.y);
      if (pinch) zoomAt((a.x + b.x) / 2, (a.y + b.y) / 2, d / pinch); pinch = d; if (drag) drag.moved = true; return;
    }
    if (drag && ptrs.has(ev.pointerId)) {
      vw.cx -= (p.x - drag.x) / vw.scale; vw.cy -= (p.y - drag.y) / vw.scale;
      if (Math.abs(p.x - drag.sx) + Math.abs(p.y - drag.sy) > 4) drag.moved = true; drag.x = p.x; drag.y = p.y; schedule(); return;
    }
    const h = pickPt(p.x, p.y); cv.style.cursor = h ? "pointer" : "grab";
    if ((h && h[1]) !== (hover && hover[1])) { hover = h; schedule(); }
  };
  cv.onpointerup = (ev) => {
    const p = local(ev); ptrs.delete(ev.pointerId); if (ptrs.size < 2) pinch = 0;
    if (drag && !drag.moved && ptrs.size === 0) { const h = pickPt(p.x, p.y); if (h) h[0] === "e" ? showEntity(h[1]) : showMemoryOnMap(h[1]); }
    if (ptrs.size === 0) drag = null;
  };
  cv.onpointerleave = () => { if (hover) { hover = null; schedule(); } };
  cv.onwheel = (ev) => { ev.preventDefault(); const p = local(ev); zoomAt(p.x, p.y, Math.exp(-ev.deltaY * .0015)); };
  cv.ondblclick = (ev) => { const p = local(ev); zoomAt(p.x, p.y, 2.5); };
  $("zin").onclick = () => zoomAt(W / 2, H / 2, 1.6);
  $("zout").onclick = () => zoomAt(W / 2, H / 2, 1 / 1.6);
  $("mapfit").onclick = () => { fit(); schedule(); };
  $("legend").innerHTML = MEM_KINDS.map((k) => `<span><i class="m-${k}"></i>${k}</span>`).join("") +
    `<span><i class="ln sw-route"></i>appear together</span><span><i class="ln sw-const"></i>part of</span><span class="note">each system keeps its own colour</span>`;

  function paintMapQuery() {
    $("mapq").hidden = !hi;
    if (hi) $("mapqtext").innerHTML = `Showing <b>${esc(hi.label || "selection")}</b>`;
  }
  $("clear").onclick = () => { hi = null; paintMapQuery(); closeDrawer(); schedule(); };
  function openDrawer(html) { $("drawerbody").innerHTML = html; $("drawer").hidden = false; }
  function closeDrawer() { $("drawer").hidden = true; }
  $("drawerclose").onclick = closeDrawer;
  wire($("drawerbody"), (id) => showMemoryOnMap(id));
  async function showMemoryOnMap(id) {
    if (view !== "map") location.hash = "map";
    $("drawer").hidden = false;
    const m = await openMemory(id, $("drawerbody"), "");
    if (m) { hi = {ents: new Set((m.links || []).map((l) => l.entity)), mems: new Set([id]), query: new Set(), label: m.what}; paintMapQuery(); schedule(); }
  }
  async function showEntity(id) {
    if (view !== "map") location.hash = "map";
    try {
      const c = await api("v1/entities/" + encodeURIComponent(id));
      const ms = c.memories || [], e = c.entity;
      hi = {ents: new Set([e.id, ...ms.flatMap((m) => (m.links || []).map((l) => l.entity))]), mems: new Set(ms.map((m) => m.id)), query: new Set([e.id]), label: e.name};
      openDrawer(`<div class="detail"><div class="facts">${c.parent ? `<span>Part of ${esc(c.parent.name)}</span>` : ""}<span>${fmt(ms.length)} memories</span></div>
        <h2>${esc(e.name)}</h2>${e.summary ? `<p class="muted">${esc(e.summary)}</p>` : ""}</div>${ms.map((m) => row(m, false)).join("") || `<div class="empty">No memories about it that you can see.</div>`}`);
      paintMapQuery(); schedule();
    } catch (err) { openDrawer(`<div class="detail"><p class="err">${esc(err.message)}</p></div>`); }
  }

  async function startMap() {
    readColors(); resize();
    const st = await refreshStats();
    if (st && !(st.live ?? st.globe?.memories)) { $("overlay").hidden = false; $("overlay").textContent = "No memories yet. They appear once conversations reach mnemo and are distilled."; }
  }
  new ResizeObserver(resize).observe(cv);
  boot();
})();
