use std::path::PathBuf;

use clap::{Parser, Subcommand};
use riptide::bt::engine;
use riptide::bt::torrent::Torrent;
use riptide::bt::tracker;
use riptide::config::Config;
use riptide::error::Result;

#[derive(Parser, Debug)]
#[command(
    name = "riptide-bt",
    version,
    about = "BitTorrent companion to riptide"
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
    Download {
        torrent: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Info { torrent } => {
            let data =
                std::fs::read(&torrent).map_err(|e| riptide::RiptideError::other(e.to_string()))?;
            let t = Torrent::from_bytes(&data)?;
            println!("name:        {}", t.name);
            println!("size:        {} bytes", t.length);
            println!("piece size:  {}", t.piece_length);
            println!("pieces:      {}", t.num_pieces());
            println!("info hash:   {}", hex::encode(t.info_hash));
            println!("announce:    {}", t.announce);
            Ok(())
        }
        Cmd::Download { torrent, out } => {
            let cfg = Config::load();
            let data =
                std::fs::read(&torrent).map_err(|e| riptide::RiptideError::other(e.to_string()))?;
            let t = Torrent::from_bytes(&data)?;
            println!("{}: {} bytes, {} pieces", t.name, t.length, t.num_pieces());

            let peer_id = *b"-RT0001-0123456789ab";
            let port = 6881u16;
            let peers = tracker::announce(&t, &peer_id, port, t.length).await?;
            println!("tracker returned {} peers", peers.len());
            if peers.is_empty() {
                return Err(riptide::RiptideError::other("no peers from tracker"));
            }

            let dir = out.unwrap_or_else(|| cfg.download_dir.clone());
            std::fs::create_dir_all(&dir).ok();
            let out_path = dir.join(&t.name);
            engine::download(&t, &peers, &out_path).await?;
            println!("done -> {}", out_path.display());
            Ok(())
        }
    }
}
