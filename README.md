# gideon

**gideon** is a manga e-reader for Kobo devices, written in Rust — a
from-scratch take on what [KOReader](https://github.com/koreader/koreader)
does, focused on manga. It is the standalone sibling of
[bobo](https://github.com/evanspn/bobo-koreader), the KOReader manga plugin:
where bobo lives inside KOReader, gideon *is* the reader.

## Status: v0

The core pipeline works end to end:

- **CBZ documents** — open `.cbz` archives, natural page ordering
  (`page2` before `page10`), junk filtering (`__MACOSX/`, dotfiles,
  `Thumbs.db`), `ComicInfo.xml` metadata
- **E-ink rendering** — scale-to-fit (contain / fit-width / fit-height),
  grayscale, Floyd–Steinberg dithering to the 16 gray levels Kobo panels
  display
- **Library** — recursive scanning, JSON-backed reading progress with
  resume
- **Manga sources from GitHub (preinstalled)** — fetches
  [Aidoku](https://github.com/Aidoku/Aidoku)-compatible source lists (the
  same format bobo uses); ships with the community source list and accepts
  any user-added list URL
- **Kobo display backend** — Linux framebuffer with mxcfb e-ink refresh
  ioctls (full/partial refresh policy), behind the `kobo` feature
- **Chapter downloads** — fetched pages are packed into `.cbz` for offline
  reading

## Try it

```sh
cargo build --workspace

# Inspect a CBZ
cargo run -- info ~/manga/berserk-v1.cbz

# Render a page exactly like the e-ink pipeline would, to a PNG
cargo run -- render ~/manga/berserk-v1.cbz -p 12 -o page.png

# Scan a library and see reading progress
cargo run -- library ~/manga

# List manga sources from the preinstalled GitHub source list
cargo run -- sources
cargo run -- sources --add-list https://example.com/index.json

# Read interactively (n = next, p = prev, q = quit); progress is saved
cargo run -- read ~/manga/berserk-v1.cbz
```

## Installing on a Kobo

Download `gideon-kobo-vX.Y.Z.zip` from the
[latest release](https://github.com/evanspn/gideon/releases/latest) (or grab
the `gideon-kobo-bundle` artifact from the latest post-merge CI run for a
bleeding-edge build), unzip it, plug in your Kobo and run `./install.sh`.
Upgrades are in-place and
**never touch your data**: settings and progress live in
`.adds/gideon/data/`, which the installer backs up before each upgrade and
never writes to. See [installer/INSTALL.md](installer/INSTALL.md) for
details, on-device installs and uninstalling.

## Building for Kobo

Kobo devices are armv7 with an e-ink framebuffer. The `kobo` feature enables
the framebuffer backend:

```sh
rustup target add armv7-unknown-linux-musleabihf
sudo apt-get install gcc-arm-linux-gnueabihf

export CC_armv7_unknown_linux_musleabihf=arm-linux-gnueabihf-gcc
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=arm-linux-gnueabihf-gcc
cargo build --release --features gideon-app/kobo --target armv7-unknown-linux-musleabihf
```

The framebuffer must be in 8-bit grayscale mode (`fbdepth -d 8`, shipped with
KOReader, does this).

## Workspace layout

| Crate | What it does |
| --- | --- |
| `gideon-core` | CBZ parsing, ComicInfo metadata, library scanning, reading progress |
| `gideon-render` | Scale/grayscale/dither pipeline producing framebuffer-ready pages |
| `gideon-sources` | Aidoku/bobo-compatible source lists, source resolution, chapter→CBZ downloads |
| `gideon-device` | `Display` abstraction: in-memory backend for tests, Kobo mxcfb backend for hardware |
| `gideon-app` | The `gideon` binary: CLI + reader session (page turns, refresh policy, resume) |

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI runs formatting, clippy (both feature sets), the full test suite, a CLI
smoke test against a generated CBZ, and a cross-check for the Kobo target.

### Releases

Releases are semantically versioned and tag-driven:

```sh
scripts/release.sh patch        # or minor / major / an explicit X.Y.Z
git push origin HEAD --follow-tags
```

The script bumps the workspace version (binary `--version` inherits it),
refreshes `Cargo.lock`, commits and tags `vX.Y.Z`. Pushing the tag triggers
the release workflow, which refuses to publish unless the tag matches
`Cargo.toml`, re-runs the full quality gate plus the QEMU integration tests
against the release binaries, and then publishes a GitHub Release with
`gideon-kobo-vX.Y.Z.zip` and auto-generated notes.

After merges to main, a post-merge workflow goes further: it builds the real
armv7 Kobo binaries and runs integration tests against them under QEMU
user-mode emulation (`ci/qemu_integration.sh`), verifies the preinstalled
GitHub source list still resolves live, and uploads the Kobo binary as a
build artifact.

See [ROADMAP.md](ROADMAP.md) for where this is going, and
[docs/LESSONS.md](docs/LESSONS.md) for the mistakes from bobo's history that
gideon is designed not to repeat.
