// OAuth helper for "Connect MyAnimeList": the only parts of the PKCE dance
// that can't run in the browser, because they involve MAL_CLIENT_SECRET.
// The secret never leaves this function; tokens go straight back to the
// requesting browser and live only there.
//
//   GET  /api/mal-oauth?action=config    → { client_id }         (public)
//   POST /api/mal-oauth?action=token     { code, verifier }      → tokens
//   POST /api/mal-oauth?action=refresh   { refresh_token }       → tokens
//
// redirect_uri is pinned server-side to the registered value, so a stolen
// authorization code can't be redeemed against a different redirect.

const TOKEN_URL = "https://myanimelist.net/v1/oauth2/token";
const REDIRECT_URI = "https://gideon-sync.vercel.app/";

// Best-effort per-instance rate limit on the exchange actions, so this
// endpoint can't be farmed as a free token-exchange oracle for our client
// credentials. Fluid Compute reuses instances, so this catches sustained
// abuse from one source even without shared state.
const RATE_WINDOW_MS = 60_000;
const RATE_MAX = 10;
const hitsByIp = new Map();
function rateLimited(ip) {
  const now = Date.now();
  const hits = (hitsByIp.get(ip) || []).filter((t) => now - t < RATE_WINDOW_MS);
  hits.push(now);
  hitsByIp.set(ip, hits);
  if (hitsByIp.size > 10_000) hitsByIp.clear(); // memory backstop
  return hits.length > RATE_MAX;
}

export default async function handler(req, res) {
  res.setHeader("Content-Type", "application/json");
  res.setHeader("Cache-Control", "no-store");
  const { MAL_CLIENT_ID, MAL_CLIENT_SECRET } = process.env;
  if (!MAL_CLIENT_ID || !MAL_CLIENT_SECRET) {
    return res.status(503).json({ error: "proxy-unconfigured" });
  }
  const action = String(req.query.action || "");

  if (action === "config") {
    // The client id is public by design (it ships in every MAL client app).
    return res.status(200).json({ client_id: MAL_CLIENT_ID, redirect_uri: REDIRECT_URI });
  }
  if (req.method !== "POST") {
    return res.status(405).json({ error: "POST only" });
  }
  const ip = String(req.headers["x-forwarded-for"] || "").split(",")[0].trim() || "unknown";
  if (rateLimited(ip)) {
    return res.status(429).json({ error: "slow down — try again in a minute" });
  }

  const body = req.body || {};
  let params;
  if (action === "token") {
    const verifier = String(body.verifier || "");
    // RFC 7636 bounds; anything shorter is a downgrade attempt or a bug.
    if (!body.code || verifier.length < 43 || verifier.length > 128) {
      return res.status(400).json({ error: "code and a 43-128 char verifier required" });
    }
    params = {
      grant_type: "authorization_code",
      code: String(body.code),
      code_verifier: verifier,
      redirect_uri: REDIRECT_URI,
    };
  } else if (action === "refresh") {
    if (!body.refresh_token) return res.status(400).json({ error: "refresh_token required" });
    params = { grant_type: "refresh_token", refresh_token: String(body.refresh_token) };
  } else {
    return res.status(400).json({ error: "unknown action" });
  }

  let upstream;
  try {
    upstream = await fetch(TOKEN_URL, {
      method: "POST",
      body: new URLSearchParams({
        client_id: MAL_CLIENT_ID,
        client_secret: MAL_CLIENT_SECRET,
        ...params,
      }),
      signal: AbortSignal.timeout(10_000),
    });
  } catch {
    return res.status(502).json({ error: "MyAnimeList didn't answer" });
  }
  const data = await upstream.json().catch(() => ({}));
  if (!upstream.ok) {
    return res.status(upstream.status).json({ error: data.error || "token exchange failed" });
  }
  // Only what the browser needs — nothing else from the upstream body.
  return res.status(200).json({
    access_token: data.access_token,
    refresh_token: data.refresh_token,
    expires_in: data.expires_in,
  });
}
