# astrs-cdr

CDR and XCDR1 serialization (plus XCDR2 read) with ROS 2 alignment rules.

The byte-level foundation of the AstRS ROS 2 pillar. Everything a DDS
participant puts on the wire — a topic sample, a service request, an SPDP
discovery announcement — is CDR, and this crate is the only place in AstRS
that knows how those octets are laid out: XCDR1 encode and decode
(big/little-endian, OMG CDR/CORBA 3.0 §15.3 alignment), XCDR2 read for
XTypes-annotated types (and write, so the read path is property-tested
against an encoder rather than hand-typed octets alone), encapsulation
headers (`CDR_BE`/`CDR_LE`, `PL_CDR_BE`/`PL_CDR_LE`, the six XCDR2
identifiers), a `CdrSerde` trait pair (`CdrSerialize`/`CdrDeserialize`)
targeted by `astrs-idl` codegen, and `ParameterList` — the `PL_CDR`
representation RTPS discovery is built from. Both the writer and the
reader track the CDR *alignment origin* (the first octet after the
four-octet encapsulation header) explicitly, so a nested scope can restate
it without arithmetic at the call site. No fixture in this crate's golden
vectors was captured from a C/C++ DDS stack — each is derived octet by
octet from the OMG specifications, with a worked derivation in the test
itself.

## Example

```rust
use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

/// `geometry_msgs/msg/Point`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point { x: f64, y: f64, z: f64 }

astrs_cdr::cdr_struct_impls!(Point { x: f64, y: f64, z: f64 });

let point = Point { x: 1.0, y: 2.0, z: 3.0 };
let bytes = astrs_cdr::to_vec_ros2(&point)?;
assert_eq!(bytes.len(), 4 + 24);
assert_eq!(&bytes[..4], &[0x00, 0x01, 0x00, 0x00]); // CDR_LE
assert_eq!(astrs_cdr::from_bytes::<Point>(&bytes)?, point);
# Ok::<(), astrs_cdr::CdrError>(())
```

See the [crate documentation](https://docs.rs/astrs-cdr) for the CDR design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
