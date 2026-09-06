//! The probe's consumer: reads its ordinary input stream and proves the bytes
//! it was handed were never copied (blueprint §6.2, §6.3, §21 M1).
//!
//! ```text
//!   [producer] ──► ready  ──► announcement (8 B, daemon path — §6.2 threshold)
//!              ──► frames ──► ring ──► session attaches by itself ──┐
//!                                                                   ▼
//!                                    Event::Input { data: Payload } (zero copy)
//!                                                                   │
//!                                                                   ▼  verify, drop
//!                                                            ProbeReport (JSON)
//! ```
//!
//! # There is no segment plumbing here any more
//!
//! This file used to open the segment itself: read the producer's incarnation
//! out of the `ready` announcement, build an [`astrs_shm::SegmentKey`] from it,
//! dial the daemon's broker socket out of `ASTRS_NODE_CONFIG`, attach an
//! [`astrs_shm::Consumer`] and read the ring in a second loop beside the node's
//! event loop. It did that because the frozen `daemon → node` family had no
//! message that could say *"your input `frames` now reads from segment S"* —
//! `RouteUpgrade` names an **output**, which is the producer's half of §6.3.
//!
//! That vocabulary now exists ([`astrs_wire::NodeEvent::InputRouteUpgrade`],
//! appended at the tail per §7.2), the daemon sends it, and `astrs-node-api`
//! acts on it. So the consumer is what a consumer should be: one event loop,
//! reading `Event::Input`. The zero-copy plane engages underneath it —
//! [`astrs_node_api::Payload::is_zero_copy`] turns true and
//! [`astrs_node_api::Payload::slot`] says which ring slot the bytes are in —
//! and this file asserts that it did, which is the *point* of the probe.
//!
//! # What "verified" means here
//!
//! Every frame is checked while it is still mapped, and its [`Payload`] is
//! dropped immediately so the slot returns to the producer (the default
//! overflow policy is `Block`; a consumer that hoards samples stalls the ring
//! it is measuring). The invariants are listed on [`shm_zero_copy_probe`]'s
//! crate docs; this file is where they are applied.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use astrs_node_api::{Event, Node, Payload};
use astrs_wire::ShmSegmentSpec;
use shm_zero_copy_probe::{
    ANNOUNCE_UPGRADED, ANNOUNCE_UPGRADED_LEN, FRAMES_PORT, ProbeFrame, ProbeReport, ProbeSettings,
    READY_PORT, now_ns,
};

/// How long one receive waits before the loop looks at its deadline.
const RECV_SLICE: Duration = Duration::from_millis(250);

/// Exit code for a probe that ran but could not prove zero copy.
const EXIT_NOT_ENGAGED: u8 = 4;

/// How long the consumer keeps reading after its last frame, so an event the
/// mux had not yet served is still seen.
const SETTLE_MS: u64 = 500;

fn main() -> ExitCode {
    let settings = ProbeSettings::from_env();
    let report = match consume(&settings) {
        Ok(report) => report,
        Err(error) => ProbeReport::failed(error.to_string()),
    };

    print!("{}", report.summary());
    if let Err(error) = write_report(&settings, &report) {
        eprintln!("consumer: could not write the report: {error}");
        return ExitCode::FAILURE;
    }
    if report.zero_copy_engaged {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_NOT_ENGAGED)
    }
}

/// Writes the verdict where the manifest asked for it.
fn write_report(settings: &ProbeSettings, report: &ProbeReport) -> Result<(), std::io::Error> {
    let json = report
        .to_json()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    std::fs::write(&settings.report_path, json)
}

