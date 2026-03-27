use std::collections::VecDeque;

use tokio::sync::{broadcast, Mutex};

use crate::event::{BlockTx, Event};

const MAX_BLOCKS: usize = 30;

struct Inner {
    tx: broadcast::Sender<Event>,
    mempool: Vec<Event>,
    blocks: VecDeque<Event>,
}

pub struct EventBus {
    inner: Mutex<Inner>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            inner: Mutex::new(Inner {
                tx,
                mempool: Vec::new(),
                blocks: VecDeque::new(),
            }),
        }
    }

    pub async fn send(&self, event: Event) {
        let mut inner = self.inner.lock().await;

        match &event {
            Event::MempoolTx(_) => {
                inner.mempool.push(event.clone());
            }
            Event::Block { txs, .. } => {
                let block_hashes: std::collections::HashSet<&str> =
                    txs.iter().map(|t| t.hash.as_str()).collect();
                inner.mempool.retain(|e| match e {
                    Event::MempoolTx(BlockTx { hash, .. }) => !block_hashes.contains(hash.as_str()),
                    _ => true,
                });
                inner.blocks.push_front(event.clone());
                if inner.blocks.len() > MAX_BLOCKS {
                    inner.blocks.pop_back();
                }
            }
            Event::Rollback { slot } => {
                let slot = *slot;
                inner.blocks.retain(|e| match e {
                    Event::Block { slot: s, .. } => *s <= slot,
                    _ => true,
                });
            }
            Event::MempoolPrune { removed } => {
                let removed_set: std::collections::HashSet<&str> =
                    removed.iter().map(|h| h.as_str()).collect();
                inner.mempool.retain(|e| match e {
                    Event::MempoolTx(BlockTx { hash, .. }) => !removed_set.contains(hash.as_str()),
                    _ => true,
                });
            }
        }

        let _ = inner.tx.send(event);
    }

    pub async fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let inner = self.inner.lock().await;

        let mut snapshot = Vec::with_capacity(inner.blocks.len() + inner.mempool.len());
        for block in inner.blocks.iter().rev() {
            snapshot.push(block.clone());
        }
        snapshot.extend(inner.mempool.iter().cloned());

        let rx = inner.tx.subscribe();
        (snapshot, rx)
    }
}
