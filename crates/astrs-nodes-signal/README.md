# astrs-nodes-signal

Ready-made AstRS signal-processing nodes and operators: FFT/IFFT, windows,
RBJ biquad filters, windowed-sinc FIR design, polyphase decimation and
interpolation, a windowed STFT spectrogram, and moving/exponential
averages.

A dataflow that has to spectrum-analyse an IMU stream, band-pass a
microphone or resample a lidar scan should not need a bespoke node written
from scratch. Every stage in this crate ships twice: as a plain, directly
callable Rust function or type (numerically tested against closed-form
cases — impulse/step responses, Parseval's theorem, exact RBJ magnitude
identities), and as a registered `astrs-operator-api` `Operator`
(`#[operator]`) configured through a manifest's `config:` map and wired to
`astrs-data`'s columnar payloads. The transforms are backed by `oxifft` —
pure Rust throughout, with no FFTW, no `rustfft`, and no C anywhere in the
graph.

| Stage | Function/type | Operator |
|---|---|---|
| Complex FFT/IFFT | `fft_forward`, `fft_inverse` | `FftOperator`, `IfftOperator` |
| Real FFT/IFFT | `real_fft`, `real_ifft` | `RfftOperator`, `IrfftOperator` |
| Windows (Hann, Hamming, Blackman, Blackman-Harris) | `WindowFunction` | `WindowOperator` |
| RBJ biquad filters (lowpass/highpass/bandpass/notch) | `RbjCoefficients`, `Biquad` | `BiquadOperator` |
| Windowed-sinc FIR design + streaming convolution | `design_lowpass`/`design_highpass`/`design_bandpass`/`design_bandstop`, `FirFilter` | `FirOperator` |
| Polyphase decimation/interpolation | `Decimator`, `Interpolator` | `DecimateOperator`, `InterpolateOperator` |
| Windowed STFT spectrogram | `stft_magnitude`, `Spectrogram` | `SpectrogramOperator` |
| Moving / exponential average | `MovingAverage`, `ExponentialAverage` | `MovingAverageOperator`, `ExponentialAverageOperator` |

## Example

```rust
use astrs_nodes_signal::{Biquad, RbjCoefficients, WindowFunction};

// RBJ cookbook lowpass: unity gain at DC, exactly, by construction.
let coefficients = RbjCoefficients::lowpass(48_000.0, 1_000.0, std::f64::consts::FRAC_1_SQRT_2)?;
assert!((coefficients.magnitude_at(48_000.0, 1e-9) - 1.0).abs() < 1e-9);

let mut filter = Biquad::new(coefficients);
let mut impulse = vec![0.0; 8];
impulse[0] = 1.0;
assert_eq!(filter.process_block_f64(&impulse).len(), 8);

// Hann uses the DFT-even (periodic) definition: zero at the frame's first
// sample, exactly 1.0 at its centre — see the crate docs for why FIR design
// wants the *symmetric* form instead.
assert!(WindowFunction::Hann.coefficient(0, 8).abs() < 1e-12);
assert!((WindowFunction::Hann.coefficient(4, 8) - 1.0).abs() < 1e-12);
# Ok::<(), astrs_nodes_signal::SignalError>(())
```

See `examples/` for a full offline processing pipeline built from this
crate's plain functions, and for wiring several of its operators into a
manifest-style dataflow.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
