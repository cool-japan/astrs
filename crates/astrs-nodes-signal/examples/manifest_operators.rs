//! Wires four of this crate's operators into a small dataflow, the way a
//! manifest's `operators:` block (blueprint §9.3) would describe it, and
//! then drives that exact graph by hand: build each operator from an
//! [`OperatorRegistry`] by its manifest `operator:` name, `configure` it
//! from a `config:` map, and push a sequence of [`OpEvent::Input`]s through
//! it — precisely the `configure` → `on_start` → `on_event*` → `on_stop`
//! lifecycle `astrs-runtime` itself drives an operator through (blueprint
//! §9.3), reusing these operators completely unmodified.
//!
//! This deliberately stops short of spawning a real `astrs-runtime`
//! process: `astrs-nodes-signal` has no dependency on it (nor should it —
//! this crate is a library of operators, not a host for them), so wiring
//! [`MANIFEST_YAML`] into an actual `astrs run` is left to whichever
//! top-level dataflow chooses to depend on both. What is exercised here is
//! everything this crate itself owns: the same registry lookup, the same
//! `config:` map shape, and the same event lifecycle a real host uses.
//!
//! The graph: a raw vibration-style sensor feeds a lowpass [`BiquadOperator`]
//! (removes fast noise above 200 Hz), whose output feeds a
//! [`DecimateOperator`] (drops the data rate 4x for downstream consumers).
//! The decimated stream then fans out to two independent consumers — a
//! [`MovingAverageOperator`] (a smoothed telemetry read-out) and a
//! [`SpectrogramOperator`] (a diagnostics view) — exactly as
//! [`MANIFEST_YAML`] describes it.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p astrs-nodes-signal --example manifest_operators
//! ```

use std::collections::BTreeMap;

use astrs_data::ipc;
use astrs_nodes_signal::{
    BiquadOperator, DecimateOperator, Frame, MovingAverageOperator, Spectrogram,
    SpectrogramOperator,
};
use astrs_operator_api::{
    AstrsMessage, OpEvent, OpOutput, Operator, OperatorRegistry, register_operator,
};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, Metadata, Parameter};

/// Sample rate the raw sensor stream (and the biquad/decimator it feeds)
/// runs at, in Hz.
const SAMPLE_RATE_HZ: f64 = 2_000.0;

/// Frequency of the synthetic signal's in-band component, in Hz — survives
/// both the biquad and the decimation.
const IN_BAND_HZ: f64 = 80.0;

/// Frequency of the synthetic signal's out-of-band component, in Hz — the
/// biquad's cutoff sits well below this.
const OUT_OF_BAND_HZ: f64 = 600.0;

/// The lowpass biquad's cutoff frequency, in Hz.
const BIQUAD_CUTOFF_HZ: f64 = 200.0;

/// The decimator's downsampling factor.
const DECIMATE_FACTOR: i64 = 4;

/// Samples per simulated input chunk (one `OpEvent::Input` per chunk).
const CHUNK_LEN: usize = 256;

/// Number of chunks streamed through the pipeline.
const NUM_CHUNKS: usize = 4;

/// The manifest fragment [`main`]'s four operators mirror exactly: one
/// `pipeline` node hosting a lowpass-biquad → decimate → {moving-average,
/// spectrogram} graph. See the [module documentation](self) for why this
/// example prints it rather than handing it to `astrs run`.
const MANIFEST_YAML: &str = r#"astrs: "1"
name: vibration-monitor
nodes:
  - id: pipeline
    operators:
      - id: biquad
        operator: BiquadOperator
        inputs:
          in: sensor/vibration
        outputs: [filtered]
        config:
          kind: lowpass
          sample_rate_hz: 2000.0
          frequency_hz: 200.0

      - id: decimate
        operator: DecimateOperator
        inputs:
          in: biquad/filtered
        outputs: [decimated]
        config:
          factor: 4
          sample_rate_hz: 2000.0

      - id: smooth
        operator: MovingAverageOperator
        inputs:
          in: decimate/decimated
        outputs: [averaged]
        config:
          capacity: 4

      - id: spectrogram
        operator: SpectrogramOperator
        inputs:
          in: decimate/decimated
        outputs: [spectrogram]
        config:
          frame_size: 32
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{MANIFEST_YAML}");

    // `register_operator!`'s bare form registers each type under its own
    // stringified Rust name (`"BiquadOperator"`, ...) -- exactly the
    // `operator:` spelling `MANIFEST_YAML` uses above, and the same
    // resolution `astrs-runtime`'s host performs via
    // `OperatorRegistry::build(&op.operator)`.
    let registry = OperatorRegistry::from_entries([
        register_operator!(BiquadOperator),
        register_operator!(DecimateOperator),
        register_operator!(MovingAverageOperator),
        register_operator!(SpectrogramOperator),
    ])?;

    let mut biquad = registry.build("BiquadOperator")?;
    let mut decimate = registry.build("DecimateOperator")?;
    let mut smooth = registry.build("MovingAverageOperator")?;
    let mut spectrogram = registry.build("SpectrogramOperator")?;

    biquad.configure(&biquad_config())?;
    decimate.configure(&decimate_config())?;
    smooth.configure(&smooth_config())?;
    spectrogram.configure(&spectrogram_config())?;

    let mut scratch = OpOutput::new();
    for operator in [&mut biquad, &mut decimate, &mut smooth, &mut spectrogram] {
        operator.on_start(&mut scratch)?;
        scratch.drain();
    }

    for chunk_index in 0..NUM_CHUNKS {
        run_one_chunk(
            chunk_index,
            biquad.as_mut(),
            decimate.as_mut(),
            smooth.as_mut(),
            spectrogram.as_mut(),
        )?;
    }

    for operator in [&mut biquad, &mut decimate, &mut smooth, &mut spectrogram] {
        operator.on_stop(&mut scratch)?;
        scratch.drain();
    }

    println!("pipeline complete");
    Ok(())
}

