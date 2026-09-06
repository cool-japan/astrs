//! CDR ⇄ columnar, dispatched on a runtime type name.
//!
//! §10.5's one sentence — "the bridge node converts CDR ⇄ columnar via the
//! generated types, preserving header timestamps into HLC metadata" — has a
//! shape problem hidden in it: the generated types are *static* Rust types
//! and `message_type: sensor_msgs/msg/LaserScan` is a *string* read out of a
//! YAML file at startup. This module is the bridge between the two.
//!
//! # The dispatch table
//!
//! [`MessageCodec`] is a plain record of two function pointers plus the
//! three names a generated type carries. [`registry::lookup`] turns
//! `"sensor_msgs/msg/LaserScan"` into one, by monomorphising [`decode_plain`]
//! / [`decode_stamped`] / [`encode`] over the generated type. Each entry
//! costs one line, and `registry`'s coverage test asserts that every type
//! `astrs-ros2` declares a [`astrs_ros2::MessageType`] for has one — so a package added
//! to either generated tree cannot be silently left unbridgeable.
//!
//! # Header stamps
//!
//! A ROS message with a `std_msgs/Header` carries the instant the *sensor*
//! observed the world, which is not the instant the bridge received the
//! datagram. §14's determinism guarantees are only worth something if that
//! first instant is what the graph sees, so a stamped type's codec extracts
//! `header.stamp` and [`crate::topic`] makes it the message's HLC
//! [`astrs_wire::Metadata`] timestamp (via [`RosTime::to_hlc`], which is
//! exact — ROS time and HLC physical time are both nanoseconds since the
//! Unix epoch).
//!
//! An unstamped type, or a stamped one whose `header.stamp` is zero (what
//! an unset stamp looks like on the wire), keeps the bridge's own HLC
//! reading instead. Zero is not a plausible observation time and treating it
//! as one would date every such sample to 1970.
//!
//! # Services and actions ride the same table
//!
//! A ROS 2 service request is `[CDR header][SampleIdentity][request body]`
//! and `astrs-idl` generates the request and response as ordinary message
//! types (`example_interfaces/srv/AddTwoInts` → `AddTwoIntsRequest`,
//! `AddTwoIntsResponse`). So [`split_identity`] and [`join_identity`] peel
//! and re-attach the twenty-four correlation octets, and everything else —
//! services, and the three services plus two topics an action is — goes
//! through this same table with no second mechanism.

pub mod registry;

use astrs_cdr::CdrSerde;
use astrs_cdr::{CdrSerialize, CdrWriter, Encoding};
use astrs_data::{AstrsMessage, RecordBatch};
use astrs_ros2::service::{SAMPLE_IDENTITY_LEN, SampleIdentity};
use astrs_ros2::time::RosTime;

use crate::error::CodecError;

/// The octets a ROS 2 CDR encapsulation header occupies.
pub const ENCAPSULATION_LEN: usize = 4;

/// What one CDR sample becomes on the AstRS side.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// The columnar batch, one row, under `astrs-data`'s payload schema.
    pub batch: RecordBatch,
    /// The `header.stamp`, when the type has one and it is not zero.
    pub stamp: Option<RosTime>,
}

/// A ROS 2 message type's identity and its two conversions.
///
/// `Copy` because it is four words of pointers and constants: an endpoint
/// holds one by value rather than reaching back into a table per sample.
#[derive(Debug, Clone, Copy)]
pub struct MessageCodec {
    /// The ROS 2 spelling, `pkg/msg/Type`.
    pub ros_type_name: &'static str,
    /// The DDS spelling, `pkg::msg::dds_::Type_`, which SEDP announces.
    pub dds_type_name: &'static str,
    /// The columnar type URN (`std/ros2/v1/…`, §10.3's mechanical minting).
    pub urn: &'static str,
    /// Whether this type carries a `std_msgs/Header`.
    pub has_header: bool,
    /// CDR octets → one columnar row.
    decode_fn: fn(&[u8]) -> Result<Decoded, CodecError>,
    /// One columnar row → CDR octets.
    encode_fn: fn(&RecordBatch) -> Result<Vec<u8>, CodecError>,
}

