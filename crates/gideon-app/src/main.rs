//! gideon — a manga e-reader for Kobo devices.
//!
//! v0 exposes the core pipeline through a CLI:
//!
//! * `gideon info <file.cbz>` — show metadata and page listing
//! * `gideon render <file.cbz> -p 3 -o page.png` — render a page like the
//!   device would (scaled, grayscale, dithered) and save it as PNG
//! * `gideon library <dir>` — scan a library and show reading progress
//! * `gideon sources [--add-list URL]` — list manga sources available from
//!   the configured (GitHub-hosted) source lists
//! * `gideon read <file.cbz>` — read on the device framebuffer (Kobo
//!   builds) or interactively in the terminal (desktop builds)

mod manga;
mod reader;
// Outside device (`kobo`) builds, only the headless screenshot path of the
// UI is reachable from the binary; the rest is exercised by unit tests.
#[cfg_attr(not(feature = "kobo"), allow(dead_code))]
mod ui;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use url::Url;

use gideon_core::{CbzDocument, Library, ProgressStore};
use gideon_render::{render_page, FitMode, RenderOptions};
use gideon_sources::{SourceLists, UreqFetcher};

use reader::Reader;

#[derive(Parser)]
#[command(name = "gideon", version, about = "A manga e-reader for Kobo devices")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show metadata and pages of a CBZ file.
    Info { path: PathBuf },

    /// Render a page to a PNG, exactly as the e-ink pipeline would.
    Render {
        path: PathBuf,
        /// Page number (1-based).
        #[arg(short, long, default_value_t = 1)]
        page: usize,
        /// Output PNG path.
        #[arg(short, long, default_value = "page.png")]
        out: PathBuf,
        /// Target screen width (Kobo Clara HD default).
        #[arg(long, default_value_t = 1072)]
        width: u32,
        /// Target screen height.
        #[arg(long, default_value_t = 1448)]
        height: u32,
        /// Disable e-ink dithering.
        #[arg(long)]
        no_dither: bool,
    },

    /// Scan a library directory for CBZ files and show progress.
    Library { dir: PathBuf },

    /// List manga sources available from configured source lists.
    Sources {
        /// Additional source list URLs (on top of the preinstalled ones).
        #[arg(long = "add-list")]
        add_lists: Vec<Url>,
        /// Skip the preinstalled default lists.
        #[arg(long)]
        no_defaults: bool,
    },

    /// Render the library as a cover grid (the library cover view),
    /// saved as a PNG on desktop builds.
    Shelf {
        dir: PathBuf,
        /// Output PNG path.
        #[arg(short, long, default_value = "shelf.png")]
        out: PathBuf,
        /// Number of columns.
        #[arg(long, default_value_t = 3)]
        cols: u32,
        /// Target screen width (Kobo Clara HD default).
        #[arg(long, default_value_t = 1072)]
        width: u32,
        /// Target screen height.
        #[arg(long, default_value_t = 1448)]
        height: u32,
    },

    /// Manage installed manga sources (the WASM programs from the source list).
    #[command(subcommand)]
    Source(SourceCommand),

    /// Search, browse and download manga through an installed source.
    #[command(subcommand)]
    Manga(MangaCommand),

    /// Check for (and install) gideon updates from GitHub releases.
    Update {
        /// Only check and report; don't download or install.
        #[arg(long)]
        check: bool,
        /// Allow installing a major version bump (off by default, like bobo).
        #[arg(long)]
        major: bool,
    },

    /// Browse the library and manga sources on the device's touch screen.
    Browse {
        /// Library directory.
        #[arg(long, default_value = "/mnt/onboard/Manga")]
        library: PathBuf,
        /// Render the home screen to a PNG and exit (headless desktop
        /// verification; no device needed).
        #[arg(long)]
        screenshot: Option<PathBuf>,
    },

    /// Open a CBZ for reading.
    Read {
        path: PathBuf,
        /// Progress file (defaults to .gideon/progress.json next to the file).
        #[arg(long)]
        progress_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SourceCommand {
    /// Download and install a source from the configured source lists.
    Install { source_id: String },
    /// List installed sources.
    Installed,
}

#[derive(Subcommand)]
enum MangaCommand {
    /// Search for manga on an installed source.
    Search {
        #[arg(short, long)]
        source: String,
        query: String,
    },
    /// List chapters of a manga.
    Chapters {
        #[arg(short, long)]
        source: String,
        manga_id: String,
    },
    /// Download a chapter into the library as a CBZ.
    Download {
        #[arg(short, long)]
        source: String,
        manga_id: String,
        chapter_id: String,
        /// Library directory to save into.
        #[arg(short, long, default_value = "/mnt/onboard/Manga")]
        library: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Info { path } => cmd_info(path),
        Command::Render {
            path,
            page,
            out,
            width,
            height,
            no_dither,
        } => cmd_render(path, page, out, width, height, !no_dither),
        Command::Library { dir } => cmd_library(dir),
        Command::Sources {
            add_lists,
            no_defaults,
        } => cmd_sources(add_lists, no_defaults),
        Command::Shelf {
            dir,
            out,
            cols,
            width,
            height,
        } => cmd_shelf(dir, out, cols, width, height),
        Command::Source(cmd) => match cmd {
            SourceCommand::Install { source_id } => {
                manga::cmd_source_install(&data_dir(), &source_id)
            }
            SourceCommand::Installed => manga::cmd_source_installed(&data_dir()),
        },
        Command::Manga(cmd) => match cmd {
            MangaCommand::Search { source, query } => {
                manga::cmd_manga_search(&data_dir(), &source, &query)
            }
            MangaCommand::Chapters { source, manga_id } => {
                manga::cmd_manga_chapters(&data_dir(), &source, &manga_id)
            }
            MangaCommand::Download {
                source,
                manga_id,
                chapter_id,
                library,
            } => manga::cmd_manga_download(&data_dir(), &source, &manga_id, &chapter_id, &library),
        },
        Command::Update { check, major } => cmd_update(check, major),
        Command::Browse {
            library,
            screenshot,
        } => cmd_browse(library, screenshot),
        Command::Read {
            path,
            progress_file,
        } => cmd_read(path, progress_file),
    }
}

fn cmd_info(path: PathBuf) -> Result<()> {
    let doc = CbzDocument::open(&path)?;
    println!("Title:  {}", doc.title());
    println!("Pages:  {}", doc.page_count());
    if let Some(info) = doc.comic_info() {
        if let Some(series) = &info.series {
            println!("Series: {series}");
        }
        if let Some(number) = &info.number {
            println!("Number: {number}");
        }
        if let Some(writer) = &info.writer {
            println!("Writer: {writer}");
        }
    }
    println!();
    for (i, name) in doc.page_names().iter().enumerate() {
        println!("{:>5}  {name}", i + 1);
    }
    Ok(())
}

fn cmd_render(
    path: PathBuf,
    page: usize,
    out: PathBuf,
    width: u32,
    height: u32,
    dither: bool,
) -> Result<()> {
    let mut doc = CbzDocument::open(&path)?;
    if page == 0 || page > doc.page_count() {
        bail!(
            "page {page} out of range (document has {} pages)",
            doc.page_count()
        );
    }

    let image = doc.decode_page(page - 1)?;
    let opts = RenderOptions {
        screen_width: width,
        screen_height: height,
        fit: FitMode::Contain,
        dither,
    };
    let rendered = render_page(&image, &opts);

    let gray = image::GrayImage::from_raw(rendered.width, rendered.height, rendered.pixels)
        .context("rendered page buffer has unexpected size")?;
    gray.save(&out)?;
    println!(
        "Rendered page {page}/{} of '{}' to {} ({}x{})",
        doc.page_count(),
        doc.title(),
        out.display(),
        rendered.width,
        rendered.height,
    );
    Ok(())
}

fn cmd_library(dir: PathBuf) -> Result<()> {
    // First boot on a device: the library folder may not exist yet. Create
    // it instead of erroring — the NickelMenu launcher points here before
    // the user has copied any manga over.
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("couldn't create library directory {}", dir.display()))?;
        println!(
            "Library initialized at {}.\nCopy .cbz files there and open gideon again.",
            dir.display()
        );
        return Ok(());
    }

    let library = Library::new(&dir);
    let entries = library.scan()?;
    if entries.is_empty() {
        println!(
            "No CBZ files found under {}.\nCopy .cbz files there and open gideon again.",
            dir.display()
        );
        return Ok(());
    }

    // Initialize the progress store on first scan so `gideon read` can find
    // the library root by walking up from a document.
    let store_path = progress_path(&dir);
    let store = ProgressStore::load(&store_path)?;
    if !store_path.exists() {
        store.save(&store_path)?;
    }
    println!("{} document(s) in {}\n", entries.len(), dir.display());
    for entry in entries {
        match store.get(&entry.relative_path) {
            Some(p) if p.is_finished() => {
                println!("  [done ] {}", entry.relative_path);
            }
            Some(p) => {
                println!(
                    "  [{:>4.0}%] {} (page {}/{})",
                    p.percent(),
                    entry.relative_path,
                    p.current_page + 1,
                    p.total_pages
                );
            }
            None => println!("  [ new ] {}", entry.relative_path),
        }
    }
    Ok(())
}

