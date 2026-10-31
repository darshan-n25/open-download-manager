use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use odm::bt::{engine, magnet::Magnet, stream, torrent::Torrent};
use odm::config::Config;
use odm::error::{OdmError, Result};

#[derive(Parser, Debug)]
#[command(
    name = "odm-bt",
    version,
    about = "BitTorrent companion to Open Download Manager"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print metadata from a .torrent file.
    Info { torrent: PathBuf },
    /// Download a torrent to the download directory.
    ///
    /// The source is either a .torrent file or a magnet link.
    Download {
        source: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Start a torrent and serve one of its files over localhost while it downloads.
    ///
    /// Prints a URL that any player can open. The transfer continues in the
    /// background; the process stops when Ctrl-C is pressed.
    Stream {
        source: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Which file in the torrent to serve, by index (see `odm-bt info`).
        #[arg(short, long, default_value_t = 0)]
        file: usize,
    },
}

#[tokio::main]
async fn main() {
    // The engine's diagnostics are only worth having if the companion can print
    // them: `RUST_LOG=odm=debug odm-bt download ...` is how a stuck swarm gets
    // diagnosed.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("odm=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Info { torrent } => {
            let t = engine::read_torrent(&torrent)?;
            println!("name:        {}", t.name);
            println!("size:        {} bytes", t.length);
            println!("piece size:  {}", t.piece_length);
            println!("pieces:      {}", t.num_pieces());
            println!("info hash:   {}", hex::encode(t.info_hash));
            println!("files:       {}", t.num_files());
            for (i, f) in t.files.iter().enumerate() {
                println!("  [{i}] {:>12}  {}", f.length, f.display_name());
            }
            println!("announce:    {}", t.announce);
            for tier in &t.tiers {
                println!("tracker tier: {}", tier.join(", "));
            }
            Ok(())
        }
        Cmd::Download { source, out } => {
            let dir = out.unwrap_or_else(|| Config::load().download_dir);
            std::fs::create_dir_all(&dir).ok();
            let (torrent, session) = open(&source, &dir).await?;
            let reporter = tokio::spawn(report(session.clone()));
            let result = session.wait().await;
            reporter.abort();
            result?;
            let path = engine::output_path(&torrent, &dir);
            println!("done -> {}", path.display());
            Ok(())
        }
        Cmd::Stream { source, out, file } => {
            let dir = out.unwrap_or_else(|| Config::load().download_dir);
            std::fs::create_dir_all(&dir).ok();
            let (torrent, session) = open(&source, &dir).await?;
            if file >= torrent.num_files() {
                return Err(OdmError::other(format!(
                    "{} has {} files",
                    torrent.name,
                    torrent.num_files()
                )));
            }
            let info = stream::start(session.clone(), file).await?;
            println!("streaming {}", torrent.files[file].display_name());
            println!("url:      {}", info.url);
            println!("downloading in the background; press Ctrl-C to stop");
            let reporter = tokio::spawn(report(session.clone()));
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = session.wait() => {
                    println!("download complete; still serving {}", info.url);
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            reporter.abort();
            Ok(())
        }
    }
}

/// A `.torrent` path or a magnet link, opened the same way.
async fn open(
    source: &str,
    dir: &std::path::Path,
) -> Result<(Arc<Torrent>, Arc<odm::bt::session::Session>)> {
    if source
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("magnet:")
    {
        let magnet = Magnet::parse(source)?;
        return engine::open_magnet(&magnet, dir, None, None).await;
    }
    let torrent = Arc::new(engine::read_torrent(std::path::Path::new(source))?);
    let session = engine::open_torrent(torrent.clone(), dir.to_path_buf(), None, None).await?;
    Ok((torrent, session))
}

async fn report(session: Arc<odm::bt::session::Session>) {
    let last = AtomicU64::new(0);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let p = session.progress();
        let pct = if p.total > 0 {
            p.downloaded as f64 * 100.0 / p.total as f64
        } else {
            0.0
        };
        let delta = p.downloaded.saturating_sub(last.load(Ordering::Relaxed));
        last.store(p.downloaded, Ordering::Relaxed);
        eprintln!(
            "{:>6.2}%  {:>10} / {:<10}  {:>9}/s  {:>3} peers  (+{})",
            pct,
            odm::util::human_bytes(p.downloaded),
            odm::util::human_bytes(p.total),
            odm::util::human_bytes(delta as f64 as u64),
            p.peers,
            odm::util::human_bytes(delta),
        );
    }
}
