//! [`CdrWriter`] — the encode half, with alignment tracked against the
//! encapsulation origin.
//!
//! A writer owns an output buffer and three pieces of state: the
//! [`Encoding`], the buffer index of the **alignment origin**, and (when a
//! header was emitted) the index of the encapsulation header so its options
//! field can be patched at the end.
//!
//! Every aligned write goes through [`CdrWriter::align`], which converts the
//! member's natural alignment through [`Encoding::align_for`] — the one place
//! the XCDR2 cap of 4 is applied — and pushes zero octets. Callers never
//! write padding themselves.
//!
//! ```
//! use astrs_cdr::{CdrWriter, Encoding};
//!
//! let mut writer = CdrWriter::new(Encoding::ROS2);
//! writer.write_u8(0x01)?;   // stream position 0, no padding
//! writer.write_u32(0x0a0b_0c0d)?; // needs position 4: 3 pad octets first
//! assert_eq!(
//!     writer.finish(),
//!     [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0d, 0x0c, 0x0b, 0x0a]
//! );
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```

use crate::align::{ALIGN_1, ALIGN_2, ALIGN_4, ALIGN_8, padding_to};
use crate::encoding::{
    ENCAPSULATION_HEADER_LEN, EncapsulationHeader, EncapsulationKind, EncapsulationOptions,
    Encoding, Endianness, WCharWidth,
};
use crate::error::{CdrError, CdrResult};
use crate::traits::{CdrSerialize, CdrType};
use crate::xcdr2::{DHEADER_LEN, EmHeader, LengthCode};

/// Where a writer puts the octets it produces.
///
/// The counting variant lets [`crate::serialized_size`] run the very same
/// [`CdrSerialize`] impls that produce bytes, so a size can never disagree
/// with the encoding it predicts.
#[derive(Debug)]
enum Sink {
    Buffer(Vec<u8>),
    Counter(usize),
}

impl Sink {
    const fn len(&self) -> usize {
        match self {
            Self::Buffer(buffer) => buffer.len(),
            Self::Counter(count) => *count,
        }
    }

    fn push_slice(&mut self, octets: &[u8]) {
        match self {
            Self::Buffer(buffer) => buffer.extend_from_slice(octets),
            Self::Counter(count) => *count += octets.len(),
        }
    }

    fn push_zeros(&mut self, count: usize) {
        match self {
            Self::Buffer(buffer) => buffer.resize(buffer.len() + count, 0),
            Self::Counter(total) => *total += count,
        }
    }

    /// Overwrite four octets already emitted. A counting sink has nothing to
    /// patch, which is exactly right: back-filling a DHEADER changes no
    /// length.
    fn patch4(&mut self, at: usize, octets: [u8; 4]) {
        if let Self::Buffer(buffer) = self
            && let Some(slot) = buffer.get_mut(at..at + 4)
        {
            slot.copy_from_slice(&octets);
        }
    }

    fn patch2(&mut self, at: usize, octets: [u8; 2]) {
        if let Self::Buffer(buffer) = self
            && let Some(slot) = buffer.get_mut(at..at + 2)
        {
            slot.copy_from_slice(&octets);
        }
    }

    fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Buffer(buffer) => buffer,
            Self::Counter(_) => Vec::new(),
        }
    }
}

/// A CDR encoder.
///
/// See the [module documentation](self) for the alignment model.
#[derive(Debug)]
pub struct CdrWriter {
    sink: Sink,
    encoding: Encoding,
    /// Buffer index of stream position 0.
    origin: usize,
    /// Buffer index of the encapsulation header, when one was written.
    header_at: Option<usize>,
}

impl CdrWriter {
    /// A writer that emits an encapsulation header for `encoding` and then
    /// the body.
    #[must_use]
    pub fn new(encoding: Encoding) -> Self {
        Self::with_capacity(encoding, ENCAPSULATION_HEADER_LEN)
    }

    /// [`CdrWriter::new`] with the output buffer pre-reserved.
    #[must_use]
    pub fn with_capacity(encoding: Encoding, capacity: usize) -> Self {
        let mut writer = Self {
            sink: Sink::Buffer(Vec::with_capacity(capacity)),
            encoding,
            origin: 0,
            header_at: None,
        };
        writer.write_encapsulation_header();
        writer
    }

    /// A writer that appends a fresh encapsulation header, and then the body,
    /// to the end of `buffer`.
    ///
    /// This is how `astrs-rtps` builds a `DATA` submessage in place: the RTPS
    /// header and submessage header are already in `buffer`, and the CDR
    /// alignment origin must start over at the serialized payload rather than
    /// counting from the front of the datagram.
    #[must_use]
    pub fn append_to(buffer: Vec<u8>, encoding: Encoding) -> Self {
        let mut writer = Self {
            sink: Sink::Buffer(buffer),
            encoding,
            origin: 0,
            header_at: None,
        };
        writer.write_encapsulation_header();
        writer
    }

    /// A writer with **no** encapsulation header: stream position 0 is buffer
    /// index 0.
    ///
    /// Used for the value of a `PL_CDR` parameter, for a CDR key blob, and by
    /// tests that want to assert body octets without the four-octet prefix.
    #[must_use]
    pub fn headerless(encoding: Encoding) -> Self {
        Self {
            sink: Sink::Buffer(Vec::new()),
            encoding,
            origin: 0,
            header_at: None,
        }
    }

    /// A writer that counts octets instead of producing them.
    pub(crate) fn measuring(encoding: Encoding, with_header: bool) -> Self {
        let mut writer = Self {
            sink: Sink::Counter(0),
            encoding,
            origin: 0,
            header_at: None,
        };
        if with_header {
            writer.write_encapsulation_header();
        }
        writer
    }

    fn write_encapsulation_header(&mut self) {
        let at = self.sink.len();
        self.sink
            .push_slice(&EncapsulationHeader::new(self.encoding.kind()).to_bytes());
        self.header_at = Some(at);
        self.origin = self.sink.len();
    }

    /// The encoding this writer produces.
    #[must_use]
    pub const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Stream position: octets written since the alignment origin.
    ///
    /// This is the number every alignment rule is stated against, *not* the
    /// buffer length.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.sink.len() - self.origin
    }

    /// Total octets in the output so far, including any encapsulation header
    /// and anything the buffer already held when the writer was created.
    #[must_use]
    pub const fn written(&self) -> usize {
        self.sink.len()
    }

    // -- primitives ---------------------------------------------------------

    /// Insert padding so the next octet lands on a `natural`-octet boundary.
    ///
    /// `natural` is the type's XCDR1 alignment; the XCDR2 cap is applied
    /// here, so callers pass 8 for a `double` in both versions.
    pub fn align(&mut self, natural: usize) {
        let pad = padding_to(self.position(), self.encoding.align_for(natural));
        if pad > 0 {
            self.sink.push_zeros(pad);
        }
    }

    /// Append octets with no alignment and no length prefix.
    ///
    /// The raw escape hatch: an IDL `octet` array, a pre-serialized
    /// parameter value, a key hash.
    pub fn write_octets(&mut self, octets: &[u8]) {
        self.sink.push_slice(octets);
    }

    /// Append `count` zero octets with no alignment.
    pub fn write_zeros(&mut self, count: usize) {
        self.sink.push_zeros(count);
    }

    /// Write an IDL `octet` / `uint8`.
    pub fn write_u8(&mut self, value: u8) -> CdrResult<()> {
        self.sink.push_slice(&[value]);
        Ok(())
    }

    /// Write an IDL `int8`.
    pub fn write_i8(&mut self, value: i8) -> CdrResult<()> {
        self.write_u8(value as u8)
    }

    /// Write an IDL `char`, which CDR defines as a single octet.
    pub fn write_char8(&mut self, value: u8) -> CdrResult<()> {
        self.write_u8(value)
    }

    /// Write an IDL `boolean` as the octet `0` or `1`.
    ///
    /// OMG CDR 15.3.1 admits exactly those two encodings.
    pub fn write_bool(&mut self, value: bool) -> CdrResult<()> {
        self.write_u8(u8::from(value))
    }

    /// Write an IDL `unsigned short` / `uint16`, aligned to 2.
    pub fn write_u16(&mut self, value: u16) -> CdrResult<()> {
        self.align(ALIGN_2);
        let octets = match self.encoding.endianness() {
            Endianness::Big => value.to_be_bytes(),
            Endianness::Little => value.to_le_bytes(),
        };
        self.sink.push_slice(&octets);
        Ok(())
    }

    /// Write an IDL `short` / `int16`, aligned to 2.
    pub fn write_i16(&mut self, value: i16) -> CdrResult<()> {
        self.write_u16(value as u16)
    }

    /// Write an IDL `unsigned long` / `uint32`, aligned to 4.
    pub fn write_u32(&mut self, value: u32) -> CdrResult<()> {
        self.align(ALIGN_4);
        self.write_u32_unaligned(value);
        Ok(())
    }

    /// Write four octets in stream order with no alignment step.
    fn write_u32_unaligned(&mut self, value: u32) {
        let octets = self.u32_octets(value);
        self.sink.push_slice(&octets);
    }

    fn u32_octets(&self, value: u32) -> [u8; 4] {
        match self.encoding.endianness() {
            Endianness::Big => value.to_be_bytes(),
            Endianness::Little => value.to_le_bytes(),
        }
    }

    /// Write an IDL `long` / `int32`, aligned to 4.
    pub fn write_i32(&mut self, value: i32) -> CdrResult<()> {
        self.write_u32(value as u32)
    }

    /// Write an IDL `unsigned long long` / `uint64`.
    ///
    /// Aligned to 8 under XCDR1 and to 4 under XCDR2 (OMG DDS-XTypes 1.3
    /// §7.4.3.4.1).
    pub fn write_u64(&mut self, value: u64) -> CdrResult<()> {
        self.align(ALIGN_8);
        let octets = match self.encoding.endianness() {
            Endianness::Big => value.to_be_bytes(),
            Endianness::Little => value.to_le_bytes(),
        };
        self.sink.push_slice(&octets);
        Ok(())
    }

    /// Write an IDL `long long` / `int64`.
    pub fn write_i64(&mut self, value: i64) -> CdrResult<()> {
        self.write_u64(value as u64)
    }

    /// Write an IDL `float`, aligned to 4.
    ///
    /// The IEEE-754 bit pattern is preserved exactly, signalling NaN payloads
    /// included: CDR transmits the bits, it does not normalise them.
    pub fn write_f32(&mut self, value: f32) -> CdrResult<()> {
        self.write_u32(value.to_bits())
    }

    /// Write an IDL `double`.
    ///
    /// Aligned to 8 under XCDR1, 4 under XCDR2. The bit pattern is preserved
    /// exactly.
    pub fn write_f64(&mut self, value: f64) -> CdrResult<()> {
        self.write_u64(value.to_bits())
    }

    /// Write an IDL `wchar`: one UTF-16 code unit, two octets by default.
    ///
    /// See [`WCharWidth`] for the four-octet compatibility mode.
    pub fn write_wchar(&mut self, value: u16) -> CdrResult<()> {
        match self.encoding.wchar_width() {
            WCharWidth::Two => self.write_u16(value),
            WCharWidth::Four => self.write_u32(u32::from(value)),
        }
    }

    // -- lengths ------------------------------------------------------------

    /// Write a four-octet length prefix (sequence length, string length,
    /// DHEADER), aligned to 4.
    pub fn write_length(&mut self, value: u32) -> CdrResult<()> {
        self.write_u32(value)
    }

    /// Write a sequence length, rejecting anything a CDR `unsigned long`
    /// cannot express.
    ///
    /// # Errors
    ///
    /// [`CdrError::SequenceTooLong`] when `len` exceeds [`u32::MAX`].
    pub fn write_sequence_len(&mut self, len: usize) -> CdrResult<()> {
        let len = u32::try_from(len).map_err(|_| CdrError::SequenceTooLong {
            length: len as u64,
            maximum: u64::from(u32::MAX),
        })?;
        self.write_length(len)
    }

    // -- strings ------------------------------------------------------------

    /// Write an IDL `string`: a four-octet length that **includes** the
    /// terminating NUL, the UTF-8 body, then the NUL.
    ///
    /// The empty string is therefore four octets of length `1` followed by a
    /// single zero octet — never a length of `0`.
    ///
    /// # Errors
    ///
    /// - [`CdrError::InteriorNul`] when `value` contains a NUL octet. OMG CDR
    ///   strings are C strings; a Rust `String` is not, so this is checked
    ///   rather than assumed.
    /// - [`CdrError::SequenceTooLong`] when the body plus its terminator does
    ///   not fit a CDR `unsigned long`.
    pub fn write_str(&mut self, value: &str) -> CdrResult<()> {
        let body = value.as_bytes();
        if let Some(index) = body.iter().position(|octet| *octet == 0) {
            return Err(CdrError::InteriorNul { index });
        }
        let with_nul = body
            .len()
            .checked_add(1)
            .ok_or(CdrError::SizeOverflow("measuring a CDR string"))?;
        self.write_sequence_len(with_nul)?;
        self.sink.push_slice(body);
        self.sink.push_slice(&[0]);
        Ok(())
    }

    /// Write an IDL `wstring`: a four-octet count of `wchar`s — **not**
    /// octets — followed by the code units, with **no** terminator.
    ///
    /// This is the DDS rule (OMG DDS-XTypes 1.3 §7.4.3.5.1), and it differs
    /// from the plain `string` rule in both respects. The empty `wstring` is
    /// four octets of zero and nothing else.
    ///
    /// # Errors
    ///
    /// [`CdrError::SequenceTooLong`] when the code-unit count does not fit a
    /// CDR `unsigned long`.
    pub fn write_wstr(&mut self, units: &[u16]) -> CdrResult<()> {
        self.write_sequence_len(units.len())?;
        for unit in units {
            self.write_wchar(*unit)?;
        }
        Ok(())
    }

    // -- collections --------------------------------------------------------

    /// Write a `sequence<octet>`: the length, then the octets verbatim.
    ///
    /// Single-octet elements need no per-element alignment, so this is the
    /// fast path `sensor_msgs/Image::data` takes.
    ///
    /// # Errors
    ///
    /// [`CdrError::SequenceTooLong`] when the length does not fit a CDR
    /// `unsigned long`.
    pub fn write_octet_sequence(&mut self, octets: &[u8]) -> CdrResult<()> {
        self.write_sequence_len(octets.len())?;
        self.sink.push_slice(octets);
        Ok(())
    }

    /// Serialize a value through its [`CdrSerialize`] impl.
    ///
    /// # Errors
    ///
    /// Whatever the value's impl returns.
    pub fn serialize<T: CdrSerialize + ?Sized>(&mut self, value: &T) -> CdrResult<()> {
        value.serialize(self)
    }

    // -- scopes -------------------------------------------------------------

    /// Run `body` with the alignment origin moved to the current position.
    ///
    /// Inside the closure, [`CdrWriter::position`] restarts at zero, so a
    /// nested object aligns as though it were alone in a buffer. This is what
    /// a `PL_CDR` parameter value needs: each value is a CDR stream in its
    /// own right, and because every parameter starts on a four-octet boundary
    /// of the enclosing stream the two readings agree for every alignment up
    /// to 4.
    ///
    /// # Errors
    ///
    /// Whatever `body` returns. The origin is restored either way.
    pub fn scoped_origin<R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> CdrResult<R>,
    ) -> CdrResult<R> {
        let previous = self.origin;
        self.origin = self.sink.len();
        let result = body(self);
        self.origin = previous;
        result
    }

    /// Run `body` inside an XCDR2 DHEADER scope.
    ///
    /// Reserves a four-octet, four-aligned slot, runs `body`, then patches
    /// the slot with the number of octets `body` produced. The alignment
    /// origin is **not** moved: a DHEADER delimits, it does not restart the
    /// stream.
    ///
    /// # Errors
    ///
    /// Whatever `body` returns, or [`CdrError::SizeOverflow`] if the body is
    /// longer than a CDR `unsigned long` can express.
    pub fn delimited<R>(&mut self, body: impl FnOnce(&mut Self) -> CdrResult<R>) -> CdrResult<R> {
        self.align(ALIGN_4);
        let slot = self.sink.len();
        self.sink.push_zeros(DHEADER_LEN);
        let start = self.sink.len();
        let result = body(self)?;
        let len = self.sink.len() - start;
        let len = u32::try_from(len).map_err(|_| CdrError::SizeOverflow("writing a DHEADER"))?;
        let octets = self.u32_octets(len);
        self.sink.patch4(slot, octets);
        Ok(result)
    }

    /// Write a constructed type's body, applying the DHEADER its
    /// extensibility calls for.
    ///
    /// Generated `serialize` impls wrap their member writes in this, so one
    /// impl serves XCDR1 (never delimited) and XCDR2 (delimited for
    /// `@appendable` and `@mutable`).
    ///
    /// # The identifier is the caller's responsibility
    ///
    /// This consults `T`'s own extensibility, **not** the stream's
    /// encapsulation kind, and that is deliberate: a nested `@appendable`
    /// member inside a `@final` top-level type still needs its DHEADER, and
    /// only the member's type knows that. What the kind decides is the
    /// *top-level* announcement — `PLAIN_CDR2` for a final type,
    /// `DELIMIT_CDR` for an appendable one, `PL_CDR2` for a mutable one — and
    /// choosing it correctly is the caller's job.
    /// [`Encoding::for_extensibility`] does the mapping:
    ///
    /// ```
    /// use astrs_cdr::{CdrType, Encoding, EncapsulationKind};
    ///
    /// astrs_cdr::cdr_struct! {
    ///     #[derive(Debug, PartialEq)]
    ///     pub struct Reading: Appendable {
    ///         pub value: f64,
    ///     }
    /// }
    ///
    /// let encoding = Encoding::new(EncapsulationKind::Cdr2Le)
    ///     .for_extensibility(Reading::EXTENSIBILITY);
    /// assert_eq!(encoding.kind(), EncapsulationKind::DCdr2Le);
    /// let bytes = astrs_cdr::to_vec(&Reading { value: 1.0 }, encoding)?;
    /// assert_eq!(&bytes[..4], &[0x00, 0x09, 0x00, 0x00]);
    /// # Ok::<(), astrs_cdr::CdrError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Whatever `body` returns.
    pub fn write_struct<T, R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> CdrResult<R>,
    ) -> CdrResult<R>
    where
        T: CdrType + ?Sized,
    {
        if self.encoding.is_v2() && T::EXTENSIBILITY.has_dheader() {
            self.delimited(body)
        } else {
            body(self)
        }
    }

    /// Write an XCDR2 EMHEADER word (and nothing else).
    ///
    /// # Errors
    ///
    /// [`CdrError::UnsupportedEncapsulation`] when the stream is XCDR1, which
    /// has no EMHEADER.
    pub fn write_emheader(&mut self, header: EmHeader) -> CdrResult<()> {
        if !self.encoding.is_v2() {
            return Err(CdrError::UnsupportedEncapsulation(
                "an EMHEADER requires an XCDR2 stream",
            ));
        }
        self.write_u32(header.to_bits())
    }

    /// Write one member of an XCDR2 mutable struct.
    ///
    /// Emits an EMHEADER with length code
    /// [`LengthCode::NextIntBytes`] and back-fills the NEXTINT with the
    /// member's serialized length once `body` has run. That length code is
    /// legal for every member type, so no analysis of the member is needed;
    /// [`CdrWriter::write_member_sized`] is the narrower, cheaper form for
    /// members of a known fixed width.
    ///
    /// # Errors
    ///
    /// [`CdrError::UnsupportedEncapsulation`] on an XCDR1 stream,
    /// [`CdrError::MemberIdOutOfRange`] for an id above 2^28-1, or whatever
    /// `body` returns.
    pub fn write_member<R>(
        &mut self,
        member_id: u32,
        must_understand: bool,
        body: impl FnOnce(&mut Self) -> CdrResult<R>,
    ) -> CdrResult<R> {
        let header = EmHeader::new(member_id, LengthCode::NextIntBytes, must_understand)?;
        self.write_emheader(header)?;
        self.align(ALIGN_4);
        let slot = self.sink.len();
        self.sink.push_zeros(DHEADER_LEN);
        let start = self.sink.len();
        let result = body(self)?;
        let len = self.sink.len() - start;
        let len = u32::try_from(len).map_err(|_| CdrError::SizeOverflow("writing a NEXTINT"))?;
        let octets = self.u32_octets(len);
        self.sink.patch4(slot, octets);
        Ok(result)
    }

    /// Write one member of an XCDR2 mutable struct using a fixed-width length
    /// code, which costs no NEXTINT.
    ///
    /// `code` must be one of [`LengthCode::Bytes1`], [`LengthCode::Bytes2`],
    /// [`LengthCode::Bytes4`] or [`LengthCode::Bytes8`], and `body` must
    /// write exactly that many octets — both are checked.
    ///
    /// # Errors
    ///
    /// [`CdrError::BadEmHeader`] when `code` is not a fixed-width code or
    /// when `body` wrote the wrong number of octets, plus the errors of
    /// [`CdrWriter::write_member`].
    pub fn write_member_sized<R>(
        &mut self,
        member_id: u32,
        must_understand: bool,
        code: LengthCode,
        body: impl FnOnce(&mut Self) -> CdrResult<R>,
    ) -> CdrResult<R> {
        let Some(width) = code.fixed_width() else {
            return Err(CdrError::BadEmHeader(
                "write_member_sized needs a fixed-width length code",
            ));
        };
        let header = EmHeader::new(member_id, code, must_understand)?;
        self.write_emheader(header)?;
        let start = self.sink.len();
        let result = body(self)?;
        if self.sink.len() - start != width {
            return Err(CdrError::BadEmHeader(
                "member body length disagrees with its fixed length code",
            ));
        }
        Ok(result)
    }

    // -- finishing ----------------------------------------------------------

    /// Consume the writer and return the octets, with no trailing padding.
    ///
    /// The result decodes cleanly under [`crate::CdrReader::finish`], which
    /// rejects trailing octets.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.sink.into_vec()
    }

    /// Consume the writer, padding the body to a four-octet multiple.
    ///
    /// RTPS carries a serialized payload whose length it prefers to be a
    /// multiple of four. Under XCDR2 the pad count is recorded in the
    /// encapsulation options, so a reader recovers the exact body length; a
    /// header-less writer, or an XCDR1 stream, has nowhere to record it, and
    /// the reader must then be told out of band or use
    /// [`crate::CdrReader::finish_tolerant`].
    #[must_use]
    pub fn finish_padded(mut self) -> Vec<u8> {
        let pad = padding_to(self.position(), ALIGN_4);
        if pad > 0 {
            self.sink.push_zeros(pad);
            if self.encoding.is_v2()
                && let Some(at) = self.header_at
            {
                let options = EncapsulationOptions::NONE.with_padding(pad as u8);
                self.sink.patch2(at + 2, options.bits().to_be_bytes());
            }
        }
        self.sink.into_vec()
    }

    /// Borrow the octets produced so far.
    ///
    /// Returns an empty slice for a counting writer.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        match &self.sink {
            Sink::Buffer(buffer) => buffer.as_slice(),
            Sink::Counter(_) => &[],
        }
    }
}

