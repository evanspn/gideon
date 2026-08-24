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
  // Supabase rotates refresh tokens, so the in-memory session must follow the
  // stored one — a later call retrying with the stale token would sign out.
  if (state.session) state.session = next;
  return next;
}

// Authenticated REST call with one transparent refresh-retry on 401, so a tab
// that sat open past the ~1h access-token lifetime keeps working (this was why
// "Send to Kobo" silently did nothing on a stale tab). Throws on any other
// non-2xx status; callers decide whether that's fatal or decor.
async function authedFetch(path, opts = {}, retry = true) {
  const session = state.session;
  const res = await fetch(`${SUPABASE_URL}/rest/v1/${path}`, {
    ...opts,
    headers: {
      apikey: SUPABASE_ANON_KEY,
      Authorization: `Bearer ${session.access_token}`,
      ...(opts.headers || {}),
    },
  });
  if (res.status === 401 && retry && session.refresh_token) {
    const next = await refreshSession(session).catch(() => null);
    if (next) return authedFetch(path, opts, false);
  }
  return res;
}

async function fetchProgress(session, retry = true, withStartedAt = true) {
  // started_at (migration 0004) powers the per-series insights; fall back to
  // the original column set if the migration hasn't been applied yet.
  const cols = withStartedAt
    ? "chapter_key,current_page,total_pages,updated_at,started_at"
    : "chapter_key,current_page,total_pages,updated_at";
  const url = `${SUPABASE_URL}/rest/v1/reading_progress?select=${cols}&order=updated_at.desc`;
  const res = await fetch(url, {
    headers: { apikey: SUPABASE_ANON_KEY, Authorization: `Bearer ${session.access_token}` },
  });
  if (res.status === 401 && retry && session.refresh_token) {
    const next = await refreshSession(session).catch(() => null);
    if (next) return fetchProgress(next, false, withStartedAt);
  }
  if (!res.ok && withStartedAt) return fetchProgress(session, retry, false);
  if (!res.ok) throw new Error(`Couldn't load progress (${res.status})`);
  return res.json();
}

// Every published chapter-page row at once, for the library's cover art: a
// series' cover is the first page of its first chapter that the device has
// published page URLs for. One request; [] on any failure (covers are decor).
async function fetchAllChapterPages() {
  const res = await authedFetch("chapter_pages?select=chapter_key,page_urls");
  if (!res.ok) return [];
  return res.json().catch(() => []);
}

// Publish reading progress from the web. Furthest-page-wins server-side, so it
// can never rewind the Kobo. Best-effort (never blocks the reader).
function upsertProgress(session, chapterKey, currentPage, totalPages) {
  return authedFetch("rpc/upsert_progress", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
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
  const res = await authedFetch(
    `chapter_pages?chapter_key=eq.${encodeURIComponent(chapterKey)}&select=page_urls`
  );
  if (!res.ok) return [];
  const rows = await res.json();
  return rows[0]?.page_urls ?? [];
}

// Remove every synced row for a series (reading progress + published pages).
// Row-level security already scopes deletes to the signed-in user, so this
// needs no backend change. The device is untouched — only the synced copy
// goes, which is exactly what clearing stale rows from an old library needs.
async function deleteSeries(session, series) {
  const filters = [
    `chapter_key=eq.${encodeURIComponent(series)}`, // a loose root chapter
    `chapter_key=like.${encodeURIComponent(series + "/*")}`, // series/…
  ];
  for (const table of ["reading_progress", "chapter_pages"]) {
    for (const filter of filters) {
      await authedFetch(`${table}?${filter}`, { method: "DELETE" }).catch(() => {});
    }
  }
}

// --- "Send to Kobo" queue -------------------------------------------------
//
// The web can't run the Kobo's source search, so we just enqueue a title; the
// device searches for it on its next sync and offers the results to add. All
// three calls go straight through PostgREST under row-level security (user_id
// defaults to auth.uid() on insert).

async function fetchSends() {
  const res = await authedFetch(
    "send_queue?status=eq.pending&select=id,title,cover_url,created_at&order=created_at.desc"
  );
  if (!res.ok) return [];
  return res.json();
}
async function enqueueSend(title, coverUrl) {
  const res = await authedFetch("send_queue", {
    method: "POST",
    headers: { "Content-Type": "application/json", Prefer: "return=representation" },
    body: JSON.stringify(coverUrl ? { title, cover_url: coverUrl } : { title }),
  });
  if (!res.ok) {
    throw new Error(res.status === 401 ? "Session expired — sign in again" : `Couldn't send (${res.status})`);
  }
  return res.json();
}
async function deleteSend(id) {
  return authedFetch(`send_queue?id=eq.${encodeURIComponent(id)}`, { method: "DELETE" });
}

// --- Discover: manga recommendations from your MyAnimeList -----------------
//
// A connected account (or any public MAL username) yields two kinds of picks,
// each with a one-tap "Send to Kobo" into the existing send_queue:
//
//   1. "Read the source" — the source manga of the anime they rated highest.
//   2. "More like what you love" — community recommendations seeded from
//      those source manga AND from the manga they've already read, so a
//      reader with no anime list still gets real suggestions.
//
// All data is first-party MyAnimeList through the same-origin serverless
// proxy (api/mal.js). MAL can still have a bad day, so every step degrades
// to a clear, retryable error state instead of a spinner that never ends.

// How many top-rated anime / read manga seed the recommendations, and how
// many cards we show per section — a phone screen only fits so much.
const REC_SEEDS = 8;
const REC_READ_SEEDS = 6;
const REC_MAX_PER_SECTION = 12;

// Titles compare loosely across MAL/library dirs ("Frieren: Beyond
// Journey's End" vs "Frieren_ Beyond Journey's End").
function normTitle(s) {
  return String(s).toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// One recommendation card everywhere: { title, cover, score (0-100 community
// score or null), reason }.

// -- MAL official API via the same-origin proxy (api/mal.js) --

async function malGet(path) {
  const res = await fetch(`api/mal?path=${encodeURIComponent(path)}`);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(
      data.error === "proxy-unconfigured"
        ? "MyAnimeList isn't configured for this site yet."
        : data.message || data.error || `MyAnimeList error (${res.status})`
    );
  }
  return data;
}

// --- "Connect MyAnimeList" (per-user OAuth) ---------------------------------
//
// One tap connects the reader's own MAL account: the browser runs the PKCE
// dance against MAL's authorize page, and /api/mal-oauth (which holds the
// client secret) exchanges the code. Tokens live only in this browser,
// scoped to the signed-in gideon account so a shared browser never leaks a
// connection between users.

const MAL_PKCE_KEY = "gideon.mal.pkce";

const malKeyFor = (email) => `gideon.mal.${email || "anon"}`;
function malKey() {
  return malKeyFor(state.session?.email || loadSession()?.email);
}
function malConn() {
  try {
    return JSON.parse(localStorage.getItem(malKey()));
  } catch {
    return null;
  }
}
function saveMalConn(c) {
  localStorage.setItem(malKey(), JSON.stringify(c));
}
function clearMalConn() {
  localStorage.removeItem(malKey());
}

function randToken(bytes) {
  const a = new Uint8Array(bytes);
  crypto.getRandomValues(a);
  return btoa(String.fromCharCode(...a)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

async function startMalConnect() {
  const res = await fetch("api/mal-oauth?action=config").catch(() => null);
  const cfg = res?.ok ? await res.json().catch(() => null) : null;
  if (!cfg?.client_id) {
    throw new Error("MyAnimeList connection isn't configured on this deployment.");
  }
  const verifier = randToken(64); // 86 chars — valid PKCE (43–128)
  const pkceState = randToken(16);
  // localStorage, not sessionStorage: phone OAuth round-trips can come back
  // in a new tab or from an in-app browser, which drops sessionStorage. A
  // 15-minute expiry keeps abandoned attempts from lingering. The record
  // carries WHO started the dance, so the returning tokens land under that
  // account even if the gideon session is gone by then — never under a
  // shared anonymous key another user could inherit.
  localStorage.setItem(
    MAL_PKCE_KEY,
    JSON.stringify({
      verifier,
      state: pkceState,
      at: Date.now(),
      email: state.session?.email || loadSession()?.email || null,
    })
  );
  const u = new URL("https://myanimelist.net/v1/oauth2/authorize");
  u.search = new URLSearchParams({
    response_type: "code",
    client_id: cfg.client_id,
    // MyAnimeList implements PKCE with the `plain` method ONLY — it has no
    // S256 support, and sending a hashed challenge makes every exchange fail
    // with invalid_grant. So the challenge IS the verifier here. Do not
    // "harden" this to S256 without checking MAL's API first; the per-attempt
    // random verifier + state check are what protect the flow.
    code_challenge: verifier,
    code_challenge_method: "plain",
    state: pkceState,
    redirect_uri: cfg.redirect_uri,
  });
  location.href = u;
}

// Completes the dance when MAL redirects back with ?code and our state.
// Returns true if this load was an OAuth return (ours), false otherwise.
async function finishMalConnect() {
  const q = new URLSearchParams(location.search);
  const code = q.get("code");
  const returnedState = q.get("state");
  // Our authorize URL always carries state; a bare ?code from some unrelated
  // link is not ours — leave the URL and everything else alone.
  if (!code || !returnedState) return false;
  let pending = null;
  try {
    pending = JSON.parse(localStorage.getItem(MAL_PKCE_KEY));
  } catch {}
  localStorage.removeItem(MAL_PKCE_KEY);
  // Scrub only our own params, and preserve the fragment — a Supabase
  // recovery link's #access_token rides in the hash and is adopted by the
  // next boot step; wiping it here would burn the reset link.
  const scrub = () => {
    q.delete("code");
    q.delete("state");
    const qs = q.toString();
    history.replaceState(null, "", location.pathname + (qs ? `?${qs}` : "") + location.hash);
  };
  scrub();
  // An already-connected browser seeing a code again is a replay (Back
  // button, history revisit, second tab) — the connected panel already says
  // everything; an error telling them to "tap Connect" would be wrong and
  // impossible (that button doesn't render while connected).
  if (malConn()) return true;
  state.tab = "discover";
  if (!pending) {
    // The dance started in a different browser than it finished in (phones
    // hop between in-app browsers and Safari) — the code is useless without
    // the verifier held over there. Never leave this silent: say what
    // happened and what one tap fixes it.
    state.malError =
      "Almost connected — that sign-in finished in a different browser than it started in. Tap Connect once more, right here.";
    return true;
  }
  if (returnedState !== pending.state || Date.now() - pending.at > 15 * 60_000) {
    // Bad state or stale attempt: quiet, actionable, no OAuth vocabulary.
    state.malError = "That sign-in didn't finish — tap Connect again.";
    return true;
  }
  try {
    const res = await fetch("api/mal-oauth?action=token", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code, verifier: pending.verifier }),
    });
    const tok = await res.json().catch(() => ({}));
    if (!res.ok || !tok.access_token) throw new Error(tok.error || "token exchange failed");
    const conn = {
      access_token: tok.access_token,
      refresh_token: tok.refresh_token,
      expires_at: Math.floor(Date.now() / 1000) + (tok.expires_in || 3600),
      username: null,
    };
    // Learn who this is, for the UI and for recommendations (best-effort).
    const me = await fetch(`api/mal?path=${encodeURIComponent("users/@me")}`, {
      headers: { "X-MAL-USER-TOKEN": conn.access_token },
    })
      .then((r) => (r.ok ? r.json() : null))
      .catch(() => null);
    conn.username = me?.name || null;
    // Store under the account that STARTED the dance — even if this browser
    // is signed out right now, the tokens belong to that user and will be
    // there the moment they sign back in. Never a shared anonymous slot.
    localStorage.setItem(malKeyFor(pending.email || state.session?.email || loadSession()?.email), JSON.stringify(conn));
    state.malToast = `MyAnimeList connected${conn.username ? ` as ${conn.username}` : ""} ✓`;
  } catch (e) {
    // OAuth error codes mean nothing to a reader; say what to do instead.
    state.malError = /invalid_grant|expired/i.test(e.message || "")
      ? "That sign-in didn't go through — tap Connect MyAnimeList to try again."
      : `Couldn't connect MyAnimeList — ${e.message || "try again"}.`;
  }
  return true;
}

