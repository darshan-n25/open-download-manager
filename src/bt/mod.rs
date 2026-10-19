//! BitTorrent support, shipped as the sibling binary `riptide-bt`.
//!
//! Deliberately **not** a mode of the HTTP engine: a swarm is a different
//! transport (peer-to-peer pieces, bencoded metadata, a tracker, SHA-1 piece
//! hashes) and pretending otherwise would twist both. What it *shares* with
//! `riptide` is the management layer — configuration, the state directory, the
//! queue, the quota budget and the scheduler's slot accounting — so a machine
//! running both binaries keeps one queue and one budget.
//!
//! Scope of this first cut: single-file torrents downloaded from a `.torrent`
//! file against an HTTP tracker, with the pieces verified by SHA-1 and written
//! to the download directory. Magnet links, multi-file torrents, DHT and
//! seeding come later.

pub mod bencode;
pub mod engine;
pub mod torrent;
pub mod tracker;