/// Serialize `value` into a fresh buffer, encapsulation header included.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns.
pub fn to_vec<T: CdrSerialize + ?Sized>(value: &T, encoding: Encoding) -> CdrResult<Vec<u8>> {
    let mut writer = CdrWriter::new(encoding);
    writer.serialize(value)?;
    Ok(writer.finish())
}

/// Serialize `value` with no encapsulation header.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns.
pub fn to_vec_headerless<T: CdrSerialize + ?Sized>(
    value: &T,
    encoding: Encoding,
) -> CdrResult<Vec<u8>> {
    let mut writer = CdrWriter::headerless(encoding);
    writer.serialize(value)?;
    Ok(writer.finish())
}

/// Serialize `value` as `CDR_LE`, the ROS 2 default for topic data.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns.
pub fn to_vec_ros2<T: CdrSerialize + ?Sized>(value: &T) -> CdrResult<Vec<u8>> {
    to_vec(value, Encoding::ROS2)
}

/// Serialize `value` and pad the payload to a four-octet multiple, the shape
/// RTPS prefers for a `SerializedPayload`.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns.
pub fn to_vec_padded<T: CdrSerialize + ?Sized>(
    value: &T,
    encoding: Encoding,
) -> CdrResult<Vec<u8>> {
    let mut writer = CdrWriter::new(encoding);
    writer.serialize(value)?;
    Ok(writer.finish_padded())
}