// A valid user access token, refreshing near expiry. Null = disconnected
// (dead refresh token); the UI falls back to the Connect button.
//
// Hardened per review: single-flight (two tabs refreshing concurrently would
// burn the rotated pair), persist-before-anything-else, and only a definitive
// invalid_grant disconnects — a MAL 5xx keeps the old token and tries again
// later rather than forcing everyone back through OAuth on a blip.
let malRefreshInFlight = null;
async function malUserToken() {
  const conn = malConn();
  if (!conn?.access_token) return null;
  if (conn.expires_at - 300 > Math.floor(Date.now() / 1000)) return conn.access_token;
  if (!malRefreshInFlight) {
    malRefreshInFlight = (async () => {
      try {
        const res = await fetch("api/mal-oauth?action=refresh", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ refresh_token: conn.refresh_token }),
        });
        const tok = await res.json().catch(() => ({}));
        if (res.ok && tok.access_token) {
          saveMalConn({
            ...conn,
            access_token: tok.access_token,
            refresh_token: tok.refresh_token,
            expires_at: Math.floor(Date.now() / 1000) + (tok.expires_in || 3600),
          });
          return tok.access_token;
        }
        if (res.status === 400 || res.status === 401) {
          // Definitive rejection. Another tab may have rotated the pair
          // under us — re-read before declaring the connection dead.
          const latest = malConn();
          if (latest && latest.refresh_token !== conn.refresh_token) return latest.access_token;
          clearMalConn();
          return null;
        }
        return conn.access_token; // transient upstream trouble: keep going
      } catch {
        return conn.access_token; // network blip: keep going
      } finally {
        malRefreshInFlight = null;
      }
    })();
  }
  return malRefreshInFlight;
}

const malCover = (n) => n?.main_picture?.large || n?.main_picture?.medium || null;
const malScore = (mean) => (mean ? Math.round(mean * 10) : null);

// --- provider operations (official MAL API, via the proxy) ---
//
// Personal reads always use the connected account's token — the connection
// is the only doorway to recommendations, so private lists just work.

// The user's completed anime, as { id, title, score }. May be empty — a
// manga-only reader still gets recommendations from what they've read.
async function opAnimeList() {
  const d = await malUserGet(
    "users/@me/animelist?status=completed&limit=1000&fields=list_status&nsfw=true"
  );
  return (d.data || [])
    .filter((e) => e.node?.id)
    .map((e) => ({ id: e.node.id, title: e.node.title || "", score: e.list_status?.score || 0 }));
}

// Their manga list, as { id, title, score, chapters } — used both to exclude
// what they've already read and to SEED "because you read X" picks.
async function opMangaList() {
  const d = await malUserGet("users/@me/mangalist?limit=1000&fields=list_status&nsfw=true");
  return (d.data || [])
    .filter((e) => e.node?.id)
    .map((e) => ({
      id: e.node.id,
      title: e.node.title || "",
      score: e.list_status?.score || 0,
      chapters: e.list_status?.num_chapters_read || 0,
    }));
}

// The source manga behind an anime: { id, title, cover, score } or null.
async function opSourceManga(anime) {
  // MAL's related_manga on anime is often empty (a long-standing API gap),
  // so fall back to searching the manga catalog by the anime's title.
  const d = await malGet(`anime/${anime.id}?fields=related_manga`).catch(() => null);
  const rel = (d?.related_manga || []).find((r) => r.node?.id);
  if (rel) {
    return { id: rel.node.id, title: rel.node.title || "", cover: malCover(rel.node), score: null };
  }
  if (normTitle(anime.title).length < 3) return null;
  const s = await malGet(
    `manga?q=${encodeURIComponent(anime.title.slice(0, 64))}&limit=1&fields=mean,main_picture`
  );
  const n = s.data?.[0]?.node;
  return n ? { id: n.id, title: n.title || "", cover: malCover(n), score: malScore(n.mean) } : null;
}

// A manga's meta + community recommendations in one call:
// { cover, score, recs: [{ id, title, cover }] }.
async function opMangaFull(id) {
  const d = await malGet(`manga/${id}?fields=mean,main_picture,recommendations`);
  return {
    cover: malCover(d),
    score: malScore(d.mean),
    recs: (d.recommendations || [])
      .filter((r) => r.node?.id)
      .map((r) => ({ id: r.node.id, title: r.node.title || "", cover: malCover(r.node) })),
  };
}

async function malRecommend(onStatus) {
  onStatus("Reading your MyAnimeList…");
  const anime = await opAnimeList();
  const mangaList = await opMangaList().catch(() => []);
  if (!anime.length && !mangaList.length) {
    throw new Error(
      "This MyAnimeList has nothing on it yet — rate a few anime, or sync your Kobo reading, and picks will appear."
    );
  }
  const alreadyReading = new Set(mangaList.map((e) => normTitle(e.title)).filter(Boolean));

  // Anime-derived: the source manga of their top-rated shows.
  const sources = [];
  if (anime.length) {
    onStatus("Finding the manga behind your favorites…");
    const seeds = anime
      .slice()
      .sort((a, b) => (b.score || 0) - (a.score || 0))
      .slice(0, REC_SEEDS);
    for (const e of seeds) {
      const m = await opSourceManga(e).catch(() => null);
      if (!m) continue;
      sources.push({
        id: m.id,
        title: m.title,
        cover: m.cover || null,
        score: m.score ?? null,
        reason: e.score
          ? `You rated the anime ${e.score}/10 — read the source`
          : `You watched ${e.title} — read the source`,
      });
    }
  }

  // Similar picks, seeded from those source manga AND from what they've
  // actually read (their best-loved / furthest-read manga) — so a reader
  // with no anime list still gets real suggestions.
  onStatus("Looking for more like what you love…");
  const readSeeds = mangaList
    .slice()
    .sort((a, b) => (b.score || 0) - (a.score || 0) || (b.chapters || 0) - (a.chapters || 0))
    .slice(0, REC_READ_SEEDS);
  const seeds = [
    ...sources.map((s, i) => ({ id: s.id, title: s.title, fill: s, collect: i < 3, read: false })),
    ...readSeeds.map((m) => ({ id: m.id, title: m.title, fill: null, collect: true, read: true })),
  ];
  const similar = [];
  const seen = new Set();
  for (const seed of seeds) {
    if (seen.has(seed.id)) continue;
    seen.add(seed.id);
    const full = await opMangaFull(seed.id).catch(() => null);
    if (!full) continue;
    if (seed.fill) {
      seed.fill.cover = seed.fill.cover || full.cover;
      seed.fill.score = seed.fill.score ?? full.score;
    }
    if (seed.collect) {
      for (const r of full.recs.slice(0, 4)) {
        similar.push({
          ...r,
          score: null,
          reason: seed.read ? `Because you read ${seed.title}` : `Loved by readers of ${seed.title}`,
        });
      }
    }
  }
  return { sources, similar, alreadyReading };
}

