//! Choosing which block to ask for next.
//!
//! Two strategies, and the difference between them is the difference between a
//! download manager and a video player:
//!
//! * **rarest first** — ask for the piece the fewest peers have. Pieces that only one
//!   peer holds are the ones that stop a download from finishing (the "last piece"
//!   problem), so they are worth more than pieces everybody has.
//! * **streaming** — a player is waiting at byte *N*. Ask for the pieces around *N*
//!   first, in order, so the file is playable minutes after it starts instead of
//!   after it finishes.
//!
//! The unit of work is a 16 KiB block, not a 4 MiB piece: several peers can fill one
//! piece at once, which is what keeps a slow peer from stalling a fast swarm.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::bt::torrent::Torrent;

/// The block size every peer speaks. The protocol caps a block at 16 KiB, and smaller
/// blocks mean more round trips for no benefit.
pub const BLOCK: u32 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub piece: usize,
    pub begin: u32,
    pub len: u32,
}

#[derive(Debug)]
pub enum Outcome {
    /// A block that was wanted arrived.
    Data,
    /// The last block of a piece arrived: the caller must hash it, and on success
    /// call [`Picker::commit`].
    Complete { piece: usize, data: Vec<u8> },
    /// A block we did not ask for, or already had.
    Unexpected,
}

const MISSING: u8 = 0;
const REQUESTED: u8 = 1;
const HAVE: u8 = 2;

pub struct Picker {
    piece_len: u64,
    total_len: u64,
    num_pieces: usize,
    /// Blocks in each piece (the last piece is usually short).
    per_piece: Vec<u32>,
    /// Flattened block state: piece * stride + block.
    state: Vec<u8>,
    stride: usize,
    /// Piece-level "we have it".
    have: Vec<bool>,
    /// How many peers hold each piece, from their bitfields.
    availability: Vec<u16>,
    /// Pieces currently being assembled: piece -> buffer.
    active: HashMap<usize, Vec<u8>>,
    /// Outstanding block -> (peer, when it was asked).
    inflight: HashMap<(usize, u32), (u32, Instant)>,
    /// Streaming window, in pieces.
    playhead: Option<(usize, usize)>,
    /// Duplicate requests sent during the endgame, so they can be cancelled.
    duplicates: Vec<(Request, u32)>,
    /// Pieces that failed their hash. They are re-fetched from **one** peer at a
    /// time, so that when the hash fails again there is exactly one suspect.
    exclusive: HashSet<usize>,
    missing_pieces: usize,
}

impl Picker {
    pub fn new(torrent: &Torrent) -> Picker {
        let num_pieces = torrent.num_pieces();
        let stride = (torrent.piece_length as usize).div_ceil(BLOCK as usize);
        let mut per_piece = Vec::with_capacity(num_pieces);
        let mut state = Vec::with_capacity(num_pieces * stride);
        for i in 0..num_pieces {
            let n = torrent.piece_size(i).div_ceil(BLOCK as u64) as u32;
            per_piece.push(n);
            state.extend(std::iter::repeat_n(MISSING, n as usize));
            // Padding keeps `piece * stride + block` addressable for every piece
            // even when the last piece is short.
            state.extend(std::iter::repeat_n(HAVE, stride - n as usize));
        }
        Picker {
            piece_len: torrent.piece_length,
            total_len: torrent.length,
            num_pieces,
            per_piece,
            state,
            stride,
            have: vec![false; num_pieces],
            availability: vec![0; num_pieces],
            active: HashMap::new(),
            inflight: HashMap::new(),
            playhead: None,
            duplicates: Vec::new(),
            exclusive: HashSet::new(),
            missing_pieces: num_pieces,
        }
    }

    pub fn num_pieces(&self) -> usize {
        self.num_pieces
    }

    pub fn have_piece(&self, piece: usize) -> bool {
        self.have.get(piece).copied().unwrap_or(false)
    }

    pub fn have_count(&self) -> usize {
        self.num_pieces - self.missing_pieces
    }

    pub fn is_complete(&self) -> bool {
        self.missing_pieces == 0
    }

    /// Mark a piece as already on disk (resume).
    pub fn mark_have(&mut self, piece: usize) {
        if piece < self.num_pieces && !self.have[piece] {
            self.have[piece] = true;
            self.missing_pieces -= 1;
            let base = piece * self.stride;
            for b in 0..self.per_piece[piece] as usize {
                self.state[base + b] = HAVE;
            }
        }
    }

    pub fn missing_count(&self) -> usize {
        self.missing_pieces
    }

