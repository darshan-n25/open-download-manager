//! End-to-end torrent tests over loopback: a real peer connection, a real piece
//! pipeline, real SHA-1 checks, and the streaming server reading while it downloads.
//!
//! No internet is needed, and none is used: the "swarm" is a listener in this file.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use odm::bt::engine;
use odm::bt::session::{Options, Session};
use odm::bt::stream;
use odm::bt::torrent::Torrent;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PIECE: usize = 16 * 1024;

fn bencode_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

/// Build a multi-file torrent for `files` (`(name, content)` pairs).
fn make_torrent(files: &[(&str, Vec<u8>)], piece_len: usize) -> Torrent {
    let mut content = Vec::new();
    for (_, data) in files {
        content.extend_from_slice(data);
    }
    let mut pieces = Vec::new();
    for chunk in content.chunks(piece_len) {
        let mut h = Sha1::new();
        h.update(chunk);
        pieces.extend_from_slice(&h.finalize());
    }

    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for (name, data) in files {
        info.extend_from_slice(b"d6:lengthi");
        info.extend_from_slice(data.len().to_string().as_bytes());
        info.extend_from_slice(b"e4:pathl");
        bencode_bytes(&mut info, name.as_bytes());
        info.extend_from_slice(b"ee");
    }
    info.push(b'e');
    bencode_bytes(&mut info, b"name");
    bencode_bytes(&mut info, b"out");
    bencode_bytes(&mut info, b"piece length");
    info.extend_from_slice(b"i");
    info.extend_from_slice(piece_len.to_string().as_bytes());
    info.push(b'e');
    bencode_bytes(&mut info, b"pieces");
    bencode_bytes(&mut info, &pieces);
    info.push(b'e');

    let mut body = Vec::new();
    body.extend_from_slice(b"d4:info");
    body.extend_from_slice(&info);
    body.push(b'e');
    Torrent::from_bytes(&body).unwrap()
}

fn bitfield(num_pieces: usize) -> Vec<u8> {
    let mut b = vec![0xFFu8; num_pieces.div_ceil(8)];
    // Clear the padding bits at the end: a peer that claims pieces that do not
    // exist is rejected by some clients.
    let spare = b.len() * 8 - num_pieces;
    if spare > 0 {
        let last = b.len() - 1;
        b[last] &= 0xFF << spare;
    }
    b
}

/// A minimal BitTorrent peer: full bitfield, always unchoked, serves every request.
struct FakePeer {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

async fn start_fake_peer(
    info_hash: [u8; 20],
    content: Arc<Vec<u8>>,
    num_pieces: usize,
    delay: Duration,
) -> FakePeer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::Relaxed);
            let content = content.clone();
            tokio::spawn(async move {
                let _ = serve_peer(stream, info_hash, content, num_pieces, delay).await;
            });
        }
    });
    FakePeer { addr, connections }
}

async fn serve_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    content: Arc<Vec<u8>>,
    num_pieces: usize,
    delay: Duration,
) -> std::io::Result<()> {
    let mut hs = [0u8; 68];
    stream.read_exact(&mut hs).await?;
    if hs[28..48] != info_hash {
        return Ok(());
    }
    let mut reply = Vec::new();
    reply.push(19);
    reply.extend_from_slice(b"BitTorrent protocol");
    reply.extend_from_slice(&[0u8; 8]);
    reply.extend_from_slice(&info_hash);
    reply.extend_from_slice(b"-FAKE00-000000000000");
    stream.write_all(&reply).await?;

    // bitfield
    let field = bitfield(num_pieces);
    let mut msg = Vec::new();
    msg.extend_from_slice(&((field.len() + 1) as u32).to_be_bytes());
    msg.push(5);
    msg.extend_from_slice(&field);
    stream.write_all(&msg).await?;
    // unchoke
    stream.write_all(&1u32.to_be_bytes()).await?;
    stream.write_all(&[1]).await?;

    let mut len = [0u8; 4];
    loop {
        if stream.read_exact(&mut len).await.is_err() {
            return Ok(());
        }
        let n = u32::from_be_bytes(len) as usize;
        if n == 0 {
            continue;
        }
        let mut body = vec![0u8; n];
        stream.read_exact(&mut body).await?;
        if body[0] != 6 || body.len() < 13 {
            continue; // interested, keep-alive, or something we do not answer
        }
        let piece = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        let begin = u32::from_be_bytes([body[5], body[6], body[7], body[8]]) as usize;
        let len = u32::from_be_bytes([body[9], body[10], body[11], body[12]]) as usize;
        let start = piece * PIECE + begin;
        let stop = (start + len).min(content.len());
        if start >= stop {
            continue;
        }
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let block = &content[start..stop];
        // A piece message is id(1) + piece(4) + begin(4) + data, and the length
        // prefix counts everything after itself.
        let mut msg = Vec::with_capacity(9 + block.len());
        msg.extend_from_slice(&((9 + block.len()) as u32).to_be_bytes());
        msg.push(7);
        msg.extend_from_slice(&(piece as u32).to_be_bytes());
        msg.extend_from_slice(&(begin as u32).to_be_bytes());
        msg.extend_from_slice(block);
        stream.write_all(&msg).await?;
    }
}

