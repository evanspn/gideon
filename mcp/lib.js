// Core logic for the gideon MCP server, kept apart from the stdio wiring so
// tests can drive it with an injected fetch. Talks to the same three surfaces
// the web dashboard uses: Supabase auth + REST (send_queue, reading_progress),
// the site's MAL proxy, and Jikan as the search fallback.

import fs from "node:fs";
import path from "node:path";
import os from "node:os";

export const SUPABASE_URL = "https://sqlkceqkdtmejhdoycsr.supabase.co";
export const SUPABASE_ANON_KEY =
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNxbGtjZXFrZHRtZWpoZG95Y3NyIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODMyOTE5MDAsImV4cCI6MjA5ODg2NzkwMH0.K8kXfcIihjw0Mz5qm1hW7nXHcymhN-yMLrV6CaLU1eo";
const SITE = "https://gideon-sync.vercel.app";
const JIKAN = "https://api.jikan.moe/v4";

const CONFIG_DIR = path.join(os.homedir(), ".config", "gideon");
const AUTH_FILE = path.join(CONFIG_DIR, "mcp-auth.json"); // { email, password }
const SESSION_FILE = path.join(CONFIG_DIR, "mcp-session.json");

// "One Piece/vol3.cbz" -> { series, chapter } (mirror of the web's parseKey).
export function parseKey(key) {
  const slash = key.lastIndexOf("/");
  const series = slash >= 0 ? key.slice(0, slash) : key;
  let chapter = slash >= 0 ? key.slice(slash + 1) : "";
  chapter = chapter.replace(/\.(cbz|zip)$/i, "");
  return { series, chapter };
}

export function displayTitle(s) {
  const tidy = String(s).replace(/_+/g, " ").replace(/\s+/g, " ").trim();
  return tidy || String(s);
}

export class GideonClient {
  constructor({ fetchImpl = fetch, authFile = AUTH_FILE, sessionFile = SESSION_FILE } = {}) {
    this.fetch = fetchImpl;
    this.authFile = authFile;
    this.sessionFile = sessionFile;
    this.session = this.#loadJson(this.sessionFile);
  }