// -- rankings / search --
//
// One card shape everywhere: { title, cover, score (0-100 or null), reason }.
// Rankings and search don't filter against the library — you're allowed to look
// at what you own — the card just shows "queued" instead of a Send button.

async function malBrowse({ search, mode, limit = 18 }) {
  const fields = "mean,genres,main_picture,media_type,nsfw";
  const path = search
    ? `manga?q=${encodeURIComponent(search)}&limit=${limit}&fields=${fields}`
    : `manga/ranking?ranking_type=${mode === "top" ? "all" : "bypopularity"}&limit=${limit}&fields=${fields}`;
  const d = await malGet(path);
  return (d.data || [])
    .map((r) => r.node)
    .filter(Boolean)
    .filter((n) => !["light_novel", "novel"].includes(n.media_type) && (n.nsfw ?? "white") === "white")
    .map((n) => ({
      title: n.title || "",
      cover: malCover(n),
      score: malScore(n.mean),
      genres: (n.genres || []).map((g) => g.name),
      reason: (n.genres || []).slice(0, 3).map((g) => g.name).join(" · "),
    }));
}

const opBrowse = (opts) => malBrowse(opts);

// Community score for one library title: { score } (null when unknown).
async function opRating(title) {
  if (normTitle(title).length < 3) return { score: null };
  const d = await malGet(`manga?q=${encodeURIComponent(title.slice(0, 64))}&limit=1&fields=mean`);
  return { score: malScore(d.data?.[0]?.node?.mean) };
}

// --- one rail, many pills ---------------------------------------------------
//
// Browse and recommendations used to be two stacked grids. They are one
// left-to-right library now, and the pills above it choose what fills it:
// "For you" (the MyAnimeList picks), Trending / Top rated (MAL rankings), or
// any genre. MAL's API has no genre query, so genre rails are filtered out of
// a single cached top-manga pool — one 200-title fetch instead of a request
// per pill tap.

const DISC_GENRES = [
  "Action", "Adventure", "Comedy", "Drama", "Fantasy", "Horror",
  "Mystery", "Romance", "Sci-Fi", "Slice of Life", "Sports", "Supernatural",
];
const GENRE_POOL_LIMIT = 200;
const GENRE_RAIL_MAX = 24;

function discPills() {
  return [
    { id: "foryou", label: "For you" },
    { id: "trending", label: "Trending" },
    { id: "top", label: "Top rated" },
    ...DISC_GENRES.map((g) => ({ id: `genre:${g}`, label: g })),
  ];
}

// Stable, readable test ids: pill-foryou, pill-genre-slice-of-life, …
const pillTestId = (id) =>
  `pill-${id.replace("genre:", "genre-").toLowerCase().replace(/[^a-z0-9]+/g, "-")}`;

// Signed out of MAL there is nothing personal to show, so the rail opens on
// Trending — day one is still a library you can scroll.
function currentPill() {
  const want = state.pill || (malConn() ? "foryou" : "trending");
  return discPills().some((p) => p.id === want) ? want : "trending";
}

// The shared genre pool: one top-manga fetch, filtered per pill. A failure
// clears the cache so the next Retry actually retries.
function genrePool() {
  if (!state.genrePool) {
    state.genrePool = opBrowse({ mode: "top", limit: GENRE_POOL_LIMIT }).catch((e) => {
      state.genrePool = null;
      throw e;
    });
  }
  return state.genrePool;
}

async function railCards(pill) {
  if (pill === "trending" || pill === "top") return opBrowse({ mode: pill });
  const genre = pill.slice("genre:".length);
  const pool = await genrePool();
  return pool.filter((c) => (c.genres || []).includes(genre)).slice(0, GENRE_RAIL_MAX);
}

async function runRail(pill, email, rows) {
  state.rails[pill] = { phase: "loading" };
  patchDiscover(email, rows);
  try {
    const cards = await railCards(pill);
    state.rails[pill] = cards.length
      ? { phase: "done", cards }
      : { phase: "error", error: "Nothing here yet — try another pill." };
  } catch (e) {
    state.rails[pill] = { phase: "error", error: e.message || "Couldn't load." };
  }
  patchDiscover(email, rows);
}

// Load whatever the selected pill needs, once. "For you" runs the (much
// slower) recommendation engine; every other pill is a ranking fetch.
function ensureRail(email, rows) {
  if (state.tab !== "discover" || state.search) return;
  const pill = currentPill();
  if (pill === "foryou") {
    if (malConn() && !state.discover) runDiscover(email, rows);
    return;
  }
  if (!state.rails[pill]) runRail(pill, email, rows);
}

function selectPill(id, email, rows) {
  state.pill = id;
  patchDiscover(email, rows);
  ensureRail(email, rows);
}

// Update just the rail. A full renderDashboard here would wipe whatever the
// reader is typing into the search box while a slow rail arrives — and it
// would reset an in-flight "Sending…" button — so patch the one region.
function patchDiscover(email, rows) {
  if (state.tab !== "discover" || state.search) return;
  const body = document.getElementById("disc-body");
  if (!body) {
    renderDashboard(email, rows);
    return;
  }
  body.innerHTML = discBodyHtml();
  wireDiscover(email, rows);
}

// Handlers for everything inside #disc-body (pills, connect, retry, card
// sends) — called after both a full render and an in-place patch.
function wireDiscover(email, rows) {
  const body = document.getElementById("disc-body");
  if (!body) return;
  for (const pill of body.querySelectorAll(".pill")) {
    pill.addEventListener("click", () => selectPill(pill.getAttribute("data-pill"), email, rows));
  }
  body.querySelector("#disc-retry")?.addEventListener("click", () => {
    const pill = currentPill();
    if (pill === "foryou") state.discover = null;
    else delete state.rails[pill];
    patchDiscover(email, rows);
    ensureRail(email, rows);
  });
  body.querySelector("#mal-connect")?.addEventListener("click", () => {
    startMalConnect().catch((e) => {
      state.malError = e.message || "Couldn't start the MyAnimeList connection.";
      renderDashboard(email, rows);
    });
  });
  for (const btn of body.querySelectorAll('[data-testid="rec-send"]')) wireRecSend(btn);
}

// One card's Send button: optimistic in-place states (Sending… → Sent ✓),
// with an inline error and re-arm on failure — no re-render, so the grid
// never jumps back to the top.
function wireRecSend(btn) {
  btn.addEventListener("click", async () => {
    const title = btn.getAttribute("data-title");
    const cover = btn.getAttribute("data-cover") || null;
    const key = normTitle(title);
    // Claim the title BEFORE the request: any re-render while it's in flight
    // must rebuild the card as already-sent, or a second tap would queue the
    // same manga twice.
    if (isQueued(title)) return;
    (state.sentTitles ||= new Set()).add(key);
    btn.disabled = true;
    btn.textContent = "Sending…";
    try {
      const [row] = await enqueueSend(title, cover);
      if (row) state.sends = [row, ...state.sends];
      btn.textContent = "Sent to Kobo ✓";
      btn.classList.add("sent");
    } catch (err) {
      state.sentTitles.delete(key); // failed — allow a retry
      btn.disabled = false;
      btn.textContent = "Send to Kobo";
      btn.insertAdjacentHTML(
        "afterend",
        `<div class="note rec-error" data-testid="rec-error">${esc(err.message || "Couldn't send.")}</div>`
      );
      setTimeout(() => btn.parentElement?.querySelector(".rec-error")?.remove(), 4000);
    }
  });
}

async function runSearch(q, email, rows) {
  state.search = { phase: "loading", q };
  renderDashboard(email, rows);
  try {
    const cards = await opBrowse({ search: q });
    state.search = cards.length
      ? { phase: "done", q, cards }
      : { phase: "error", q, error: `Nothing found for “${q}”.` };
  } catch (e) {
    state.search = { phase: "error", q, error: e.message || "Search failed." };
  }
  if (state.tab === "discover") renderDashboard(email, rows);
}

// -- community ratings for the library (decor; cached) --
//
// Library titles come from the device's directory names, so each one is
// resolved with a MAL search (throttled — one request per unresolved title
// per week; the localStorage cache absorbs the rest). Failures (MAL
// outages) just mean no stars.

const RATING_CACHE_KEY = "gideon.ratings";
const RATING_TTL_MS = 7 * 86400e3;

function loadRatingCache() {
  try {
    return JSON.parse(localStorage.getItem(RATING_CACHE_KEY)) || {};
  } catch {
    return {};
  }
}

async function fetchLibraryRatings(titles) {
  const cache = loadRatingCache();
  const nowMs = Date.now();
  const fresh = (t) => cache[normTitle(t)] && nowMs - cache[normTitle(t)].at < RATING_TTL_MS;
  const missing = [...new Set(titles.filter((t) => !fresh(t)))];
  for (const t of missing) {
    const r = await opRating(t).catch(() => null);
    // Outage or rate limit: stop the whole sweep — hammering a down API only
    // makes things worse. Unresolved titles stay uncached, so a later visit
    // picks up where this one stopped.
    if (r === null) break;
    cache[normTitle(t)] = { s: r.score, at: nowMs };
  }
  localStorage.setItem(RATING_CACHE_KEY, JSON.stringify(cache));
  return new Map(titles.map((t) => [normTitle(t), cache[normTitle(t)]?.s ?? null]));
}