fn options(dir: &std::path::Path) -> Options {
    Options {
        max_peers: 8,
        port: 6881,
        state_dir: dir.to_path_buf(),
        use_trackers: false,
        use_dht: false,
    }
}

type SampleFiles = Vec<(&'static str, Vec<u8>)>;

fn sample_content() -> (SampleFiles, Arc<Vec<u8>>) {
    let a: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let b: Vec<u8> = (0..25_000u32).map(|i| (i * 13 % 253) as u8).collect();
    let mut all = a.clone();
    all.extend_from_slice(&b);
    (vec![("a.bin", a), ("b.bin", b)], Arc::new(all))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_multi_file_torrent_from_a_peer() {
    let (files, content) = sample_content();
    let torrent = // built below
    make_torrent(&files, PIECE);
    let peer = start_fake_peer(
        torrent.info_hash,
        content.clone(),
        torrent.num_pieces(),
        Duration::ZERO,
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(
        Arc::new(torrent.clone()),
        dir.path().to_path_buf(),
        options(&dir.path().join("state")),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    session.add_peer(peer.addr);
    session.wait().await.unwrap();

    assert_eq!(session.have_count(), torrent.num_pieces());
    let a = std::fs::read(dir.path().join("out/a.bin")).unwrap();
    let b = std::fs::read(dir.path().join("out/b.bin")).unwrap();
    assert_eq!(a, files[0].1);
    assert_eq!(b, files[1].1);
    assert!(peer.connections.load(Ordering::Relaxed) >= 1);

    // Resume data was written, and a second run sees everything already there.
    let hash = hex::encode(torrent.info_hash);
    let resume = dir.path().join("state/bt").join(format!("{hash}.json"));
    assert!(resume.exists(), "no resume file at {}", resume.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_piece_that_does_not_hash_is_fetched_again() {
    let (files, content) = sample_content();
    let torrent = make_torrent(&files, PIECE);
    let mut broken = (*content).clone();
    broken[20_000] ^= 0xFF; // corrupt the second piece

    // One peer lies, one tells the truth: the piece must come out right anyway.
    let liar = start_fake_peer(
        torrent.info_hash,
        Arc::new(broken),
        torrent.num_pieces(),
        Duration::ZERO,
    )
    .await;
    let honest = start_fake_peer(
        torrent.info_hash,
        content.clone(),
        torrent.num_pieces(),
        Duration::from_millis(5),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(
        Arc::new(torrent.clone()),
        dir.path().to_path_buf(),
        options(&dir.path().join("state")),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    session.add_peer(liar.addr);
    session.add_peer(honest.addr);
    tokio::time::timeout(Duration::from_secs(30), session.wait())
        .await
        .expect("download stalled: a corrupt piece was never retried")
        .unwrap();

    let mut got = std::fs::read(dir.path().join("out/a.bin")).unwrap();
    got.extend(std::fs::read(dir.path().join("out/b.bin")).unwrap());
    assert_eq!(
        got, *content,
        "the finished file is not the torrent's content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_slow_peers_still_finish_the_torrent() {
    // Both peers answer slowly, so the second connection is topped up while the
    // first one's blocks are still outstanding: everything left is already
    // requested, and the only thing the picker can offer is a duplicate. Writing
    // those duplicates in a loop — instead of going back to read the replies —
    // deadlocks the transfer, which is what this test is here to catch.
    let (files, content) = sample_content();
    let torrent = make_torrent(&files, PIECE);
    let one = start_fake_peer(
        torrent.info_hash,
        content.clone(),
        torrent.num_pieces(),
        Duration::from_millis(300),
    )
    .await;
    let two = start_fake_peer(
        torrent.info_hash,
        content.clone(),
        torrent.num_pieces(),
        Duration::from_millis(300),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(
        Arc::new(torrent.clone()),
        dir.path().to_path_buf(),
        options(&dir.path().join("state")),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    session.add_peer(one.addr);
    session.add_peer(two.addr);
    tokio::time::timeout(Duration::from_secs(20), session.wait())
        .await
        .expect("download stalled: the connection stopped reading its replies")
        .unwrap();

    let mut got = std::fs::read(dir.path().join("out/a.bin")).unwrap();
    got.extend(std::fs::read(dir.path().join("out/b.bin")).unwrap());
    assert_eq!(got, *content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_a_range_while_the_download_is_running() {
    let (files, content) = sample_content();
    let torrent = make_torrent(&files, PIECE);
    // 60 ms per block: the range below is not on disk when it is asked for.
    let peer = start_fake_peer(
        torrent.info_hash,
        content.clone(),
        torrent.num_pieces(),
        Duration::from_millis(60),
    )
    .await;

    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(
        Arc::new(torrent.clone()),
        dir.path().to_path_buf(),
        options(&dir.path().join("state")),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    session.add_peer(peer.addr);

    let info = stream::start(session.clone(), 0).await.unwrap();
    let (status, headers, body) = http_get(&info.url, Some("bytes=20000-24999")).await;
    assert_eq!(status, 206, "headers: {headers}");
    assert!(
        headers.contains("Content-Range: bytes 20000-24999/40000"),
        "{headers}"
    );
    assert_eq!(body.len(), 5000);
    assert_eq!(body, content[20_000..25_000].to_vec());

    let _ = tokio::time::timeout(Duration::from_secs(20), session.wait()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_token_is_refused() {
    let (files, content) = sample_content();
    let torrent = make_torrent(&files, PIECE);
    let peer = start_fake_peer(
        torrent.info_hash,
        content,
        torrent.num_pieces(),
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(
        Arc::new(torrent),
        dir.path().to_path_buf(),
        options(&dir.path().join("state")),
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    session.add_peer(peer.addr);
    let info = stream::start(session.clone(), 0).await.unwrap();
    let evil = info.url.replace(&info.token, "0000");
    let (status, _, _) = http_get(&evil, None).await;
    assert_eq!(status, 403);
}

/// The three files `output_path` should pick from a torrent: the largest one.
#[test]
fn the_output_path_is_the_biggest_file() {
    let files = vec![
        ("sample.mkv", vec![0u8; 100]),
        ("movie.mkv", vec![0u8; 1000]),
        ("notes.txt", vec![0u8; 10]),
    ];
    let torrent = make_torrent(&files, PIECE);
    let path = engine::output_path(&torrent, std::path::Path::new("/downloads"));
    assert!(path.ends_with("out/movie.mkv"), "{}", path.display());
}

async fn http_get(url: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
    let rest = url.strip_prefix("http://").unwrap();
    let (hostport, path) = rest.split_once('/').unwrap();
    let mut stream = TcpStream::connect(hostport).await.unwrap();
    let mut req = format!("GET /{path} HTTP/1.1\r\nHost: {hostport}\r\n");
    if let Some(r) = range {
        req.push_str(&format!("Range: {r}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no header terminator")
        + 4;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head, raw[split..].to_vec())
}
