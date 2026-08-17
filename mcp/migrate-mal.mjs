#!/usr/bin/env node
// Migrate the Kobo's synced reading history onto the user's MyAnimeList
// manga list. Reads reading_progress via the gideon sync account
// (~/.config/gideon/mcp-auth.json), matches each series against MAL, and
// writes my_list_status with the user's OAuth token
// (~/.config/gideon/mal-api.json, from the one-tap authorize flow).
//
//   node migrate-mal.mjs           # dry run: print the plan, write nothing
//   node migrate-mal.mjs --apply   # write to MAL
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { GideonClient, parseKey, displayTitle } from "./lib.js";

const MAL_API = "https://api.myanimelist.net/v2";
const CRED_FILE = path.join(os.homedir(), ".config", "gideon", "mal-api.json");
const APPLY = process.argv.includes("--apply");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const norm = (s) => String(s).toLowerCase().replace(/[^a-z0-9]+/g, " ").trim();

const creds = JSON.parse(fs.readFileSync(CRED_FILE, "utf8"));
if (!creds.access_token) {
  console.error("No MAL OAuth token yet — finish the authorize step first.");
  process.exit(1);
}

async function mal(pathname, opts = {}, retry = true) {
  const res = await fetch(`${MAL_API}/${pathname}`, {
    ...opts,
    headers: { Authorization: `Bearer ${creds.access_token}`, ...(opts.headers || {}) },
  });
  if (res.status === 401 && retry && creds.refresh_token) {
    const body = new URLSearchParams({
      client_id: creds.client_id,
      client_secret: creds.client_secret,
      grant_type: "refresh_token",
      refresh_token: creds.refresh_token,
    });
    const tr = await fetch("https://myanimelist.net/v1/oauth2/token", { method: "POST", body });
    const tok = await tr.json();
    if (tr.ok) {
      Object.assign(creds, { access_token: tok.access_token, refresh_token: tok.refresh_token });
      fs.writeFileSync(CRED_FILE, JSON.stringify(creds), { mode: 0o600 });
      return mal(pathname, opts, false);
    }
  }
  if (!res.ok) throw new Error(`MAL ${res.status} on ${pathname.split("?")[0]}`);
  return res.json();
}

// --- gather the Kobo history --------------------------------------------------
const gideon = new GideonClient();
const rows = await (await gideon.rest(
  "reading_progress?select=chapter_key,current_page,total_pages,updated_at&order=updated_at.desc"
)).json();
if (!rows.length) {
  console.log("No synced reading history found on the gideon account.");
  process.exit(0);
}
const bySeries = new Map();
for (const r of rows) {
  const { series } = parseKey(r.chapter_key);
  if (!bySeries.has(series)) bySeries.set(series, []);
  bySeries.get(series).push(r);
}

// --- match + plan -------------------------------------------------------------
const plan = [];
for (const [dir, chapters] of bySeries) {
  const title = displayTitle(dir);
  // Author/edition parentheticals and scanlation prefixes derail MAL search
  // ("Judge (TONOGAI Yoshiki)" matched a different manga entirely).
  const query = title.replace(/\s*\([^)]*\)/g, "").replace(/^manga[\s-]+/i, "").trim() || title;
  const finished = chapters.filter((c) => c.total_pages > 0 && c.current_page + 1 >= c.total_pages).length;
  await sleep(400);
  let match = null;
  try {
    const d = await mal(`manga?q=${encodeURIComponent(query.slice(0, 64))}&limit=3&fields=num_chapters,media_type`);
    const nodes = (d.data || []).map((x) => x.node).filter((n) => !["light_novel", "novel"].includes(n.media_type));
    match = nodes.find((n) => norm(n.title) === norm(query)) || nodes[0] || null;
  } catch (e) {
    plan.push({ title, error: String(e.message) });
    continue;
  }
  if (!match) {
    plan.push({ title, error: "no MAL match found" });
    continue;
  }
  const total = match.num_chapters || 0;
  const status = total > 0 && finished >= total ? "completed" : "reading";
  plan.push({
    title,
    mal_id: match.id,
    mal_title: match.title,
    exact: norm(match.title) === norm(query),
    chapters_read: finished,
    status,
  });
}

// Different library folders can resolve to the same MAL entry ("Vagabond" ×2,
// "Bleach" + "Bleach (Color)") — merge, keeping the furthest progress.
const byId = new Map();
const merged = [];
for (const p of plan) {
  if (p.error || !byId.has(p.mal_id)) {
    merged.push(p);
    if (!p.error) byId.set(p.mal_id, p);
    continue;
  }
  const kept = byId.get(p.mal_id);
  kept.chapters_read = Math.max(kept.chapters_read, p.chapters_read);
  if (p.status === "completed") kept.status = "completed";
  kept.title += ` + ${p.title}`;
}
plan.length = 0;
plan.push(...merged);

console.log(`${APPLY ? "APPLYING" : "DRY RUN"} — ${plan.length} series from the Kobo:`);
for (const p of plan) {
  if (p.error) console.log(`  ✗ ${p.title}: ${p.error}`);
  else console.log(`  ${p.exact ? "=" : "≈"} ${p.title} → [${p.mal_id}] ${p.mal_title} · ${p.status}, ${p.chapters_read} ch read`);
}

// --- write --------------------------------------------------------------------
if (APPLY) {
  let ok = 0;
  for (const p of plan) {
    if (p.error) continue;
    await sleep(400);
    try {
      await mal(`manga/${p.mal_id}/my_list_status`, {
        method: "PATCH",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ status: p.status, num_chapters_read: String(p.chapters_read) }),
      });
      ok++;
    } catch (e) {
      console.log(`  write failed for ${p.mal_title}: ${e.message}`);
    }
  }
  console.log(`Wrote ${ok}/${plan.filter((p) => !p.error).length} entries to MyAnimeList.`);
}
