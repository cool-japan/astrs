//! Golden vectors: structs, sequences, arrays and enums, in the shapes real
//! ROS 2 message types have.
//!
//! Each vector names the `.msg` definition it models and derives the octets
//! from the OMG CDR alignment rules (CORBA 3.0 §15.3), member by member. The
//! types are declared with [`cdr_struct!`](astrs_cdr::cdr_struct), which
//! generates exactly the impls `astrs-idl` codegen will emit — so a vector
//! that passes here is a statement about generated code, not only about this
//! crate's primitives.
//!
//! # Struct rule
//!
//! OMG CDR has **no** struct-level framing under XCDR1: a struct is its
//! members, in declaration order, each at its own alignment. There is no
//! length, no member count, and no padding at the end. That is why a
//! ROS 2 message and its members share one alignment origin, and why a
//! trailing `octet` member leaves the payload at an odd length.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_cdr::{
    CdrEnum, CdrError, CdrResult, CdrType, EncapsulationKind, Encoding, cdr_struct, from_bytes,
    impl_cdr_enum, to_vec, to_vec_headerless,
};

fn le() -> Encoding {
    Encoding::new(EncapsulationKind::CdrLe)
}

fn be() -> Encoding {
    Encoding::new(EncapsulationKind::CdrBe)
}

cdr_struct! {
    /// `builtin_interfaces/msg/Time`:
    ///
    /// ```text
    /// int32 sec
    /// uint32 nanosec
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Time {
        /// Whole seconds since the epoch.
        pub sec: i32,
        /// Nanoseconds within the second.
        pub nanosec: u32,
    }
}

cdr_struct! {
    /// `std_msgs/msg/Header`:
    ///
    /// ```text
    /// builtin_interfaces/Time stamp
    /// string frame_id
    /// ```
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Header {
        /// Acquisition time.
        pub stamp: Time,
        /// Coordinate frame this data is expressed in.
        pub frame_id: String,
    }
}

cdr_struct! {
    /// `geometry_msgs/msg/Vector3`:
    ///
    /// ```text
    /// float64 x
    /// float64 y
    /// float64 z
    /// ```
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Vector3 {
        /// X component.
        pub x: f64,
        /// Y component.
        pub y: f64,
        /// Z component.
        pub z: f64,
    }
}

cdr_struct! {
    /// `sensor_msgs/msg/PointField`:
    ///
    /// ```text
    /// string name
    /// uint32 offset
    /// uint8 datatype
    /// uint32 count
    /// ```
    ///
    /// Chosen as a vector because the member order forces padding twice.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PointField {
        /// Field name.
        pub name: String,
        /// Octet offset within a point.
        pub offset: u32,
        /// Datatype code.
        pub datatype: u8,
        /// Number of elements.
        pub count: u32,
    }
}

cdr_struct! {
    /// A trimmed `sensor_msgs/msg/LaserScan`:
    ///
    /// ```text
    /// std_msgs/Header header
    /// float32 angle_min
    /// float32[] ranges
    /// ```
    #[derive(Debug, Clone, PartialEq)]
    pub struct Scan {
        /// Standard header.
        pub header: Header,
        /// Start angle of the sweep.
        pub angle_min: f32,
        /// Range readings.
        pub ranges: Vec<f32>,
    }
}

cdr_struct! {
    /// A message with a fixed array, as `geometry_msgs/PoseWithCovariance`
    /// has (`float64[36]`), shrunk to a 3x3 for a readable vector.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Covariance3 {
        /// Row-major 3x3 matrix.
        pub matrix: [f64; 9],
    }
}

cdr_struct! {
    /// A message whose only member is a sequence of structs, which is what
    /// makes `sensor_msgs/PointCloud2::fields` interesting.
    #[derive(Debug, Clone, PartialEq)]
    pub struct FieldList {
        /// The fields.
        pub fields: Vec<PointField>,
    }
}

/// An IDL enumerated type, as `.idl` interface files declare them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational.
    Info,
    /// Something is wrong.
    Warn,
    /// Something failed.
    Error,
}

impl CdrType for Severity {
    const MIN_SERIALIZED_SIZE: usize = 4;
}

impl CdrEnum for Severity {
    const TYPE_NAME: &'static str = "Severity";

    fn discriminant(self) -> u32 {
        match self {
            Self::Info => 20,
            Self::Warn => 30,
            Self::Error => 40,
        }
    }

    fn from_discriminant(value: u32) -> CdrResult<Self> {
        match value {
            20 => Ok(Self::Info),
            30 => Ok(Self::Warn),
            40 => Ok(Self::Error),
            other => Err(CdrError::UnknownEnumerator {
                type_name: Self::TYPE_NAME,
                discriminant: other,
            }),
        }
    }
}

