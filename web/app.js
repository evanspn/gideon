// gideon web — reading-progress dashboard.
//
// Signs in with email + password (Supabase Auth — the same account the Kobo
// signs into) and shows the `reading_progress` rows the device syncs, newest
// first. Set the account up here once (Create account), then sign in with the
// same email + password on your Kobo. It reads and never writes here, so it
// can't rewind your place — the device is the writer.
//
// The anon key is public by design: row-level security (auth.uid()), not the
// key, is what scopes every row to its owner.

import { createClient } from "https://esm.sh/@supabase/supabase-js@2.58.0";

const SUPABASE_URL = "https://sqlkceqkdtmejhdoycsr.supabase.co";
const SUPABASE_ANON_KEY =
  "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNxbGtjZXFrZHRtZWpoZG95Y3NyIiwicm9sZSI6ImFub24iLCJpYXQiOjE3ODMyOTE5MDAsImV4cCI6MjA5ODg2NzkwMH0.K8kXfcIihjw0Mz5qm1hW7nXHcymhN-yMLrV6CaLU1eo";

const supabase = createClient(SUPABASE_URL, SUPABASE_ANON_KEY, {
  auth: { detectSessionInUrl: true, persistSession: true, autoRefreshToken: true },
});

const app = document.getElementById("app");

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
          <button class="primary" type="submit" data-mode="signin">Sign in</button>
          <button class="ghost" type="button" id="create">Create account</button>
        </div>
      </form>
      <div class="note ${message ? "ok" : ""}" id="note">${message ? esc(message) : ""}</div>
    </div>`;

  const form = document.getElementById("signin");
  const note = document.getElementById("note");
  const emailEl = document.getElementById("email");
  const pwEl = document.getElementById("password");

  async function submit(mode) {
    const email = emailEl.value.trim();
    const password = pwEl.value;
    if (!email || !password) return;
    form.querySelectorAll("button").forEach((b) => (b.disabled = true));
    note.className = "note";
    note.textContent = mode === "signup" ? "Creating account…" : "Signing in…";
    const { error } =
      mode === "signup"
        ? await supabase.auth.signUp({ email, password })
        : await supabase.auth.signInWithPassword({ email, password });
    if (error) {
      note.className = "note";
      note.textContent = error.message;
      form.querySelectorAll("button").forEach((b) => (b.disabled = false));
    }
    // On success, onAuthStateChange swaps to the library view.
  }

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    submit("signin");
  });
  document.getElementById("create").addEventListener("click", () => submit("signup"));
}

function renderLibrary(email, rows) {
  const items = rows
    .map((r) => {
      const { series, chapter } = parseKey(r.chapter_key);
      const total = r.total_pages || 0;
      const page = Math.min(r.current_page + 1, total || r.current_page + 1);
      const pct = total > 0 ? Math.round((page / total) * 100) : 0;
      return `
        <div class="item">
          <div class="title">${esc(series)}</div>
          ${chapter ? `<div class="chapter">${esc(chapter)}</div>` : ""}
          <div class="meta">
            <div class="bar"><i style="width:${pct}%"></i></div>
            <div class="pct">${total > 0 ? `${page}/${total}` : `p.${page}`}</div>
          </div>
          <div class="ago">${esc(timeAgo(r.updated_at))}</div>
        </div>`;
    })
    .join("");

  app.innerHTML = `
    <div class="head">
      <div class="brand">gideon <span>· sync</span></div>
      <div class="who">${esc(email)}<button id="signout">Sign out</button></div>
    </div>
    <div class="section-label">Continue reading</div>
    ${
      rows.length
        ? `<div class="list">${items}</div>`
        : `<div class="empty"><div class="big">📖</div><p>No reading progress yet.<br/>Read something on your Kobo and it'll show up here.</p></div>`
    }`;
  document.getElementById("signout").addEventListener("click", async () => {
    await supabase.auth.signOut();
    renderSignIn("Signed out.");
  });
}

async function showLibrary(session) {
  const email = session.user?.email ?? "signed in";
  const { data, error } = await supabase
    .from("reading_progress")
    .select("chapter_key,current_page,total_pages,updated_at")
    .order("updated_at", { ascending: false });
  if (error) {
    renderLibrary(email, []);
    document.querySelector(".section-label").insertAdjacentHTML(
      "afterend",
      `<div class="note">Couldn't load progress: ${esc(error.message)}</div>`
    );
    return;
  }
  renderLibrary(email, data ?? []);
}

async function boot() {
  const { data: { session } } = await supabase.auth.getSession();
  if (session) showLibrary(session);
  else renderSignIn("");

  supabase.auth.onAuthStateChange((_event, session) => {
    if (session) showLibrary(session);
    else renderSignIn("");
  });
}

boot();
