// gideon web — reading-progress dashboard.
//
// Signs in with email + password (Supabase Auth — the same account the Kobo
// signs into) and shows the `reading_progress` rows the device syncs, newest
// first. Set the account up here once (Create account), then sign in with the
// same email + password on your Kobo. It reads and never writes, so it can't
// rewind your place — the device is the writer.
//
// Self-contained: talks to Supabase's REST/Auth endpoints with plain fetch (no
// SDK, no CDN). The anon key is public by design — row-level security
// (auth.uid()), not the key, is what scopes every row to its owner.

const SUPABASE_URL = "https://sqlkceqkdtmejhdoycsr.supabase.co";
const SUPABASE_ANON_KEY =
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNxbGtjZXFrZHRtZWpoZG95Y3NyIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODMyOTE5MDAsImV4cCI6MjA5ODg2NzkwMH0.K8kXfcIihjw0Mz5qm1hW7nXHcymhN-yMLrV6CaLU1eo";
const SESSION_KEY = "gideon.session";

const app = document.getElementById("app");

// --- tiny Supabase client (auth + one read) -------------------------------

function loadSession() {
  try {
    return JSON.parse(localStorage.getItem(SESSION_KEY));
  } catch {
    return null;
  }
}
function saveSession(s) {
  localStorage.setItem(SESSION_KEY, JSON.stringify(s));
}
function clearSession() {
  localStorage.removeItem(SESSION_KEY);
}

function sessionFrom(data, email) {
  return {
    access_token: data.access_token,
    refresh_token: data.refresh_token,
    email: data.user?.email || email,
    expires_at: Math.floor(Date.now() / 1000) + (data.expires_in || 3600),
  };
}