/// Reads the node's own event stream and verifies every frame it delivers.
fn consume(settings: &ProbeSettings) -> Result<ProbeReport, Box<dyn std::error::Error>> {
    let (node, mut events) = Node::init_from_env()?;
    let source = node
        .input_source(FRAMES_PORT)
        .ok_or("the `frames` input is not wired to anything")?
        .clone();

    node.log_info(format!(
        "probe consumer up: expecting {} frames from {source}",
        settings.frames
    ));

    let mut tally = Tally::new(settings);
    let deadline = Instant::now() + Duration::from_millis(settings.upgrade_timeout_ms);
    let started = Instant::now();

    while tally.verified < settings.frames {
        // `recv_timeout` answers `Ok(None)` both for "nothing yet" and for
        // "the session ended", so the two are told apart explicitly rather
        // than by ending the loop on the first quiet slice.
        match events.recv_timeout(RECV_SLICE)? {
            Some(Event::Input { id, data, .. }) if id.as_str() == FRAMES_PORT => {
                tally.accept(&data);
                // Dropped here, on purpose: the slot is pinned until it is,
                // and a pinned slot is one the producer cannot refill.
                drop(data);
            }
            Some(Event::Input { id, data, .. }) if id.as_str() == READY_PORT => {
                tally.accept_announcement(&data);
            }
            Some(Event::InputClosed { id, .. }) if id.as_str() == FRAMES_PORT => {
                tally.problem(format!(
                    "the producer closed `frames` after {} of {} frames",
                    tally.verified, settings.frames
                ));
                break;
            }
            Some(Event::Stop(cause)) => {
                tally.problem(format!("stopped before the last frame: {cause}"));
                break;
            }
            Some(Event::Error(message)) => tally.problem(message),
            Some(_) => {}
            None if events.session_ended() => {
                tally.problem(format!(
                    "the daemon connection ended after {} of {} frames",
                    tally.verified, settings.frames
                ));
                break;
            }
            None => {
                if Instant::now() >= deadline {
                    tally.problem(format!(
                        "only {} of {} frames arrived before the deadline",
                        tally.verified, settings.frames
                    ));
                    break;
                }
            }
        }
        // Read every pass, not once at the end: the attachment can be revoked
        // (a producer restart, a tap) and the last geometry seen is the one the
        // addresses were checked against.
        tally.observe_segment(node.input_segment(FRAMES_PORT));
    }

    // The frame loop ends the moment the last frame is verified, which can be
    // before the mux has served the *other* input at all — the `ready`
    // announcement may still be queued. Drain what is left, briefly: an event
    // already queued is not something the probe may fail to look at.
    let settle = Instant::now() + Duration::from_millis(SETTLE_MS);
    while Instant::now() < settle {
        match events.recv_timeout(Duration::from_millis(20))? {
            Some(Event::Input { id, data, .. }) if id.as_str() == READY_PORT => {
                tally.accept_announcement(&data);
                if tally.announcement_after_upgrade {
                    break;
                }
            }
            Some(_) => {}
            None if events.session_ended() => break,
            None => {}
        }
    }

    // The plane's own account of what it did, from the other side of the same
    // events: attachments made, samples read in place, fallbacks taken.
    let plane = node.input_plane_stats();
    if plane.attaches == 0 {
        tally.problem(
            "the session never attached to a ring: the daemon sent no input route upgrade"
                .to_owned(),
        );
    }
    tally.observe_segment(node.input_segment(FRAMES_PORT));

    let report = tally.finish(started.elapsed());
    node.log_info(format!(
        "planes: {:?}",
        node.input_planes()
            .iter()
            .map(|(id, plane)| format!("{id}={plane}"))
            .collect::<Vec<_>>()
    ));
    node.log_info(format!(
        "verified {} frames, zero-copy {} ({} attach(es), {} sample(s), {} fallback(s))",
        report.frames_verified,
        report.zero_copy_engaged,
        plane.attaches,
        plane.samples,
        plane.refusals,
    ));
    Ok(report)
}

/// The running verdict, one frame at a time.
struct Tally<'a> {
    /// What the run was asked to do.
    settings: &'a ProbeSettings,
    /// The segment the session attached to, once it has.
    segment: Option<ShmSegmentSpec>,
    /// How many frames passed every check.
    verified: u64,
    /// Distinct payload addresses seen.
    addresses: BTreeSet<usize>,
    /// The address each ring slot was seen at, which must never change.
    slots: BTreeMap<u32, usize>,
    /// Every payload sat where its slot's layout offset puts it.
    addresses_match_layout: bool,
    /// Every payload address was inside the mapping.
    addresses_inside_mapping: bool,
    /// Every in-band sequence matched the ring's own.
    sequences_match: bool,
    /// Every body folded to its declared fingerprint.
    fingerprints_match: bool,
    /// Whether a `ready` announcement arrived after the upgrade.
    announcement_after_upgrade: bool,
    /// Total commit-to-read nanoseconds.
    delivery_ns: u128,
    /// The slowest commit-to-read time, in nanoseconds.
    max_delivery_ns: u64,
    /// Total in-place verification nanoseconds.
    verify_ns: u128,
    /// Everything that went wrong.
    problems: Vec<String>,
}