/// The encapsulation kinds a plain (non-parameter-list) value may be written
/// with.
///
/// Handing a `PL_CDR` kind to [`to_vec`] is legal — the value is simply
/// written with plain rules under a parameter-list identifier, which is what
/// a caller building the *inside* of a discovery sample wants — but it is
/// rarely what an application means, so this predicate exists for callers
/// that want to check.
#[must_use]
pub const fn is_plain_kind(kind: EncapsulationKind) -> bool {
    !kind.is_parameter_list()
}

/// Alignment of the smallest CDR primitive, re-exported so callers can spell
/// [`CdrWriter::align`] arguments without importing [`crate::align`].
pub const ALIGN_OCTET: usize = ALIGN_1;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::EncapsulationKind;

    fn body(encoding: Encoding, write: impl FnOnce(&mut CdrWriter) -> CdrResult<()>) -> Vec<u8> {
        let mut writer = CdrWriter::headerless(encoding);
        write(&mut writer).expect("write succeeds");
        writer.finish()
    }

    #[test]
    fn header_is_four_octets_and_sets_the_origin() {
        let writer = CdrWriter::new(Encoding::ROS2);
        assert_eq!(writer.written(), 4);
        assert_eq!(writer.position(), 0);
        assert_eq!(writer.as_slice(), &[0x00, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn headerless_writer_starts_at_position_zero_with_no_octets() {
        let writer = CdrWriter::headerless(Encoding::ROS2);
        assert_eq!(writer.written(), 0);
        assert_eq!(writer.position(), 0);
    }

    #[test]
    fn append_to_restarts_the_origin_after_the_new_header() {
        let mut writer = CdrWriter::append_to(vec![0xaa; 5], Encoding::ROS2);
        assert_eq!(writer.position(), 0);
        assert_eq!(writer.written(), 9);
        writer.write_u32(1).expect("write");
        // No padding: the origin, not the buffer, is what alignment counts
        // from, and position was 0.
        assert_eq!(writer.written(), 13);
        assert_eq!(
            writer.finish(),
            [
                0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x00, 0x01, 0x00, 0x00, 1, 0, 0, 0
            ]
        );
    }

    #[test]
    fn little_and_big_endian_bodies_are_byte_reversed() {
        let le = body(Encoding::new(EncapsulationKind::CdrLe), |w| {
            w.write_u32(0x0a0b_0c0d)
        });
        let be = body(Encoding::new(EncapsulationKind::CdrBe), |w| {
            w.write_u32(0x0a0b_0c0d)
        });
        assert_eq!(le, [0x0d, 0x0c, 0x0b, 0x0a]);
        assert_eq!(be, [0x0a, 0x0b, 0x0c, 0x0d]);
    }

    #[test]
    fn alignment_padding_is_zero_filled() {
        let octets = body(Encoding::ROS2, |w| {
            w.write_u8(0xff)?;
            w.write_u64(0x0102_0304_0506_0708)
        });
        // position 1 -> 7 pad octets -> the u64 starts at position 8.
        assert_eq!(octets.len(), 16);
        assert_eq!(&octets[..8], &[0xff, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &octets[8..],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
    }

    #[test]
    fn xcdr2_caps_eight_octet_alignment_at_four() {
        let v1 = body(Encoding::new(EncapsulationKind::CdrLe), |w| {
            w.write_u8(0xff)?;
            w.write_f64(1.0)
        });
        let v2 = body(Encoding::new(EncapsulationKind::Cdr2Le), |w| {
            w.write_u8(0xff)?;
            w.write_f64(1.0)
        });
        assert_eq!(v1.len(), 16, "XCDR1 pads to position 8");
        assert_eq!(v2.len(), 12, "XCDR2 pads only to position 4");
        assert_eq!(&v2[..4], &[0xff, 0, 0, 0]);
    }

    #[test]
    fn string_length_includes_the_terminator() {
        let octets = body(Encoding::ROS2, |w| w.write_str("hi"));
        assert_eq!(octets, [0x03, 0x00, 0x00, 0x00, b'h', b'i', 0x00]);

        let empty = body(Encoding::ROS2, |w| w.write_str(""));
        assert_eq!(empty, [0x01, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn interior_nul_is_refused() {
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        assert_eq!(
            writer.write_str("a\0b"),
            Err(CdrError::InteriorNul { index: 1 })
        );
    }

    #[test]
    fn wstring_counts_code_units_and_omits_the_terminator() {
        let octets = body(Encoding::ROS2, |w| w.write_wstr(&[0x0041, 0x00e9]));
        assert_eq!(octets, [0x02, 0x00, 0x00, 0x00, 0x41, 0x00, 0xe9, 0x00]);

        let empty = body(Encoding::ROS2, |w| w.write_wstr(&[]));
        assert_eq!(empty, [0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn four_octet_wchar_mode_zero_extends() {
        let encoding = Encoding::ROS2.with_wchar_width(WCharWidth::Four);
        let octets = body(encoding, |w| w.write_wstr(&[0x0041]));
        assert_eq!(octets, [0x01, 0x00, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn booleans_use_exactly_zero_and_one() {
        let octets = body(Encoding::ROS2, |w| {
            w.write_bool(true)?;
            w.write_bool(false)
        });
        assert_eq!(octets, [0x01, 0x00]);
    }

    #[test]
    fn floats_keep_their_bit_pattern() {
        let nan = f64::from_bits(0x7ff8_0000_0000_0001);
        let octets = body(Encoding::ROS2, |w| w.write_f64(nan));
        assert_eq!(octets, nan.to_bits().to_le_bytes());
    }

    #[test]
    fn sequence_length_rejects_more_than_a_u32() {
        if usize::BITS <= 32 {
            return;
        }
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        let too_long = u32::MAX as usize + 1;
        assert_eq!(
            writer.write_sequence_len(too_long),
            Err(CdrError::SequenceTooLong {
                length: too_long as u64,
                maximum: u64::from(u32::MAX),
            })
        );
    }

    #[test]
    fn octet_sequence_needs_no_element_alignment() {
        let octets = body(Encoding::ROS2, |w| {
            w.write_u8(0xaa)?;
            w.write_octet_sequence(&[1, 2, 3])
        });
        // 1 octet, 3 pad, length 3, then the three octets back to back.
        assert_eq!(octets, [0xaa, 0, 0, 0, 3, 0, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn scoped_origin_restarts_alignment_and_is_restored() {
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        writer.write_u8(0xaa).expect("write");
        writer
            .scoped_origin(|inner| {
                assert_eq!(inner.position(), 0);
                // A u64 at a fresh origin needs no padding even though the
                // enclosing buffer is at index 1.
                inner.write_u64(1)
            })
            .expect("scope");
        assert_eq!(writer.written(), 9);
        assert_eq!(writer.position(), 9);
    }

    #[test]
    fn delimited_backfills_the_octet_count_after_it() {
        let octets = body(Encoding::new(EncapsulationKind::Cdr2Le), |w| {
            w.delimited(|inner| {
                inner.write_u32(0x1122_3344)?;
                inner.write_u16(0x5566)
            })
        });
        // DHEADER = 6: four octets of u32 plus two of u16, not counting
        // itself.
        assert_eq!(
            octets,
            [0x06, 0x00, 0x00, 0x00, 0x44, 0x33, 0x22, 0x11, 0x66, 0x55]
        );
    }

    #[test]
    fn emheader_is_refused_on_an_xcdr1_stream() {
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        let header = EmHeader::new(1, LengthCode::Bytes4, false).expect("id fits");
        assert_eq!(
            writer.write_emheader(header),
            Err(CdrError::UnsupportedEncapsulation(
                "an EMHEADER requires an XCDR2 stream"
            ))
        );
    }

    #[test]
    fn mutable_member_backfills_its_nextint() {
        let octets = body(Encoding::new(EncapsulationKind::PlCdr2Le), |w| {
            w.write_member(3, true, |inner| inner.write_u16(0x0102))
        });
        // EMHEADER: M=1, LC=4, id=3 -> 0xc000_0003, little-endian.
        // NEXTINT: 2 (the u16). Then the member.
        assert_eq!(
            octets,
            [0x03, 0x00, 0x00, 0xc0, 0x02, 0x00, 0x00, 0x00, 0x02, 0x01]
        );
    }

    #[test]
    fn fixed_width_member_emits_no_nextint_and_checks_the_width() {
        let octets = body(Encoding::new(EncapsulationKind::PlCdr2Le), |w| {
            w.write_member_sized(1, false, LengthCode::Bytes4, |inner| {
                inner.write_u32(0x0a0b_0c0d)
            })
        });
        // EMHEADER: M=0, LC=2, id=1 -> 0x2000_0001.
        assert_eq!(octets, [0x01, 0x00, 0x00, 0x20, 0x0d, 0x0c, 0x0b, 0x0a]);

        let mut writer = CdrWriter::headerless(Encoding::new(EncapsulationKind::PlCdr2Le));
        assert_eq!(
            writer.write_member_sized(1, false, LengthCode::Bytes4, |inner| inner.write_u16(1)),
            Err(CdrError::BadEmHeader(
                "member body length disagrees with its fixed length code"
            ))
        );

        let mut writer = CdrWriter::headerless(Encoding::new(EncapsulationKind::PlCdr2Le));
        assert_eq!(
            writer.write_member_sized(1, false, LengthCode::NextIntBytes, |inner| inner
                .write_u16(1)),
            Err(CdrError::BadEmHeader(
                "write_member_sized needs a fixed-width length code"
            ))
        );
    }

    #[test]
    fn finish_padded_records_the_pad_count_only_under_xcdr2() {
        let mut v2 = CdrWriter::new(Encoding::new(EncapsulationKind::Cdr2Le));
        v2.write_u8(0xff).expect("write");
        let octets = v2.finish_padded();
        // Header, one octet, three pad octets; options records 3.
        assert_eq!(octets, [0x00, 0x07, 0x00, 0x03, 0xff, 0x00, 0x00, 0x00]);

        let mut v1 = CdrWriter::new(Encoding::ROS2);
        v1.write_u8(0xff).expect("write");
        let octets = v1.finish_padded();
        assert_eq!(octets, [0x00, 0x01, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn finish_padded_is_a_no_op_when_already_aligned() {
        let mut writer = CdrWriter::new(Encoding::new(EncapsulationKind::Cdr2Le));
        writer.write_u32(1).expect("write");
        assert_eq!(writer.finish_padded().len(), 8);
    }

    #[test]
    fn helper_constructors_agree_with_the_writer() {
        #[derive(Debug)]
        struct One(u32);
        impl CdrType for One {
            const MIN_SERIALIZED_SIZE: usize = 4;
        }
        impl CdrSerialize for One {
            fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
                writer.write_u32(self.0)
            }
        }

        assert_eq!(
            to_vec(&One(1), Encoding::ROS2).expect("encode"),
            [0x00, 0x01, 0x00, 0x00, 1, 0, 0, 0]
        );
        assert_eq!(
            to_vec_ros2(&One(1)).expect("encode"),
            to_vec(&One(1), Encoding::ROS2).expect("encode")
        );
        assert_eq!(
            to_vec_headerless(&One(1), Encoding::ROS2).expect("encode"),
            [1, 0, 0, 0]
        );
        assert_eq!(
            to_vec_padded(&One(1), Encoding::ROS2)
                .expect("encode")
                .len(),
            8
        );
        assert!(is_plain_kind(EncapsulationKind::CdrLe));
        assert!(!is_plain_kind(EncapsulationKind::PlCdrLe));
        assert_eq!(ALIGN_OCTET, 1);
    }

    #[test]
    fn measuring_writer_counts_what_a_buffer_writer_produces() {
        let mut counting = CdrWriter::measuring(Encoding::ROS2, true);
        counting.write_u8(1).expect("write");
        counting.write_f64(2.0).expect("write");
        counting.write_str("abc").expect("write");
        let counted = counting.written();

        let mut buffered = CdrWriter::new(Encoding::ROS2);
        buffered.write_u8(1).expect("write");
        buffered.write_f64(2.0).expect("write");
        buffered.write_str("abc").expect("write");
        assert_eq!(counted, buffered.finish().len());
    }
}
