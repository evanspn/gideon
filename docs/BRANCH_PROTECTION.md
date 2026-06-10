# Protecting `main`

Branch protection lives in repository settings, not in files, so it has to
be enabled once by an admin. This documents the recommended setup and the
one pitfall specific to this repo.

## Good news: no bypass needed

The release workflow never pushes to `main`. Versions live in tags: the
workflow bakes the version into the build inside the runner and creates
the `vX.Y.Z` tag when it publishes the release (the releases API, not a
branch push). Strict protection can be enabled as-is.

## Recommended ruleset

Settings → Rules → Rulesets → New ruleset → "New branch ruleset":

1. **Name:** `protect-main` — **Enforcement:** Active
2. **Target branches:** Include default branch
3. **Rules to enable:**
   - ✅ Restrict deletions
   - ✅ Block force pushes
   - ✅ Require a pull request before merging
     - Required approvals: `0` (you're the only human; CODEOWNERS still
       auto-requests you on every PR). Set to `1` + "Require review from
       Code Owners" only if a second maintainer joins — otherwise you
       can't approve your own PRs.
   - ✅ Require status checks to pass — add these checks:
     - `Format & Clippy`
     - `Tests`
     - `CLI smoke test`
     - `Installer tests`
     - `Cross-check (Kobo armv7)`
4. **Bypass list:** optionally add **Repository admin** (you) for
   emergencies; nothing else is required.

With this in place:

- Nobody (including bots) can merge to `main` without a PR and green CI
- You are auto-requested as reviewer on every PR via CODEOWNERS
- Force pushes and branch deletion are blocked
- Automatic releases keep working — the workflow only creates tags and
  releases, which branch rules don't block