// The community score for a library series, if we've resolved one.
function seriesRating(series) {
  return state.ratings?.get(normTitle(displayTitle(series))) ?? null;
}

// --- authenticated MAL calls (through the proxy, user token forwarded) ------

async function malUserGet(path) {
  const token = await malUserToken();
  if (!token) {
    throw Object.assign(new Error("MyAnimeList needs a reconnect."), { reconnect: true });
  }
  const res = await fetch(`api/mal?path=${encodeURIComponent(path)}`, {
    headers: { "X-MAL-USER-TOKEN": token },
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw Object.assign(new Error(data.error || `MyAnimeList error (${res.status})`), {
      status: res.status,
    });
  }
  return data;
}

async function malUserPatch(path, fields) {
  const token = await malUserToken();
  if (!token) {
    throw Object.assign(new Error("MyAnimeList needs a reconnect."), { reconnect: true });
  }
  const res = await fetch(`api/mal?path=${encodeURIComponent(path)}`, {
    method: "PATCH",
    headers: { "X-MAL-USER-TOKEN": token, "Content-Type": "application/json" },
    body: JSON.stringify(fields),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw Object.assign(new Error(data.error || `MyAnimeList error (${res.status})`), {
      status: res.status,
    });
  }
  return data;
}

// --- Sync the Kobo's reading history onto the connected MAL list ------------
//
// Safety rules from review: only write entries whose normalized title matches
// the MAL search result EXACTLY (a fuzzy write could scribble on a stranger
// series in the user's real list — the one failure recovery can't undo);
// never move MAL progress backwards (furthest-wins, gideon's cardinal rule,
// applied to the MAL side too); duplicates of one MAL entry across library
// folders merge to the furthest chapter. Idempotent, so re-running resumes.

const seriesQuery = (title) =>
  title.replace(/\s*\([^)]*\)/g, "").replace(/^manga[\s-]+/i, "").trim() || title;

async function syncKoboToMal(email, rows) {
  if (!malConn() || state.malSync?.phase === "running") return;
  const groups = groupBySeries(rows);
  state.malSync = { phase: "running", note: "Reading your MAL list…", report: [] };
  patchMalSync();

  let existing;
  try {
    const d = await malUserGet("users/@me/mangalist?limit=1000&fields=list_status&nsfw=true");
    existing = new Map((d.data || []).filter((e) => e.node?.id).map((e) => [e.node.id, e.list_status || {}]));
  } catch (e) {
    state.malSync = {
      phase: "error",
      error: e.reconnect ? "MyAnimeList needs a reconnect first." : "Couldn't read your MAL list — try again.",
    };
    patchMalSync();
    return;
  }

  // Phase 1: match every series (exact title only), merging duplicate folders.
  const targets = new Map(); // mal_id -> { titles, finished, num_chapters }
  const report = [];
  state.malSync.report = report;
  let i = 0;
  for (const g of groups) {
    i++;
    state.malSync.note = `Matching ${i}/${groups.length}…`;
    patchMalSync();
    const title = displayTitle(g.series);
    const finished = g.chapters.filter((c) => c.total_pages > 0 && c.current_page + 1 >= c.total_pages).length;
    const q = seriesQuery(title);
    let match = null;
    try {
      await sleep(250);
      const d = await malGet(
        `manga?q=${encodeURIComponent(q.slice(0, 64))}&limit=3&fields=num_chapters,media_type`
      );
      match = (d.data || [])
        .map((x) => x.node)
        .filter((n) => !["light_novel", "novel"].includes(n.media_type))
        .find((n) => normTitle(n.title) === normTitle(q));
    } catch {
      report.push({ title, outcome: "skipped", note: "search unavailable — run sync again later" });
      continue;
    }
    if (!match) {
      report.push({ title, outcome: "skipped", note: "no confident match — add it on MAL by hand" });
      continue;
    }
    const t = targets.get(match.id) || {
      titles: [],
      malTitle: match.title,
      finished: 0,
      num_chapters: match.num_chapters || 0,
    };
    t.titles.push(title);
    t.finished = Math.max(t.finished, finished);
    targets.set(match.id, t);
  }

  // Phase 2: furthest-wins writes.
  let updated = 0;
  let j = 0;
  for (const [id, t] of targets) {
    j++;
    state.malSync.note = `Updating ${j}/${targets.size}…`;
    patchMalSync();
    const ls = existing.get(id);
    const current = ls?.num_chapters_read || 0;
    const target = Math.max(t.finished, current);
    if (ls && target <= current) {
      report.push({ title: t.malTitle, outcome: "kept", note: `MAL already at ${current} ch` });
      continue;
    }
    const status =
      (t.num_chapters > 0 && target >= t.num_chapters) || ls?.status === "completed"
        ? "completed"
        : "reading";
    try {
      await sleep(250);
      await malUserPatch(`manga/${id}/my_list_status`, { status, num_chapters_read: target });
      updated++;
      report.push({ title: t.malTitle, outcome: "updated", note: `${status}, ${target} ch` });
    } catch (e) {
      if (e.status === 429 || e.status >= 500) {
        // One paused retry; if MAL is really struggling, stop — the run is
        // idempotent, so "Sync again" resumes exactly where this left off.
        await sleep(2000);
        try {
          await malUserPatch(`manga/${id}/my_list_status`, { status, num_chapters_read: target });
          updated++;
          report.push({ title: t.malTitle, outcome: "updated", note: `${status}, ${target} ch` });
          continue;
        } catch {}
        state.malSync = {
          phase: "error",
          error: "MyAnimeList is rate-limiting — tap Sync again in a minute to resume.",
          report,
        };
        patchMalSync();
        return;
      }
      report.push({ title: t.malTitle, outcome: "skipped", note: "MAL rejected the update" });
    }
  }

  state.malSync = { phase: "done", updated, report };
  patchMalSync();
}

// Dedupe, drop what they already have (gideon library, pending sends, their
// own manga list), cap each section.
function buildRecommendations({ sources, similar, alreadyReading }) {
  const libSeries = new Set(
    (state.rows || []).map((r) => normTitle(displayTitle(parseKey(r.chapter_key).series)))
  );
  const pending = new Set((state.sends || []).map((s) => normTitle(s.title)));
  const skip = (t) => !t || libSeries.has(t) || pending.has(t) || alreadyReading.has(t);

  const seen = new Set();
  const take = (list) => {
    const out = [];
    for (const rec of list) {
      const key = normTitle(rec.title);
      if (skip(key) || seen.has(key)) continue;
      seen.add(key);
      out.push(rec);
      if (out.length >= REC_MAX_PER_SECTION) break;
    }
    return out;
  };
  return { sources: take(sources), similar: take(similar) };
}

async function runDiscover(email, rows) {
  state.discover = { phase: "loading", status: "Reading your MyAnimeList…" };
  patchDiscover(email, rows);
  const onStatus = (msg) => {
    if (state.discover?.phase === "loading") state.discover.status = msg;
    const el = document.getElementById("disc-status");
    if (el) el.textContent = msg;
  };
  try {
    const raw = await malRecommend(onStatus);
    const recs = buildRecommendations(raw);
    if (!recs.sources.length && !recs.similar.length) {
      throw new Error("Nothing new to recommend — everything we found is already in your library or queue.");
    }
    state.discover = { phase: "done", recs };
  } catch (e) {
    state.discover = { phase: "error", error: e.message || "Something went wrong." };
  }
  patchDiscover(email, rows);
}

// Session + resume state, so the reader can push progress and return home.
const state = { session: null, resume: {}, sends: [], rails: {} };

// --- theme (defaults to dark; a header toggle persists the choice) ---------

const THEME_KEY = "gideon.theme";
function currentTheme() {
  return localStorage.getItem(THEME_KEY) || "dark";
}
function applyTheme(t) {
  document.documentElement.dataset.theme = t;
}
function themeButtonHtml() {
  const dark = currentTheme() === "dark";
  return `<button class="theme-toggle" id="theme" data-testid="theme" title="Switch to ${dark ? "light" : "dark"} mode" aria-label="Toggle theme">${dark ? "☀️" : "🌙"}</button>`;
}
function wireThemeButton() {
  const btn = document.getElementById("theme");
  if (!btn) return;
  btn.addEventListener("click", () => {
    const next = currentTheme() === "dark" ? "light" : "dark";
    localStorage.setItem(THEME_KEY, next);
    applyTheme(next);
    btn.textContent = next === "dark" ? "☀️" : "🌙";
    btn.title = `Switch to ${next === "dark" ? "light" : "dark"} mode`;
  });
}
applyTheme(currentTheme());

// --- rendering ------------------------------------------------------------

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// Display cleanup for titles that came through the device's FAT32-safe
// filename sanitizer: characters like ':' '?' '*' were stored as '_' in the
// directory name (which is also the sync key). Collapse underscore runs to a
// space for DISPLAY only — keys stay untouched, so progress and grouping are
// unaffected. A title that was all underscores keeps its original form rather
// than vanishing.
function displayTitle(s) {
  const tidy = String(s).replace(/_+/g, " ").replace(/\s+/g, " ").trim();
  return tidy || String(s);
}

// --- library view (cover shelf by default, list on demand) -----------------

const LIBVIEW_KEY = "gideon.libview";
function libView() {
  return localStorage.getItem(LIBVIEW_KEY) === "list" ? "list" : "grid";
}

// --- hidden titles (per account, local to this browser) --------------------

function hiddenKey(email) {
  return `gideon.hidden.${email || "anon"}`;
}
function loadHidden(email) {
  try {
    return new Set(JSON.parse(localStorage.getItem(hiddenKey(email))) || []);
  } catch {
    return new Set();
  }
}
function saveHidden(email, set) {
  localStorage.setItem(hiddenKey(email), JSON.stringify([...set]));
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
    <div class="head"><div class="brand">gideon <span>· sync</span></div>${themeButtonHtml()}</div>
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
  wireThemeButton();
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

// A chapter is "finished" when the last page has been reached.
function isFinished(r) {
  return r.total_pages > 0 && r.current_page + 1 >= r.total_pages;
}

// Compact "3 days" / "5 hours" / "under an hour" for time-to-completion.
function humanDuration(ms) {
  const hours = ms / 3600000;
  if (hours < 1) return "under an hour";
  if (hours < 48) return `${Math.round(hours)} hour${Math.round(hours) === 1 ? "" : "s"}`;
  const days = Math.round(hours / 24);
  return `${days} day${days === 1 ? "" : "s"}`;
}

// Per-series insights for the library card, from the synced rows alone:
// first-read day (earliest started_at, falling back to updated_at for rows
// that predate migration 0004), completion across the *tracked* chapters, and
// — once every tracked chapter is finished — how long start-to-finish took.
function seriesInsights(g) {
  const startOf = (c) => c.started_at || c.updated_at;
  const started = g.chapters.map(startOf).sort()[0];
  const finished = g.chapters.filter(isFinished);
  const complete = g.chapters.length > 0 && finished.length === g.chapters.length;
  const lastFinish = finished.map((c) => c.updated_at).sort().at(-1);
  const span =
    complete && started && lastFinish
      ? humanDuration(Math.max(0, new Date(lastFinish) - new Date(started)))
      : null;
  const day = started ? dayKey(started) : null;
  return {
    firstRead: day ? prettyDate(day) : null,
    finished: finished.length,
    tracked: g.chapters.length,
    complete,
    span,
  };
}

// A series' cover: the first page of its numerically-first chapter with
// published page URLs. Empty map when nothing is published yet.
function coverBySeries(pageRows) {
  const best = new Map();
  for (const row of pageRows || []) {
    const url = row?.page_urls?.[0];
    if (!url || typeof row.chapter_key !== "string") continue;
    const { series } = parseKey(row.chapter_key);
    const prev = best.get(series);
    if (!prev || row.chapter_key.localeCompare(prev.key, undefined, { numeric: true }) < 0) {
      best.set(series, { key: row.chapter_key, url });
    }
  }
  return new Map([...best].map(([s, v]) => [s, v.url]));
}

function computeStats(rows) {
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
// Cut points the cells shade against: the 25th, 50th and 75th percentile of
// the days that had any reading. Scaling against the busiest day instead —
// which this did — means one 400-page afternoon against a habit of 30 drags
// every ordinary day onto level 1, leaving a pale wash with a single dark
// square. `heatmap_thresholds` in crates/gideon-core/src/stats.rs does the
// same arithmetic so the device and the dashboard shade a day identically.
function heatmapLevels(byDay) {
  const counts = [...byDay.values()].filter((v) => v > 0).sort((a, b) => a - b);
  if (!counts.length) return [0, 0, 0];
  const at = (pct) => counts[Math.min(counts.length - 1, Math.floor((pct * counts.length) / 100))];
  return [at(25), at(50), at(75)];
}

// Level 0..4 for a day, against those cut points. All three equal means every
// active day read the same amount: nothing to rank, so they share one mid tone
// rather than all collapsing onto the palest.
function heatmapLevel(val, levels) {
  if (!val) return 0;
  if (levels[0] === levels[2]) return 3;
  if (val <= levels[0]) return 1;
  if (val <= levels[1]) return 2;
  if (val <= levels[2]) return 3;
  return 4;
}

function heatmapHtml(s) {
  const WEEKS = 18;
  const levels = heatmapLevels(s.byDay);
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
        const lvl = heatmapLevel(val, levels);
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
        <div class="ts-name" title="${esc(displayTitle(t.name))}">${esc(displayTitle(t.name))}</div>
        <div class="ts-bar"><i style="width:${Math.round((t.count / max) * 100)}%"></i></div>
        <div class="ts-val">${t.count}</div>
      </div>`
    )
    .join("");
  return `<section class="panel"><div class="section-label">Most read</div><div class="ts-list">${rows}</div></section>`;
}

// One row per book (series), newest first — its most-recent chapter is where
// you are. Showing every chapter here was too cluttered; the Library tab has
// the per-chapter breakdown. Tapping opens the series' latest chapter.
function recentHtml(groups) {
  const items = groups
    .slice(0, 6)
    .map((g) => {
      const m = progressMeta(g.current);
      return `
        <button class="sub" data-testid="chapter" data-key="${esc(g.current.chapter_key)}">
          <span class="rc-title"><span class="rc-series">${esc(displayTitle(g.series))}</span></span>
          <span class="bar small"><i style="width:${m.pct}%"></i></span>
          <span class="ago">${esc(timeAgo(g.current.updated_at))}</span>
        </button>`;
    })
    .join("");
  return `<section class="panel"><div class="section-label">Recently read</div><div class="chapters">${items}</div></section>`;
}

// Enqueue-a-title panel + the list of what's still waiting on the device.
function sendPanelHtml(sends) {
  const list = sends.length
    ? `<div class="sends">${sends
        .map(
          (s) => `
        <div class="send-row" data-testid="send-item">
          ${s.cover_url ? `<img class="send-cover" src="${esc(s.cover_url)}" alt="" loading="lazy" referrerpolicy="no-referrer" />` : ""}
          <span class="send-title">${esc(s.title)}</span>
          <span class="ago">${esc(timeAgo(s.created_at))}</span>
          <button class="send-x" data-id="${esc(s.id)}" data-testid="send-remove" aria-label="remove">×</button>
        </div>`
        )
        .join("")}</div>`
    : `<p class="send-hint">Send a manga to your Kobo: type a title and it shows up on the device as a notification — tap it there to search your sources and add it.</p>`;
  return `<section class="panel send-panel">
    <div class="section-label">Send to Kobo</div>
    <form class="send-form" id="send-form">
      <input type="text" id="send-title" data-testid="send-input" placeholder="Manga title…" maxlength="512" autocomplete="off" />
      <button class="primary" type="submit" data-testid="send-btn">Send</button>
    </form>
    <div class="note send-note" id="send-note" data-testid="send-note"></div>
    ${list}
  </section>`;
}

function viewStats(stats, groups, sends) {
  return `${sendPanelHtml(sends)}
    ${statTilesHtml(stats)}
    <section class="panel">
      <div class="section-label">Reading activity</div>
      ${heatmapHtml(stats)}
    </section>
    ${topSeriesHtml(stats)}
    ${recentHtml(groups)}`;
}

// --- Discover view ----------------------------------------------------------

// Whether a title is already on its way to the Kobo (sent this session, or
// still pending in the queue) — shared by every card grid.
function isQueued(title) {
  const key = normTitle(title);
  return !!(
    state.sentTitles?.has(key) || (state.sends || []).some((s) => normTitle(s.title) === key)
  );
}

function recCardHtml(rec) {
  const sent = isQueued(rec.title);
  const art = rec.cover
    ? `<img class="rec-cover" src="${esc(rec.cover)}" alt="" loading="lazy" referrerpolicy="no-referrer" />`
    : `<span class="rec-cover rec-ph">${esc([...rec.title][0] || "?")}</span>`;
  return `
    <div class="rec-card" data-testid="rec-card">
      <div class="rec-art">
        ${art}
        ${rec.score ? `<span class="rec-score" title="Community score">★ ${(rec.score / 10).toFixed(1)}</span>` : ""}
      </div>
      <div class="rec-title" title="${esc(rec.title)}">${esc(rec.title)}</div>
      <div class="rec-reason">${esc(rec.reason)}</div>
      <button class="rec-send ${sent ? "sent" : ""}" data-testid="rec-send"
        data-title="${esc(rec.title)}" data-cover="${esc(rec.cover || "")}" ${sent ? "disabled" : ""}>
        ${sent ? "Sent to Kobo ✓" : "Send to Kobo"}
      </button>
    </div>`;
}

// A labelled rail in its own panel — used for search results. The Discover
// rail itself is unlabelled: the selected pill is the label.
function recSectionHtml(label, recs, testid) {
  if (!recs.length) return "";
  return `<section class="panel">
    <div class="section-label">${esc(label)}</div>
    ${railHtml(recs, testid)}
  </section>`;
}

// The library rail: cards two rows deep, scrolling left to right.
function railHtml(cards, testid) {
  return `<div class="rec-rail" data-testid="${testid}">${cards.map(recCardHtml).join("")}</div>`;
}

// The search bar sits above everything on the Discover tab; results replace
// the pills + rail until cleared.
function searchPanelHtml() {
  const s = state.search;
  return `<section class="panel">
    <form class="search-form" id="search-form">
      <input type="search" id="search-q" data-testid="search-input" placeholder="Search manga…"
        autocomplete="off" value="${esc(s?.q || "")}" />
      <button class="primary" type="submit" data-testid="search-btn">Search</button>
      ${s ? `<button class="ghost" type="button" id="search-clear" data-testid="search-clear">Clear</button>` : ""}
    </form>
  </section>`;
}

function searchResultsHtml() {
  const s = state.search;
  if (s.phase === "loading") {
    return `<section class="panel disc-loading" data-testid="search-loading">
      <div class="spinner" aria-hidden="true"></div>
      <div class="disc-status">Searching…</div>
    </section>`;
  }
  if (s.phase === "error") {
    return `<div class="note disc-error" data-testid="search-error">${esc(s.error)}</div>`;
  }
  return recSectionHtml(`Results for “${s.q}”`, s.cards, "search-results");
}

// The pill rail: one row of preferences, itself scrolling left to right so
// twelve genres fit a phone without a menu.
function pillsHtml() {
  const cur = currentPill();
  return `<div class="pills" role="tablist" aria-label="Picks" data-testid="disc-pills">${discPills()
    .map(
      (p) => `<button type="button" class="pill ${p.id === cur ? "on" : ""}" role="tab"
        aria-selected="${p.id === cur}" data-pill="${esc(p.id)}"
        data-testid="${esc(pillTestId(p.id))}">${esc(p.label)}</button>`
    )
    .join("")}</div>`;
}

// What the selected pill is showing right now — recommendations and rankings
// collapsed into one shape so the rail renders them identically.
function railState() {
  const pill = currentPill();
  if (pill !== "foryou") return state.rails[pill] || { phase: "loading" };
  if (!malConn()) return { phase: "connect" };
  const d = state.discover;
  if (!d || d.phase === "loading") return { phase: "loading", status: d?.status };
  if (d.phase === "error") return { phase: "error", error: d.error };
  return { phase: "done", cards: [...d.recs.sources, ...d.recs.similar] };
}

function discBodyHtml() {
  const s = railState();
  let body;
  if (s.phase === "connect") {
    body = connectCardHtml();
  } else if (s.phase === "loading") {
    body = `<div class="disc-loading" data-testid="disc-loading">
      <div class="spinner" aria-hidden="true"></div>
      <div class="disc-status" id="disc-status">${esc(s.status || "Loading…")}</div>
    </div>`;
  } else if (s.phase === "error") {
    body = `<div class="note disc-error" data-testid="disc-error">${esc(s.error)}
      <button class="ghost" id="disc-retry" data-testid="disc-retry">Retry</button></div>`;
  } else {
    body = railHtml(s.cards, "disc-rail");
  }
  return `${pillsHtml()}${body}`;
}

// One-shot MAL notices (connect success/failure), rendered at the top of
// the Discover tab wherever it is in its lifecycle.
function malNoticesHtml() {
  const toast = state.malToast
    ? `<div class="note ok mal-notice" data-testid="mal-toast">${esc(state.malToast)}</div>`
    : "";
  const err = state.malError
    ? `<div class="note disc-error mal-notice" data-testid="mal-error">${esc(state.malError)}</div>`
    : "";
  return toast + err;
}

// The one-time gateway: shown until the account is connected, and again only
// when the connection dies and needs a re-auth.
function connectCardHtml() {
  return `<div class="mal-connect-card" data-testid="mal-connect-card">
    <p class="send-hint">Connect your MyAnimeList and this rail fills with personal manga picks — from what you've read and what you've watched. One tap, private lists included.</p>
    <button class="primary" id="mal-connect" data-testid="mal-connect">Connect MyAnimeList</button>
  </div>`;
}

// Once connected the account hides away: a slim footer at the bottom of the
// tab, holding the sync action and Disconnect.
function malFooterHtml() {
  const conn = malConn();
  return `<section class="panel mal-footer" data-testid="mal-connected">
    <div class="mal-row">
      <span class="mal-badge" data-testid="mal-badge">✓ MyAnimeList</span>
      ${conn.username ? `<span class="mal-user">${esc(conn.username)}</span>` : ""}
      <span class="mal-actions">
        <button class="ghost" id="mal-sync" data-testid="mal-sync">Sync Kobo reading</button>
        <button class="ghost" id="mal-disconnect" data-testid="mal-disconnect">Disconnect</button>
      </span>
    </div>
    <div id="mal-sync-body">${malSyncBodyHtml()}</div>
  </section>`;
}

// The sync progress/report area inside the connected panel — patched in
// place (like Browse) so running progress never wipes other inputs.
function malSyncBodyHtml() {
  const s = state.malSync;
  if (!s) return "";
  if (s.phase === "running") {
    return `<div class="send-hint" data-testid="mal-sync-running">${esc(s.note || "Syncing…")}</div>`;
  }
  if (s.phase === "error") {
    return `<div class="note disc-error" data-testid="mal-sync-error">${esc(s.error)}</div>${malSyncReportHtml(s.report)}`;
  }
  return `<div class="send-hint" data-testid="mal-sync-done">Done — ${s.updated} update${s.updated === 1 ? "" : "s"} written to your MAL list.</div>${malSyncReportHtml(s.report)}`;
}

function malSyncReportHtml(report) {
  if (!report?.length) return "";
  const rows = report
    .map(
      (r) => `<div class="sync-row sync-${esc(r.outcome)}" data-testid="mal-sync-row">
        <span class="sync-outcome">${esc(r.outcome)}</span>
        <span class="sync-title">${esc(r.title)}</span>
        <span class="sync-note">${esc(r.note || "")}</span>
      </div>`
    )
    .join("");
  return `<div class="sync-report" data-testid="mal-sync-report">${rows}</div>`;
}

function patchMalSync() {
  const el = document.getElementById("mal-sync-body");
  if (el) el.innerHTML = malSyncBodyHtml();
}

// Search results replace the rail until cleared; otherwise the tab is the
// pills and the single library rail underneath them, with the connected
// MyAnimeList account tucked into a slim footer.
function viewDiscover() {
  const search = searchPanelHtml();
  if (state.search) return `${search}${searchResultsHtml()}`;
  const notices = malNoticesHtml();
  // Not connected, the offer follows you across every pill as a slim strip —
  // except on "For you", where the rail itself is already making the offer.
  const footer = malConn()
    ? malFooterHtml()
    : currentPill() === "foryou"
      ? ""
      : `<section class="panel mal-footer" data-testid="mal-connect-strip">
          <div class="mal-row">
            <span class="send-hint">Connect MyAnimeList for picks made from what you read and watch.</span>
            <span class="mal-actions"><button class="primary" id="mal-connect" data-testid="mal-connect">Connect</button></span>
          </div>
        </section>`;
  return `${search}${notices}
    <section class="panel"><div id="disc-body">${discBodyHtml()}</div></section>
    ${footer}`;
}

// One library card: cover (published page art, or a lettered placeholder),
// title, per-series insights (first read day · completion · time to
// completion), progress, and a hide/unhide control.
function libraryCardHtml(g, covers, hidden) {
  const { chapter } = parseKey(g.current.chapter_key);
  const meta = progressMeta(g.current);
  const ins = seriesInsights(g);
  const title = displayTitle(g.series);
  const cover = covers.get(g.series);
  const coverHtml = cover
    ? `<img class="cover" src="${esc(cover)}" alt="" loading="lazy" referrerpolicy="no-referrer" />`
    : `<span class="cover cover-ph">${esc([...title][0] || "?")}</span>`;
  const badge = ins.complete
    ? `<span class="badge done" data-testid="completed">Completed</span>`
    : `<span class="badge">${ins.finished}/${ins.tracked} chapters</span>`;
  const rating = seriesRating(g.series);
  const ratingHtml = rating
    ? `<span class="badge rating" data-testid="rating" title="Community score">★ ${(rating / 10).toFixed(1)}</span>`
    : "";
  const facts = [
    ins.firstRead ? `First read ${ins.firstRead}` : null,
    ins.complete && ins.span ? `Finished in ${ins.span}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const chapterRows = g.chapters
    .slice()
    .sort((a, b) => a.chapter_key.localeCompare(b.chapter_key, undefined, { numeric: true }))
    .map((c) => {
      const m = progressMeta(c);
      return `
        <button class="sub" data-testid="chapter" data-key="${esc(c.chapter_key)}">
          <span class="sub-title">${esc(displayTitle(parseKey(c.chapter_key).chapter))}</span>
          <span class="bar small"><i style="width:${m.pct}%"></i></span>
          <span class="pct">${esc(m.label)}</span>
        </button>`;
    })
    .join("");
  return `
    <details class="item" data-testid="item" data-series="${esc(g.series)}">
      <summary>
        <div class="row">
          ${coverHtml}
          <div class="grow">
            <div class="title">${esc(title)}</div>
            ${chapter ? `<div class="chapter">${esc(displayTitle(chapter))}</div>` : ""}
            <div class="facts">${badge}${ratingHtml}${facts ? `<span class="fact">${esc(facts)}</span>` : ""}</div>
          </div>
          <button class="hide-btn" data-testid="hide" data-series="${esc(g.series)}" title="${
            hidden ? "Unhide this title" : "Hide this title"
          }">${hidden ? "Unhide" : "Hide"}</button>
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
}

// One shelf tile: cover art (or a lettered placeholder), a thin progress
// bar, a check for completed series, and the title beneath. Tapping opens
// the series' current chapter in the reader.
function tileHtml(g, covers) {
  const title = displayTitle(g.series);
  const m = progressMeta(g.current);
  const ins = seriesInsights(g);
  const cover = covers.get(g.series);
  const art = cover
    ? `<img class="tile-cover" src="${esc(cover)}" alt="" loading="lazy" referrerpolicy="no-referrer" />`
    : `<span class="tile-cover tile-ph">${esc([...title][0] || "?")}</span>`;
  return `
    <button class="tile" data-testid="tile" data-key="${esc(g.current.chapter_key)}" title="${esc(title)}">
      <span class="tile-art">
        ${art}
        ${ins.complete ? `<span class="tile-done" title="Completed">✓</span>` : ""}
        <span class="tile-bar"><i style="width:${m.pct}%"></i></span>
      </span>
      <span class="tile-title">${esc(title)}</span>
    </button>`;
}

function viewLibrary(groups, covers, hiddenSet, showHidden, view) {
  const visible = groups.filter((g) => !hiddenSet.has(g.series));
  const hiddenGroups = groups.filter((g) => hiddenSet.has(g.series));
  const toggle = `
    <div class="view-toggle" role="group" aria-label="Library view">
      <button class="vt ${view === "grid" ? "on" : ""}" data-testid="view-grid" title="Cover shelf">⊞</button>
      <button class="vt ${view === "list" ? "on" : ""}" data-testid="view-list" title="List">☰</button>
    </div>`;
  const head = `<div class="lib-head"><div class="section-label">Continue reading</div>${toggle}</div>`;

  // Default: the cover shelf — a 3-row grid of tiles that scrolls
  // horizontally, three columns to a screen. Hidden titles are managed
  // from the list view.
  if (view === "grid") {
    const tiles = visible.map((g) => tileHtml(g, covers)).join("");
    return `${head}<div class="shelf" data-testid="shelf">${tiles}</div>`;
  }

  const items = visible.map((g) => libraryCardHtml(g, covers, false)).join("");
  const hiddenToggle = hiddenGroups.length
    ? `<button class="ghost hidden-toggle" data-testid="hidden-toggle">${
        showHidden ? "Hide" : "Show"
      } hidden titles (${hiddenGroups.length})</button>`
    : "";
  const hiddenItems = showHidden
    ? `<div class="section-label">Hidden</div><div class="list">${hiddenGroups
        .map((g) => libraryCardHtml(g, covers, true))
        .join("")}</div>`
    : "";
  return `${head}<div class="list">${items}</div>${hiddenToggle}${hiddenItems}`;
}

// --- book action sheet (long press on a tile / list card) ------------------
//
// An iOS-style bottom sheet: grouped rounded options over a dimmed backdrop,
// a separate Cancel, destructive action in red. Everything it offers reuses
// existing plumbing — open (reader), per-series stats (seriesInsights),
// hide/unhide (localStorage), and remove (RLS-scoped row deletes).

function closeSheet() {
  document.getElementById("sheet-backdrop")?.remove();
}

function sheetHtml(inner) {
  return `
    <div class="sheet-backdrop" id="sheet-backdrop" data-testid="sheet">
      <div class="sheet" role="dialog" aria-modal="true">${inner}</div>
    </div>`;
}

function openBookSheet(g, email, rows) {
  closeSheet();
  const title = displayTitle(g.series);
  const hidden = loadHidden(email).has(g.series);
  const ins = seriesInsights(g);
  document.body.insertAdjacentHTML(
    "beforeend",
    sheetHtml(`
      <div class="sheet-group">
        <div class="sheet-head">
          <div class="sheet-title">${esc(title)}</div>
          <div class="sheet-sub">${ins.finished}/${ins.tracked} chapters${
            ins.complete ? " · Completed" : ""
          }</div>
        </div>
        <button class="sheet-btn" data-act="open" data-testid="sheet-open">Open</button>
        <button class="sheet-btn" data-act="stats" data-testid="sheet-stats">View stats</button>
        <button class="sheet-btn" data-act="hide" data-testid="sheet-hide">${
          hidden ? "Unhide title" : "Hide title"
        }</button>
        <button class="sheet-btn destructive" data-act="remove" data-testid="sheet-remove">Remove from library</button>
      </div>
      <div class="sheet-group">
        <button class="sheet-btn cancel" data-act="cancel" data-testid="sheet-cancel">Cancel</button>
      </div>`)
  );
  const backdrop = document.getElementById("sheet-backdrop");
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeSheet();
  });
  backdrop.querySelectorAll(".sheet-btn").forEach((btn) =>
    btn.addEventListener("click", () => {
      const act = btn.getAttribute("data-act");
      if (act === "open") {
        closeSheet();
        openReader(g.current.chapter_key, parseKey(g.current.chapter_key));
      } else if (act === "stats") {
        openStatsSheet(g);
      } else if (act === "hide") {
        const set = loadHidden(email);
        if (set.has(g.series)) set.delete(g.series);
        else set.add(g.series);
        saveHidden(email, set);
        closeSheet();
        renderDashboard(email, rows);
      } else if (act === "remove") {
        openRemoveSheet(g, email, rows);
      } else {
        closeSheet();
      }
    })
  );
}

// Per-series stats: everything the synced rows can answer, in one card.
function openStatsSheet(g) {
  closeSheet();
  const ins = seriesInsights(g);
  const pages = g.chapters.reduce(
    (n, c) => n + Math.min(c.current_page + 1, c.total_pages > 0 ? c.total_pages : c.current_page + 1),
    0
  );
  const lastRead = g.chapters.map((c) => c.updated_at).sort().at(-1);
  const rating = seriesRating(g.series);
  const facts = [
    ["Chapters", `${ins.finished} finished · ${ins.tracked} tracked`],
    ["Pages read", pages.toLocaleString()],
    ["First read", ins.firstRead || "—"],
    ["Last read", lastRead ? timeAgo(lastRead) : "—"],
    ["Status", ins.complete ? `Completed${ins.span ? ` in ${ins.span}` : ""}` : "In progress"],
    ...(rating ? [["Community score", `★ ${(rating / 10).toFixed(1)} / 10`]] : []),
  ];
  document.body.insertAdjacentHTML(
    "beforeend",
    sheetHtml(`
      <div class="sheet-group">
        <div class="sheet-head">
          <div class="sheet-title">${esc(displayTitle(g.series))}</div>
          <div class="sheet-sub">Reading stats</div>
        </div>
        ${facts
          .map(
            ([k, v]) => `
          <div class="sheet-fact" data-testid="sheet-fact">
            <span class="sf-k">${esc(k)}</span><span class="sf-v">${esc(v)}</span>
          </div>`
          )
          .join("")}
      </div>
      <div class="sheet-group">
        <button class="sheet-btn cancel" data-act="cancel" data-testid="sheet-cancel">Done</button>
      </div>`)
  );
  const backdrop = document.getElementById("sheet-backdrop");
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop || e.target.closest("[data-act=cancel]")) closeSheet();
  });
}

// Destructive confirm, iOS-alert style. Removes the synced rows (progress +
// published pages) for the series and re-renders from the updated local state.
function openRemoveSheet(g, email, rows) {
  closeSheet();
  document.body.insertAdjacentHTML(
    "beforeend",
    sheetHtml(`
      <div class="sheet-group">
        <div class="sheet-head">
          <div class="sheet-title">Remove "${esc(displayTitle(g.series))}"?</div>
          <div class="sheet-sub">Removes this title's synced reading data from the web library. Your Kobo's downloads and progress are untouched.</div>
        </div>
        <button class="sheet-btn destructive" data-act="confirm" data-testid="sheet-confirm-remove">Remove</button>
      </div>
      <div class="sheet-group">
        <button class="sheet-btn cancel" data-act="cancel" data-testid="sheet-cancel">Cancel</button>
      </div>`)
  );
  const backdrop = document.getElementById("sheet-backdrop");
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop || e.target.closest("[data-act=cancel]")) closeSheet();
  });
  backdrop.querySelector("[data-act=confirm]").addEventListener("click", () => {
    closeSheet();
    deleteSeries(state.session, g.series); // fire-and-forget; UI updates now
    const keep = (r) => parseKey(r.chapter_key).series !== g.series;
    state.rows = (state.rows || rows).filter(keep);
    renderDashboard(email, state.rows);
  });
}

// Long-press (touch) and right-click both open the sheet; a completed long
// press swallows the click that follows so the reader doesn't also open.
function wireBookSheet(el, group, email, rows) {
  let timer = null;
  let fired = false;
  const start = () => {
    fired = false;
    timer = setTimeout(() => {
      fired = true;
      openBookSheet(group, email, rows);
    }, 450);
  };
  const cancel = () => clearTimeout(timer);
  el.addEventListener("pointerdown", start);
  el.addEventListener("pointerup", cancel);
  el.addEventListener("pointerleave", cancel);
  el.addEventListener("pointermove", cancel);
  el.addEventListener(
    "click",
    (e) => {
      if (fired) {
        e.preventDefault();
        e.stopPropagation();
        fired = false;
      }
    },
    true
  );
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openBookSheet(group, email, rows);
  });
}

function signOut() {
  clearSession();
  state.session = null;
  state.resume = {};
  state.rows = null;
  state.sends = [];
  state.discover = null;
  state.search = null;
  state.rails = {};
  state.pill = null;
  state.genrePool = null;
  state.sentTitles = null;
  state.ratings = null;
  state.malSync = null;
  state.malToast = null;
  state.malError = null;
  state.tab = "stats";
  renderSignIn("Signed out.");
}

// The signed-in dashboard: a header, a Stats/Library tab switch, and the active
// view. Rows are fetched once and reused across tab switches.
function renderDashboard(email, rows) {
  const tab = ["library", "discover"].includes(state.tab) ? state.tab : "stats";
  let body;
  if (tab === "discover") {
    body = viewDiscover();
  } else if (!rows.length) {
    body = `<div class="empty" data-testid="empty"><div class="big">📖</div><p>No reading progress yet.<br/>Read something on your Kobo and it'll show up here.</p></div>`;
  } else if (tab === "library") {
    body = viewLibrary(
      groupBySeries(rows),
      state.covers || new Map(),
      loadHidden(email),
      !!state.showHidden,
      libView()
    );
  } else {
    body = viewStats(computeStats(rows), groupBySeries(rows), state.sends);
  }
  app.innerHTML = `
    <div class="head">
      <div class="brand">gideon <span>· stats</span></div>
      <div class="head-right">
        ${themeButtonHtml()}
        <div class="who">${esc(email)}<button id="signout" data-testid="signout">Sign out</button></div>
      </div>
    </div>
    <div class="tabs" role="tablist">
      <button class="tab ${tab === "stats" ? "on" : ""}" data-tab="stats" data-testid="tab-stats">Stats</button>
      <button class="tab ${tab === "library" ? "on" : ""}" data-tab="library" data-testid="tab-library">Library</button>
      <button class="tab ${tab === "discover" ? "on" : ""}" data-tab="discover" data-testid="tab-discover">Discover</button>
    </div>
    ${body}`;

  wireThemeButton();
  document.getElementById("signout").addEventListener("click", signOut);
  for (const b of app.querySelectorAll(".tab")) {
    b.addEventListener("click", () => {
      state.tab = b.getAttribute("data-tab");
      // Leaving the tab dismisses one-shot MAL notices.
      state.malToast = null;
      state.malError = null;
      renderDashboard(email, rows);
    });
  }
  // Long-press (or right-click) a tile / list card for the book action
  // sheet (open, stats, hide, remove).
  {
    const groups = groupBySeries(rows);
    const bySeries = new Map(groups.map((g) => [g.series, g]));
    for (const tile of app.querySelectorAll('[data-testid="tile"]')) {
      const g = bySeries.get(parseKey(tile.getAttribute("data-key")).series);
      if (g) wireBookSheet(tile, g, email, rows);
    }
    for (const item of app.querySelectorAll('[data-testid="item"]')) {
      const g = bySeries.get(item.getAttribute("data-series"));
      if (g) wireBookSheet(item.querySelector("summary"), g, email, rows);
    }
  }
  // Grid/list view switch, persisted.
  for (const btn of app.querySelectorAll(".view-toggle .vt")) {
    btn.addEventListener("click", () => {
      localStorage.setItem(
        LIBVIEW_KEY,
        btn.getAttribute("data-testid") === "view-list" ? "list" : "grid"
      );
      renderDashboard(email, rows);
    });
  }
  // Tapping a chapter (library list or recent-read) or a shelf tile opens
  // the reader.
  for (const btn of app.querySelectorAll('[data-testid="chapter"], [data-testid="tile"]')) {
    btn.addEventListener("click", () => {
      const key = btn.getAttribute("data-key");
      openReader(key, parseKey(key));
    });
  }
  // Hide/unhide a title (persists per account in this browser); the button
  // sits inside a <summary>, so stop the click from toggling the card open.
  for (const btn of app.querySelectorAll('[data-testid="hide"]')) {
    btn.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      const series = btn.getAttribute("data-series");
      const hidden = loadHidden(email);
      if (hidden.has(series)) hidden.delete(series);
      else hidden.add(series);
      saveHidden(email, hidden);
      renderDashboard(email, rows);
    });
  }
  const hiddenToggle = app.querySelector('[data-testid="hidden-toggle"]');
  if (hiddenToggle) {
    hiddenToggle.addEventListener("click", () => {
      state.showHidden = !state.showHidden;
      renderDashboard(email, rows);
    });
  }
  // Send-to-Kobo: enqueue a title, and remove a pending send.
  const sendForm = document.getElementById("send-form");
  if (sendForm) {
    sendForm.addEventListener("submit", async (e) => {
      e.preventDefault();
      const input = document.getElementById("send-title");
      const btn = sendForm.querySelector("button");
      const note = document.getElementById("send-note");
      const title = input.value.trim();
      if (!title) return;
      btn.disabled = true;
      note.textContent = "";
      try {
        const [row] = await enqueueSend(title);
        if (row) state.sends = [row, ...state.sends];
        renderDashboard(email, rows);
      } catch (err) {
        // Keep the typed title so a retry is one tap, and say what went wrong
        // (a silent failure here is how "Send" used to look broken).
        btn.disabled = false;
        note.textContent = err.message || "Couldn't send — try again.";
      }
    });
  }
  for (const btn of app.querySelectorAll('[data-testid="send-remove"]')) {
    btn.addEventListener("click", async () => {
      const id = btn.getAttribute("data-id");
      deleteSend(id).catch(() => {});
      state.sends = state.sends.filter((s) => s.id !== id);
      renderDashboard(email, rows);
    });
  }
  // Discover: the MyAnimeList account footer. The connect button inside the
  // rail is wired by wireDiscover (the two are never on screen together).
  for (const btn of app.querySelectorAll('[data-testid="mal-connect"]')) {
    if (btn.closest("#disc-body")) continue;
    btn.addEventListener("click", () => {
      startMalConnect().catch((e) => {
        state.malError = e.message || "Couldn't start the MyAnimeList connection.";
        renderDashboard(email, rows);
      });
    });
  }
  document.getElementById("mal-disconnect")?.addEventListener("click", () => {
    clearMalConn();
    state.discover = null;
    state.malSync = null;
    renderDashboard(email, rows);
  });
  document.getElementById("mal-sync")?.addEventListener("click", () => {
    syncKoboToMal(email, rows);
  });
  // Search: submit runs it, Clear returns to the pills + rail.
  const searchForm = document.getElementById("search-form");
  if (searchForm) {
    searchForm.addEventListener("submit", (e) => {
      e.preventDefault();
      const q = document.getElementById("search-q").value.trim();
      if (q) runSearch(q, email, rows);
    });
    document.getElementById("search-clear")?.addEventListener("click", () => {
      state.search = null;
      renderDashboard(email, rows);
    });
  }
  // Search results sit outside the rail, so they need their own send wiring;
  // everything inside #disc-body is wired by wireDiscover.
  for (const btn of app.querySelectorAll('[data-testid="rec-send"]')) {
    if (!btn.closest("#disc-body")) wireRecSend(btn);
  }
  wireDiscover(email, rows);
  ensureRail(email, rows);
}