impl MessageCodec {
    /// The codec for an unstamped generated type.
    #[must_use]
    pub fn plain<T: RosNames + CdrSerde + AstrsMessage>() -> Self {
        Self {
            ros_type_name: T::TYPE_NAME,
            dds_type_name: T::DDS_NAME,
            urn: T::URN,
            has_header: false,
            decode_fn: decode_plain::<T>,
            encode_fn: encode::<T>,
        }
    }

    /// The codec for a generated type that carries a `std_msgs/Header`.
    #[must_use]
    pub fn stamped<T: RosNames + CdrSerde + AstrsMessage + Stamped>() -> Self {
        Self {
            ros_type_name: T::TYPE_NAME,
            dds_type_name: T::DDS_NAME,
            urn: T::URN,
            has_header: true,
            decode_fn: decode_stamped::<T>,
            encode_fn: encode::<T>,
        }
    }

    /// Decode one CDR sample.
    ///
    /// # Errors
    ///
    /// [`CodecError::Cdr`] when the octets are not a sample of this type,
    /// and [`CodecError::Columnar`] when the columnar layer refuses the
    /// value (an allocation failure, in practice — the layout is generated
    /// from the same definition).
    pub fn decode(&self, bytes: &[u8]) -> Result<Decoded, CodecError> {
        (self.decode_fn)(bytes)
    }

    /// Encode one columnar row as a CDR sample.
    ///
    /// # Errors
    ///
    /// [`CodecError::Columnar`] when the batch is not one row of this
    /// type's layout, and [`CodecError::Cdr`] when the value will not
    /// serialize (a bounded field over its bound, an interior NUL in a
    /// string).
    pub fn encode(&self, batch: &RecordBatch) -> Result<Vec<u8>, CodecError> {
        (self.encode_fn)(batch)
    }
}

/// Two codecs are equal when they are for the same ROS 2 type.
///
/// Written by hand rather than derived: a derived `PartialEq` would compare
/// the two function pointers, and comparing function addresses is a
/// [documented non-answer](https://doc.rust-lang.org/nightly/core/ptr/fn.fn_addr_eq.html)
/// — the same function can have two addresses across codegen units, and two
/// functions can share one after merging. The ROS 2 type name *is* the
/// identity, and it is what the table is keyed on.
impl PartialEq for MessageCodec {
    fn eq(&self, other: &Self) -> bool {
        self.ros_type_name == other.ros_type_name
    }
}

impl Eq for MessageCodec {}

/// A type's two ROS 2 spellings.
///
/// Every generated type carries them as *inherent* constants
/// (`LaserScan::ROS_TYPE_NAME`, `LaserScan::DDS_TYPE_NAME`), which a trait
/// bound cannot name — that is exactly why `astrs-ros2` defines
/// [`MessageType`](astrs_ros2::MessageType). This crate needs the same
/// thing for one type `astrs-ros2` does *not* cover: `astrs-tf`'s
/// hand-written `tf2_msgs/msg/TFMessage`, which the orphan rule forbids
/// this crate from implementing a foreign trait for.
///
/// So the bound is local, [`registry`]'s `impl_ros_names!` derives it from
/// the inherent constants uniformly, and `/tf` bridges through exactly the
/// same path as `/scan`. The constants are spelled `TYPE_NAME`/`DDS_NAME`
/// rather than `ROS_TYPE_NAME`/`DDS_TYPE_NAME` for the reason
/// `astrs_ros2::msg`'s own docs give: an impl body that wrote
/// `const ROS_TYPE_NAME: &str = Self::ROS_TYPE_NAME;` would be reading
/// itself.
pub trait RosNames {
    /// The ROS 2 spelling: `pkg/msg/Type`.
    const TYPE_NAME: &'static str;
    /// The DDS spelling: `pkg::msg::dds_::Type_`.
    const DDS_NAME: &'static str;
}

