//! One torrent, many peers at once.
//!
//! The session owns the three things that outlive any single connection — the
//! picker, the store and the peer table — and the long-running loops that fill the
//! peer table: trackers, the DHT, and the peer manager that keeps a connection per
//! address alive with a backoff.
//!
//! A peer connection is cheap to retry and expensive to keep idle, so the manager
//! calls the shots: it decides how many connections to run, restarts the ones that
//! died, and forgets the ones that keep failing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Notify};

use crate::bt::picker::{self, Outcome, Picker, Request};
use crate::bt::store::Store;
use crate::bt::torrent::Torrent;
use crate::bt::tracker::{self, Announce, Event};
use crate::error::{OdmError, Result};

/// How a torrent download is going.
#[derive(Clone, Copy, Debug, Default)]
pub struct TorrentProgress {
    /// Verified bytes on disk.
    pub downloaded: u64,
    /// The total size.
    pub total: u64,
    /// The current rate, in bytes per second.
    pub speed_bps: f64,
    /// Live peer connections.
    pub peers: usize,
}

/// Reported as the download proceeds. Called from a background task, so it should be
/// cheap and must not block.
pub type ProgressFn = Arc<dyn Fn(TorrentProgress) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Options {
    /// Upper bound on simultaneous peer connections.
    pub max_peers: usize,
    /// The port we claim in announces. We download only, so this is cosmetic.
    pub port: u16,
    /// Where the resume file lives.
    pub state_dir: PathBuf,
    /// Ask trackers and the DHT for peers.
    pub use_trackers: bool,
    pub use_dht: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_peers: 80,
            port: 6881,
            state_dir: crate::config::Config::load().state_dir,
            use_trackers: true,
            use_dht: true,
        }
    }
}

struct PeerSlot {
    have: Vec<u8>,
    /// Bytes this connection has delivered (for the per-peer rate).
    down: u64,
    last_block: Instant,
}

pub struct Session {
    pub torrent: Arc<Torrent>,
    pub store: Arc<Store>,
    picker: Mutex<Picker>,
    peers: Mutex<HashMap<SocketAddr, PeerSlot>>,
    peer_tx: mpsc::Sender<SocketAddr>,
    cancel: Arc<AtomicBool>,
    /// Set while the user has paused the torrent. Connections stay up and
    /// finish the blocks they already have, but the picker hands out no new
    /// work, so resuming is instant and loses nothing.
    paused: AtomicBool,
    done: AtomicBool,
    complete_announced: AtomicBool,
    options: Options,
    progress: Option<ProgressFn>,
    notify: Notify,
    resume_path: PathBuf,
    resume_dirty: AtomicBool,
    verified: AtomicU64,
    speed: Mutex<(u64, Instant, f64)>,
    connected: AtomicU32,
    started: Instant,
    /// Which peers supplied each piece currently being assembled. When a piece
    /// fails its hash, this is how we know who to distrust.
    piece_src: Mutex<HashMap<usize, Vec<SocketAddr>>>,
    hash_failures: Mutex<HashMap<SocketAddr, u32>>,
    /// Peers that have sent too much bad data to be worth another request.
    banned: Mutex<std::collections::HashSet<SocketAddr>>,
}

