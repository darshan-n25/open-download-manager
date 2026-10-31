//! The torrent front door: turn a `.torrent` file or a magnet link into output.
//!
//! Everything interesting lives in [`crate::bt::session`] — this module only decides
//! where the metadata comes from, where the files go, and what "finished" means.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use crate::bt::magnet::Magnet;
use crate::bt::session::{Options, ProgressFn, Session, TorrentProgress};
use crate::bt::torrent::Torrent;
use crate::bt::tracker::{self, Announce, Peer};
use crate::error::{OdmError, Result};

/// Read a `.torrent` file.
pub fn read_torrent(path: &Path) -> Result<Torrent> {
    let data = std::fs::read(path).map_err(|e| OdmError::other(e.to_string()))?;
    Torrent::from_bytes(&data)
}

/// Start a download and return the live session — the handle a player streams from.
pub async fn start(
    torrent: Arc<Torrent>,
    base: PathBuf,
    peers: Vec<SocketAddr>,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Arc<Session>> {
    let cancel = cancel.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let session = Session::start(torrent, base, Options::default(), progress, cancel).await?;
    session.add_peers(peers);
    Ok(session)
}

/// Download a torrent's files into `base`, and return the path of the main file.
pub async fn download_with(
    torrent: &Torrent,
    peers: &[Peer],
    base: &Path,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf> {
    let addrs: Vec<SocketAddr> = peers.iter().map(|p| p.addr).collect();
    let session = start(
        Arc::new(torrent.clone()),
        base.to_path_buf(),
        addrs,
        progress,
        cancel,
    )
    .await?;
    session.wait().await?;
    Ok(output_path(torrent, base))
}

pub async fn download(torrent: &Torrent, peers: &[Peer], base: &Path) -> Result<PathBuf> {
    download_with(torrent, peers, base, None, None).await
}

/// The file a finished download is "about": the largest one, which for a movie
/// release is the movie and not the sample or the subtitles.
pub fn output_path(torrent: &Torrent, base: &Path) -> PathBuf {
    let biggest = torrent
        .files
        .iter()
        .max_by_key(|f| f.length)
        .or_else(|| torrent.files.first());
    match biggest {
        Some(f) => base.join(f.relative_path()),
        None => base.join(&torrent.name),
    }
}

/// Download whatever a magnet link points at.
///
/// A magnet carries a hash and perhaps some trackers but no file information, so the
/// swarm is asked for peers first (trackers, then the DHT) and the torrent's metadata
/// is fetched from those peers. Only then can any piece be checked.
pub async fn download_magnet(
    magnet: &Magnet,
    out_dir: &Path,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf> {
    let (torrent, session) = open_magnet(magnet, out_dir, progress, cancel).await?;
    session.wait().await?;
    Ok(output_path(&torrent, out_dir))
}

/// Resolve a magnet to a torrent and a *running* session — what a player needs,
/// because streaming has to start before the download finishes.
pub async fn open_magnet(
    magnet: &Magnet,
    out_dir: &Path,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(Arc<Torrent>, Arc<Session>)> {
    let cancel = cancel.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let peers = find_peers_for_magnet(magnet, &cancel).await?;
    let mut torrent = crate::bt::metadata::fetch(&magnet.info_hash, &peers).await?;
    if torrent.tiers.is_empty() && !magnet.trackers.is_empty() {
        torrent.tiers = vec![magnet.trackers.clone()];
        torrent.announce_list = magnet.trackers.clone();
    }
    if let Some(name) = &magnet.name {
        if torrent.name.is_empty() {
            torrent.name = name.clone();
        }
    }

    let addrs: Vec<SocketAddr> = peers.iter().map(|p| p.addr).collect();
    let torrent = Arc::new(torrent);
    let session = start(
        torrent.clone(),
        out_dir.to_path_buf(),
        addrs,
        progress,
        Some(cancel),
    )
    .await?;
    Ok((torrent, session))
}

/// Open a `.torrent` file and start it, letting trackers and the DHT find peers.
pub async fn open_torrent(
    torrent: Arc<Torrent>,
    base: PathBuf,
    progress: Option<ProgressFn>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Arc<Session>> {
    start(torrent, base, Vec::new(), progress, cancel).await
}

/// Peers for a magnet's info hash: every tracker, plus the DHT.
///
/// This is deliberately not "the first tracker that answers": in a public swarm a
/// single tracker's list can be entirely unreachable peers, while the reachable few
/// are listed elsewhere. The DHT runs at the same time, because a magnet is often
/// only alive there.
async fn find_peers_for_magnet(magnet: &Magnet, cancel: &Arc<AtomicBool>) -> Result<Vec<Peer>> {
    let mut seen = std::collections::HashSet::new();
    let mut peers: Vec<Peer> = Vec::new();

    // The DHT walk in the background; it may take longer than the trackers.
    let found = Arc::new(std::sync::Mutex::new(Vec::<SocketAddr>::new()));
    let sink = found.clone();
    let dht_cancel = cancel.clone();
    let info_hash = magnet.info_hash;
    let dht = tokio::spawn(async move {
        crate::bt::dht::lookup(
            info_hash,
            Arc::new(move |addr| {
                if let Ok(mut v) = sink.lock() {
                    v.push(addr);
                }
            }),
            dht_cancel,
        )
        .await
    });

    let mut announces = tokio::task::JoinSet::new();
    for url in &magnet.trackers {
        let url = url.clone();
        let mut req = Announce::new(magnet.info_hash, crate::bt::session::peer_id(), 6881);
        // `xl` is the size the magnet advertises. A tracker told `left=0` thinks
        // we are a seeder and may hand a leecher's share of peers to somebody
        // else; when the magnet does not carry `xl`, say "one byte left" rather
        // than "finished" — the size is unknown, not zero.
        req.left = magnet.length.or(Some(1));
        announces.spawn(async move {
            match tracker::announce(&url, &req).await {
                Ok(resp) => Some((url, resp.peers)),
                Err(e) => {
                    tracing::debug!(tracker = url, "magnet announce failed: {e}");
                    None
                }
            }
        });
    }
    // A magnet can list a dozen trackers, several of them dead. Waiting for the
    // slowest HTTP tracker before even asking a peer for metadata makes a healthy
    // swarm look like a magnet that never starts, so the tracker phase is bounded:
    // whatever has answered by then is enough to begin.
    let collect = async {
        while let Some(joined) = announces.join_next().await {
            let Ok(Some((url, list))) = joined else {
                continue;
            };
            tracing::debug!(tracker = url, peers = list.len(), "magnet announce");
            for addr in list {
                if seen.insert(addr) {
                    peers.push(Peer::new(addr));
                }
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(20), collect)
        .await
        .is_err()
    {
        tracing::debug!("magnet trackers: still answering after 20s, using the peers found so far");
        announces.abort_all();
    }

    // Give the walk a bounded slice of the user's patience: it usually answers in
    // a few seconds, and a magnet that is alive on the trackers should not wait for
    // a slow DHT round.
    match tokio::time::timeout(Duration::from_secs(12), dht).await {
        Ok(Ok(Ok(n))) => tracing::info!(peers = n, "dht answered"),
        Ok(Ok(Err(e))) => tracing::debug!("dht: {e}"),
        Ok(Err(e)) => tracing::debug!("dht task: {e}"),
        Err(_) => tracing::debug!("dht: still walking after 12s"),
    }
    for addr in found.lock().unwrap().iter().copied() {
        if seen.insert(addr) {
            peers.push(Peer::new(addr));
        }
    }

    if peers.is_empty() {
        return Err(OdmError::other(
            "no tracker and no DHT node returned a peer for this magnet",
        ));
    }
    Ok(peers)
}

/// Keep a session alive and report progress on stdout — the shape `odm-bt download`
/// and the streaming command both want.
pub async fn wait_with_progress(
    session: &Arc<Session>,
    progress: Option<ProgressFn>,
) -> Result<()> {
    let _ = progress;
    loop {
        if session.is_done() {
            return Ok(());
        }
        if session.is_cancelled() {
            return Err(OdmError::other("torrent download cancelled"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let p: TorrentProgress = session.progress();
        if p.total > 0 && p.downloaded > 0 {
            tracing::debug!(downloaded = p.downloaded, total = p.total, "progress");
        }
    }
}
