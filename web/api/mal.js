// Same-origin proxy to MyAnimeList's official API v2.
//
// The static app can't call api.myanimelist.net directly (no CORS), and the
// community Jikan mirror has multi-day outages — so the app prefers this
// proxy and falls back to Jikan only when this reports it isn't configured.
//
// Configure with a single Vercel env var: MAL_CLIENT_ID (create one at
// myanimelist.net → Preferences → API → "Create ID"; the app never sees it).
//
// GET /api/mal?path=<MAL v2 path + query>, e.g.
//   /api/mal?path=manga%3Fq%3Dberserk%26limit%3D18%26fields%3Dmean
// The path is allowlisted (read-only, public data only) so this can't be
// used as an open proxy.

const ALLOWED_PATHS = [
  /^anime\/\d+$/, // anime details (related_manga)
  /^manga\/\d+$/, // manga details (mean, picture, recommendations)
  /^manga$/, // search
  /^manga\/ranking$/, // browse rows
  /^users\/[A-Za-z0-9_-]{2,16}\/animelist$/,
  /^users\/[A-Za-z0-9_-]{2,16}\/mangalist$/,
];
// Paths that may be called with a user's own OAuth token (X-MAL-USER-TOKEN,
// forwarded as the Bearer) instead of the app client id. Personal data only.
const USER_TOKEN_PATHS = [/^users\/@me$/];
const ALLOWED_PARAMS = new Set([
  "q", "limit", "offset", "fields", "status", "ranking_type", "nsfw", "sort",
]);

export default async function handler(req, res) {
  res.setHeader("Content-Type", "application/json");
  if (req.method !== "GET") {
    return res.status(405).json({ error: "GET only" });
  }
  const clientId = process.env.MAL_CLIENT_ID;
  if (!clientId) {
    // The app treats this as "proxy not configured" and falls back to Jikan.
    return res.status(503).json({ error: "proxy-unconfigured" });
  }

  const raw = String(req.query.path || "");
  const [pathname, query = ""] = raw.split("?");
  const userToken = req.headers["x-mal-user-token"];
  if (userToken) {
    // Personal responses must never enter the shared edge cache (the cache
    // key is the URL — headers don't partition it). Set before any branching
    // so no code path can forget it.
    res.setHeader("Cache-Control", "no-store");
  }
  const userPath = USER_TOKEN_PATHS.some((re) => re.test(pathname));
  if (!(ALLOWED_PATHS.some((re) => re.test(pathname)) || (userPath && userToken))) {
    return res.status(400).json({ error: "path not allowed" });
  }
  if (userPath && !userToken) {
    return res.status(401).json({ error: "user token required" });
  }
  const params = new URLSearchParams();
  for (const [k, v] of new URLSearchParams(query)) {
    if (ALLOWED_PARAMS.has(k)) params.set(k, v);
  }

  let upstream;
  try {
    upstream = await fetch(
      `https://api.myanimelist.net/v2/${pathname}?${params}`,
      {
        headers: userToken
          ? { Authorization: `Bearer ${userToken}` }
          : { "X-MAL-CLIENT-ID": clientId },
        signal: AbortSignal.timeout(10_000),
      }
    );
  } catch {
    return res.status(502).json({ error: "MyAnimeList didn't answer" });
  }

  const body = await upstream.text();
  // Public catalog data (search/ranking/details) is safe to cache at the
  // edge; anything fetched with a user token was already pinned no-store
  // above and must stay that way.
  if (!userToken) {
    const cacheable = pathname === "manga" || pathname === "manga/ranking" || /^\w+\/\d+$/.test(pathname);
    res.setHeader(
      "Cache-Control",
      cacheable ? "s-maxage=3600, stale-while-revalidate=86400" : "no-store"
    );
  }
  // Pass MAL's status through (404 unknown user, 400 bad request, …) so the
  // app can tell an authoritative answer from an infrastructure failure.
  return res.status(upstream.status).send(body || "{}");
}