impl_cdr_enum!(Severity);

#[test]
fn golden_builtin_interfaces_time() {
    // Two four-octet members, both already aligned, so no padding is possible
    // anywhere in this type.
    //
    //   sec     = 1_234_567_890 = 0x4996_02d2
    //   nanosec =   987_654_321 = 0x3ade_68b1
    //
    // position 0   d2 02 96 49   sec, little-endian
    // position 4   b1 68 de 3a   nanosec
    //
    // Body length 8.
    let value = Time {
        sec: 1_234_567_890,
        nanosec: 987_654_321,
    };
    assert_eq!(
        to_vec(&value, le()).expect("encode"),
        [
            0x00, 0x01, 0x00, 0x00, // CDR_LE
            0xd2, 0x02, 0x96, 0x49, //
            0xb1, 0x68, 0xde, 0x3a,
        ]
    );

    // The same value big-endian: each member reverses independently.
    assert_eq!(
        to_vec(&value, be()).expect("encode"),
        [
            0x00, 0x00, 0x00, 0x00, // CDR_BE
            0x49, 0x96, 0x02, 0xd2, //
            0x3a, 0xde, 0x68, 0xb1,
        ]
    );

    assert_eq!(
        from_bytes::<Time>(&to_vec(&value, be()).expect("encode")).expect("decode"),
        value
    );
}