    /// May `peer` be asked for this piece? A piece that failed its hash is fetched
    /// from a single peer, so blocks already in flight to someone else block it.
    fn can_take(&self, piece: usize, peer: u32) -> bool {
        if !self.exclusive.contains(&piece) {
            return true;
        }
        self.inflight
            .iter()
            .all(|(&(p, _), &(owner, _))| p != piece || owner == peer)
    }

    /// A peer's bitfield arrived: it can serve these pieces.
    pub fn add_availability(&mut self, have: &[u8]) {
        for (i, slot) in self
            .availability
            .iter_mut()
            .enumerate()
            .take(self.num_pieces)
        {
            if bit(have, i) {
                *slot = slot.saturating_add(1);
            }
        }
    }

    /// A peer sent `have`: it can now serve one more piece.
    pub fn add_one(&mut self, piece: usize) {
        if let Some(slot) = self.availability.get_mut(piece) {
            *slot = slot.saturating_add(1);
        }
    }

    /// Point the picker at the byte a player is about to read.
    ///
    /// `readahead` is in pieces. The window is deliberately wider than one piece:
    /// a player buffers, and a burst of requests for the next few seconds is what
    /// keeps playback from stalling between pieces.
    pub fn set_playhead(&mut self, piece: usize, readahead: usize) {
        let start = piece.min(self.num_pieces.saturating_sub(1));
        let end = (start + readahead.max(1)).min(self.num_pieces);
        self.playhead = Some((start, end));
    }

    pub fn clear_playhead(&mut self) {
        self.playhead = None;
    }

    /// The piece a byte offset falls in.
    pub fn piece_of_byte(&self, offset: u64) -> usize {
        if self.piece_len == 0 {
            return 0;
        }
        ((offset / self.piece_len) as usize).min(self.num_pieces.saturating_sub(1))
    }

    /// Ask for a specific block — used by the streaming server to jump the queue.
    pub fn request_block(&mut self, piece: usize, begin: u32, len: u32) -> Option<Request> {
        if piece >= self.num_pieces || self.have[piece] {
            return None;
        }
        let idx = piece * self.stride + (begin / BLOCK) as usize;
        if self.state.get(idx).copied().unwrap_or(HAVE) == MISSING {
            Some(Request { piece, begin, len })
        } else {
            None
        }
    }

    /// The next block to ask `peer` for, if there is one it can serve.
    ///
    /// `peer_has` is that peer's bitfield. `now` is passed in rather than read here so
    /// the tests can control it.
    pub fn pick(&mut self, peer: u32, peer_has: &[u8], now: Instant) -> Option<Request> {
        if self.missing_pieces == 0 {
            return None;
        }
        if let Some(r) = self.pick_streaming(peer, peer_has, now) {
            return Some(r);
        }
        if let Some(r) = self.pick_rarest(peer, peer_has, now) {
            return Some(r);
        }
        // Endgame: everything left is already requested. Duplicating the request is
        // the only way to finish when the peer holding a block has gone quiet — and
        // it is cheap, because the piece is hashed once whichever copy arrives first.
        if self.missing_pieces <= 8 {
            return self.pick_endgame(peer, peer_has, now);
        }
        None
    }

    fn claim(&mut self, piece: usize, block: usize, peer: u32, now: Instant) -> Request {
        let idx = piece * self.stride + block;
        self.state[idx] = REQUESTED;
        let size = self.piece_bytes(piece);
        self.active.entry(piece).or_insert_with(|| vec![0u8; size]);
        let begin = block as u32 * BLOCK;
        let len = self.block_len(piece, block);
        let req = Request { piece, begin, len };
        self.inflight.insert((piece, begin), (peer, now));
        req
    }

    fn pick_streaming(&mut self, peer: u32, peer_has: &[u8], now: Instant) -> Option<Request> {
        let (start, end) = self.playhead?;
        // The window first, in order: that is what the player is about to decode.
        for piece in start..end {
            if let Some(r) = self.first_free(piece, peer, peer_has, now) {
                return Some(r);
            }
        }
        // Then the rest of the file in order, so playback can continue past the window.
        for piece in end..self.num_pieces {
            if let Some(r) = self.first_free(piece, peer, peer_has, now) {
                return Some(r);
            }
        }
        // Anything before the playhead at the end, rather than never.
        for piece in 0..start {
            if let Some(r) = self.first_free(piece, peer, peer_has, now) {
                return Some(r);
            }
        }
        None
    }

