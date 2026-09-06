//! The probe's producer: allocates 4 MiB frames in the ring and publishes
//! them without a copy (blueprint §6.2, §6.3, §9.1).
//!
//! ```text
//!   astrs/timer/millis/20 ──► tick ──┐
//!                                    ├─► [producer] ──► ready   (announcement)
//!                                    │                └─► frames  (4 MiB, zero copy)
//! ```
//!
//! # The shape of the loop, and why it is this shape
//!
//! Blueprint §6.3 starts *every* output on the reliable daemon path and moves
//! it to shared memory only once the daemon has seen every same-host consumer
//! attach to the ring. A producer therefore cannot assume the zero-copy plane
//! at startup — it has to *observe* the upgrade. This node does that on a
//! timer tick rather than in a sleep loop, which is how an AstRS node waits
//! for anything: the wait is an ordinary input, so the node stays in its event
//! loop and the daemon stays in charge of the schedule.
//!
//! # The `ready` announcement, and what it is for now
//!
//! It used to carry the producer's *incarnation* number, because the consumer
//! had to name the ring itself. It does not any more — the daemon tells the
//! consumer which segment feeds its input (§6.3's
//! [`astrs_wire::NodeEvent::InputRouteUpgrade`]) — so the announcement has
//! become something better: a **sub-threshold message on an upgraded output**.
//!
//! Eight bytes are far below §6.2's zero-copy threshold, so they ride the
//! daemon's control channel even after `frames` has moved to the ring. That is
//! §6.2 working as specified, and it is the one case where an upgraded route
//! still has two carriers. The producer therefore publishes one announcement
//! *after* the upgrade lands, and the consumer asserts it arrived: a small
//! message on a route whose large messages bypass the daemon must not be lost.
//!
//! The announcements *before* the upgrade stay as they were — they are the
//! rendezvous that lets the consumer register before anything is published,
//! the same one `bins/astrs-cli/tests/run_e2e.rs` documents.
//!
//! # Why a frame is skipped rather than downgraded
//!
//! [`RawOutput::allocate`] falls back to a heap buffer when the ring cannot
//! take the message (§6.2: never sleep-retry). That is the right behaviour
//! for a real node — the message still gets delivered, one plane down. For a
//! *probe* it would be a lie: an inline frame never reaches the consumer's
//! ring, and counting it would inflate the very number the probe exists to
//! report. So a non-zero-copy sample is aborted and retried on the next tick,
//! and the skip is counted and printed.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use astrs_node_api::{Event, Node, RawOutput};
use shm_zero_copy_probe::{
    ANNOUNCE_UPGRADED, ANNOUNCE_UPGRADED_LEN, FRAMES_PORT, ProbeFrame, ProbeSettings, READY_PORT,
};

/// The input the timer drives.
const TICK_PORT: &str = "tick";

/// Exit code for a probe that never reached the zero-copy plane.
const EXIT_NO_UPGRADE: u8 = 3;

fn main() -> ExitCode {
    match produce() {
        Ok(Outcome::Sent(frames)) => {
            println!("producer: published {frames} zero-copy frames");
            ExitCode::SUCCESS
        }
        Ok(Outcome::NeverUpgraded { waited }) => {
            eprintln!(
                "producer: the shared-memory plane never engaged after {waited:?}; \
                 the consumer did not attach to the ring"
            );
            ExitCode::from(EXIT_NO_UPGRADE)
        }
        Err(error) => {
            eprintln!("producer: {error}");
            ExitCode::FAILURE
        }
    }
}

/// How the run ended.
enum Outcome {
    /// Every frame was published on the zero-copy plane.
    Sent(u64),
    /// The §6.3 upgrade never arrived.
    NeverUpgraded {
        /// How long the producer waited for it.
        waited: Duration,
    },
}

