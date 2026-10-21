//! BitTorrent support, shipped as the sibling binary `odm-bt`.
//!
//! Deliberately **not** a mode of the HTTP engine: a swarm is a different
//! transport (peer-to-peer pieces, bencoded metadata, a tracker, SHA-1 piece
//! hashes) and pretending otherwise would twist both. What it *shares* with
//! `odm-cli` is the management layer — configuration, the state directory, the
//! queue, the quota budget and the scheduler's slot accounting — so a machine
//! running both binaries keeps one queue and one budget.
//!
//! Scope: single-file torrents, from a `.torrent` file or from a magnet link,
//! fetched over HTTP trackers, with every piece verified by SHA-1 before it
//! is written. A magnet link carries no metadata of its own, so the torrent
//! file is fetched from the swarm itself (see [`metadata`]). Multi-file
//! torrents, the DHT and seeding come later.

pub mod bencode;
pub mod engine;
pub mod magnet;
pub mod metadata;
pub mod torrent;
pub mod tracker;