#[test]
fn golden_time_with_a_negative_second() {
    // `sec` is a signed `long`; -1 is 0xffff_ffff.
    let value = Time {
        sec: -1,
        nanosec: 0,
    };
    assert_eq!(
        to_vec_headerless(&value, le()).expect("encode"),
        [0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn golden_std_msgs_header() {
    // A nested struct contributes its members directly — no framing of its
    // own — so `Header` is `sec`, `nanosec`, then the string.
    //
    //   position 0   01 00 00 00   stamp.sec = 1
    //   position 4   02 00 00 00   stamp.nanosec = 2
    //   position 8   04 00 00 00   frame_id length = 4 ("map" plus NUL)
    //   position 12  6d 61 70 00   "map\0"
    //
    // Body length 16.
    let value = Header {
        stamp: Time { sec: 1, nanosec: 2 },
        frame_id: "map".to_owned(),
    };
    let bytes = to_vec(&value, le()).expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x01, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00, //
            0x02, 0x00, 0x00, 0x00, //
            0x04, 0x00, 0x00, 0x00, //
            0x6d, 0x61, 0x70, 0x00,
        ]
    );
    assert_eq!(from_bytes::<Header>(&bytes).expect("decode"), value);
}

#[test]
fn golden_geometry_msgs_vector3() {
    // Three `double`s, each aligned to 8 and each starting on a multiple of
    // 8 already, so the encoding is 24 octets of pure payload.
    //
    //   1.0  -> 3ff0_0000_0000_0000
    //   2.0  -> 4000_0000_0000_0000
    //  -3.0  -> c008_0000_0000_0000
    let value = Vector3 {
        x: 1.0,
        y: 2.0,
        z: -3.0,
    };
    let bytes = to_vec_headerless(&value, le()).expect("encode");
    assert_eq!(bytes.len(), 24);
    assert_eq!(
        bytes,
        [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, // 1.0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, // 2.0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xc0, // -3.0
        ]
    );
}

#[test]
fn golden_sensor_msgs_point_field() {
    // The member order forces padding twice, which is what makes this a
    // useful vector:
    //
    //   position 0   02 00 00 00   name length = 2 ("x" plus NUL)
    //   position 4   78 00         "x\0"
    //   position 6   00 00         pad: `offset` is a long and needs 4
    //   position 8   00 00 00 00   offset = 0
    //   position 12  07            datatype = 7 (FLOAT32)
    //   position 13  00 00 00      pad: `count` is a long and needs 4
    //   position 16  01 00 00 00   count = 1
    //
    // Body length 20.
    let value = PointField {
        name: "x".to_owned(),
        offset: 0,
        datatype: 7,
        count: 1,
    };
    let bytes = to_vec_headerless(&value, le()).expect("encode");
    assert_eq!(
        bytes,
        [
            0x02, 0x00, 0x00, 0x00, //
            0x78, 0x00, //
            0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
            0x07, //
            0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );
    assert_eq!(bytes.len(), 20);
    assert_eq!(
        from_bytes::<PointField>(&to_vec(&value, le()).expect("encode")).expect("decode"),
        value
    );
}

#[test]
fn golden_sequence_of_longs() {
    // OMG CDR 15.3.2.5: a sequence is an `unsigned long` count followed by
    // the elements, each at its own alignment.
    //
    //   position 0   02 00 00 00   count = 2
    //   position 4   01 00 00 00   1
    //   position 8   ff ff ff ff   -1
    assert_eq!(
        to_vec_headerless(&vec![1_i32, -1], le()).expect("encode"),
        [
            0x02, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00, //
            0xff, 0xff, 0xff, 0xff,
        ]
    );

    // The empty sequence is four zero octets — a count of 0 and nothing else.
    assert_eq!(
        to_vec_headerless(&Vec::<i32>::new(), le()).expect("encode"),
        [0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn golden_sequence_of_doubles_pads_after_its_count() {
    // The count leaves position 4; a `double` needs a multiple of 8, so four
    // pad octets separate the count from the first element. This is the
    // single most commonly mis-encoded shape in hand-written CDR.
    //
    //   position 0   01 00 00 00                 count = 1
    //   position 4   00 00 00 00                 four pad octets
    //   position 8   00 00 00 00 00 00 f0 3f     1.0
    //
    // Body length 16 for a one-element sequence.
    assert_eq!(
        to_vec_headerless(&vec![1.0_f64], le()).expect("encode"),
        [
            0x01, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f,
        ]
    );
}

#[test]
fn golden_sequence_of_octets_has_no_element_padding() {
    // `uint8[]` — `sensor_msgs/Image::data`, `sensor_msgs/PointCloud2::data`.
    // One-octet elements need no alignment, so the payload is the count and
    // then the octets verbatim, however many there are.
    //
    //   position 0   03 00 00 00   count = 3
    //   position 4   0a 0b 0c      the octets
    assert_eq!(
        to_vec_headerless(&vec![0x0a_u8, 0x0b, 0x0c], le()).expect("encode"),
        [0x03, 0x00, 0x00, 0x00, 0x0a, 0x0b, 0x0c]
    );
}

#[test]
fn golden_fixed_array_has_no_count() {
    // The difference between `T[N]` and `sequence<T>`: an array carries no
    // length, because its length is part of its type.
    //
    // Nine `double`s at positions 0, 8, 16, … 64 — 72 octets, and the first
    // four octets are the first element rather than a count.
    let value = Covariance3 {
        matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    };
    let bytes = to_vec_headerless(&value, le()).expect("encode");
    assert_eq!(bytes.len(), 72);
    assert_eq!(
        &bytes[..8],
        &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f]
    );
    assert_eq!(&bytes[8..16], &[0x00; 8]);
    assert_eq!(
        from_bytes::<Covariance3>(&to_vec(&value, le()).expect("encode")).expect("decode"),
        value
    );
}

#[test]
fn golden_sequence_of_structs() {
    // Under XCDR1 a sequence of structs is just the count and the structs;
    // the element type never adds framing.
    //
    //   position 0   01 00 00 00   count = 1
    //   position 4   02 00 00 00   fields[0].name length = 2
    //   position 8   79 00         "y\0"
    //   position 10  00 00         pad before `offset`
    //   position 12  04 00 00 00   offset = 4
    //   position 16  07            datatype = 7
    //   position 17  00 00 00      pad before `count`
    //   position 20  01 00 00 00   count = 1
    //
    // Body length 24.
    let value = FieldList {
        fields: vec![PointField {
            name: "y".to_owned(),
            offset: 4,
            datatype: 7,
            count: 1,
        }],
    };
    let bytes = to_vec_headerless(&value, le()).expect("encode");
    assert_eq!(
        bytes,
        [
            0x01, 0x00, 0x00, 0x00, //
            0x02, 0x00, 0x00, 0x00, //
            0x79, 0x00, //
            0x00, 0x00, //
            0x04, 0x00, 0x00, 0x00, //
            0x07, //
            0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );
    assert_eq!(bytes.len(), 24);
    assert_eq!(
        from_bytes::<FieldList>(&to_vec(&value, le()).expect("encode")).expect("decode"),
        value
    );
}

#[test]
fn golden_trimmed_laser_scan() {
    // The composite vector: a nested header with a string, a float that has
    // to re-align after it, and a sequence.
    //
    //   position 0   01 00 00 00               header.stamp.sec = 1
    //   position 4   02 00 00 00               header.stamp.nanosec = 2
    //   position 8   06 00 00 00               frame_id length = 6
    //   position 12  6c 61 73 65 72 00         "laser\0"
    //   position 18  00 00                     pad: angle_min is a float
    //   position 20  00 00 80 bf               angle_min = -1.0 (0xbf80_0000)
    //   position 24  02 00 00 00               ranges count = 2
    //   position 28  00 00 80 3f               1.0 (0x3f80_0000)
    //   position 32  00 00 00 40               2.0 (0x4000_0000)
    //
    // Body length 36.
    let value = Scan {
        header: Header {
            stamp: Time { sec: 1, nanosec: 2 },
            frame_id: "laser".to_owned(),
        },
        angle_min: -1.0,
        ranges: vec![1.0, 2.0],
    };
    let bytes = to_vec_headerless(&value, le()).expect("encode");
    assert_eq!(
        bytes,
        [
            0x01, 0x00, 0x00, 0x00, //
            0x02, 0x00, 0x00, 0x00, //
            0x06, 0x00, 0x00, 0x00, //
            0x6c, 0x61, 0x73, 0x65, 0x72, 0x00, //
            0x00, 0x00, //
            0x00, 0x00, 0x80, 0xbf, //
            0x02, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x80, 0x3f, //
            0x00, 0x00, 0x00, 0x40,
        ]
    );
    assert_eq!(bytes.len(), 36);
    assert_eq!(
        from_bytes::<Scan>(&to_vec(&value, le()).expect("encode")).expect("decode"),
        value
    );
}

#[test]
fn golden_enumerated_type_is_an_unsigned_long() {
    // OMG DDS-XTypes 1.3 §7.3.1.2.1.5: an enumerated type has a default
    // `@bit_bound` of 32 and travels as the corresponding unsigned integer.
    //
    //   position 0   1e 00 00 00   Warn == 30
    assert_eq!(
        to_vec_headerless(&Severity::Warn, le()).expect("encode"),
        [0x1e, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        to_vec_headerless(&Severity::Warn, be()).expect("encode"),
        [0x00, 0x00, 0x00, 0x1e]
    );

    // A discriminant no enumerator claims is refused, which is what makes the
    // round trip total: nothing decodes to a value that cannot be re-encoded.
    let unknown = [0x00, 0x01, 0x00, 0x00, 0x63, 0x00, 0x00, 0x00];
    assert_eq!(
        from_bytes::<Severity>(&unknown),
        Err(CdrError::UnknownEnumerator {
            type_name: "Severity",
            discriminant: 99,
        })
    );
}

#[test]
fn golden_nested_sequences() {
    // `string[]` — a sequence whose elements each carry their own length.
    //
    //   position 0   02 00 00 00   count = 2
    //   position 4   03 00 00 00   "ab" length = 3
    //   position 8   61 62 00      "ab\0"
    //   position 11  00            pad before the next length
    //   position 12  02 00 00 00   "c" length = 2
    //   position 16  63 00         "c\0"
    //
    // Body length 18.
    let value = vec!["ab".to_owned(), "c".to_owned()];
    let bytes = to_vec_headerless(&value, le()).expect("encode");
    assert_eq!(
        bytes,
        [
            0x02, 0x00, 0x00, 0x00, //
            0x03, 0x00, 0x00, 0x00, //
            0x61, 0x62, 0x00, //
            0x00, //
            0x02, 0x00, 0x00, 0x00, //
            0x63, 0x00,
        ]
    );
}

#[test]
fn golden_struct_min_serialized_sizes_are_true_lower_bounds() {
    // The generated `MIN_SERIALIZED_SIZE` is what the reader multiplies a
    // sequence count by before allocating, so it must never exceed what a
    // value can actually occupy. Check each against its smallest encoding.
    assert!(
        Time::MIN_SERIALIZED_SIZE
            <= to_vec_headerless(&Time { sec: 0, nanosec: 0 }, le())
                .expect("encode")
                .len()
    );
    assert!(
        Header::MIN_SERIALIZED_SIZE
            <= to_vec_headerless(
                &Header {
                    stamp: Time { sec: 0, nanosec: 0 },
                    frame_id: String::new(),
                },
                le()
            )
            .expect("encode")
            .len()
    );
    assert!(
        PointField::MIN_SERIALIZED_SIZE
            <= to_vec_headerless(
                &PointField {
                    name: String::new(),
                    offset: 0,
                    datatype: 0,
                    count: 0,
                },
                le()
            )
            .expect("encode")
            .len()
    );
}

#[test]
fn golden_a_short_payload_is_refused_rather_than_zero_filled() {
    // A `Header` needs at least 13 octets of body. Cut it off inside the
    // frame_id and the decode fails instead of inventing an empty string.
    let full = to_vec(
        &Header {
            stamp: Time { sec: 1, nanosec: 2 },
            frame_id: "map".to_owned(),
        },
        le(),
    )
    .expect("encode");
    for cut in 4..full.len() {
        assert!(
            from_bytes::<Header>(&full[..cut]).is_err(),
            "a {cut}-octet prefix must not decode"
        );
    }
    assert!(from_bytes::<Header>(&full).is_ok());
}
