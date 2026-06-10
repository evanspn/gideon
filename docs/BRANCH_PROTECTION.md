# Protecting `main`

Branch protection lives in repository settings, not in files, so it has to
be enabled once by an admin. This documents the recommended setup and the
one pitfall specific to this repo.

## ⚠️ The release workflow must keep push access

`.github/workflows/release.yml` pushes the version-bump commit and the
`vX.Y.Z` tag straight back to `main` after every merge. Protection rules
that block direct pushes will **break automatic releases** unless the
GitHub Actions app is allowed to bypass them. Use a **ruleset** (they
support bypass lists; classic branch protection does not exempt the
default `GITHUB_TOKEN`).

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
4. **Bypass list:** add **GitHub Actions** (the app) — this is what lets
   the release workflow push the version bump + tag. Optionally add
   **Repository admin** (you) for emergencies.

With this in place:

- Nobody (including bots) can merge to `main` without a PR and green CI
- You are auto-requested as reviewer on every PR via CODEOWNERS
- Force pushes and branch deletion are blocked
- Automatic releases keep working because the Actions app bypasses the
  push restriction
