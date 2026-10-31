//! BitTorrent support, shipped as the sibling binary `odm-bt`.
//!
//! Deliberately **not** a mode of the HTTP engine: a swarm is a different
//! transport (peer-to-peer pieces, bencoded metadata, a tracker, SHA-1 piece
//! hashes) and pretending otherwise would twist both. What it *shares* with
//! `odm-cli` is the management layer — configuration, the state directory, the
//! queue, the quota budget and the scheduler's slot accounting — so a machine
//! running both binaries keeps one queue and one budget.
//!
//! Scope: single- and multi-file torrents, from a `.torrent` file or a magnet,
//! with peers from HTTP and UDP trackers, the DHT (BEP 5) and peer exchange
//! (BEP 11), every piece verified by SHA-1 before it is written, resumable across
//! runs, and playable while it downloads (see [`stream`]). Seeding and the uTP
//! transport are the deliberate omissions.

pub mod bencode;
pub mod dht;
pub mod engine;
pub mod magnet;
pub mod metadata;
pub mod peer;
pub mod picker;
pub mod session;
pub mod store;
pub mod stream;
pub mod torrent;
pub mod tracker;