/// Runs the producer to completion.
fn produce() -> Result<Outcome, Box<dyn std::error::Error>> {
    let settings = ProbeSettings::from_env();
    let (mut node, mut events) = Node::init_from_env()?;
    let generation = node.generation();
    let mut ready = node.raw_output(READY_PORT)?;
    let mut frames = node.raw_output(FRAMES_PORT)?;

    node.log_info(format!(
        "probe producer up: {} frames of {} B, generation {generation}",
        settings.frames, settings.frame_bytes,
    ));

    let started = Instant::now();
    let deadline = started + Duration::from_millis(settings.upgrade_timeout_ms);
    let mut sent = 0_u64;
    let mut skipped = 0_u64;
    let mut upgraded_at: Option<Instant> = None;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, .. } if id.as_str() == TICK_PORT => {
                // Apply any route change the daemon signalled since the last
                // tick. `send`/`allocate` do this themselves; doing it here
                // too is what lets the node *observe* the plane before it
                // decides what to publish.
                frames.refresh();

                if !frames.plane().is_zero_copy() {
                    if Instant::now() >= deadline {
                        return Ok(Outcome::NeverUpgraded {
                            waited: started.elapsed(),
                        });
                    }
                    announce(&mut ready, generation)?;
                    continue;
                }

                if upgraded_at.is_none() {
                    upgraded_at = Some(Instant::now());
                    let segment = frames.segment().map_or_else(
                        || "?".to_owned(),
                        |spec| {
                            format!(
                                "{} generation {} ({} slots x {} B)",
                                spec.name, spec.generation, spec.slot_count, spec.slot_size
                            )
                        },
                    );
                    node.log_info(format!("shared-memory plane engaged: {segment}"));
                    println!(
                        "producer: zero-copy plane engaged after {:?}",
                        started.elapsed()
                    );
                }

                match publish_frame(&mut frames, &settings, sent)? {
                    Published::ZeroCopy => sent += 1,
                    Published::RingFull => skipped += 1,
                }

                if sent == 1 {
                    // One announcement behind the first ring frame: sixteen
                    // bytes is far below §6.2's threshold, so it rides the
                    // daemon path even though `ready` is itself on the
                    // shared-memory plane. It carries `ANNOUNCE_UPGRADED` so
                    // the consumer identifies it by *content* — cross-input
                    // arrival order is the mux's business (§11.2), not a fact
                    // the probe may lean on.
                    announce_upgraded(&mut ready, generation)?;
                }

                if sent >= settings.frames {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {sent} frames: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // Closing tells every consumer the stream ended (§7.3) and marks the ring
    // closed, so the consumer drains what is resident and then sees
    // `RecvError::Closed` instead of waiting for a producer that has gone.
    frames.close()?;
    ready.close()?;
    node.log_info(format!(
        "published {sent} zero-copy frames, skipped {skipped} full-ring ticks"
    ));
    Ok(Outcome::Sent(sent))
}

/// Whether a frame made it onto the ring.
enum Published {
    /// The frame was written straight into a ring slot.
    ZeroCopy,
    /// The ring had no free slot this tick.
    RingFull,
}

/// Writes one frame into a ring slot and publishes it.
///
/// The frame is generated *into* the slot: there is no staging buffer, no
/// intermediate `Vec`, and the bytes are touched exactly once between
/// `allocate` and `send`.
fn publish_frame(
    frames: &mut RawOutput,
    settings: &ProbeSettings,
    index: u64,
) -> Result<Published, Box<dyn std::error::Error>> {
    // Stamped before the slot is reserved: the sample borrows the output for
    // as long as it lives, and a fresh HLC reading is what makes the message
    // orderable against every other message in the graph (§4.3).
    let metadata = frames.metadata();

    let mut sample = frames.allocate(settings.slot_bytes())?;
    if !sample.is_zero_copy() {
        // The heap fallback: correct for a node, useless for a probe.
        sample.abort();
        return Ok(Published::RingFull);
    }

    let seq = sample.sequence().unwrap_or_default();
    let frame = ProbeFrame {
        index,
        seq,
        body_len: settings.frame_bytes as u64,
        fingerprint: 0,
        written_ns: 0,
    };
    frame.write_into(sample.as_mut_slice())?;
    sample.send(metadata)?;
    Ok(Published::ZeroCopy)
}

/// Publishes the ring incarnation the consumer needs to name the segment.
///
/// Eight little-endian bytes rather than a columnar message: the announcement
/// is plumbing for a protocol gap (see the README), and keeping it raw makes
/// it obvious that it is not part of the data path being measured.
fn announce(ready: &mut RawOutput, generation: u64) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = ready.metadata();
    ready.send_bytes(generation.to_le_bytes(), metadata)?;
    Ok(())
}

/// Publishes the post-upgrade announcement: §6.2's threshold rule, checked.
///
/// Sixteen bytes rather than eight, with [`ANNOUNCE_UPGRADED`] behind the
/// generation, so the consumer knows *which* announcement this is without
/// depending on the order two different inputs happen to be served in.
fn announce_upgraded(
    ready: &mut RawOutput,
    generation: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = ready.metadata();
    let mut bytes = [0_u8; ANNOUNCE_UPGRADED_LEN];
    bytes[..8].copy_from_slice(&generation.to_le_bytes());
    bytes[8..].copy_from_slice(&ANNOUNCE_UPGRADED.to_le_bytes());
    ready.send_bytes(bytes, metadata)?;
    Ok(())
}