fn cmd_sources(add_lists: Vec<Url>, no_defaults: bool) -> Result<()> {
    let mut lists = if no_defaults {
        SourceLists::new(Vec::new())
    } else {
        SourceLists::default()
    };
    // Source lists configured in settings.json are always included.
    let settings = gideon_core::Settings::load(&data_dir())?;
    for raw in &settings.source_lists {
        match Url::parse(raw) {
            Ok(url) => lists.add(url),
            Err(e) => eprintln!("warning: ignoring invalid source list '{raw}' in settings: {e}"),
        }
    }
    for url in add_lists {
        lists.add(url);
    }
    if lists.urls().is_empty() {
        bail!("no source lists configured");
    }

    println!("Fetching {} source list(s)...", lists.urls().len());
    let fetcher = UreqFetcher::new();
    let sources = lists.available_sources(&fetcher)?;

    println!("{} source(s) available:\n", sources.len());
    for source in sources {
        let lang = source.primary_language().unwrap_or("?");
        let origin = source.origin.as_deref().unwrap_or("?");
        println!(
            "  {:<30} [{}] v{} — {} (from {})",
            source.name, lang, source.version, source.id, origin
        );
    }
    Ok(())
}

fn progress_path(library_dir: &std::path::Path) -> PathBuf {
    library_dir.join(".gideon").join("progress.json")
}