    fn first_free(
        &mut self,
        piece: usize,
        peer: u32,
        peer_has: &[u8],
        now: Instant,
    ) -> Option<Request> {
        if self.have[piece] || !bit(peer_has, piece) || !self.can_take(piece, peer) {
            return None;
        }
        for block in 0..self.per_piece[piece] as usize {
            if self.state[piece * self.stride + block] == MISSING {
                return Some(self.claim(piece, block, peer, now));
            }
        }
        None
    }

    fn pick_rarest(&mut self, peer: u32, peer_has: &[u8], now: Instant) -> Option<Request> {
        // Every piece this peer can serve, rarest first. Taking only the single
        // rarest piece is wrong: once its blocks are all claimed there may still be
        // work on the second-rarest, and refusing it would leave the connection idle
        // with the whole torrent unfinished.
        let mut candidates: Vec<(u16, usize)> = (0..self.num_pieces)
            .filter(|&piece| {
                !self.have[piece] && bit(peer_has, piece) && self.can_take(piece, peer)
            })
            .map(|piece| (self.availability[piece], piece))
            .collect();
        candidates.sort_by_key(|&(avail, piece)| (avail, piece));
        for (_, piece) in candidates {
            for block in 0..self.per_piece[piece] as usize {
                if self.state[piece * self.stride + block] == MISSING {
                    return Some(self.claim(piece, block, peer, now));
                }
            }
        }
        None
    }

    fn pick_endgame(&mut self, peer: u32, peer_has: &[u8], now: Instant) -> Option<Request> {
        let mut oldest: Option<(usize, u32, Instant)> = None;
        for piece in 0..self.num_pieces {
            if self.have[piece] || !bit(peer_has, piece) || !self.can_take(piece, peer) {
                continue;
            }
            for block in 0..self.per_piece[piece] as usize {
                if self.state[piece * self.stride + block] != REQUESTED {
                    continue;
                }
                let begin = block as u32 * BLOCK;
                if let Some((owner, at)) = self.inflight.get(&(piece, begin)) {
                    if *owner == peer {
                        continue;
                    }
                    match oldest {
                        Some((_, _, t)) if t <= *at => {}
                        _ => oldest = Some((piece, begin, *at)),
                    }
                }
            }
        }
        let (piece, begin, _) = oldest?;
        let len = self.block_len(piece, (begin / BLOCK) as usize);
        let req = Request { piece, begin, len };
        self.duplicates.push((req, peer));
        let _ = now;
        Some(req)
    }

    /// A block arrived. The caller hashes and calls [`Picker::commit`] on `Complete`.
    pub fn on_block(&mut self, piece: usize, begin: u32, data: &[u8], peer: u32) -> Outcome {
        if piece >= self.num_pieces || self.have[piece] {
            return Outcome::Unexpected;
        }
        let block = (begin / BLOCK) as usize;
        if block >= self.per_piece[piece] as usize {
            return Outcome::Unexpected;
        }
        let idx = piece * self.stride + block;
        if self.state[idx] == HAVE {
            return Outcome::Unexpected;
        }
        let expected = self.block_len(piece, block) as usize;
        if data.len() != expected {
            // A short or long block is a lie about the piece, not data.
            return Outcome::Unexpected;
        }
        if self.exclusive.contains(&piece) {
            // A piece that already failed is being fetched from exactly one peer.
            // A copy that arrives from anyone else — an endgame duplicate sent
            // before the failure, say — would mix two sources into the piece again
            // and make the next failure un-attributable. Drop it instead.
            match self.inflight.get(&(piece, begin)) {
                Some(&(owner, _)) if owner == peer => {}
                _ => return Outcome::Unexpected,
            }
        }
        self.inflight.remove(&(piece, begin));
        self.duplicates
            .retain(|(r, _)| !(r.piece == piece && r.begin == begin));
        // Whatever had been asked before, the first copy that arrives wins.
        self.state[idx] = HAVE;
        let size = self.piece_bytes(piece);
        let buf = self.active.entry(piece).or_insert_with(|| vec![0u8; size]);
        let at = block * BLOCK as usize;
        if at + data.len() <= buf.len() {
            buf[at..at + data.len()].copy_from_slice(data);
        }
        let base = piece * self.stride;
        let done = (0..self.per_piece[piece] as usize).all(|b| self.state[base + b] == HAVE);
        if done {
            let data = self.active.remove(&piece).unwrap_or_default();
            Outcome::Complete { piece, data }
        } else {
            Outcome::Data
        }
    }

