//! Per-hop progress stamps for the block pipeline.
//!
//! Each hop records the unix second at which it last moved. These are bare statics rather than
//! fields of [`crate::state::State`]: the liveness watchdog reads them from a plain OS thread
//! without taking a lock or entering a runtime, because the stall they exist to characterise is
//! the one where every lock and every runtime may be unavailable.
//!
//! One stamp per hop, because "no block was applied" does not say where a block stopped. The
//! stages run on separate threads with no shared name — gasket spawns them unnamed, so a thread
//! dump cannot tell them apart either. These five ages, in one line, do.

use std::sync::atomic::{AtomicU64, Ordering};

/// A hop in the block pipeline, in the order a block travels it.
#[derive(Clone, Copy)]
pub enum Link {
    /// A chain-sync message arrived from the node socket (`source::schedule`). Stale here while
    /// the others are fresh ⇒ the node or the socket, not us.
    NodeRead,
    /// A block left the source: `output.send()` returned, so the port had room
    /// (`source::process_next`). Fresh `NodeRead` with stale `PortSend` ⇒ the port is full,
    /// i.e. the sink stopped consuming and the source is only blocked behind it.
    PortSend,
    /// The sink took a message off the port (`sink::schedule`).
    SinkRecv,
    /// A block reached the state (`sink::handle_apply`). Fresh `SinkRecv` with stale
    /// `BlockApplied` ⇒ the stall is inside the apply itself.
    BlockApplied,
    /// The mempool stage finished a monitor pass (`mempool::execute`). It shares the node socket
    /// with the source but nothing else, so it separates a sick node from a sick pipeline.
    MempoolPass,
}

impl Link {
    /// Every hop, in pipeline order — the order [`summary`] reports them in.
    const ALL: [Link; LINK_COUNT] = [
        Link::NodeRead,
        Link::PortSend,
        Link::SinkRecv,
        Link::BlockApplied,
        Link::MempoolPass,
    ];

    fn label(self) -> &'static str {
        match self {
            Link::NodeRead => "node_read",
            Link::PortSend => "port_send",
            Link::SinkRecv => "sink_recv",
            Link::BlockApplied => "applied",
            Link::MempoolPass => "mempool",
        }
    }
}

const LINK_COUNT: usize = 5;

/// Indexed by `Link as usize`, so the declaration order of [`Link`] is load-bearing. 0 means
/// "never happened", which is what keeps a cold reset — many minutes with no block, by design —
/// from tripping the watchdog.
static STAMPS: [AtomicU64; LINK_COUNT] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Stamp a hop as having just moved.
pub fn mark(link: Link) {
    STAMPS[link as usize].store(unix_now(), Ordering::Relaxed);
}

/// Seconds since a hop last moved, or `None` if it never has.
pub fn secs_since(link: Link) -> Option<u64> {
    match STAMPS[link as usize].load(Ordering::Relaxed) {
        0 => None,
        t => Some(unix_now().saturating_sub(t)),
    }
}

/// Every hop's age as one log field, e.g.
/// `node_read=3s port_send=3s sink_recv=3s applied=421s mempool=9s`.
pub fn summary() -> String {
    Link::ALL
        .iter()
        .map(|link| match secs_since(*link) {
            Some(age) => format!("{}={age}s", link.label()),
            None => format!("{}=never", link.label()),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Link as usize` indexes `STAMPS`, so a reordered or extended enum must not silently
    /// alias two hops onto one slot.
    #[test]
    fn every_link_has_its_own_slot() {
        let mut seen: Vec<usize> = Link::ALL.iter().map(|l| *l as usize).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), LINK_COUNT, "two links share a stamp slot");
        assert_eq!(seen.last(), Some(&(LINK_COUNT - 1)), "index out of range");
    }

    /// The summary is the whole point of the module — it must name every hop, since a missing
    /// one reads as "that hop is fine" in the one log line we get from a stall.
    #[test]
    fn summary_names_every_link() {
        let line = summary();
        for link in Link::ALL {
            assert!(line.contains(link.label()), "{line} omits {}", link.label());
        }
    }

    #[test]
    fn marking_a_link_makes_it_recent() {
        mark(Link::MempoolPass);
        assert!(secs_since(Link::MempoolPass).is_some_and(|age| age < 5));
    }
}