/// Reader fit mode and rotation from settings.json. Lenient end to end: a
/// missing or unreadable settings file means the defaults (Contain, 0°) —
/// the reader must always come up.
fn reader_settings() -> (FitMode, u32) {
    let settings = gideon_core::Settings::load(&data_dir()).unwrap_or_default();
    (
        FitMode::from_setting(&settings.reader_fit),
        settings.reader_rotation,
    )
}

/// Data directory holding settings.json: $GIDEON_DATA_DIR if set (the Kobo
/// install uses .adds/gideon/data), otherwise ~/.config/gideon.
fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GIDEON_DATA_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".config").join("gideon"))
        .unwrap_or_else(|_| PathBuf::from(".gideon-data"))
}

fn cmd_shelf(dir: PathBuf, out: PathBuf, cols: u32, width: u32, height: u32) -> Result<()> {
    use gideon_render::shelf::{compose_shelf, ShelfEntry, ShelfLayout};

    let library = Library::new(&dir);
    let scanned = library.scan()?;
    if scanned.is_empty() {
        bail!("no CBZ files found under {}", dir.display());
    }
    let store = ProgressStore::load(&progress_path(&dir))?;

    let layout = ShelfLayout::new(width, height, cols);
    let mut entries = Vec::new();
    for entry in scanned.iter().take(layout.capacity()) {
        let mut doc = CbzDocument::open(&entry.path)?;
        let cover = doc.decode_page(0)?;
        let progress = store.get(&entry.relative_path).map(|p| {
            if p.total_pages == 0 {
                0.0
            } else {
                (p.current_page + 1) as f32 / p.total_pages as f32
            }
        });
        entries.push(ShelfEntry { cover, progress });
    }

    let page = compose_shelf(&entries, &layout);
    let gray = image::GrayImage::from_raw(page.width, page.height, page.pixels)
        .context("shelf buffer has unexpected size")?;
    gray.save(&out)?;
    println!(
        "Rendered shelf with {} cover(s) ({} visible max) to {}",
        entries.len(),
        layout.capacity(),
        out.display()
    );
    Ok(())
}

