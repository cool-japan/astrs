//! `camera-sim` — the source of the canonical pipeline (blueprint §8.1).
//!
//! Publishes one synthetic RGB frame per timer tick on the typed port
//! `frames`, declared in the manifest as
//! `std/media/v1/Image[pixel=rgb8]`.
//!
//! ```text
//!   astrs/timer/hz/20 ──► tick ──► [camera-sim] ──frames──► (detector, recorder)
//! ```
//!
//! # Why `send_batch` and not `send`
//!
//! [`Image`]'s columnar layout depends on its URN parameter, so it is not an
//! `AstrsMessage` with a `const URN` (see this example's `lib.rs`). It encodes
//! itself instead, and the encoded batch goes out through the raw port — which
//! is the same wire, the same `Payload`, and the same
//! [`astrs_node_api::Payload::view`] on the far side. A message of your own
//! (the detector's [`rust_pipeline::Detections`]) uses the typed
//! [`astrs_node_api::Output`] handle instead.

use std::process::ExitCode;

use astrs_node_api::message::{Image, ImageSamples, PixelFormat};
use astrs_node_api::{Event, Node};
use rust_pipeline::{FRAME_HEIGHT, FRAME_WIDTH, FRAMES_PORT, TICK_PORT, frame_budget};

fn main() -> ExitCode {
    match publish_frames() {
        Ok(frames) => {
            println!("camera-sim: published {frames} frames");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("camera-sim: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the frame budget, one frame per tick, then finishes.
fn publish_frames() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = frame_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut frames = node.raw_output(FRAMES_PORT)?;
    node.log_info(format!(
        "camera up: {budget} frames of {FRAME_WIDTH}x{FRAME_HEIGHT} rgb8"
    ));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                let image = synthetic_frame(published)?;
                // `meta.follow()` keeps the causal chain: the frame is stamped
                // as *caused by* the tick that produced it, which is what the
                // recorder's trace and `astrs trace` read (§4.3).
                frames.send_batch(&image.to_record_batch()?, meta.follow())?;
                published += 1;
                if published >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} frames: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // Closing tells the detector and the recorder that no more frames are
    // coming, so they finish instead of waiting. Dropping the handle would do
    // the same; saying it explicitly is clearer in an example.
    frames.close()?;
    node.log_info(format!("published {published} frames"));
    Ok(published)
}

/// One deterministic frame: a moving diagonal gradient.
///
/// Deterministic so a conformance test can assert on what the detector saw,
/// and cheap so the example measures the dataflow rather than a renderer.
fn synthetic_frame(index: u64) -> Result<Image, astrs_data::DataError> {
    let channels = usize::from(PixelFormat::Rgb8.channels());
    let mut samples = Vec::with_capacity((FRAME_WIDTH * FRAME_HEIGHT) as usize * channels);
    for row in 0..FRAME_HEIGHT {
        for column in 0..FRAME_WIDTH {
            let shade = u8::try_from((u64::from(row + column) + index) % 256).unwrap_or(0);
            samples.push(shade);
            samples.push(shade.wrapping_add(64));
            samples.push(shade.wrapping_add(128));
        }
    }
    Image::new(
        PixelFormat::Rgb8,
        FRAME_WIDTH,
        FRAME_HEIGHT,
        ImageSamples::U8(samples),
    )
}