/// Pushes one synthetic chunk through the whole graph: `biquad` ->
/// `decimate` -> (`smooth` and `spectrogram`, both fed the same decimated
/// bytes, mirroring [`MANIFEST_YAML`]'s fan-out), printing what each branch
/// produced.
fn run_one_chunk(
    chunk_index: usize,
    biquad: &mut dyn Operator,
    decimate: &mut dyn Operator,
    smooth: &mut dyn Operator,
    spectrogram: &mut dyn Operator,
) -> Result<(), Box<dyn std::error::Error>> {
    let chunk = synthesize_chunk(chunk_index * CHUNK_LEN);
    let payload = ipc::encode_payload(&Frame::new(chunk).to_record_batch()?)?.to_vec();

    let mut out = OpOutput::new();
    biquad.on_event(&input_event("sensor/vibration", payload)?, &mut out)?;
    let filtered_bytes = take_one_payload(&mut out, "biquad")?;

    decimate.on_event(&input_event("biquad/filtered", filtered_bytes)?, &mut out)?;
    let decimated_bytes = take_one_payload(&mut out, "decimate")?;

    smooth.on_event(
        &input_event("decimate/decimated", decimated_bytes.clone())?,
        &mut out,
    )?;
    let averaged_bytes = take_one_payload(&mut out, "smooth")?;
    let averaged = Frame::from_record_batch(&ipc::decode_payload(&averaged_bytes)?)?;

    spectrogram.on_event(
        &input_event("decimate/decimated", decimated_bytes)?,
        &mut out,
    )?;
    let spectrogram_bytes = take_one_payload(&mut out, "spectrogram")?;
    let diagnostics = Spectrogram::from_record_batch(&ipc::decode_payload(&spectrogram_bytes)?)?;

    println!(
        "chunk {chunk_index}: smoothed tail sample = {:.4}, spectrogram = {} frame(s) x {} bin(s)",
        averaged.samples.last().copied().unwrap_or(0.0),
        diagnostics.frames,
        diagnostics.bins,
    );
    Ok(())
}

/// Synthesizes `CHUNK_LEN` samples of the raw sensor signal starting at
/// `start_sample`, continuing the same underlying tone across chunk
/// boundaries (so the operators streaming across calls see one continuous
/// signal, not `NUM_CHUNKS` unrelated fragments).
fn synthesize_chunk(start_sample: usize) -> Vec<f32> {
    (0..CHUNK_LEN)
        .map(|offset| {
            let t = (start_sample + offset) as f64 / SAMPLE_RATE_HZ;
            let value = (2.0 * std::f64::consts::PI * IN_BAND_HZ * t).sin()
                + 0.5 * (2.0 * std::f64::consts::PI * OUT_OF_BAND_HZ * t).sin();
            value as f32
        })
        .collect()
}

/// The `biquad` operator's `config:` map, matching [`MANIFEST_YAML`].
fn biquad_config() -> BTreeMap<String, Parameter> {
    let mut config = BTreeMap::new();
    config.insert("kind".to_owned(), Parameter::String("lowpass".to_owned()));
    config.insert(
        "sample_rate_hz".to_owned(),
        Parameter::Float(SAMPLE_RATE_HZ),
    );
    config.insert(
        "frequency_hz".to_owned(),
        Parameter::Float(BIQUAD_CUTOFF_HZ),
    );
    config
}

/// The `decimate` operator's `config:` map, matching [`MANIFEST_YAML`].
fn decimate_config() -> BTreeMap<String, Parameter> {
    let mut config = BTreeMap::new();
    config.insert("factor".to_owned(), Parameter::Integer(DECIMATE_FACTOR));
    config.insert(
        "sample_rate_hz".to_owned(),
        Parameter::Float(SAMPLE_RATE_HZ),
    );
    config
}

/// The `smooth` operator's `config:` map, matching [`MANIFEST_YAML`].
fn smooth_config() -> BTreeMap<String, Parameter> {
    let mut config = BTreeMap::new();
    config.insert("capacity".to_owned(), Parameter::Integer(4));
    config
}

/// The `spectrogram` operator's `config:` map, matching [`MANIFEST_YAML`]
/// (`hop_size` and `window` are both left at their operator defaults, as the
/// manifest fragment itself does by omitting them).
fn spectrogram_config() -> BTreeMap<String, Parameter> {
    let mut config = BTreeMap::new();
    config.insert("frame_size".to_owned(), Parameter::Integer(32));
    config
}

/// Builds one [`OpEvent::Input`] carrying `payload`, as if it had just
/// arrived from `source`.
fn input_event(source: &str, payload: Vec<u8>) -> Result<OpEvent, Box<dyn std::error::Error>> {
    Ok(OpEvent::Input {
        id: DataId::new("in")?,
        source: source.parse()?,
        metadata: Metadata::new(HlcTimestamp::EPOCH),
        payload,
    })
}

/// Drains exactly one buffered send from `out` and returns its payload
/// bytes, failing loudly (rather than indexing into an empty buffer) if
/// `what` produced none.
fn take_one_payload(out: &mut OpOutput, what: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut sends = out.drain();
    if sends.is_empty() {
        return Err(format!("{what} produced no output").into());
    }
    Ok(sends.remove(0).into_parts().2)
}
