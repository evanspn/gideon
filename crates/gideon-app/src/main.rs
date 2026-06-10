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

mod reader;

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

    /// Open a CBZ for reading.
    Read {
        path: PathBuf,
        /// Progress file (defaults to .gideon/progress.json next to the file).
        #[arg(long)]
        progress_file: Option<PathBuf>,
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
    let library = Library::new(&dir);
    let entries = library.scan()?;
    if entries.is_empty() {
        println!("No CBZ files found under {}", dir.display());
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
        let lang = source.lang.as_deref().unwrap_or("?");
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
    let mut reader = Reader::new(doc, display, FitMode::Contain);
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