/// A generated type whose first field is a `std_msgs/Header`.
///
/// Implemented in [`registry`] for every such type, by macro. Kept as a
/// trait rather than a `fn(&T) -> RosTime` field on the codec because the
/// codec's constructor is generic over `T` and a trait bound is what lets
/// [`MessageCodec::stamped`] refuse a type that has no header at compile
/// time.
pub trait Stamped {
    /// The `header.stamp` of this value.
    fn ros_stamp(&self) -> RosTime;
}

/// Decode a type with no header.
///
/// # Errors
///
/// As [`MessageCodec::decode`].
pub fn decode_plain<T: RosNames + CdrSerde + AstrsMessage>(
    bytes: &[u8],
) -> Result<Decoded, CodecError> {
    let value = decode_value::<T>(bytes)?;
    Ok(Decoded {
        batch: to_batch::<T>(&value)?,
        stamp: None,
    })
}

/// Decode a type with a header, keeping its stamp.
///
/// # Errors
///
/// As [`MessageCodec::decode`].
pub fn decode_stamped<T: RosNames + CdrSerde + AstrsMessage + Stamped>(
    bytes: &[u8],
) -> Result<Decoded, CodecError> {
    let value = decode_value::<T>(bytes)?;
    let stamp = value.ros_stamp();
    let batch = to_batch::<T>(&value)?;
    Ok(Decoded {
        batch,
        // A zero stamp is what an *unset* `header.stamp` looks like, and
        // dating a sample to 1970 is worse than not dating it at all.
        stamp: (!stamp.is_zero()).then_some(stamp),
    })
}

/// Encode one row of a columnar batch as a ROS 2 CDR sample.
///
/// # Errors
///
/// As [`MessageCodec::encode`].
pub fn encode<T: RosNames + CdrSerde + AstrsMessage>(
    batch: &RecordBatch,
) -> Result<Vec<u8>, CodecError> {
    let value = T::from_record_batch(batch).map_err(|source| CodecError::Columnar {
        type_name: T::TYPE_NAME,
        source,
    })?;
    astrs_cdr::to_vec(&value, Encoding::ROS2).map_err(|source| CodecError::Cdr {
        type_name: T::TYPE_NAME,
        len: 0,
        source,
    })
}

/// Deserialize one value, tolerating the trailing padding a DDS writer adds.
///
/// RTPS pads a serialized payload up to a four-octet boundary, so a strict
/// "the reader must be exhausted" check would reject perfectly good samples
/// from every stack including this one.
fn decode_value<T: RosNames + CdrSerde>(bytes: &[u8]) -> Result<T, CodecError> {
    astrs_cdr::from_bytes_tolerant::<T>(bytes).map_err(|source| CodecError::Cdr {
        type_name: T::TYPE_NAME,
        len: bytes.len(),
        source,
    })
}

/// Convert one value into its single-row columnar batch.
fn to_batch<T: RosNames + AstrsMessage>(value: &T) -> Result<RecordBatch, CodecError> {
    value
        .to_record_batch()
        .map_err(|source| CodecError::Columnar {
            type_name: T::TYPE_NAME,
            source,
        })
}

/// Split a service request or reply into its identity and a standalone CDR
/// body.
///
/// The body is returned with a fresh encapsulation header copied from the
/// original, so it decodes exactly as the same value would standalone. That
/// is sound rather than lucky: [`SAMPLE_IDENTITY_LEN`] is twenty-four, a
/// multiple of eight, so every alignment boundary inside the body falls in
/// the same place whether the body starts at CDR offset 24 or at 0.
///
/// # Errors
///
/// [`CodecError::Cdr`] when the payload is shorter than a header plus an
/// identity.
pub fn split_identity(
    payload: &[u8],
    type_name: &'static str,
) -> Result<(SampleIdentity, Vec<u8>), CodecError> {
    let identity =
        astrs_ros2::service::peek_identity(payload).map_err(|source| CodecError::Cdr {
            type_name,
            len: payload.len(),
            source,
        })?;
    let start = ENCAPSULATION_LEN.saturating_add(SAMPLE_IDENTITY_LEN);
    let tail = payload.get(start..).unwrap_or(&[]);
    let mut body = Vec::with_capacity(tail.len().saturating_add(ENCAPSULATION_LEN));
    body.extend_from_slice(payload.get(..ENCAPSULATION_LEN).unwrap_or(&[]));
    body.extend_from_slice(tail);
    Ok((identity, body))
}

