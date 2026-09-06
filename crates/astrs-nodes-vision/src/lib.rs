//! Ready-made AstRS vision nodes and operators.
//!
//! Camera frames arrive in whatever format the driver produces and are
//! wanted in whatever format, size, and state the consumer expects. This
//! crate ships that plumbing once: an owned pixel buffer ([`ImageBuffer`])
//! over [`astrs_data`]'s columnar image payloads, the classic per-pixel and
//! per-neighbourhood ops built on it, a pinhole camera model, and a
//! hand-rolled PNG codec — each exposed as a plain function and, where the
//! op reduces to a handful of config values, as an [`astrs_operator_api`]
//! operator registerable into a runtime host (blueprint §9.3).
//!
//! # Modules
//!
//! | Module | Covers |
//! |---|---|
//! | [`buffer`] | [`ImageBuffer`] — the owned, row-major frame every op below reads and writes |
//! | [`pixel`] | [`PixelFormat`] — mono/RGB/BGR/RGBA/YUYV/UYVY byte layouts |
//! | [`color`] | Conversion between every [`PixelFormat`] pair |
//! | [`resize`] | Nearest-neighbour and bilinear resizing |
//! | [`blur`] | Separable Gaussian blur |
//! | [`sobel`] | Sobel gradients and gradient magnitude |
//! | [`threshold`] | Fixed and Otsu binary thresholding |
//! | [`morphology`] | Erode, dilate, open, close |
//! | [`components`] | Connected-component labeling with per-component stats |
//! | [`draw`] | Line, rectangle and circle drawing primitives (no text — see the module doc) |
//! | [`camera`] | Pinhole intrinsics, plumb-bob distortion, undistort maps |
//! | [`png`] | A from-scratch PNG codec over `oxiarc-deflate` |
//! | [`ops`] | [`ops::operators`] — every operator above, registered by name |
//! | [`error`] | [`VisionError`] — this crate's one error type |
//!
//! # Pure Rust, codecs included
//!
//! Image handling is where C creeps into a robotics stack — `libpng`,
//! `libjpeg-turbo`, OpenCV. Nothing here links any of them: [`png`] is
//! hand-written against the spec, its one compression dependency is
//! `oxiarc-deflate` (the COOLJAPAN pure-Rust DEFLATE implementation —
//! `flate2` and `miniz_oxide` are both banned by `deny.toml`), and the
//! `*-sys` sweep stays clean. **JPEG is explicitly out of scope** — see
//! [`png`]'s own module doc for the one escape hatch
//! (`std/media/v1/CompressedImage` carries a JPEG frame through the graph
//! unopened; this crate never decodes one).
//!
//! # Format contracts, in one place
//!
//! Every op in this crate falls into exactly one of two families, and
//! which family decides what [`PixelFormat`] it accepts:
//!
//! * **Single-channel** ([`threshold`], [`sobel`], [`morphology`],
//!   [`components`]) requires [`PixelFormat::Mono8`] — each is defined over
//!   one greyscale/binary plane, with no sensible per-channel
//!   generalisation the algorithm itself calls for.
//! * **Geometric/filtering** ([`resize`], [`blur`], [`draw`], [`color`]'s
//!   own conversions) accepts any [`PixelFormat::is_fully_sampled`] format
//!   ([`PixelFormat::Mono8`]/[`PixelFormat::Rgb8`]/[`PixelFormat::Bgr8`]/
//!   [`PixelFormat::Rgba8`]), operating on each byte plane independently —
//!   refused only for the two packed 4:2:2 formats
//!   ([`PixelFormat::Yuyv`]/[`PixelFormat::Uyvy`]), which have no
//!   single-pixel byte range to read or write; convert with [`color`]
//!   first.
//!
//! Every function that rejects a format does so with
//! [`VisionError::UnsupportedFormat`], naming both the operation and the
//! format it was handed.
//!
//! # Examples
//!
//! ```
//! use astrs_nodes_vision::color::to_gray;
//! use astrs_nodes_vision::threshold::{ThresholdMode, threshold_otsu};
//! use astrs_nodes_vision::{ImageBuffer, PixelFormat};
//!
//! // A 4x1 RGB strip: two dark pixels, two bright ones.
//! let strip = ImageBuffer::new(
//!     PixelFormat::Rgb8,
//!     4,
//!     1,
//!     vec![10, 10, 10, /**/ 20, 20, 20, /**/ 220, 220, 220, /**/ 230, 230, 230],
//! )?;
//!
//! let grey = to_gray(&strip)?;
//! let (binary, level) = threshold_otsu(&grey, ThresholdMode::Normal)?;
//! assert!((20..220).contains(&level));
//! assert_eq!(binary.data(), &[0, 0, 255, 255]);
//! # Ok::<(), astrs_nodes_vision::VisionError>(())
//! ```

pub mod blur;
pub mod buffer;
pub mod camera;
pub mod color;
pub mod components;
pub mod draw;
pub mod error;
pub mod morphology;
pub mod ops;
pub mod pixel;
pub mod png;
pub mod resize;
pub mod sobel;
pub mod threshold;

pub use buffer::ImageBuffer;
pub use camera::{Distortion, Interpolation, Intrinsics, UndistortMap};
pub use components::{ComponentStats, Connectivity, Labels};
pub use error::{Result, VisionError};
pub use morphology::StructuringElement;
pub use ops::operators;
pub use pixel::PixelFormat;
pub use png::PngError;
pub use resize::ResizeMode;
pub use sobel::SobelGradients;
pub use threshold::ThresholdMode;
