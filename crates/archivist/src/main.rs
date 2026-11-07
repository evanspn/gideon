//! `archivist`: the Internet Archive's free books from the command line.
//!
//!   archivist search "slaughterhouse five"
//!   archivist show slaughterhouse-five-kurt-vonnegut
//!   archivist get slaughterhouse-five-kurt-vonnegut --out /mnt/onboard/Books
//!   archivist popular

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use archivist::{Client, SearchOptions, Sort};

#[derive(Parser)]
#[command(
    name = "archivist",
    version,
    about = "Search and download public-domain books from the Internet Archive"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search by title or author.
    Search {
        query: Vec<String>,
        /// Results per page.
        #[arg(long, default_value_t = 25)]
        rows: u32,
        /// Page number (1-based).
        #[arg(long, default_value_t = 1)]
        page: u32,
        /// Order: downloads, added, title.
        #[arg(long, default_value = "downloads")]
        sort: String,
        /// Match the words anywhere in the record, not just title/author.
        #[arg(long)]
        anywhere: bool,
        /// Include items that only have a PDF.
        #[arg(long)]
        pdf: bool,
    },
    /// The most downloaded free books.
    Popular {
        #[arg(long, default_value_t = 25)]
        rows: u32,
        #[arg(long, default_value_t = 1)]
        page: u32,
    },
    /// Show an item: its metadata and downloadable files.
    Show { identifier: String },
    /// Download an item's EPUB (or PDF) into a Books folder as Author/Title.ext.
    Get {
        identifier: String,
        /// Books folder (/mnt/onboard/Books on a Kobo).
        #[arg(long, default_value = ".")]
        out: PathBuf,
        /// Take the PDF instead of the EPUB.
        #[arg(long)]
        pdf: bool,
        /// Download this exact file name from the item.
        #[arg(long)]
        file: Option<String>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("archivist: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> archivist::Result<()> {
    let client = Client::new();
    match cli.command {
        Command::Search {
            query,
            rows,
            page,
            sort,
            anywhere,
            pdf,
        } => {
            let sort = match sort.as_str() {
                "added" => Sort::Added,
                "title" => Sort::Title,
                _ => Sort::Downloads,
            };
            let opts = SearchOptions {
                rows,
                page,
                sort,
                anywhere,
                allow_pdf_only: pdf,
            };
            let result = client.search(&query.join(" "), &opts)?;
            print_hits(&result);
        }
        Command::Popular { rows, page } => {
            let result = client.popular(page, rows)?;
            print_hits(&result);
        }
        Command::Show { identifier } => {
            let item = client.item(&identifier)?;
            println!("{}", item.label());
            println!("  identifier: {}", item.identifier);
            if let Some(d) = &item.date {
                println!("  date:       {d}");
            }
            if let Some(l) = &item.language {
                println!("  language:   {l}");
            }
            if let Some(l) = &item.license_url {
                println!("  license:    {l}");
            }
            if let Some(d) = &item.description {
                let one_line: String = d.split_whitespace().collect::<Vec<_>>().join(" ");
                let short: String = one_line.chars().take(200).collect();
                println!("  about:      {short}");
            }
            println!("  files:");
            for f in item.files.iter().filter(|f| f.is_epub() || f.is_pdf()) {
                println!("    [{:>9}] {}  ({})", fmt_bytes(f.size), f.name, f.format);
            }
            if let Some(e) = item.best_epub() {
                println!("  best EPUB:  {}", e.name);
            }
        }
        Command::Get {
            identifier,
            out,
            pdf,
            file,
        } => {
            let item = client.item(&identifier)?;
            let chosen = match file {
                Some(name) => item.files.iter().find(|f| f.name == name).ok_or_else(|| {
                    archivist::Error::NoFile {
                        identifier: identifier.clone(),
                        what: "file by that name",
                    }
                })?,
                None if pdf => item.best_pdf().ok_or_else(|| archivist::Error::NoFile {
                    identifier: identifier.clone(),
                    what: "PDF",
                })?,
                None => item.best_epub().ok_or_else(|| archivist::Error::NoFile {
                    identifier: identifier.clone(),
                    what: "EPUB",
                })?,
            };
            eprintln!("downloading {} ({})…", chosen.name, fmt_bytes(chosen.size));
            let (path, existed) = client.download(&item, chosen, &out)?;
            if existed {
                println!("already here: {}", path.display());
            } else {
                println!("saved {}", path.display());
            }
        }
    }
    Ok(())
}

fn print_hits(page: &archivist::SearchPage) {
    if page.hits.is_empty() {
        println!("nothing found");
        return;
    }
    for hit in &page.hits {
        println!("{}", hit.label());
        println!(
            "    {}  ·  {} downloads{}",
            hit.identifier,
            hit.downloads,
            hit.language
                .as_deref()
                .map(|l| format!("  ·  {l}"))
                .unwrap_or_default()
        );
    }
    println!(
        "\n{} of {} (page {}){}",
        page.hits.len(),
        page.total,
        page.page,
        if page.has_more() {
            " — more with --page"
        } else {
            ""
        }
    );
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{} KB", n >> 10)
    } else {
        format!("{n} B")
    }
}