  #loadJson(p) {
    try {
      return JSON.parse(fs.readFileSync(p, "utf8"));
    } catch {
      return null;
    }
  }
  #saveSession(s) {
    this.session = s;
    try {
      fs.mkdirSync(path.dirname(this.sessionFile), { recursive: true });
      fs.writeFileSync(this.sessionFile, JSON.stringify(s), { mode: 0o600 });
    } catch {}
  }

  async #authPost(pathname, body) {
    const res = await this.fetch(`${SUPABASE_URL}/auth/v1/${pathname}`, {
      method: "POST",
      headers: { apikey: SUPABASE_ANON_KEY, "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) {
      throw new Error(data.error_description || data.msg || data.message || `Auth error ${res.status}`);
    }
    return data;
  }

  async signIn() {
    const creds = this.#loadJson(this.authFile);
    if (!creds?.email || !creds?.password) {
      throw new Error(
        `Not signed in. Create ${this.authFile} containing {"email":"…","password":"…"} — the same ` +
          `account your Kobo and gideon-sync.vercel.app use — then try again.`
      );
    }
    const data = await this.#authPost("token?grant_type=password", creds);
    this.#saveSession({ access_token: data.access_token, refresh_token: data.refresh_token });
    return this.session;
  }

  async #refresh() {
    if (!this.session?.refresh_token) return this.signIn();
    try {
      const data = await this.#authPost("token?grant_type=refresh_token", {
        refresh_token: this.session.refresh_token,
      });
      this.#saveSession({ access_token: data.access_token, refresh_token: data.refresh_token });
      return this.session;
    } catch {
      return this.signIn(); // rotated/expired refresh token — fall back to creds
    }
  }

  // Authenticated Supabase REST call with one refresh-retry on 401.
  async rest(pathname, opts = {}, retry = true) {
    if (!this.session?.access_token) await this.#refresh();
    const res = await this.fetch(`${SUPABASE_URL}/rest/v1/${pathname}`, {
      ...opts,
      headers: {
        apikey: SUPABASE_ANON_KEY,
        Authorization: `Bearer ${this.session.access_token}`,
        ...(opts.headers || {}),
      },
    });
    if (res.status === 401 && retry) {
      await this.#refresh();
      return this.rest(pathname, opts, false);
    }
    if (!res.ok) throw new Error(`gideon sync error (${res.status})`);
    return res;
  }

  // --- tools ---------------------------------------------------------------

  // Catalog search: the site's official-MAL proxy first, Jikan fallback.
  async searchManga(query, limit = 8) {
    const clamp = Math.min(Math.max(limit, 1), 15);
    try {
      const p = encodeURIComponent(
        `manga?q=${encodeURIComponent(query)}&limit=${clamp}&fields=mean,genres,main_picture,media_type,synopsis,start_date,nsfw`
      );
      const res = await this.fetch(`${SITE}/api/mal?path=${p}`);
      if (!res.ok) throw new Error(`proxy ${res.status}`);
      const d = await res.json();
      const rows = (d.data || [])
        .map((r) => r.node)
        .filter((n) => n && !["light_novel", "novel"].includes(n.media_type) && (n.nsfw ?? "white") === "white")
        .map((n) => ({
          title: n.title,
          score: n.mean ?? null,
          year: n.start_date?.slice(0, 4) || null,
          genres: (n.genres || []).map((g) => g.name).slice(0, 4),
          cover_url: n.main_picture?.large || n.main_picture?.medium || null,
          synopsis: (n.synopsis || "").slice(0, 280),
          source: "myanimelist",
        }));
      if (rows.length) return rows;
      throw new Error("empty");
    } catch {
      const res = await this.fetch(
        `${JIKAN}/manga?q=${encodeURIComponent(query)}&sfw=true&limit=${clamp}&order_by=members&sort=desc`
      );
      const d = await res.json().catch(() => ({}));
      if (!res.ok || (d.status && d.status >= 400)) {
        throw new Error(d.message || `Search unavailable right now (MyAnimeList ${d.status || res.status}).`);
      }
      return (d.data || [])
        .filter((m) => (m.type || "").toLowerCase() !== "light novel")
        .map((m) => ({
          title: m.title,
          score: m.score ?? null,
          year: m.published?.from?.slice(0, 4) || null,
          genres: (m.genres || []).map((g) => g.name).slice(0, 4),
          cover_url: m.images?.jpg?.large_image_url || m.images?.jpg?.image_url || null,
          synopsis: (m.synopsis || "").slice(0, 280),
          source: "jikan",
        }));
    }
  }

  // Queue a title for the Kobo (what the web's Send box does). The device
  // picks it up on its next sync and runs its own source search.
  async sendToKobo(title, coverUrl) {
    const res = await this.rest("send_queue", {
      method: "POST",
      headers: { "Content-Type": "application/json", Prefer: "return=representation" },
      body: JSON.stringify(coverUrl ? { title, cover_url: coverUrl } : { title }),
    });
    const [row] = await res.json();
    return { id: row.id, title: row.title, queued_at: row.created_at };
  }

  async pendingSends() {
    const res = await this.rest(
      "send_queue?status=eq.pending&select=id,title,created_at&order=created_at.desc"
    );
    return res.json();
  }

  async removeSend(id) {
    await this.rest(`send_queue?id=eq.${encodeURIComponent(id)}`, { method: "DELETE" });
    return { removed: id };
  }

  // The Kobo's synced library: one row per series with where-you-are.
  async library() {
    const res = await this.rest(
      "reading_progress?select=chapter_key,current_page,total_pages,updated_at&order=updated_at.desc"
    );
    const rows = await res.json();
    const bySeries = new Map();
    for (const r of rows) {
      const { series } = parseKey(r.chapter_key);
      if (!bySeries.has(series)) bySeries.set(series, []);
      bySeries.get(series).push(r);
    }
    return [...bySeries.entries()].map(([series, chapters]) => {
      const current = chapters.reduce((a, b) => (b.updated_at > a.updated_at ? b : a));
      const { chapter } = parseKey(current.chapter_key);
      return {
        series: displayTitle(series),
        chapters_tracked: chapters.length,
        current_chapter: displayTitle(chapter),
        current_page: current.current_page + 1,
        total_pages: current.total_pages,
        last_read: current.updated_at,
      };
    });
  }
}
