//! CDR serialization for ROS 2 and DDS interoperability.
//!
//! The byte-level foundation of the AstRS ROS 2 pillar (blueprint §10.1).
//! Everything a DDS participant puts on the wire — a topic sample, a service
//! request, an SPDP discovery announcement — is CDR, and this crate is the
//! only place in AstRS that knows how those octets are laid out.
//!
//! - **XCDR1 encode and decode**, big- and little-endian, with the ROS 2
//!   alignment rules (OMG CDR, CORBA 3.0 §15.3).
//! - **XCDR2 read** for XTypes-annotated types, and write as well, so the
//!   read path can be property-tested against an encoder rather than against
//!   hand-typed octets alone (OMG DDS-XTypes 1.3 §7.4.3).
//! - **Encapsulation headers**: `CDR_BE`/`CDR_LE`, `PL_CDR_BE`/`PL_CDR_LE`,
//!   and the six XCDR2 identifiers, with the trailing-padding field the
//!   options word carries.
//! - **A [`CdrSerde`] trait pair** — [`CdrSerialize`] and
//!   [`CdrDeserialize`] — designed for `astrs-idl` codegen and for
//!   zero-copy `&str` / `&[u8]` accessors.
//! - **[`ParameterList`]**, the `PL_CDR` representation RTPS discovery is
//!   built from: parameter id, padded length, value, `PID_SENTINEL`.
//!
//! # The alignment origin
//!
//! CDR aligns every primitive to a multiple of its own size, counted from the
//! first octet **after** the four-octet encapsulation header. That octet is
//! the *alignment origin*, and both [`CdrWriter`] and [`CdrReader`] track it
//! explicitly so a nested scope — a `PL_CDR` parameter value, a payload
//! appended into a larger RTPS datagram — can restate it without arithmetic
//! at the call site.
//!
//! ```
//! use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};
//!
//! /// `geometry_msgs/msg/Point`.
//! #[derive(Debug, Clone, Copy, PartialEq)]
//! struct Point {
//!     x: f64,
//!     y: f64,
//!     z: f64,
//! }
//!
//! astrs_cdr::cdr_struct_impls!(Point { x: f64, y: f64, z: f64 });
//!
//! let point = Point { x: 1.0, y: 2.0, z: 3.0 };
//! let bytes = astrs_cdr::to_vec_ros2(&point)?;
//! assert_eq!(bytes.len(), 4 + 24);
//! assert_eq!(&bytes[..4], &[0x00, 0x01, 0x00, 0x00]); // CDR_LE
//! assert_eq!(astrs_cdr::from_bytes::<Point>(&bytes)?, point);
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```
//!
//! # Golden vectors
//!
//! The `tests/` directory carries the conformance vectors, each derived
//! octet by octet from the OMG CDR and XTypes specifications with a worked
//! derivation in the test itself: which offset every member lands on, how
//! many pad octets precede it, and why. No fixture in this repository was
//! captured from a C or C++ DDS stack — cross-stack validation lives in a
//! separate out-of-repo project, per blueprint §18.
//!
//! # Test methodology
//!
//! The golden vectors above are fixed, hand-derived fixtures — nothing
//! about *them* is generated. What is generated is the input to
//! `tests/property.rs`'s properties (round trip in every encapsulation kind,
//! alignment invariants, truncation and trailing-octet rejection, no panic
//! and no unbounded allocation on arbitrary octets): **property-tested**
//! via `proptest`, not fuzzed via `cargo-fuzz` — `cargo-fuzz`/libfuzzer
//! links a C++ runtime and is excluded outright by the Pure Rust policy
//! (§3.1). Deeper coverage of the reader specifically — one of the four
//! attack surfaces this stack cares about — is **deep-fuzzed** via the
//! `astrs-fuzz` estate (a pure-Rust, env-gated harness under parallel
//! development): structured random-input generation with a committed
//! regression corpus, run deep via `ASTRS_FUZZ_ITERS` in the nightly local
//! lane, never as part of this crate's own `cargo test` path.
//!
//! # Strictness
//!
//! Three rules this crate does not bend, because each of them turns a silent
//! misinterpretation into an error:
//!
//! 1. **Trailing octets are fatal.** [`CdrReader::finish`] rejects a payload
//!    the declared type did not consume. Use
//!    [`CdrReader::finish_tolerant`] where alignment padding is genuinely
//!    expected.
//! 2. **A `boolean` is `0` or `1`.** Nothing else decodes.
//! 3. **A `string` has no interior NUL**, on the way in *or* out, so that
//!    decode-then-encode is total.

pub mod align;
pub mod bounded;
pub mod encoding;
pub mod error;
pub mod impls;
pub mod macros;
pub mod parameter_list;
pub mod reader;
pub mod size;
pub mod traits;
pub mod writer;
pub mod xcdr2;

pub use align::{ALIGN_1, ALIGN_2, ALIGN_4, ALIGN_8, align_up, is_aligned, padding_to};
pub use bounded::{BoundedSequence, BoundedString, BoundedWString};
pub use encoding::{
    CdrVersion, ENCAPSULATION_HEADER_LEN, EncapsulationHeader, EncapsulationKind,
    EncapsulationOptions, Encoding, Endianness, Representation, WCharWidth,
};
pub use error::{CdrError, CdrResult};
pub use impls::collection::{read_array, read_sequence, write_array, write_sequence};
pub use impls::string::WString;
pub use parameter_list::{
    PID_PAD, PID_SENTINEL, Parameter, ParameterId, ParameterList, SENTINEL_LEN, pid,
};
pub use reader::{
    CdrReader, MAX_ALIGNMENT_SLACK, from_bytes, from_bytes_headerless, from_bytes_tolerant,
};
pub use size::{serialized_size, serialized_size_headerless};
pub use traits::{CdrDefault, CdrDeserialize, CdrEnum, CdrSerde, CdrSerialize, CdrType};
pub use writer::{CdrWriter, to_vec, to_vec_headerless, to_vec_padded, to_vec_ros2};
pub use xcdr2::{
    DHEADER_LEN, EMHEADER_LEN, EmHeader, Extensibility, LengthCode, MEMBER_ID_MAX, MemberHeader,
};