fn cmd_update(check_only: bool, allow_major: bool) -> Result<()> {
    use gideon_sources::update;

    let repo = std::env::var("GIDEON_UPDATE_REPO")
        .unwrap_or_else(|_| update::DEFAULT_UPDATE_REPO.to_string());
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: {current}");
    println!("Checking {repo} for updates...");

    let fetcher = UreqFetcher::new();

    // Primary: the VERSION asset on the latest release (no API, no rate
    // limits). Fallback: the GitHub API, the way bobo checks.
    let base = update::release_base();
    let release = match update::check_update_via_assets(&fetcher, &base, &repo, current) {
        Ok(release) => release,
        Err(assets_err) => match update::check_update(&fetcher, &repo, current) {
            Ok(release) => release,
            Err(api_err) => bail!(
                "couldn't check releases for {repo}.\n\
                 - asset check failed: {assets_err}\n\
                 - API check failed: {api_err}\n\
                 If the repository is private, OTA updates need a public repo \
                 or a GIDEON_GITHUB_TOKEN environment variable."
            ),
        },
    };

    let Some(release) = release else {
        println!("Already up to date.");
        return Ok(());
    };

    println!("Update available: {} -> {}", current, release.version);
    if let Some(notes) = &release.notes {
        println!("\n{notes}\n");
    }
    if check_only {
        println!("Run `gideon update` to install it.");
        return Ok(());
    }
    if !update::is_auto_installable(current, &release.version) && !allow_major {
        bail!(
            "{} is a major version bump from {} — review the release notes and \
             re-run with --major to install it.",
            release.version,
            current
        );
    }

    let exe = std::env::current_exe()?;
    let bin_dir = exe
        .parent()
        .context("can't determine the binary's directory")?;
    println!("Downloading {}...", release.asset_url);
    update::stage_update(&fetcher, &release, bin_dir)?;
    if update::apply_staged(bin_dir)? {
        println!(
            "Updated to {}. Restart gideon to run the new version (previous binary kept as gideon.old).",
            release.version
        );
    }
    Ok(())
}

/// Render the browse home screen headlessly to a PNG (desktop builds and
/// CI smoke tests).
fn browse_screenshot(library: PathBuf, out: PathBuf) -> Result<()> {
    use gideon_device::{Display as _, FakeInput, MemoryDisplay};

    let display = MemoryDisplay::new(1072, 1448);
    let input = FakeInput::new(Vec::new());
    let gateway = ui::AidokuGateway::new(data_dir());
    let mut app = ui::UiApp::new(display, input, gateway, library);
    app.render_once()?;

    let display = app.display();
    let gray =
        image::GrayImage::from_raw(display.width(), display.height(), display.buffer.clone())
            .context("screen buffer has unexpected size")?;
    gray.save(&out)?;
    println!("Rendered home screen to {}", out.display());
    Ok(())
}