async function authPost(path, body) {
  const res = await fetch(`${SUPABASE_URL}/auth/v1/${path}`, {
    method: "POST",
    headers: { apikey: SUPABASE_ANON_KEY, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(data.error_description || data.msg || data.message || `Error ${res.status}`);
  }
  return data;
}

async function signIn(email, password) {
  const data = await authPost("token?grant_type=password", { email, password });
  saveSession(sessionFrom(data, email));
  return loadSession();
}

async function signUp(email, password) {
  const data = await authPost("signup", { email, password });
  // With auto-confirm, signup returns a session directly; otherwise fall back
  // to a normal sign-in with the same credentials.
  if (data.access_token) {
    saveSession(sessionFrom(data, email));
    return loadSession();
  }
  return signIn(email, password);
}

async function refreshSession(session) {
  const data = await authPost("token?grant_type=refresh_token", {
    refresh_token: session.refresh_token,
  });
  const next = sessionFrom(data, session.email);
  saveSession(next);
  return next;
}

async function fetchProgress(session, retry = true) {
  const url = `${SUPABASE_URL}/rest/v1/reading_progress?select=chapter_key,current_page,total_pages,updated_at&order=updated_at.desc`;
  const res = await fetch(url, {
    headers: { apikey: SUPABASE_ANON_KEY, Authorization: `Bearer ${session.access_token}` },
  });
  if (res.status === 401 && retry && session.refresh_token) {
    const next = await refreshSession(session).catch(() => null);
    if (next) return fetchProgress(next, false);
  }
  if (!res.ok) throw new Error(`Couldn't load progress (${res.status})`);
  return res.json();
}

// Publish reading progress from the web. Furthest-page-wins server-side, so it
// can never rewind the Kobo. Best-effort (never blocks the reader).
function upsertProgress(session, chapterKey, currentPage, totalPages) {
  return fetch(`${SUPABASE_URL}/rest/v1/rpc/upsert_progress`, {
    method: "POST",
    headers: {
      apikey: SUPABASE_ANON_KEY,
      Authorization: `Bearer ${session.access_token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      p_chapter_key: chapterKey,
      p_current_page: currentPage,
      p_total_pages: totalPages,
    }),
  }).catch(() => {});
}

// The page image URLs the device published for a chapter (or [] if it hasn't
// been resolved for the web yet).
async function fetchChapterPages(session, chapterKey) {
  const url = `${SUPABASE_URL}/rest/v1/chapter_pages?chapter_key=eq.${encodeURIComponent(
    chapterKey
  )}&select=page_urls`;
  const res = await fetch(url, {
    headers: { apikey: SUPABASE_ANON_KEY, Authorization: `Bearer ${session.access_token}` },
  });
  if (!res.ok) return [];
  const rows = await res.json();
  return rows[0]?.page_urls ?? [];
}

// Session + resume state, so the reader can push progress and return home.
const state = { session: null, resume: {} };

// --- rendering ------------------------------------------------------------

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// "One Piece/vol1.cbz" -> { series: "One Piece", chapter: "vol1" }
function parseKey(key) {
  const slash = key.lastIndexOf("/");
  const series = slash >= 0 ? key.slice(0, slash) : key;
  let chapter = slash >= 0 ? key.slice(slash + 1) : "";
  chapter = chapter.replace(/\.(cbz|zip)$/i, "");
  return { series, chapter };
}

function timeAgo(iso) {
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return "";
  const secs = Math.max(0, (Date.now() - then) / 1000);
  const units = [["year", 31536000], ["month", 2592000], ["week", 604800], ["day", 86400], ["hour", 3600], ["minute", 60]];
  for (const [name, size] of units) {
    const n = Math.floor(secs / size);
    if (n >= 1) return `${n} ${name}${n === 1 ? "" : "s"} ago`;
  }
  return "just now";
}

function renderSignIn(message) {
  app.innerHTML = `
    <div class="head"><div class="brand">gideon <span>· sync</span></div></div>
    <div class="card">
      <h1>Your reading, everywhere</h1>
      <p>Sign in to see where you left off on your Kobo. Create the account here once, then use the same email &amp; password on your device.</p>
      <form id="signin">
        <div class="stack">
          <input type="email" id="email" placeholder="you@example.com" autocomplete="email" required />
          <input type="password" id="password" placeholder="password" autocomplete="current-password" required minlength="6" />
        </div>
        <div class="field actions">
          <button class="primary" type="submit" data-testid="signin">Sign in</button>
          <button class="ghost" type="button" data-testid="create" id="create">Create account</button>
        </div>
      </form>
      <div class="note ${message ? "ok" : ""}" id="note" data-testid="note">${message ? esc(message) : ""}</div>
    </div>`;

  const form = document.getElementById("signin");
  const note = document.getElementById("note");
  const emailEl = document.getElementById("email");
  const pwEl = document.getElementById("password");
  const buttons = form.querySelectorAll("button");

  async function submit(mode) {
    const email = emailEl.value.trim();
    const password = pwEl.value;
    if (!email || !password) return;
    buttons.forEach((b) => (b.disabled = true));
    note.className = "note";
    note.textContent = mode === "signup" ? "Creating account…" : "Signing in…";
    try {
      const session = mode === "signup" ? await signUp(email, password) : await signIn(email, password);
      await showDashboard(session);
    } catch (e) {
      note.className = "note";
      note.textContent = e.message || "Sign-in failed.";
      buttons.forEach((b) => (b.disabled = false));
    }
  }

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    submit("signin");
  });
  document.getElementById("create").addEventListener("click", () => submit("signup"));
}

// Progress numbers for a row: 1-based page, percent, and a compact label.
function progressMeta(r) {
  const total = r.total_pages || 0;
  const page = Math.min(r.current_page + 1, total || r.current_page + 1);
  const pct = total > 0 ? Math.round((page / total) * 100) : 0;
  return { pct, label: total > 0 ? `${page}/${total}` : `p.${page}` };
}

// One entry per series (like the Kobo shelf): its most-recently-read chapter is
// "where you are"; the full chapter list rides along for the expanded view.
// Series ordered by most recent activity.
function groupBySeries(rows) {
  const bySeries = new Map();
  for (const r of rows) {
    const { series } = parseKey(r.chapter_key);
    if (!bySeries.has(series)) bySeries.set(series, []);
    bySeries.get(series).push(r);
  }
  const groups = [...bySeries.entries()].map(([series, chapters]) => {
    const current = chapters.reduce((a, b) => (b.updated_at > a.updated_at ? b : a));
    return { series, current, chapters };
  });
  return groups.sort((a, b) => (a.current.updated_at < b.current.updated_at ? 1 : -1));
}

// --- reading stats --------------------------------------------------------
//
// Everything is derived from the `reading_progress` rows the device backs up
// (chapter_key, current_page, total_pages, updated_at) — no extra tables. A
// chapter is "finished" when the last page is reached; pages read is the
// 1-based page of each tracked chapter (a chapter's progress is attributed to
// the day it was last read). Charts are single-hue (the app accent as a
// light→dark ramp) since they show magnitude, not identity.

const pad2 = (n) => String(n).padStart(2, "0");
// Local calendar day of a timestamp, so the heatmap lines up with the reader's
// own days rather than UTC. `null` for an unparseable value.
function dayKey(iso) {
  const d = new Date(iso);
  if (!Number.isFinite(d.getTime())) return null;
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
}
function dateFromKey(k) {
  const [y, m, d] = k.split("-").map(Number);
  return new Date(y, m - 1, d);
}
function keyFromDate(dt) {
  return `${dt.getFullYear()}-${pad2(dt.getMonth() + 1)}-${pad2(dt.getDate())}`;
}
function prevDayKey(k) {
  const dt = dateFromKey(k);
  dt.setDate(dt.getDate() - 1);
  return keyFromDate(dt);
}
function prettyDate(k) {
  return dateFromKey(k).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

// Current streak (consecutive days up to today, or up to yesterday if today
// hasn't been read yet) and the longest run ever.
function streaks(daySet) {
  if (daySet.size === 0) return { current: 0, longest: 0 };
  const days = [...daySet].sort();
  let longest = 1;
  let run = 1;
  for (let i = 1; i < days.length; i++) {
    run = prevDayKey(days[i]) === days[i - 1] ? run + 1 : 1;
    longest = Math.max(longest, run);
  }
  const today = keyFromDate(new Date());
  let cursor = daySet.has(today) ? today : prevDayKey(today);
  let current = 0;
  while (daySet.has(cursor)) {
    current++;
    cursor = prevDayKey(cursor);
  }
  return { current, longest };
}

function computeStats(rows) {
  const isFinished = (r) => r.total_pages > 0 && r.current_page + 1 >= r.total_pages;
  const pagesOf = (r) => Math.min(r.current_page + 1, r.total_pages > 0 ? r.total_pages : r.current_page + 1);

  let finished = 0;
  let pages = 0;
  const series = new Set();
  const seriesFinished = new Map();
  const byDay = new Map();
  for (const r of rows) {
    const { series: s } = parseKey(r.chapter_key);
    series.add(s);
    pages += pagesOf(r);
    if (isFinished(r)) {
      finished++;
      seriesFinished.set(s, (seriesFinished.get(s) || 0) + 1);
    }
    const day = dayKey(r.updated_at);
    if (day) byDay.set(day, (byDay.get(day) || 0) + pagesOf(r));
  }
  const top = [...seriesFinished.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, 6)
    .map(([name, count]) => ({ name, count }));
  const dates = [...byDay.keys()].sort();
  const { current, longest } = streaks(new Set(byDay.keys()));
  return {
    chapters: rows.length,
    finished,
    pages,
    series: series.size,
    activeDays: byDay.size,
    firstDay: dates[0] || null,
    byDay,
    maxDay: Math.max(0, ...byDay.values()),
    top,
    currentStreak: current,
    longestStreak: longest,
  };
}

function statTilesHtml(s) {
  const tiles = [
    ["Chapters read", String(s.finished), `${s.chapters} tracked`],
    ["Pages read", s.pages.toLocaleString(), `${s.series} series`],
    ["Day streak", String(s.currentStreak), `best ${s.longestStreak}`],
    ["Active days", String(s.activeDays), s.firstDay ? `since ${prettyDate(s.firstDay)}` : ""],
  ];
  return `<div class="tiles">${tiles
    .map(
      ([label, val, sub]) => `
      <div class="tile" data-testid="stat">
        <div class="tile-val">${esc(val)}</div>
        <div class="tile-label">${esc(label)}</div>
        ${sub ? `<div class="tile-sub">${esc(sub)}</div>` : ""}
      </div>`
    )
    .join("")}</div>`;
}

// GitHub-style calendar heatmap: one column per week, seven day-cells each,
// shaded by how many pages were read that day. Month labels ride above the
// columns where the month changes.
function heatmapHtml(s) {
  const WEEKS = 18;
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const start = new Date(today);
  start.setDate(start.getDate() - (WEEKS * 7 - 1));
  start.setDate(start.getDate() - start.getDay()); // back to Sunday
  const months = [];
  const cols = [];
  const cursor = new Date(start);
  let lastMonth = -1;
  while (cursor <= today) {
    const colMonth = cursor.getMonth();
    // Label a month at its first column, but skip the very first column (it's a
    // partial month whose label would collide with the next one).
    const showLabel = cols.length > 0 && colMonth !== lastMonth;
    months.push(
      showLabel
        ? `<span class="hm-mon">${cursor.toLocaleDateString(undefined, { month: "short" })}</span>`
        : `<span class="hm-mon"></span>`
    );
    lastMonth = colMonth;
    const cells = [];
    for (let d = 0; d < 7; d++) {
      if (cursor > today) {
        cells.push(`<span class="hm-cell hm-pad"></span>`);
      } else {
        const key = keyFromDate(cursor);
        const val = s.byDay.get(key) || 0;
        const lvl = val === 0 ? 0 : Math.min(4, Math.ceil((val / (s.maxDay || 1)) * 4));
        const label = val ? `${val} page${val === 1 ? "" : "s"} · ${prettyDate(key)}` : `No reading · ${prettyDate(key)}`;
        cells.push(`<span class="hm-cell lvl-${lvl}" title="${esc(label)}"></span>`);
      }
      cursor.setDate(cursor.getDate() + 1);
    }
    cols.push(`<div class="hm-col">${cells.join("")}</div>`);
  }
  return `
    <div class="hm-scroll">
      <div class="hm-months">${months.join("")}</div>
      <div class="heatmap" data-testid="heatmap">${cols.join("")}</div>
    </div>
    <div class="hm-legend">Less ${[1, 2, 3, 4]
      .map((l) => `<span class="hm-cell lvl-${l}"></span>`)
      .join("")} More</div>`;
}

function topSeriesHtml(s) {
  if (!s.top.length) return "";
  const max = s.top[0].count || 1;
  const rows = s.top
    .map(
      (t) => `
      <div class="ts-row" data-testid="top-series">
        <div class="ts-name" title="${esc(t.name)}">${esc(t.name)}</div>
        <div class="ts-bar"><i style="width:${Math.round((t.count / max) * 100)}%"></i></div>
        <div class="ts-val">${t.count}</div>
      </div>`
    )
    .join("");
  return `<section class="panel"><div class="section-label">Most read</div><div class="ts-list">${rows}</div></section>`;
}

function recentHtml(rows) {
  const recent = rows.slice(0, 6);
  const items = recent
    .map((r) => {
      const { series, chapter } = parseKey(r.chapter_key);
      const m = progressMeta(r);
      return `
        <button class="sub" data-testid="chapter" data-key="${esc(r.chapter_key)}">
          <span class="rc-title"><span class="rc-series">${esc(series)}</span>${chapter ? ` <span class="rc-chapter">${esc(chapter)}</span>` : ""}</span>
          <span class="bar small"><i style="width:${m.pct}%"></i></span>
          <span class="ago">${esc(timeAgo(r.updated_at))}</span>
        </button>`;
    })
    .join("");
  return `<section class="panel"><div class="section-label">Recently read</div><div class="chapters">${items}</div></section>`;
}

function viewStats(stats, rows) {
  return `${statTilesHtml(stats)}
    <section class="panel">
      <div class="section-label">Reading activity</div>
      ${heatmapHtml(stats)}
    </section>
    ${topSeriesHtml(stats)}
    ${recentHtml(rows)}`;
}

function viewLibrary(groups) {
  const items = groups
    .map((g) => {
      const { chapter } = parseKey(g.current.chapter_key);
      const meta = progressMeta(g.current);
      const chapterRows = g.chapters
        .slice()
        .sort((a, b) => a.chapter_key.localeCompare(b.chapter_key, undefined, { numeric: true }))
        .map((c) => {
          const m = progressMeta(c);
          return `
            <button class="sub" data-testid="chapter" data-key="${esc(c.chapter_key)}">
              <span class="sub-title">${esc(parseKey(c.chapter_key).chapter)}</span>
              <span class="bar small"><i style="width:${m.pct}%"></i></span>
              <span class="pct">${esc(m.label)}</span>
            </button>`;
        })
        .join("");
      return `
        <details class="item" data-testid="item">
          <summary>
            <div class="row">
              <div class="grow">
                <div class="title">${esc(g.series)}</div>
                ${chapter ? `<div class="chapter">${esc(chapter)}</div>` : ""}
              </div>
              <div class="chev" aria-hidden="true">›</div>
            </div>
            <div class="meta">
              <div class="bar"><i style="width:${meta.pct}%"></i></div>
              <div class="pct">${esc(meta.label)}</div>
            </div>
            <div class="ago">${esc(timeAgo(g.current.updated_at))}</div>
          </summary>
          <div class="chapters" data-testid="chapters">${chapterRows}</div>
        </details>`;
    })
    .join("");
  return `<div class="section-label">Continue reading</div><div class="list">${items}</div>`;
}

function signOut() {
  clearSession();
  state.session = null;
  state.resume = {};
  state.rows = null;
  state.tab = "stats";
  renderSignIn("Signed out.");
}

// The signed-in dashboard: a header, a Stats/Library tab switch, and the active
// view. Rows are fetched once and reused across tab switches.
function renderDashboard(email, rows) {
  const tab = state.tab === "library" ? "library" : "stats";
  let body;
  if (!rows.length) {
    body = `<div class="empty" data-testid="empty"><div class="big">📖</div><p>No reading progress yet.<br/>Read something on your Kobo and it'll show up here.</p></div>`;
  } else if (tab === "library") {
    body = viewLibrary(groupBySeries(rows));
  } else {
    body = viewStats(computeStats(rows), rows);
  }
  app.innerHTML = `
    <div class="head">
      <div class="brand">gideon <span>· stats</span></div>
      <div class="who">${esc(email)}<button id="signout" data-testid="signout">Sign out</button></div>
    </div>
    <div class="tabs" role="tablist">
      <button class="tab ${tab === "stats" ? "on" : ""}" data-tab="stats" data-testid="tab-stats">Stats</button>
      <button class="tab ${tab === "library" ? "on" : ""}" data-tab="library" data-testid="tab-library">Library</button>
    </div>
    ${body}`;

  document.getElementById("signout").addEventListener("click", signOut);
  for (const b of app.querySelectorAll(".tab")) {
    b.addEventListener("click", () => {
      state.tab = b.getAttribute("data-tab");
      renderDashboard(email, rows);
    });
  }
  // Tapping a chapter (library list or recent-read) opens the reader.
  for (const btn of app.querySelectorAll('[data-testid="chapter"]')) {
    btn.addEventListener("click", () => {
      const key = btn.getAttribute("data-key");
      openReader(key, parseKey(key));
    });
  }
}

// --- reader ---------------------------------------------------------------

async function openReader(chapterKey, { series, chapter }) {
  const title = chapter ? `${series} · ${chapter}` : series;
  app.innerHTML = `
    <div class="reader">
      <div class="reader-bar">
        <button class="ghost" data-testid="reader-back" id="r-back">‹ Library</button>
        <div class="reader-title">${esc(title)}</div>
        <div class="reader-count"></div>
      </div>
      <div class="reader-msg">Loading…</div>
    </div>`;
  document.getElementById("r-back").addEventListener("click", () => showDashboard(state.session));

  const pages = await fetchChapterPages(state.session, chapterKey);
  if (!pages.length) {
    document.querySelector(".reader-msg").innerHTML =
      "This chapter isn't available to read on the web yet.<br/>Open it on your Kobo once while signed in and it'll show up here.";
    return;
  }
  renderReader(chapterKey, title, pages);
}

function renderReader(chapterKey, title, pages) {
  const total = pages.length;
  let page = Math.min(Math.max(state.resume[chapterKey] ?? 0, 0), total - 1);
  let pushTimer = null;

  app.innerHTML = `
    <div class="reader" data-testid="reader">
      <div class="reader-bar">
        <button class="ghost" data-testid="reader-back" id="r-back">‹ Library</button>
        <div class="reader-title">${esc(title)}</div>
        <div class="reader-count" data-testid="reader-count"></div>
      </div>
      <div class="reader-page">
        <img id="r-img" data-testid="reader-img" alt="page" referrerpolicy="no-referrer" />
        <button class="nav-zone left" data-testid="reader-prev" aria-label="previous page"></button>
        <button class="nav-zone right" data-testid="reader-next" aria-label="next page"></button>
      </div>
    </div>`;

  const img = document.getElementById("r-img");
  const count = app.querySelector(".reader-count");

  function pushProgress() {
    state.resume[chapterKey] = page;
    upsertProgress(state.session, chapterKey, page, total);
  }
  function show() {
    img.src = pages[page];
    count.textContent = `${page + 1} / ${total}`;
    window.scrollTo(0, 0);
    clearTimeout(pushTimer);
    pushTimer = setTimeout(pushProgress, 600);
  }
  function go(delta) {
    const next = Math.min(Math.max(page + delta, 0), total - 1);
    if (next !== page) {
      page = next;
      show();
    }
  }

  app.querySelector('[data-testid="reader-next"]').addEventListener("click", () => go(1));
  app.querySelector('[data-testid="reader-prev"]').addEventListener("click", () => go(-1));
  app.querySelector('[data-testid="reader-back"]').addEventListener("click", () => {
    clearTimeout(pushTimer);
    pushProgress();
    showDashboard(state.session);
  });
  const onKey = (e) => {
    if (e.key === "ArrowRight" || e.key === " ") go(1);
    else if (e.key === "ArrowLeft") go(-1);
    else if (e.key === "Escape") app.querySelector('[data-testid="reader-back"]').click();
  };
  document.addEventListener("keydown", onKey);
  // Drop the key handler when we leave the reader (back to a fresh DOM).
  app.querySelector('[data-testid="reader-back"]').addEventListener("click", () =>
    document.removeEventListener("keydown", onKey)
  );

  show();
}

async function showDashboard(session) {
  state.session = session;
  const email = session.email ?? "signed in";
  try {
    const rows = (await fetchProgress(session)) ?? [];
    // Remember where each chapter was left off, so the reader resumes there.
    for (const r of rows) state.resume[r.chapter_key] = r.current_page;
    state.rows = rows;
    renderDashboard(email, rows);
  } catch (e) {
    if (String(e.message).includes("401")) {
      clearSession();
      state.session = null;
      state.resume = {};
      renderSignIn("Session expired — please sign in again.");
      return;
    }
    renderDashboard(email, []);
    const tabs = document.querySelector(".tabs");
    if (tabs) tabs.insertAdjacentHTML("afterend", `<div class="note">${esc(e.message)}</div>`);
  }
}

function boot() {
  const session = loadSession();
  if (session?.access_token) showDashboard(session);
  else renderSignIn("");
}

boot();