/// The inverse of [`split_identity`]: put a correlation header back in front
/// of an encoded body.
///
/// The result is built through a fresh [`CdrWriter`] rather than by copying
/// `body`'s own encapsulation header, so the identity's `int64` is written
/// in the encoding the header announces instead of in whatever endianness
/// the host happens to have.
///
/// # Errors
///
/// [`CodecError::Cdr`] when `body` is not even an encapsulation header long,
/// which for a body this crate encoded cannot happen.
pub fn join_identity(
    identity: SampleIdentity,
    body: &[u8],
    type_name: &'static str,
) -> Result<Vec<u8>, CodecError> {
    let Some(payload_body) = body.get(ENCAPSULATION_LEN..) else {
        return Err(CodecError::Cdr {
            type_name,
            len: body.len(),
            source: astrs_cdr::CdrError::Truncated {
                needed: ENCAPSULATION_LEN,
                available: body.len(),
                context: "service payload encapsulation header",
            },
        });
    };
    let mut writer = CdrWriter::new(Encoding::ROS2);
    identity
        .write(&mut writer)
        .map_err(|source| CodecError::Cdr {
            type_name,
            len: body.len(),
            source,
        })?;
    writer.write_octets(payload_body);
    Ok(writer.finish())
}

/// Serialize a value that is not one of the generated message types.
///
/// Used for the handful of protocol messages the action bridge synthesizes
/// itself (a `CancelGoal` request, a `GoalStatusArray`); they are ordinary
/// generated types, but they never appear in a manifest, so they are reached
/// by Rust type rather than by name.
///
/// # Errors
///
/// [`CodecError::Cdr`] when the value will not serialize.
pub fn encode_value<T: CdrSerialize + ?Sized>(
    value: &T,
    type_name: &'static str,
) -> Result<Vec<u8>, CodecError> {
    astrs_cdr::to_vec(value, Encoding::ROS2).map_err(|source| CodecError::Cdr {
        type_name,
        len: 0,
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_idl::generated::builtin_interfaces::Time;
    use astrs_idl::generated::sensor_msgs::LaserScan;
    use astrs_idl::generated::std_msgs::{Header, String as RosString};

    use super::*;

    fn scan(sec: i32, nanosec: u32) -> LaserScan {
        LaserScan {
            header: Header {
                stamp: Time { sec, nanosec },
                frame_id: "laser".to_owned(),
            },
            angle_min: -1.5,
            angle_max: 1.5,
            angle_increment: 0.01,
            time_increment: 0.0,
            scan_time: 0.05,
            range_min: 0.1,
            range_max: 30.0,
            ranges: vec![1.0, 2.0, 3.0],
            intensities: vec![10.0, 20.0, 30.0],
        }
    }

    #[test]
    fn a_stamped_type_round_trips_through_the_codec() {
        let codec = registry::lookup("sensor_msgs/msg/LaserScan").unwrap();
        assert!(codec.has_header);
        assert_eq!(codec.dds_type_name, "sensor_msgs::msg::dds_::LaserScan_");
        assert_eq!(codec.urn, "std/ros2/v1/SensorMsgsLaserScan");

        let original = scan(1_700_000_000, 250_000_000);
        let cdr = astrs_cdr::to_vec(&original, Encoding::ROS2).unwrap();

        let decoded = codec.decode(&cdr).unwrap();
        assert_eq!(
            decoded.stamp,
            Some(RosTime::new(1_700_000_000, 250_000_000)),
            "the header stamp survives into the decoded form"
        );

        let re_encoded = codec.encode(&decoded.batch).unwrap();
        let recovered: LaserScan = astrs_cdr::from_bytes_tolerant(&re_encoded).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn an_unset_header_stamp_is_reported_as_absent() {
        let codec = registry::lookup("sensor_msgs/msg/LaserScan").unwrap();
        let cdr = astrs_cdr::to_vec(&scan(0, 0), Encoding::ROS2).unwrap();
        assert_eq!(codec.decode(&cdr).unwrap().stamp, None);
    }

    #[test]
    fn an_unstamped_type_round_trips_and_reports_no_stamp() {
        let codec = registry::lookup("std_msgs/msg/String").unwrap();
        assert!(!codec.has_header);

        let original = RosString {
            data: "hello".to_owned(),
        };
        let cdr = astrs_cdr::to_vec(&original, Encoding::ROS2).unwrap();
        let decoded = codec.decode(&cdr).unwrap();
        assert_eq!(decoded.stamp, None);

        let recovered: RosString =
            astrs_cdr::from_bytes_tolerant(&codec.encode(&decoded.batch).unwrap()).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn a_truncated_sample_is_a_typed_error_naming_the_type() {
        let codec = registry::lookup("sensor_msgs/msg/LaserScan").unwrap();
        let error = codec.decode(&[0x00, 0x01, 0x00, 0x00, 0x01]).unwrap_err();
        match error {
            CodecError::Cdr { type_name, len, .. } => {
                assert_eq!(type_name, "sensor_msgs/msg/LaserScan");
                assert_eq!(len, 5);
            }
            other => panic!("expected Cdr, got {other}"),
        }
    }

    #[test]
    fn the_identity_splice_is_the_inverse_of_the_identity_encoder() {
        let request = astrs_idl::generated::std_srvs::SetBoolRequest { data: true };
        let identity = SampleIdentity::new([7; 16], 42);
        let wire = astrs_ros2::service::encode_with_identity(identity, &request).unwrap();

        let (read_identity, body) = split_identity(&wire, "std_srvs/srv/SetBool").unwrap();
        assert_eq!(read_identity, identity);

        let codec = registry::lookup("std_srvs/srv/SetBool_Request").unwrap();
        let decoded = codec.decode(&body).unwrap();
        let re_encoded = codec.encode(&decoded.batch).unwrap();

        let rejoined = join_identity(identity, &re_encoded, "std_srvs/srv/SetBool").unwrap();
        assert_eq!(rejoined, wire, "the splice is byte-exact both ways");
    }

    #[test]
    fn joining_onto_a_stub_body_is_refused_rather_than_panicking() {
        let error = join_identity(SampleIdentity::UNKNOWN, &[0x00], "x").unwrap_err();
        assert!(matches!(error, CodecError::Cdr { len: 1, .. }));
    }

    #[test]
    fn splitting_a_payload_with_no_identity_is_refused() {
        let error = split_identity(&[0x00, 0x01, 0x00, 0x00], "x").unwrap_err();
        assert!(matches!(error, CodecError::Cdr { .. }));
    }

    #[test]
    fn a_batch_of_the_wrong_type_is_a_columnar_error() {
        let scan_codec = registry::lookup("sensor_msgs/msg/LaserScan").unwrap();
        let string_codec = registry::lookup("std_msgs/msg/String").unwrap();
        let cdr = astrs_cdr::to_vec(
            &RosString {
                data: "x".to_owned(),
            },
            Encoding::ROS2,
        )
        .unwrap();
        let decoded = string_codec.decode(&cdr).unwrap();
        let error = scan_codec.encode(&decoded.batch).unwrap_err();
        assert!(matches!(error, CodecError::Columnar { .. }), "{error}");
    }

    #[test]
    fn encoding_a_protocol_value_by_rust_type_works() {
        let bytes =
            encode_value(&Time { sec: 1, nanosec: 2 }, "builtin_interfaces/msg/Time").unwrap();
        let recovered: Time = astrs_cdr::from_bytes_tolerant(&bytes).unwrap();
        assert_eq!(recovered, Time { sec: 1, nanosec: 2 });
    }
}
