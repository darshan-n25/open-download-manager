//! Riptide — an open-source, cross-platform download manager.
//!
//! Design derived from the static reverse-engineering of Internet Download Manager
//! documented in `docs/IDM-REVERSE-ENGINEERING.md`:
//!
//! * a hand-rolled segmented HTTP/HTTPS engine using `Range` + `If-Range` (see [`engine`]);
//! * a *mutable* segment table that is re-split at runtime to imitate IDM's dynamic chunk
//!   sizing (`Change chunk size to %ld`) — see [`segment`] and [`engine::run`];
//! * crash-safe resume state persisted next to the output file (see [`state`]);
//! * independent global / per-file bandwidth ceilings (see [`throttle`]) and a
//!   bytes-per-window budget (IDM's "Download limits", see [`quota`]);
//! * Basic / Digest / Bearer / NTLMv2 credential handling (see [`auth`]);
//! * HLS and DASH manifest capture and remux (see [`media`]);
//! * a localhost daemon that speaks a `MSG#`-framed protocol to a browser extension so the
//!   browser stays dumb and the engine stays powerful (see [`daemon`]);
//! * recurring projects: periodic file synchronization and scheduled site crawls
//!   (see [`project`]);
//! * offline site archiving that rewrites markup to work from disk (see [`archive`]).

pub mod archive;
pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod engine;
pub mod error;
pub mod ftp;
pub mod grabber;
pub mod job;
pub mod media;
pub mod net;
pub mod project;
pub mod quota;
pub mod scheduler;
pub mod segment;
pub mod state;
pub mod storage;
pub mod throttle;
pub mod util;

pub use error::{Result, RiptideError};
pub use job::{Job, JobKind, JobState, Segment, SegmentState};