impl Session {
    /// Start downloading into `base`. The download runs in the background; await
    /// [`Session::wait`] (or use [`Session::stream_url`] while it runs).
    pub async fn start(
        torrent: Arc<Torrent>,
        base: PathBuf,
        options: Options,
        progress: Option<ProgressFn>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Arc<Session>> {
        std::fs::create_dir_all(&base).ok();
        let store = Arc::new(Store::create(&base, &torrent)?);
        let mut picker = Picker::new(&torrent);

        std::fs::create_dir_all(&options.state_dir).ok();
        let resume_path = options
            .state_dir
            .join("bt")
            .join(format!("{}.json", hex::encode(torrent.info_hash)));
        // Only trust a resume bitfield when the files were already on disk. A file
        // that had to be created is preallocated to its final length, and that looks
        // exactly like a finished download to a bitfield that never reads the bytes:
        // the job then reports 100% instantly and leaves a file of zeros behind.
        if !store.is_fresh() {
            if let Some(bits) = load_resume(&resume_path, &torrent, &base) {
                for (i, have) in bits.iter().enumerate() {
                    if *have {
                        picker.mark_have(i);
                    }
                }
            }
        }

        let verified = picker.verified_bytes(torrent.piece_length, torrent.length);
        // Room for the union of every tracker's peer list: the manager keeps a
        // large table now, and dropping addresses here would lose the reachable
        // minority that the next tracker listed first.
        let (peer_tx, peer_rx) = mpsc::channel::<SocketAddr>(4096);
        let session = Arc::new(Session {
            torrent: torrent.clone(),
            store,
            picker: Mutex::new(picker),
            peers: Mutex::new(HashMap::new()),
            peer_tx,
            cancel: cancel.clone(),
            paused: AtomicBool::new(false),
            done: AtomicBool::new(false),
            complete_announced: AtomicBool::new(false),
            options: options.clone(),
            progress,
            notify: Notify::new(),
            resume_path,
            resume_dirty: AtomicBool::new(false),
            verified: AtomicU64::new(verified),
            speed: Mutex::new((verified, Instant::now(), 0.0)),
            connected: AtomicU32::new(0),
            started: Instant::now(),
            piece_src: Mutex::new(HashMap::new()),
            hash_failures: Mutex::new(HashMap::new()),
            banned: Mutex::new(std::collections::HashSet::new()),
        });

        if session.is_complete() {
            session.finish().await?;
            return Ok(session);
        }

        tokio::spawn(peer_manager(session.clone(), peer_rx));
        if options.use_trackers && !torrent.tiers.is_empty() {
            tokio::spawn(tracker_loop(session.clone()));
        }
        if options.use_dht && !torrent.private {
            tokio::spawn(dht_loop(session.clone()));
        }
        tokio::spawn(tick_loop(session.clone()));

        Ok(session)
    }

    pub fn info_hash(&self) -> [u8; 20] {
        self.torrent.info_hash
    }

    pub fn peer_id(&self) -> [u8; 20] {
        peer_id()
    }

    pub fn port(&self) -> u16 {
        self.options.port
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Pause or resume handing out new blocks.
    ///
    /// Pausing is deliberately not the same as cancelling: the connections, the
    /// pieces in flight and the picker's bookkeeping all stay, so a resume is a
    /// flag flip rather than a fresh start against the swarm.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    fn is_complete(&self) -> bool {
        self.picker.lock().unwrap().is_complete()
    }

    pub fn verified_bytes(&self) -> u64 {
        self.verified.load(Ordering::Relaxed)
    }

    pub fn have_count(&self) -> usize {
        self.picker.lock().unwrap().have_count()
    }

    pub fn piece_count(&self) -> usize {
        self.torrent.num_pieces()
    }

    pub fn live_peers(&self) -> u32 {
        self.connected.load(Ordering::Relaxed)
    }

    /// Add a peer we learned from a tracker, the DHT or peer exchange.
    pub fn add_peer(&self, addr: SocketAddr) {
        if addr.port() == 0 || self.is_done() || self.is_cancelled() || self.is_banned(addr) {
            return;
        }
        let _ = self.peer_tx.try_send(addr);
    }

    /// A peer that has failed several hash checks is not worth another request: it
    /// is either broken or deliberately poisoning the swarm.
    pub fn is_banned(&self, addr: SocketAddr) -> bool {
        self.banned.lock().unwrap().contains(&addr)
    }

    fn note_hash_failure(&self, sources: &[SocketAddr]) {
        // A piece assembled from several peers says the data was wrong, not who was
        // wrong. Blaming everyone would eventually ban the honest peer too. Such a
        // piece is re-fetched from one peer at a time (the picker marks it
        // exclusive), so the *next* failure names somebody.
        if sources.len() != 1 {
            return;
        }
        let mut ban = Vec::new();
        {
            let mut fails = self.hash_failures.lock().unwrap();
            for addr in sources {
                let n = fails.entry(*addr).or_insert(0);
                *n += 1;
                // One corrupt piece can be damage in transit. Three is a pattern.
                if *n >= 3 {
                    ban.push(*addr);
                }
            }
        }
        if ban.is_empty() {
            return;
        }
        let mut banned = self.banned.lock().unwrap();
        for addr in ban {
            if banned.insert(addr) {
                tracing::debug!(%addr, "peer banned after repeated corrupt pieces");
            }
        }
    }

    pub fn add_peers<I: IntoIterator<Item = SocketAddr>>(&self, peers: I) {
        for p in peers {
            self.add_peer(p);
        }
    }

    /// Progress, measured over a window rather than since the start: a torrent that
    /// spent two minutes waiting for peers has not been slow for two minutes.
    pub fn progress(&self) -> TorrentProgress {
        let downloaded = self.verified_bytes();
        let now = Instant::now();
        let speed_bps = {
            let mut s = self.speed.lock().unwrap();
            let (last_bytes, last_at, ewma) = *s;
            let dt = now.duration_since(last_at).as_secs_f64();
            if dt >= 0.5 {
                let instant = (downloaded.saturating_sub(last_bytes)) as f64 / dt;
                let next = if ewma == 0.0 {
                    instant
                } else {
                    ewma * 0.6 + instant * 0.4
                };
                *s = (downloaded, now, next);
                next
            } else {
                ewma
            }
        };
        TorrentProgress {
            downloaded,
            total: self.torrent.length,
            speed_bps,
            peers: self.live_peers() as usize,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Ask for the pieces around a byte offset — the call a player makes when it
    /// starts, seeks, or runs low on buffer.
    pub fn seek(&self, byte_offset: u64) {
        let mut p = self.picker.lock().unwrap();
        let piece = p.piece_of_byte(byte_offset);
        // About 32 MiB ahead, or the next 8 pieces, whichever is more: enough for a
        // player to decode while the next pieces arrive, without starving the rest.
        let readahead = (32 * 1024 * 1024 / self.torrent.piece_length.max(1)) as usize;
        p.set_playhead(piece, readahead.max(8));
    }

    pub fn clear_seek(&self) {
        self.picker.lock().unwrap().clear_playhead();
    }

    /// Wait until a piece is verified on disk. Returns false on timeout or cancel.
    pub async fn wait_for_piece(&self, piece: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.picker.lock().unwrap().have_piece(piece) {
                return true;
            }
            if self.is_done() || self.is_cancelled() {
                return self.picker.lock().unwrap().have_piece(piece);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let _ =
                tokio::time::timeout(left.min(Duration::from_millis(250)), self.notify.notified())
                    .await;
        }
    }

    /// Read bytes that are already verified. The caller is responsible for waiting
    /// on the covering pieces first (see [`Session::wait_for_piece`]).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> crate::error::Result<()> {
        self.store
            .read_at(offset, buf)
            .map_err(|e| OdmError::other(format!("reading the torrent's data: {e}")))
    }

    /// Wait for the whole download to finish.
    pub async fn wait(&self) -> Result<()> {
        loop {
            if self.is_done() {
                return Ok(());
            }
            if self.is_cancelled() {
                return Err(OdmError::other("torrent download cancelled"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // ----- called by peer connections -------------------------------------

    pub fn register_peer(&self, addr: SocketAddr) {
        let mut peers = self.peers.lock().unwrap();
        if let Some(slot) = peers.get_mut(&addr) {
            // A reconnect racing the old socket's teardown: same peer, same slot,
            // and above all the same count.
            slot.last_block = Instant::now();
            return;
        }
        peers.insert(
            addr,
            PeerSlot {
                have: vec![0u8; self.torrent.num_pieces().div_ceil(8)],
                down: 0,
                last_block: Instant::now(),
            },
        );
        self.connected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn unregister_peer(&self, addr: SocketAddr) {
        let mut peers = self.peers.lock().unwrap();
        if peers.remove(&addr).is_some() {
            self.connected.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Bytes this connection has delivered, for the pipeline depth calculation.
    pub fn peer_progress(&self, addr: SocketAddr) -> u64 {
        self.peers
            .lock()
            .unwrap()
            .get(&addr)
            .map(|s| s.down)
            .unwrap_or(0)
    }

    pub fn peer_block(&self, addr: SocketAddr, bytes: usize) {
        if let Some(slot) = self.peers.lock().unwrap().get_mut(&addr) {
            slot.down += bytes as u64;
            slot.last_block = Instant::now();
        }
    }

    pub fn peer_is_stale(&self, addr: SocketAddr, idle: Duration) -> bool {
        self.peers
            .lock()
            .unwrap()
            .get(&addr)
            .map(|s| s.last_block.elapsed() > idle)
            .unwrap_or(false)
    }

    /// This peer's bitfield, which the picker needs to avoid asking for pieces the
    /// peer does not have.
    pub fn peer_have(&self, addr: SocketAddr) -> Vec<u8> {
        self.peers
            .lock()
            .unwrap()
            .get(&addr)
            .map(|s| s.have.clone())
            .unwrap_or_else(|| vec![0u8; self.torrent.num_pieces().div_ceil(8)])
    }

    pub fn set_peer_have(&self, addr: SocketAddr, have: Vec<u8>) {
        if let Some(slot) = self.peers.lock().unwrap().get_mut(&addr) {
            slot.have = have.clone();
        }
        self.picker.lock().unwrap().add_availability(&have);
    }

    pub fn set_peer_bit(&self, addr: SocketAddr, piece: usize) {
        let mut want = false;
        {
            let mut peers = self.peers.lock().unwrap();
            if let Some(slot) = peers.get_mut(&addr) {
                if let Some(byte) = slot.have.get_mut(piece / 8) {
                    *byte |= 0x80 >> (piece % 8);
                    want = true;
                }
            }
        }
        if want {
            self.picker.lock().unwrap().add_one(piece);
        }
    }

    /// The known peers, for peer exchange: addresses only, compact form.
    pub fn peer_addrs(&self) -> Vec<SocketAddr> {
        self.peers.lock().unwrap().keys().copied().collect()
    }

    pub fn pick(&self, addr: SocketAddr, now: Instant) -> Option<Request> {
        if self.is_banned(addr) || self.is_paused() {
            return None;
        }
        let have = self.peer_have(addr);
        self.picker.lock().unwrap().pick(key_of(addr), &have, now)
    }

    /// A connection ended: give its unfulfilled blocks back and stop counting it.
    pub fn drop_peer(&self, addr: SocketAddr) {
        self.picker.lock().unwrap().release_peer(key_of(addr));
        self.hash_failures.lock().unwrap().remove(&addr);
        self.unregister_peer(addr);
    }

    /// The peer will not deliver this block; hand it back to the swarm.
    pub fn release(&self, req: Request) {
        self.picker.lock().unwrap().release(req);
    }

    /// A block arrived: hash the piece when it is whole, write it, and tell everyone
    /// waiting on the disk.
    pub async fn on_block(&self, addr: SocketAddr, req: Request, data: &[u8]) {
        let outcome = {
            let mut p = self.picker.lock().unwrap();
            p.on_block(req.piece, req.begin, data, key_of(addr))
        };
        self.peer_block(addr, data.len());
        // Only data the picker accepted identifies who supplied the piece; a stale
        // copy it threw away must not be blamed on the peer that sent it.
        if !matches!(outcome, Outcome::Unexpected) {
            let mut src = self.piece_src.lock().unwrap();
            let list = src.entry(req.piece).or_default();
            if !list.contains(&addr) {
                list.push(addr);
            }
        }

        match outcome {
            Outcome::Unexpected => {}
            Outcome::Data => {}
            Outcome::Complete { piece, data } => {
                let sources = self
                    .piece_src
                    .lock()
                    .unwrap()
                    .remove(&piece)
                    .unwrap_or_default();
                let torrent = self.torrent.clone();
                let expected = torrent.pieces[piece];
                let ok = tokio::task::spawn_blocking(move || {
                    use sha1::{Digest, Sha1};
                    let mut h = Sha1::new();
                    h.update(&data);
                    let got: [u8; 20] = h.finalize().into();
                    (got == expected, data)
                })
                .await;
                let (ok, data) = match ok {
                    Ok(v) => v,
                    Err(_) => {
                        self.piece_src.lock().unwrap().remove(&piece);
                        self.picker.lock().unwrap().fail(piece);
                        return;
                    }
                };
                if !ok {
                    // A peer that sends a piece that does not hash is either broken
                    // or lying; the piece goes back in the queue and is fetched again
                    // — from someone else, if the same peers keep getting it wrong.
                    tracing::debug!(piece, "piece failed its hash");
                    self.picker.lock().unwrap().fail(piece);
                    self.note_hash_failure(&sources);
                    return;
                }
                let offset = piece as u64 * self.torrent.piece_length;
                let store = self.store.clone();
                let write =
                    tokio::task::spawn_blocking(move || store.write_at(offset, &data)).await;
                match write {
                    Ok(Ok(())) => {}
                    _ => {
                        self.piece_src.lock().unwrap().remove(&piece);
                        self.picker.lock().unwrap().fail(piece);
                        return;
                    }
                }
                {
                    let mut p = self.picker.lock().unwrap();
                    p.commit(piece);
                    let verified = p.verified_bytes(self.torrent.piece_length, self.torrent.length);
                    self.verified.store(verified, Ordering::Relaxed);
                }
                self.resume_dirty.store(true, Ordering::Relaxed);
                self.notify.notify_waiters();
                if self.is_complete() && !self.done.swap(true, Ordering::SeqCst) {
                    let _ = self.finish().await;
                }
            }
        }
    }

    // ----- lifecycle ------------------------------------------------------

    async fn finish(&self) -> Result<()> {
        let store = self.store.clone();
        let _ = tokio::task::spawn_blocking(move || store.sync()).await;
        self.save_resume();
        if !self.complete_announced.swap(true, Ordering::SeqCst) {
            let _ = tracker::announce(
                self.torrent
                    .announce_list
                    .first()
                    .map(String::as_str)
                    .unwrap_or(""),
                &self.announce_for(Event::Completed),
            )
            .await;
        }
        self.notify.notify_waiters();
        Ok(())
    }

    fn announce_for(&self, event: Event) -> Announce {
        let verified = self.verified_bytes();
        let mut a = Announce::new(self.torrent.info_hash, peer_id(), self.options.port);
        a.downloaded = verified;
        a.left = Some(self.torrent.length.saturating_sub(verified));
        a.event = event;
        if event == Event::Stopped {
            a.numwant = 0;
        }
        a
    }

    fn save_resume(&self) {
        let p = self.picker.lock().unwrap();
        let mut bits = vec![0u8; self.torrent.num_pieces().div_ceil(8)];
        for i in 0..self.torrent.num_pieces() {
            if p.have_piece(i) {
                bits[i / 8] |= 0x80 >> (i % 8);
            }
        }
        drop(p);
        let body = serde_json::json!({
            "version": 2,
            "have": base64_encode(&bits),
            "pieces": self.torrent.num_pieces(),
            "size": self.torrent.length,
            // The bitfield describes bytes at this location and nowhere else. The
            // same torrent downloaded to another folder must not inherit it.
            "base": resume_base(self.store.base()).to_string_lossy(),
            "updated": crate::util::now_ms(),
        });
        if let Some(parent) = self.resume_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = crate::util::atomic_write(&self.resume_path, body.to_string().as_bytes());
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.resume_dirty.load(Ordering::Relaxed) {
            self.save_resume();
        }
    }
}

/// A stable per-connection identity for the picker, derived from the address so it
/// survives a reconnect.
fn key_of(addr: SocketAddr) -> u32 {
    let mut h: u32 = 2166136261;
    for b in addr.to_string().as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

pub fn peer_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(b"-OD0001-");
    let mut tail = [0u8; 12];
    rand::Rng::fill(&mut rand::thread_rng(), &mut tail);
    for (i, b) in tail.iter().enumerate() {
        id[8 + i] = b"0123456789abcdefghijklmnopqrstuvwxyz"[(*b as usize) % 36];
    }
    id
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

fn load_resume(
    path: &std::path::Path,
    torrent: &Torrent,
    base: &std::path::Path,
) -> Option<Vec<bool>> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Format 1 had no location, so it cannot be tied to the bytes it describes.
    // Every file written before this check is treated as "no resume state".
    if v.get("version")?.as_u64()? != 2 {
        return None;
    }
    if v.get("base")?.as_str()? != resume_base(base).to_string_lossy() {
        // The same torrent was downloaded to a different folder: the bitfield says
        // nothing about the files at this one.
        return None;
    }
    let pieces = v.get("pieces")?.as_u64()? as usize;
    let size = v.get("size")?.as_u64()?;
    if pieces != torrent.num_pieces() || size != torrent.length {
        // The torrent changed, or the file is from another torrent with the same
        // name: start over rather than trust a bitfield that does not describe it.
        return None;
    }
    let bits = base64_decode(v.get("have")?.as_str()?)?;
    let bits = if bits.len() * 8 < pieces {
        return None;
    } else {
        bits
    };
    Some((0..pieces).map(|i| picker::bit(&bits, i)).collect())
}

/// The path a resume bitfield is tied to, in a form that survives a round trip.
fn resume_base(base: &std::path::Path) -> std::path::PathBuf {
    base.canonicalize().unwrap_or_else(|_| base.to_path_buf())
}

// ---------------------------------------------------------------------------
// Loops
// ---------------------------------------------------------------------------

async fn peer_manager(session: Arc<Session>, mut rx: mpsc::Receiver<SocketAddr>) {
    struct Entry {
        task: tokio::task::JoinHandle<()>,
        /// Set when the task has finished and a reconnect has been scheduled.
        pending: bool,
        failures: u32,
        /// When the connection was started, to tell a long useful connection from a
        /// connection that never worked.
        started: Instant,
        next_try: Instant,
    }
    let mut peers: HashMap<SocketAddr, Entry> = HashMap::new();
    // How many addresses we remember. In a public swarm most peers never answer —
    // a torrent may have a thousand addresses of which a dozen are reachable — so
    // this is deliberately much larger than the number of connections. Capping it at
    // `max_peers` used to mean the first eighty dead addresses hid every live one.
    let tracked = session.options.max_peers.saturating_mul(24).max(512);
    // The other half of that: how many are dialled or connected at once. Every
    // address beyond this waits its turn instead of being dropped.
    let dials = Arc::new(tokio::sync::Semaphore::new(
        session.options.max_peers.max(1),
    ));
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if session.is_done() || session.is_cancelled() {
            for (_, e) in peers.drain() {
                e.task.abort();
            }
            return;
        }
        tokio::select! {
            maybe = rx.recv() => {
                let Some(addr) = maybe else { continue };
                if peers.len() >= tracked {
                    // The table is full of addresses that keep failing; the tracker
                    // will hand us a fresh list at the next interval.
                    continue;
                }
                if let Some(entry) = peers.get(&addr) {
                    if !entry.task.is_finished() {
                        continue;
                    }
                }
                let s = session.clone();
                let permits = dials.clone();
                let task = tokio::spawn(async move {
                    let Ok(_permit) = permits.acquire_owned().await else {
                        return;
                    };
                    match crate::bt::peer::run(s, addr).await {
                        Ok(()) => {}
                        Err(e) => tracing::debug!(%addr, "peer: {e}"),
                    }
                });
                peers.insert(addr, Entry {
                    task,
                    pending: false,
                    failures: 0,
                    started: Instant::now(),
                    next_try: Instant::now(),
                });
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                let mut reconnect = Vec::new();
                peers.retain(|addr, entry| {
                    if entry.task.is_finished() && !entry.pending {
                        entry.pending = true;
                        // A connection that carried real data and lasted a while was
                        // not a failure, even though it has now ended: try it again
                        // straight away instead of treating it like a dead address.
                        if entry.started.elapsed() > Duration::from_secs(60) {
                            entry.failures = 0;
                        }
                        entry.failures = entry.failures.saturating_add(1);
                        if entry.failures > 12 {
                            return false;
                        }
                        // 2s, 4s, 8s ... capped at five minutes.
                        let backoff = Duration::from_secs(2u64 << entry.failures.min(7))
                            .min(Duration::from_secs(300));
                        entry.next_try = now + backoff;
                    }
                    if entry.pending && entry.next_try <= now && !session.is_done() {
                        reconnect.push(*addr);
                    }
                    true
                });
                for addr in reconnect {
                    let s = session.clone();
                    let permits = dials.clone();
                    let task = tokio::spawn(async move {
                        let Ok(_permit) = permits.acquire_owned().await else {
                            return;
                        };
                        let _ = crate::bt::peer::run(s, addr).await;
                    });
                    if let Some(entry) = peers.get_mut(&addr) {
                        entry.task = task;
                        entry.pending = false;
                        entry.started = now;
                    }
                }
            }
        }
    }
}

async fn tracker_loop(session: Arc<Session>) {
    let tiers = session.torrent.tiers.clone();
    loop {
        if session.is_done() || session.is_cancelled() {
            let stopped = session.announce_for(Event::Stopped);
            for tier in &tiers {
                for url in tier {
                    if tracker::announce(url, &stopped).await.is_ok() {
                        break;
                    }
                }
            }
            return;
        }
        let event = if session.verified_bytes() == 0 {
            Event::Started
        } else {
            Event::None
        };
        let mut announce = session.announce_for(event);
        announce.left = Some(
            session
                .torrent
                .length
                .saturating_sub(session.verified_bytes()),
        );
        let mut next: Option<Duration> = None;
        // Every tier, not just the first one that answers. A public swarm's peers
        // are spread across a dozen trackers, and the ones that can actually be
        // reached are often listed by a different tracker than the first. Tiers are
        // independent in BEP 12, so they are announced concurrently — a dead tracker
        // must not hold up a live one.
        let mut announces = tokio::task::JoinSet::new();
        for tier in &tiers {
            let tier = tier.clone();
            let announce = announce.clone();
            let s = session.clone();
            announces.spawn(async move {
                for url in tier {
                    match tracker::announce(&url, &announce).await {
                        Ok(resp) => {
                            let peers = resp.peers.len();
                            let wait = resp.next_wait();
                            let seeders = resp.seeders;
                            for p in resp.peers {
                                s.add_peer(p);
                            }
                            return Some((url, wait, seeders, peers));
                        }
                        Err(e) => tracing::debug!(tracker = url, "announce failed: {e}"),
                    }
                }
                None
            });
        }
        while let Some(joined) = announces.join_next().await {
            let Ok(Some((url, wait, seeders, peers))) = joined else {
                continue;
            };
            tracing::info!(tracker = url, peers, seeders, "tracker answered");
            next = Some(next.map_or(wait, |n: Duration| n.min(wait)));
        }
        // A tracker that answered decides the next announce; when every tracker
        // failed there is no interval to respect, so try again in a minute rather
        // than leaving the swarm untouched for half an hour.
        tokio::time::sleep(next.unwrap_or(Duration::from_secs(60))).await;
    }
}

async fn dht_loop(session: Arc<Session>) {
    loop {
        if session.is_done() || session.is_cancelled() {
            return;
        }
        let sink = session.clone();
        let found = match crate::bt::dht::lookup(
            session.torrent.info_hash,
            Arc::new(move |addr| sink.add_peer(addr)),
            session.cancel.clone(),
        )
        .await
        {
            Ok(n) => {
                tracing::debug!(peers = n, "dht lookup finished");
                n
            }
            Err(e) => {
                tracing::debug!("dht: {e}");
                0
            }
        };
        // A lookup that found nobody is not a settled matter — the walk may simply
        // have started from a bad bootstrap node — so it is retried sooner than a
        // successful one.
        tokio::time::sleep(Duration::from_secs(if found == 0 { 300 } else { 900 })).await;
    }
}

async fn tick_loop(session: Arc<Session>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_resume = Instant::now();
    loop {
        ticker.tick().await;
        if session.is_done() || session.is_cancelled() {
            session.save_resume();
            if let Some(cb) = &session.progress {
                cb(session.progress());
            }
            return;
        }
        // Free blocks that no peer delivered, so another peer can have them.
        session
            .picker
            .lock()
            .unwrap()
            .expire(Instant::now(), Duration::from_secs(30));
        if last_resume.elapsed() > Duration::from_secs(5)
            && session.resume_dirty.swap(false, Ordering::Relaxed)
        {
            session.save_resume();
            last_resume = Instant::now();
        }
        if let Some(cb) = &session.progress {
            cb(session.progress());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_piece_torrent() -> Torrent {
        let mut info = Vec::new();
        info.extend_from_slice(b"d6:lengthi4e4:name1:a12:piece lengthi4e6:pieces20:");
        info.extend_from_slice(&[b'x'; 20]);
        info.extend_from_slice(b"e");
        let mut r = Vec::new();
        r.extend_from_slice(b"d4:info");
        r.extend_from_slice(&info);
        r.push(b'e');
        Torrent::from_bytes(&r).unwrap()
    }

    /// The JSON `save_resume` writes, with the location and version made optional so
    /// the older formats can be tested too.
    fn resume_json(
        torrent: &Torrent,
        base: Option<&std::path::Path>,
        version: Option<u64>,
        have: &[u8],
    ) -> String {
        let mut v = serde_json::json!({
            "have": base64_encode(have),
            "pieces": torrent.num_pieces(),
            "size": torrent.length,
            "updated": 0,
        });
        let obj = v.as_object_mut().unwrap();
        if let Some(base) = base {
            obj.insert(
                "base".into(),
                serde_json::json!(resume_base(base).to_string_lossy()),
            );
        }
        if let Some(version) = version {
            obj.insert("version".into(), serde_json::json!(version));
        }
        v.to_string()
    }

    fn two_folders() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().join("here");
        let there = dir.path().join("there");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&there).unwrap();
        (dir, here, there)
    }

    #[test]
    fn a_matching_bitfield_round_trips() {
        let (dir, here, _) = two_folders();
        let t = one_piece_torrent();
        let path = dir.path().join("resume.json");
        std::fs::write(&path, resume_json(&t, Some(&here), Some(2), &[0x80])).unwrap();
        assert_eq!(load_resume(&path, &t, &here), Some(vec![true]));
    }

    #[test]
    fn a_bitfield_from_another_folder_is_ignored() {
        // This is the bug that made a torrent report 100% instantly: the same
        // torrent had been downloaded elsewhere, and its bitfield was applied to a
        // freshly preallocated file of zeros.
        let (dir, here, there) = two_folders();
        let t = one_piece_torrent();
        let path = dir.path().join("resume.json");
        std::fs::write(&path, resume_json(&t, Some(&there), Some(2), &[0x80])).unwrap();
        assert!(load_resume(&path, &t, &here).is_none());
        assert!(load_resume(&path, &t, &there).is_some());
    }

    #[test]
    fn a_bitfield_without_a_location_is_ignored() {
        // Format 1 carried no location, so it cannot be proven to describe these
        // files; treating it as stale is the only safe reading.
        let (dir, here, _) = two_folders();
        let t = one_piece_torrent();
        let path = dir.path().join("resume.json");
        std::fs::write(&path, resume_json(&t, None, None, &[0x80])).unwrap();
        assert!(load_resume(&path, &t, &here).is_none());
    }
}