#[cfg(feature = "kobo")]
fn cmd_browse(library: PathBuf, screenshot: Option<PathBuf>) -> Result<()> {
    use gideon_device::kobo::KoboDisplay;
    use gideon_device::kobo_input::KoboTouch;
    use gideon_device::Display as _;

    if let Some(out) = screenshot {
        return browse_screenshot(library, out);
    }

    let display = KoboDisplay::open()
        .context("failed to open the e-ink framebuffer — are you running on a Kobo device?")?;
    let (width, height) = (display.width(), display.height());
    let input = KoboTouch::open(width, height)
        .context("failed to open the touch screen — are you running on a Kobo device?")?;
    let gateway = ui::AidokuGateway::new(data_dir());
    let (fit, rotation) = reader_settings();
    ui::UiApp::new(display, input, gateway, library)
        .with_reader_settings(fit, rotation)
        .run()
}

#[cfg(not(feature = "kobo"))]
fn cmd_browse(library: PathBuf, screenshot: Option<PathBuf>) -> Result<()> {
    match screenshot {
        Some(out) => browse_screenshot(library, out),
        None => {
            println!(
                "gideon browse drives the e-ink display and touch screen, which need \
                 a Kobo device build (--features kobo).\nUse --screenshot out.png to \
                 render the home screen headlessly instead."
            );
            Ok(())
        }
    }
}

#[cfg(feature = "kobo")]
fn cmd_read(path: PathBuf, progress_file: Option<PathBuf>) -> Result<()> {
    use gideon_device::kobo::KoboDisplay;

    let display = KoboDisplay::open()
        .context("failed to open the e-ink framebuffer — are you running on a Kobo device?")?;
    run_reader(path, progress_file, display)
}

#[cfg(not(feature = "kobo"))]
fn cmd_read(path: PathBuf, progress_file: Option<PathBuf>) -> Result<()> {
    // Desktop build: drive a memory display from terminal input so the
    // reading loop is exercisable during development.
    let display = gideon_device::MemoryDisplay::new(1072, 1448);
    run_reader(path, progress_file, display)
}

fn run_reader<D: gideon_device::Display>(
    path: PathBuf,
    progress_file: Option<PathBuf>,
    display: D,
) -> Result<()> {
    use std::io::BufRead;

    // Use the same progress file `gideon library` reads: walk up from the
    // document looking for an existing .gideon/progress.json, falling back
    // to the file's own directory.
    let path = path.canonicalize().unwrap_or(path);
    let library_root = path
        .ancestors()
        .skip(1)
        .find(|dir| progress_path(dir).exists())
        .map(PathBuf::from)
        .or_else(|| path.parent().map(PathBuf::from))
        .unwrap_or_default();
    let progress_file = progress_file.unwrap_or_else(|| progress_path(&library_root));
    let progress_key = path
        .strip_prefix(&library_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string());

    let doc = CbzDocument::open(&path)?;
    let mut store = ProgressStore::load(&progress_file)?;
    let (fit, rotation) = reader_settings();
    let mut reader = Reader::new(doc, display, fit, rotation);
    reader.resume_from(&store, &progress_key);
    reader.show_current_page()?;

    println!(
        "Reading '{}' — page {}/{}",
        reader.title(),
        reader.current_page() + 1,
        reader.page_count()
    );
    println!("Commands: n (next), p (prev), q (quit)");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        match line.trim() {
            "n" => {
                if !reader.next_page()? {
                    println!("End of document.");
                }
            }
            "p" => {
                if !reader.prev_page()? {
                    println!("Already at the first page.");
                }
            }
            "q" => break,
            _ => continue,
        }
        println!("Page {}/{}", reader.current_page() + 1, reader.page_count());
        reader.save_progress(&mut store, &progress_key);
        store.save(&progress_file)?;
    }

    reader.save_progress(&mut store, &progress_key);
    store.save(&progress_file)?;
    Ok(())
}