// --- reader ---------------------------------------------------------------

async function openReader(chapterKey, { series, chapter }) {
  const title = chapter
    ? `${displayTitle(series)} · ${displayTitle(chapter)}`
    : displayTitle(series);
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
    state.sends = await fetchSends().catch(() => []);
    // Covers for the library shelf (best-effort decor).
    state.covers = coverBySeries(await fetchAllChapterPages().catch(() => []));
    renderDashboard(email, rows);
    // Community ratings for the shelf (decor): resolved in the background,
    // then the library re-renders if it's on screen. Never blocks sign-in.
    const seriesTitles = [...new Set(rows.map((r) => displayTitle(parseKey(r.chapter_key).series)))];
    if (seriesTitles.length) {
      fetchLibraryRatings(seriesTitles)
        .then((map) => {
          state.ratings = map;
          if (state.session && state.tab === "library") renderDashboard(email, state.rows || rows);
        })
        .catch(() => {});
    }
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

// Supabase auth links (password recovery) land here with tokens in the URL
// fragment. Adopt the session so the link actually signs you in, scrub the
// tokens from the address bar, and for recovery links offer to set a new
// password immediately.
function adoptHashSession() {
  const h = new URLSearchParams(location.hash.replace(/^#/, ""));
  if (!h.get("access_token")) return null;
  saveSession({
    access_token: h.get("access_token"),
    refresh_token: h.get("refresh_token") || "",
    email: null,
    expires_at: Math.floor(Date.now() / 1000) + Number(h.get("expires_in") || 3600),
  });
  const type = h.get("type");
  history.replaceState(null, "", location.pathname);
  return type || "signin";
}

function showRecoveryBanner() {
  document.body.insertAdjacentHTML(
    "afterbegin",
    `<div class="oauth-banner" data-testid="recovery-banner">
      <div class="ob-label">You're signed in via your reset link — choose a new password:</div>
      <form id="pw-form" style="display:flex;gap:8px;justify-content:center;flex-wrap:wrap">
        <input type="password" id="new-pw" placeholder="new password" minlength="6" required
          autocomplete="new-password" style="padding:10px 12px;border-radius:8px;border:none;font:inherit" />
        <button class="primary ob-copy" type="submit">Save password</button>
      </form>
      <div class="ob-label" id="pw-note" style="margin-top:8px"></div>
    </div>`
  );
  document.getElementById("pw-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const note = document.getElementById("pw-note");
    const res = await fetch(`${SUPABASE_URL}/auth/v1/user`, {
      method: "PUT",
      headers: {
        apikey: SUPABASE_ANON_KEY,
        Authorization: `Bearer ${loadSession()?.access_token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ password: document.getElementById("new-pw").value }),
    }).catch(() => null);
    note.textContent = res?.ok
      ? "Password updated ✓ — use it everywhere (Kobo included)."
      : "Couldn't update the password — try once more.";
  });
}

async function boot() {
  await finishMalConnect(); // OAuth return from "Connect MyAnimeList"
  const linkType = adoptHashSession();
  if (linkType === "recovery") showRecoveryBanner();
  const session = loadSession();
  if (session?.access_token) showDashboard(session);
  // Signed out, the MAL panel never renders — surface a pending connection
  // message on the sign-in screen instead of dropping it.
  else renderSignIn(state.malError || "");
}

boot();
