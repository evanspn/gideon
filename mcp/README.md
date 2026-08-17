# gideon MCP server

Lets Claude find manga and put them on your Kobo in one breath:

> "find something like Berserk and send the best one to my kobo"

## Tools

| Tool | What it does |
|---|---|
| `search_manga` | Search the MyAnimeList catalog (site proxy first, Jikan fallback) — title, ★ score, year, genres, cover, synopsis |
| `send_to_kobo` | Queue a title (+ cover art) into `send_queue`; the Kobo shows it as a Home-screen bell after its next sync |
| `pending_sends` | What's queued and not yet opened on the device |
| `remove_send` | Un-queue a title before the device sees it |
| `library` | The synced shelf: series, current chapter/page, last read — so Claude doesn't send what you already have |

## Setup

Registered project-wide via `.mcp.json` (repo root) — Claude Code picks it up
on the next session in this repo and asks once for approval. One-time install
and sign-in:

```sh
cd mcp && npm install
mkdir -p ~/.config/gideon
cat > ~/.config/gideon/mcp-auth.json <<'EOF'
{"email":"you@example.com","password":"your gideon sync password"}
EOF
chmod 600 ~/.config/gideon/mcp-auth.json
```

Same account as the Kobo and gideon-sync.vercel.app. The server keeps a
rotating session in `~/.config/gideon/mcp-session.json` so the password is
rarely touched.

## Tests

```sh
node --test mcp/test.mjs
```

Everything network-facing is injected, so tests run offline.