    /// Hash check passed: the piece is ours.
    pub fn commit(&mut self, piece: usize) {
        if piece < self.num_pieces && !self.have[piece] {
            self.have[piece] = true;
            self.missing_pieces -= 1;
            let base = piece * self.stride;
            for b in 0..self.per_piece[piece] as usize {
                self.state[base + b] = HAVE;
            }
        }
        self.inflight.retain(|(p, _), _| *p != piece);
        self.exclusive.remove(&piece);
    }

    /// A piece did not hash: throw it away and let it be fetched again — from a
    /// single peer, so the next failure has one owner and not a crowd.
    pub fn fail(&mut self, piece: usize) {
        if piece >= self.num_pieces {
            return;
        }
        let base = piece * self.stride;
        for b in 0..self.per_piece[piece] as usize {
            self.state[base + b] = MISSING;
        }
        self.active.remove(&piece);
        self.inflight.retain(|(p, _), _| *p != piece);
        self.duplicates.retain(|(r, _)| r.piece != piece);
        self.exclusive.insert(piece);
    }

    /// Blocks that have been outstanding too long: free them for another peer, and
    /// report them so the owner can cancel.
    pub fn expire(&mut self, now: Instant, after: Duration) -> Vec<(Request, u32)> {
        let mut out = Vec::new();
        let stale: Vec<((usize, u32), (u32, Instant))> = self
            .inflight
            .iter()
            .filter(|(_, (_, at))| now.duration_since(*at) > after)
            .map(|(k, v)| (*k, *v))
            .collect();
        for ((piece, begin), (peer, _)) in stale {
            self.inflight.remove(&(piece, begin));
            self.duplicates
                .retain(|(r, _)| !(r.piece == piece && r.begin == begin));
            let block = (begin / BLOCK) as usize;
            if block < self.per_piece[piece] as usize {
                let idx = piece * self.stride + block;
                if self.state[idx] == REQUESTED {
                    self.state[idx] = MISSING;
                }
            }
            out.push((
                Request {
                    piece,
                    begin,
                    len: self.block_len(piece, block),
                },
                peer,
            ));
        }
        out
    }

    /// A peer gave up on a block (timeout, choke, disconnect): make it available to
    /// the rest of the swarm again.
    pub fn release(&mut self, req: Request) {
        if req.piece >= self.num_pieces {
            return;
        }
        let block = (req.begin / BLOCK) as usize;
        if block >= self.per_piece[req.piece] as usize {
            return;
        }
        let idx = req.piece * self.stride + block;
        if self.state[idx] == REQUESTED {
            self.state[idx] = MISSING;
        }
        self.inflight.remove(&(req.piece, req.begin));
    }

    /// Put every block this peer had claimed back in the queue. A connection that
    /// dies holding ten blocks would otherwise keep them for the full expiry window
    /// — and the peer that reconnects a second later would be unable to ask for them.
    pub fn release_peer(&mut self, peer: u32) {
        let held: Vec<(usize, u32)> = self
            .inflight
            .iter()
            .filter(|(_, (owner, _))| *owner == peer)
            .map(|(&(piece, begin), _)| (piece, begin))
            .collect();
        for (piece, begin) in held {
            self.release(Request {
                piece,
                begin,
                len: 0,
            });
        }
        self.duplicates.retain(|(_, owner)| *owner != peer);
    }

    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// Bytes of the torrent that are verified.
    pub fn verified_bytes(&self, piece_len: u64, total: u64) -> u64 {
        if self.missing_pieces == 0 {
            return total;
        }
        let full = self.have_count() as u64 * piece_len;
        full.min(total)
    }

    /// The size of a piece in bytes — the last one is short.
    fn piece_bytes(&self, piece: usize) -> usize {
        let offset = piece as u64 * self.piece_len;
        (self.total_len - offset).min(self.piece_len) as usize
    }

    fn block_len(&self, piece: usize, block: usize) -> u32 {
        let offset = block as u64 * BLOCK as u64;
        let remaining = (self.piece_bytes(piece) as u64).saturating_sub(offset);
        remaining.min(BLOCK as u64) as u32
    }
}

