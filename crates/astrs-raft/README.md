# astrs-raft

Raft consensus over `astrs-wire`: coordinator high availability for AstRS
clusters.

A single coordinator is a single point of failure for the whole cluster's
control plane. This crate replicates the coordinator's authoritative state —
the dataflow registry, node placement decisions and daemon membership — as a
Raft log across an odd-sized peer set, so a coordinator loss costs an
election timeout rather than the fleet. Leader election, log replication,
snapshotting and membership changes all ride the existing `astrs-wire`
framing and `oxicode` encoding; there is no second transport and no second
serialization format to keep in step.

## What is implemented

| Piece | Notes |
|---|---|
| Figure 2 | terms, elections, `AppendEntries`, the commit rule including the Figure 8 condition |
| Pre-vote | a partitioned peer cannot disrupt a healthy leader by returning with an inflated term |
| Write-ahead log | `oxicode`-framed records with CRC-32C, an explicit `FsyncPolicy`, and a torn tail **repaired** on open rather than merely skipped |
| Snapshots | trait-driven state-machine capture/restore, log compaction, `InstallSnapshot` for a follower behind the compaction |
| Membership | single-server add/remove, in force on append, at most one change in flight |
| Transports | an in-process channel transport and a TCP one framed exactly like every other AstRS control link |
| Simulator | a virtual clock and a seeded network that drops, duplicates, delays and partitions — with the four safety properties checked after every tick |

## The core is a pure state machine

`RaftNode` performs no I/O and reads no clock. It consumes ticks, messages
and proposals, and produces messages a caller drains. That is what lets the
deterministic simulator run the *same* consensus code a production replica
runs, and replay any failing seed exactly.

## Example

```rust
use astrs_raft::{MemoryLog, MemoryStateMachine, PeerId, RaftConfig, RaftNode, Role, Term};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Terms are monotonic: a follower that sees a higher term steps down.
    let current = Term::new(7);
    assert!(current.is_stale_against(Term::new(8)));
    assert_eq!(current.next(), Term::new(8));

    // A cluster of one elects itself, because one vote is already a majority.
    let config = RaftConfig::new(PeerId::new(1))
        .with_peer(PeerId::new(1), "127.0.0.1:7601".parse()?);
    let mut node = RaftNode::new(config, MemoryLog::new(), MemoryStateMachine::new())?;
    for _ in 0..64 {
        node.tick()?;
    }
    assert_eq!(node.role(), Role::Leader);

    let index = node.propose(b"set gain 3".to_vec())?;
    assert!(node.commit_index() >= index);
    Ok(())
}
```

See [`astrs-coordinator`](https://docs.rs/astrs-coordinator)'s `ha` feature
for the integration.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