impl<'a> Tally<'a> {
    /// An empty tally with every invariant still intact.
    fn new(settings: &'a ProbeSettings) -> Self {
        Self {
            settings,
            segment: None,
            verified: 0,
            addresses: BTreeSet::new(),
            slots: BTreeMap::new(),
            addresses_match_layout: true,
            addresses_inside_mapping: true,
            sequences_match: true,
            fingerprints_match: true,
            announcement_after_upgrade: false,
            delivery_ns: 0,
            max_delivery_ns: 0,
            verify_ns: 0,
            problems: Vec::new(),
        }
    }

    /// Records a problem, at most a handful of times.
    fn problem(&mut self, problem: impl Into<String>) {
        if self.problems.len() < 8 {
            self.problems.push(problem.into());
        }
    }

    /// Remembers the ring geometry the session reports for `frames`.
    fn observe_segment(&mut self, segment: Option<ShmSegmentSpec>) {
        if let Some(segment) = segment {
            self.segment = Some(segment);
        }
    }

    /// Records the `ready` announcement, which proves §6.2's threshold rule
    /// still delivers on an upgraded route.
    fn accept_announcement(&mut self, payload: &Payload) {
        if payload.is_zero_copy() {
            self.problem("a sub-threshold announcement took a ring slot".to_owned());
            return;
        }
        // Identified by content, not by arrival order: which of two inputs the
        // event mux serves first is its business (§11.2), and the rendezvous
        // announcements published before the upgrade prove nothing.
        let bytes = payload.bytes();
        if bytes.len() == ANNOUNCE_UPGRADED_LEN
            && bytes
                .get(8..16)
                .and_then(|word| <[u8; 8]>::try_from(word).ok())
                == Some(ANNOUNCE_UPGRADED.to_le_bytes())
        {
            self.announcement_after_upgrade = true;
        }
    }

    /// Checks one delivered payload against every invariant.
    fn accept(&mut self, payload: &Payload) {
        let read_ns = now_ns();
        let started = Instant::now();

        let Some(slot) = payload.slot() else {
            // An inline frame is not a probe failure by itself — §6.2 falls
            // back rather than sleep-retrying — but it is not a *verified*
            // frame either, and the count is what the verdict rests on.
            self.problem(format!(
                "a {}-byte frame arrived on the daemon path, not the ring",
                payload.len()
            ));
            return;
        };

        self.addresses.insert(slot.address);
        if !slot.is_at_layout_offset() {
            self.addresses_match_layout = false;
            self.problem(format!(
                "frame in slot {} sits at {:#x}, the layout puts it at {:#x}",
                slot.slot, slot.address, slot.layout_address
            ));
        }
        if !slot.is_inside_mapping() {
            self.addresses_inside_mapping = false;
            self.problem(format!(
                "payload address {:#x} is outside the mapping [{:#x}, {:#x})",
                slot.address,
                slot.mapping_base,
                slot.mapping_base + slot.mapping_len
            ));
        }
        // A ring reuses a fixed set of slots: slot *n* is the same address
        // every time it comes round. A copying path could not be.
        match self.slots.get(&slot.slot) {
            Some(&seen) if seen != slot.address => {
                self.addresses_match_layout = false;
                self.problem(format!(
                    "slot {} was at {seen:#x} and is now at {:#x}",
                    slot.slot, slot.address
                ));
            }
            Some(_) => {}
            None => {
                let _ = self.slots.insert(slot.slot, slot.address);
            }
        }

        // `bytes()` borrows the mapping; nothing is copied out of it here,
        // which is the whole point.
        let bytes = payload.bytes();
        match ProbeFrame::read_from(bytes) {
            Ok(frame) => {
                if frame.seq != slot.sequence {
                    self.sequences_match = false;
                    self.problem(format!(
                        "frame {} carries sequence {} but the ring says {}",
                        frame.index, frame.seq, slot.sequence
                    ));
                }
                if let Err(error) = frame.verify_body(ProbeFrame::body(bytes)) {
                    self.fingerprints_match = false;
                    self.problem(format!("frame {}: {error}", frame.index));
                } else {
                    self.verified += 1;
                }
            }
            Err(error) => {
                self.fingerprints_match = false;
                self.problem(format!("unreadable frame: {error}"));
            }
        }

        self.verify_ns += started.elapsed().as_nanos();
        let written = frame_written_ns(bytes);
        let delivery = read_ns.saturating_sub(written);
        self.delivery_ns += u128::from(delivery);
        self.max_delivery_ns = self.max_delivery_ns.max(delivery);
    }