pub fn bit(field: &[u8], index: usize) -> bool {
    field
        .get(index / 8)
        .map(|b| b & (0x80 >> (index % 8)) != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(pieces: usize, piece_len: u64) -> Torrent {
        let mut info = Vec::new();
        let length = piece_len as usize * (pieces - 1) + 1;
        info.extend_from_slice(
            format!("d6:lengthi{length}e4:name4:test12:piece lengthi{piece_len}e6:pieces")
                .as_bytes(),
        );
        info.extend_from_slice(format!("{}:", pieces * 20).as_bytes());
        info.extend(std::iter::repeat_n(b'x', pieces * 20));
        info.extend_from_slice(b"e");
        let mut r = Vec::new();
        r.extend_from_slice(b"d4:info");
        r.extend_from_slice(&info);
        r.push(b'e');
        Torrent::from_bytes(&r).unwrap()
    }

    #[test]
    fn rarest_piece_wins() {
        let t = torrent(4, 1024 * 1024);
        let mut p = Picker::new(&t);
        // Three peers. Pieces 0 and 2 are everywhere; piece 1 is rarer; piece 3 is
        // rarest of all, but the only peer that has it is this one.
        p.add_availability(&[0b1111_0000]);
        p.add_availability(&[0b1001_0000]);
        p.add_availability(&[0b1001_0000]);
        let now = Instant::now();
        let r = p.pick(1, &[0b1111_0000], now).unwrap();
        assert_eq!(
            r.piece, 1,
            "rarest first should skip the pieces everyone has"
        );
    }

    #[test]
    fn streaming_follows_the_playhead() {
        // One block per piece, so each pick moves to the next piece.
        let t = torrent(8, 16 * 1024);
        let mut p = Picker::new(&t);
        p.set_playhead(5, 2);
        let now = Instant::now();
        let mut got = Vec::new();
        for peer in 1..=4u32 {
            if let Some(r) = p.pick(peer, &[0xFF], now) {
                got.push(r.piece);
            }
        }
        assert_eq!(
            got,
            vec![5, 6, 7, 0],
            "window first, then the rest in order"
        );
    }

    #[test]
    fn a_piece_completes_only_when_every_block_arrives() {
        let t = torrent(2, 32 * 1024);
        let mut p = Picker::new(&t);
        let now = Instant::now();
        let a = p.pick(1, &[0xFF], now).unwrap();
        let b = p.pick(2, &[0xFF], now).unwrap();
        assert_eq!(a.piece, 0);
        assert_eq!(b.piece, 0);
        let mut out = p.on_block(0, 0, &vec![1u8; 16 * 1024], 1);
        assert!(matches!(out, Outcome::Data));
        out = p.on_block(0, 16 * 1024, &vec![2u8; 16 * 1024], 2);
        match out {
            Outcome::Complete { piece, data } => {
                assert_eq!(piece, 0);
                assert_eq!(data.len(), 32 * 1024);
                assert_eq!(data[0], 1);
                assert_eq!(data[30_000], 2);
            }
            _ => panic!("expected completion"),
        }
    }

    #[test]
    fn a_hash_failure_returns_the_piece_to_the_queue() {
        let t = torrent(2, 16 * 1024);
        let mut p = Picker::new(&t);
        let now = Instant::now();
        let r = p.pick(1, &[0xFF], now).unwrap();
        p.on_block(r.piece, r.begin, &vec![0u8; r.len as usize], 1);
        p.fail(r.piece);
        assert_eq!(p.missing_count(), 2);
        // It can be picked again.
        assert!(p.pick(1, &[0xFF], Instant::now()).is_some());
    }

    #[test]
    fn a_failed_piece_is_fetched_from_one_peer_at_a_time() {
        // Two blocks per piece, so a single piece can be split between peers — which
        // is exactly what makes a hash failure un-attributable.
        let t = torrent(2, 32 * 1024);
        let mut p = Picker::new(&t);
        let now = Instant::now();
        for peer in 1..=2u32 {
            let r = p.pick(peer, &[0xFF], now).unwrap();
            p.on_block(r.piece, r.begin, &vec![0u8; r.len as usize], peer);
        }
        p.fail(0);

        // From here on, only one peer fills the piece: the next hash failure then has
        // exactly one owner to blame.
        let mine = p.pick(1, &[0xFF], now).unwrap();
        assert_eq!(mine.piece, 0);
        if let Some(other) = p.pick(2, &[0xFF], now) {
            assert_ne!(
                other.piece, 0,
                "a second peer was drawn into a suspicious piece"
            );
        }

        // Once the piece is committed, the restriction is gone with it.
        let rest = p.pick(1, &[0xFF], now).unwrap();
        p.on_block(rest.piece, rest.begin, &vec![0u8; rest.len as usize], 1);
        p.commit(0);
        assert!(!p.exclusive.contains(&0));
    }

    #[test]
    fn stale_requests_are_freed() {
        let t = torrent(2, 64 * 1024);
        let mut p = Picker::new(&t);
        let now = Instant::now();
        p.pick(1, &[0xFF], now);
        let expired = p.expire(now + Duration::from_secs(31), Duration::from_secs(30));
        assert_eq!(expired.len(), 1);
        assert_eq!(p.in_flight(), 0);
    }
}
