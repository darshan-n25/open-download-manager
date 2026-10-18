//! Segment planning and runtime re-splitting.
//!
//! IDM's engine keeps a *mutable* chunk list — the strings
//! `Insert next chunk in list`, `Erase chunk_list`, `Change chunk size to %ld` and
//! `Chunk order violation error` all come from its `ChLst` class. So instead of one
//! fixed split up front, Riptide plans an initial split and then lets idle workers
//! carve the largest outstanding remainder in half (see [`split_off`]).

use crate::job::Segment;

/// Never create a segment smaller than this — below it, extra connections cost more
/// than they save.
pub const DEFAULT_MIN_CHUNK: u64 = 1 << 20; // 1 MiB

/// Plan the initial segment table.
///
/// `connections` is the desired parallelism, `min_chunk` the smallest useful piece.
/// Small files transparently fall back to fewer connections so we never emit a
/// degenerate range such as `bytes=5-4`.
pub fn plan(total: u64, connections: usize, min_chunk: u64) -> Vec<Segment> {
    if total == 0 {
        return vec![Segment::new(0, 0, 0)];
    }
    let min_chunk = min_chunk.max(1);
    let max_by_size = (total / min_chunk).max(1) as usize;
    let n = connections.max(1).min(max_by_size);
    if n <= 1 {
        return vec![Segment::new(0, 0, total - 1)];
    }

    let base = total / n as u64;
    let extra = total % n as u64;
    let mut segs = Vec::with_capacity(n);
    let mut off = 0u64;
    for i in 0..n {
        let len = base + if (i as u64) < extra { 1 } else { 0 };
        segs.push(Segment::new(i as u64, off, off + len - 1));
        off += len;
    }
    segs
}

/// Halve the outstanding part of `seg`.
///
/// Returns the trailing segment (its `id` is left at `0` — the caller assigns one) plus
/// the new inclusive `end` for `seg`. The original segment is only ever *shortened*, so
/// the worker already streaming it cannot collide with the new segment: it simply stops
/// earlier. This is what makes re-splitting safe without preemption.
pub fn split_off(seg: &Segment, min_chunk: u64) -> Option<(Segment, u64)> {
    let remaining = seg.remaining()?;
    if remaining < min_chunk.saturating_mul(2) {
        return None;
    }
    let keep = remaining / 2;
    let cut = seg.cursor() + keep;
    let new_end = cut - 1;
    let tail = Segment::new(0, cut, seg.end);
    Some((tail, new_end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::SegmentState;

    fn covered(segs: &[Segment]) -> Vec<u64> {
        let mut v = Vec::new();
        for s in segs {
            v.push(s.start);
            v.push(s.end);
        }
        v
    }

    #[test]
    fn plan_tiles_the_file_exactly() {
        let segs = plan(1000, 8, 10);
        assert_eq!(segs.len(), 8);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs.last().unwrap().end, 999);
        for w in segs.windows(2) {
            assert_eq!(w[0].end + 1, w[1].start, "segments must be contiguous");
        }
    }

    #[test]
    fn plan_shrinks_for_small_files() {
        // 3 MiB with a 1 MiB floor => at most 3 segments even if 8 were requested.
        let segs = plan(3 << 20, 8, 1 << 20);
        assert_eq!(segs.len(), 3);
    }

    #[test]
    fn plan_handles_single_byte_and_zero() {
        let segs = plan(1, 8, 1 << 20);
        assert_eq!(segs.len(), 1);
        assert_eq!((segs[0].start, segs[0].end), (0, 0));

        let zero = plan(0, 8, 1 << 20);
        assert_eq!(zero.len(), 1);
        assert_eq!((zero[0].start, zero[0].end), (0, 0));
    }

    #[test]
    fn plan_never_emits_inverted_ranges() {
        for total in [1u64, 7, 100, 4095, 1 << 20, (1 << 20) + 1] {
            for conns in [1usize, 2, 8, 32] {
                let segs = plan(total, conns, 4096);
                for s in &segs {
                    assert!(s.start <= s.end, "inverted range {s:?}");
                    assert!(s.end < total);
                }
            }
        }
    }

    #[test]
    fn split_preserves_coverage() {
        let mut seg = Segment::new(0, 0, 999);
        let (mut tail, new_end) = split_off(&seg, 100).expect("splittable");
        let old_end = seg.end;
        seg.end = new_end;
        tail.id = 1;
        assert_eq!(seg.end + 1, tail.start, "no gap and no overlap");
        assert_eq!(tail.end, old_end);
        assert_eq!(seg.start, 0);
    }

    #[test]
    fn split_respects_min_chunk_and_progress() {
        let mut seg = Segment::new(0, 0, 999);
        // 1000 bytes, floor 600 => 1000 < 1200, refuse.
        assert!(split_off(&seg, 600).is_none());

        // Once 900 bytes are done only 100 remain, so splitting is pointless.
        seg.done = 900;
        seg.state = SegmentState::Running;
        assert!(split_off(&seg, 100).is_none());

        // A segment that already finished has nothing to give.
        seg.done = 1000;
        assert_eq!(seg.remaining(), Some(0));
        assert!(split_off(&seg, 1).is_none());
    }

    #[test]
    fn split_of_unknown_length_is_refused() {
        let seg = Segment::new(0, 0, u64::MAX);
        assert!(split_off(&seg, 1).is_none());
    }

    #[test]
    fn repeated_splitting_still_tiles() {
        let mut segs = plan(1 << 22, 2, 1 << 10);
        let mut next_id = segs.len() as u64;
        // Clippy would rather this were an `enumerate`, but the counter must survive across
        // iterations while the index is recomputed each time.
        #[allow(clippy::explicit_counter_loop)]
        for _ in 0..5 {
            // Split the biggest one each time.
            let idx = segs
                .iter()
                .enumerate()
                .filter(|(_, s)| s.remaining().is_some())
                .max_by_key(|(_, s)| s.remaining().unwrap())
                .map(|(i, _)| i)
                .unwrap();
            let (mut tail, new_end) = split_off(&segs[idx].clone(), 1 << 10).unwrap();
            segs[idx].end = new_end;
            tail.id = next_id;
            next_id += 1;
            segs.push(tail);
        }
        segs.sort_by_key(|s| s.start);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs.last().unwrap().end, (1 << 22) - 1);
        for w in segs.windows(2) {
            assert_eq!(w[0].end + 1, w[1].start);
        }
    }

    #[test]
    fn coverage_helper_is_contiguous_for_default_plan() {
        let segs = plan(4096, 4, 512);
        let c = covered(&segs);
        assert_eq!(c.len(), 8);
        assert_eq!(c[0], 0);
        assert_eq!(c[7], 4095);
    }
}