    /// Turns the tally into the report the M1 test reads.
    fn finish(self, elapsed: Duration) -> ProbeReport {
        let frames = self.verified;
        let divisor = frames.max(1) as f64;
        let bytes_moved = frames * self.settings.slot_bytes() as u64;
        let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let slot_count = self.segment.as_ref().map_or(0, |spec| spec.slot_count);

        // Every invariant, plus the address set staying inside the ring: over
        // a run long enough to wrap, a copying path could not.
        //
        // The last conjunct is §6.2's threshold rule, and it belongs in the
        // verdict rather than beside it: a run where the large frames took the
        // ring but the small message behind them vanished is not a working
        // plane, it is a plane that loses messages. The M1 conformance test
        // reads `zero_copy_engaged`, so putting it here is what checks it.
        let engaged = frames == self.settings.frames
            && frames > 0
            && self.addresses_match_layout
            && self.addresses_inside_mapping
            && self.sequences_match
            && self.fingerprints_match
            && slot_count > 0
            && self.addresses.len() <= slot_count as usize
            && self.announcement_after_upgrade;

        let mut problems = self.problems;
        if !self.announcement_after_upgrade {
            problems.push(
                "the sub-threshold announcement published after the upgrade never arrived \
                 (§6.2's below-threshold rule)"
                    .to_owned(),
            );
        }
        if !engaged && problems.is_empty() {
            problems.push(format!(
                "{} distinct addresses over {slot_count} slots",
                self.addresses.len()
            ));
        }

        ProbeReport {
            zero_copy_engaged: engaged,
            frames_verified: frames,
            frame_bytes: self.settings.frame_bytes as u64,
            bytes_moved,
            distinct_addresses: self.addresses.len() as u64,
            slot_count,
            segment: self
                .segment
                .as_ref()
                .map_or_else(|| "<none>".to_owned(), |spec| spec.name.clone()),
            generation: self.segment.as_ref().map_or(0, |spec| spec.generation),
            addresses_match_layout: self.addresses_match_layout,
            addresses_inside_mapping: self.addresses_inside_mapping,
            sequences_match: self.sequences_match,
            fingerprints_match: self.fingerprints_match,
            attached_automatically: true,
            announcement_after_upgrade: self.announcement_after_upgrade,
            mean_delivery_us: self.delivery_ns as f64 / divisor / 1_000.0,
            max_delivery_us: f64::from(
                u32::try_from(self.max_delivery_ns / 1_000).unwrap_or(u32::MAX),
            ),
            mean_verify_us: self.verify_ns as f64 / divisor / 1_000.0,
            throughput_mib_s: bytes_moved as f64 / (1024.0 * 1024.0) / seconds,
            problems,
        }
    }
}

/// When the producer stamped this frame, read out of the frame itself.
///
/// The commit timestamp used to come from [`astrs_shm::Sample::commit_ns`],
/// which the node API deliberately does not expose: a payload is bytes and a
/// plane, not a slot header. The producer already writes its own wall-clock
/// reading into [`ProbeFrame::written_ns`], from the same clock, so the
/// delivery figure is measured the same way with one fewer layer told about
/// shared memory.
fn frame_written_ns(bytes: &[u8]) -> u64 {
    ProbeFrame::read_from(bytes).map_or(0, |frame| frame.written_ns)
}
