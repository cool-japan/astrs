//! [`CdrReader`] — the decode half, zero-copy where CDR allows it.
//!
//! A reader holds the whole input slice plus four indices: the current
//! position, the alignment origin, the end of the current scope, and the
//! [`Encoding`] the encapsulation header selected. Positions reported to
//! callers are always *stream positions* — offsets from the origin — because
//! that is what CDR's alignment rules are stated against.
//!
//! # Hostile input
//!
//! Every length on the wire is validated against the octets that remain
//! **before** anything is allocated:
//!
//! - a sequence length is multiplied by the element type's
//!   [`CdrType::MIN_SERIALIZED_SIZE`] and compared with
//!   [`CdrReader::remaining`];
//! - a string length must fit, and must end in the NUL its length promised;
//! - a DHEADER or EMHEADER length may not run past the enclosing scope.
//!
//! A `0xffff_ffff` length therefore costs one comparison, not four gigabytes.
//!
//! # Trailing octets are fatal
//!
//! [`CdrReader::finish`] refuses a stream that decoded successfully but has
//! octets left. A sample whose declared type does not consume its payload is
//! a type disagreement between writer and reader, and silently ignoring the
//! tail is how schema drift survives a week of testing.
//! [`CdrReader::finish_tolerant`] exists for the one case where the caller
//! genuinely owns padding it was not told about: a `PL_CDR` parameter value,
//! which is padded to four octets by its enclosing list.
//!
//! ```
//! use astrs_cdr::{CdrReader, CdrError};
//!
//! let bytes = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0d, 0x0c, 0x0b, 0x0a];
//! let mut reader = CdrReader::new(&bytes)?;
//! assert_eq!(reader.read_u8()?, 0x01);
//! assert_eq!(reader.read_u32()?, 0x0a0b_0c0d);
//! reader.finish()?;
//! # Ok::<(), CdrError>(())
//! ```

use crate::align::{ALIGN_2, ALIGN_4, ALIGN_8, padding_to};
use crate::encoding::{
    ENCAPSULATION_HEADER_LEN, EncapsulationHeader, Encoding, Endianness, WCharWidth,
};
use crate::error::{CdrError, CdrResult};
use crate::traits::{CdrDeserialize, CdrType};
use crate::xcdr2::{DHEADER_LEN, EmHeader, MemberHeader};

/// The largest slack [`CdrReader::finish_tolerant`] will forgive: three
/// octets, the most any four-octet alignment can require.
pub const MAX_ALIGNMENT_SLACK: usize = 3;

/// A CDR decoder over a borrowed buffer.
///
/// See the [module documentation](self) for the safety model.
#[derive(Debug, Clone)]
pub struct CdrReader<'de> {
    data: &'de [u8],
    /// Absolute index of the next octet.
    pos: usize,
    /// Absolute index of stream position 0.
    origin: usize,
    /// Absolute exclusive end of the current scope.
    end: usize,
    encoding: Encoding,
}

impl<'de> CdrReader<'de> {
    /// Open a reader on a payload that starts with an encapsulation header.
    ///
    /// The header selects the [`Encoding`], the alignment origin is set to
    /// the first octet after it, and any trailing padding the options field
    /// declares is excluded from the scope so [`CdrReader::finish`] does not
    /// mistake it for leftover data.
    ///
    /// # Errors
    ///
    /// - [`CdrError::Truncated`] when fewer than four octets are present.
    /// - [`CdrError::UnknownEncapsulation`] for an identifier outside the
    ///   XTypes table.
    /// - [`CdrError::PaddingOverrun`] when the options declare more padding
    ///   than the payload holds.
    pub fn new(data: &'de [u8]) -> CdrResult<Self> {
        let header = EncapsulationHeader::from_bytes(data)?;
        let body_len = data.len() - ENCAPSULATION_HEADER_LEN;
        let padding = usize::from(header.options.padding());
        if padding > body_len {
            return Err(CdrError::PaddingOverrun {
                padding: header.options.padding(),
                available: body_len,
            });
        }
        Ok(Self {
            data,
            pos: ENCAPSULATION_HEADER_LEN,
            origin: ENCAPSULATION_HEADER_LEN,
            end: data.len() - padding,
            encoding: Encoding::new(header.kind),
        })
    }

    /// [`CdrReader::new`], then override the `wchar` width.
    ///
    /// The width is not carried on the wire, so a peer that needs the
    /// four-octet compatibility mode has to be configured for it.
    ///
    /// # Errors
    ///
    /// Those of [`CdrReader::new`].
    pub fn new_with_wchar_width(data: &'de [u8], width: WCharWidth) -> CdrResult<Self> {
        let mut reader = Self::new(data)?;
        reader.encoding = reader.encoding.with_wchar_width(width);
        Ok(reader)
    }

    /// Open a reader on a body with **no** encapsulation header: stream
    /// position 0 is index 0 of `data`.
    ///
    /// This is how a `PL_CDR` parameter value is decoded, and how a caller
    /// that learned the encoding elsewhere (an RTPS inline-QoS blob) reads a
    /// bare body.
    #[must_use]
    pub const fn with_encoding(data: &'de [u8], encoding: Encoding) -> Self {
        Self {
            data,
            pos: 0,
            origin: 0,
            end: data.len(),
            encoding,
        }
    }

    /// The encoding in force.
    #[must_use]
    pub const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Stream position: octets consumed since the alignment origin.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos - self.origin
    }

    /// Octets left in the current scope.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.end - self.pos
    }

    /// True when the current scope is exhausted.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pos >= self.end
    }

    /// The octets left in the current scope, without consuming them.
    #[must_use]
    pub fn peek_remaining(&self) -> &'de [u8] {
        self.data.get(self.pos..self.end).unwrap_or(&[])
    }

    // -- low level ----------------------------------------------------------

    /// Skip padding so the next read starts on a `natural`-octet boundary.
    ///
    /// Padding octets are consumed without being inspected: the OMG
    /// specification requires a sender to zero them, but a receiver that
    /// rejects non-zero padding rejects real traffic for no benefit.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when the scope ends inside the padding.
    pub fn align(&mut self, natural: usize) -> CdrResult<()> {
        let pad = padding_to(self.position(), self.encoding.align_for(natural));
        if pad == 0 {
            return Ok(());
        }
        let available = self.remaining();
        if pad > available {
            return Err(CdrError::Truncated {
                needed: pad,
                available,
                context: "alignment padding",
            });
        }
        self.pos += pad;
        Ok(())
    }

    /// Consume `count` octets with no alignment step.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when fewer than `count` octets remain.
    pub fn read_octets(&mut self, count: usize) -> CdrResult<&'de [u8]> {
        let available = self.remaining();
        if count > available {
            return Err(CdrError::Truncated {
                needed: count,
                available,
                context: "raw octets",
            });
        }
        let slice = self
            .data
            .get(self.pos..self.pos + count)
            .ok_or(CdrError::Truncated {
                needed: count,
                available,
                context: "raw octets",
            })?;
        self.pos += count;
        Ok(slice)
    }

    /// Skip `count` octets with no alignment step.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when fewer than `count` octets remain.
    pub fn skip(&mut self, count: usize) -> CdrResult<()> {
        self.read_octets(count).map(|_| ())
    }

    /// Move forward to an absolute stream position.
    ///
    /// Forward only: a reader never rewinds past data it has handed out.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when `position` is behind the cursor or past
    /// the end of the scope.
    pub fn skip_to(&mut self, position: usize) -> CdrResult<()> {
        let current = self.position();
        let Some(delta) = position.checked_sub(current) else {
            return Err(CdrError::Truncated {
                needed: 0,
                available: self.remaining(),
                context: "a backwards seek",
            });
        };
        self.skip(delta)
    }

    fn take_array<const N: usize>(&mut self, context: &'static str) -> CdrResult<[u8; N]> {
        let available = self.remaining();
        if N > available {
            return Err(CdrError::Truncated {
                needed: N,
                available,
                context,
            });
        }
        let mut octets = [0_u8; N];
        let slice = self
            .data
            .get(self.pos..self.pos + N)
            .ok_or(CdrError::Truncated {
                needed: N,
                available,
                context,
            })?;
        octets.copy_from_slice(slice);
        self.pos += N;
        Ok(octets)
    }

    // -- primitives ---------------------------------------------------------

    /// Read an IDL `octet` / `uint8`.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] at the end of the scope.
    pub fn read_u8(&mut self) -> CdrResult<u8> {
        Ok(self.take_array::<1>("u8")?[0])
    }

    /// Read an IDL `int8`.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] at the end of the scope.
    pub fn read_i8(&mut self) -> CdrResult<i8> {
        Ok(self.read_u8()? as i8)
    }

    /// Read an IDL `char`, one octet.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] at the end of the scope.
    pub fn read_char8(&mut self) -> CdrResult<u8> {
        self.read_u8()
    }

    /// Read an IDL `boolean`.
    ///
    /// # Errors
    ///
    /// [`CdrError::InvalidBoolean`] for any octet other than `0` or `1`;
    /// OMG CDR 15.3.1 defines only those two.
    pub fn read_bool(&mut self) -> CdrResult<bool> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(CdrError::InvalidBoolean(other)),
        }
    }

    /// Read an IDL `unsigned short` / `uint16`, aligned to 2.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_u16(&mut self) -> CdrResult<u16> {
        self.align(ALIGN_2)?;
        let octets = self.take_array::<2>("u16")?;
        Ok(match self.encoding.endianness() {
            Endianness::Big => u16::from_be_bytes(octets),
            Endianness::Little => u16::from_le_bytes(octets),
        })
    }

    /// Read an IDL `short` / `int16`, aligned to 2.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_i16(&mut self) -> CdrResult<i16> {
        Ok(self.read_u16()? as i16)
    }

    /// Read an IDL `unsigned long` / `uint32`, aligned to 4.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_u32(&mut self) -> CdrResult<u32> {
        self.align(ALIGN_4)?;
        let octets = self.take_array::<4>("u32")?;
        Ok(match self.encoding.endianness() {
            Endianness::Big => u32::from_be_bytes(octets),
            Endianness::Little => u32::from_le_bytes(octets),
        })
    }

    /// Read an IDL `long` / `int32`, aligned to 4.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_i32(&mut self) -> CdrResult<i32> {
        Ok(self.read_u32()? as i32)
    }

    /// Read an IDL `unsigned long long` / `uint64`.
    ///
    /// Aligned to 8 under XCDR1, 4 under XCDR2.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_u64(&mut self) -> CdrResult<u64> {
        self.align(ALIGN_8)?;
        let octets = self.take_array::<8>("u64")?;
        Ok(match self.encoding.endianness() {
            Endianness::Big => u64::from_be_bytes(octets),
            Endianness::Little => u64::from_le_bytes(octets),
        })
    }

    /// Read an IDL `long long` / `int64`.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_i64(&mut self) -> CdrResult<i64> {
        Ok(self.read_u64()? as i64)
    }

    /// Read an IDL `float`, aligned to 4.
    ///
    /// The bit pattern is taken verbatim: NaN payloads and signalling bits
    /// survive the round trip.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_f32(&mut self) -> CdrResult<f32> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    /// Read an IDL `double`.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_f64(&mut self) -> CdrResult<f64> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    /// Read an IDL `wchar`: one UTF-16 code unit.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`], or [`CdrError::InvalidUtf16`] when the
    /// four-octet compatibility mode carries a value above `0xffff`.
    pub fn read_wchar(&mut self) -> CdrResult<u16> {
        match self.encoding.wchar_width() {
            WCharWidth::Two => self.read_u16(),
            WCharWidth::Four => {
                let wide = self.read_u32()?;
                u16::try_from(wide).map_err(|_| CdrError::InvalidUtf16 { index: 0 })
            }
        }
    }

    // -- lengths ------------------------------------------------------------

    /// Read a four-octet length prefix.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`].
    pub fn read_length(&mut self) -> CdrResult<u32> {
        self.read_u32()
    }

    /// Read a sequence length and check it against the octets that remain.
    ///
    /// `element_min` is the element type's [`CdrType::MIN_SERIALIZED_SIZE`];
    /// it is clamped to at least one, so the bound "there cannot be more
    /// elements than there are octets left" always holds.
    ///
    /// # Errors
    ///
    /// [`CdrError::LengthOverflow`] when the declared count could not
    /// possibly fit, before any allocation happens.
    pub fn read_sequence_len(
        &mut self,
        element_min: usize,
        context: &'static str,
    ) -> CdrResult<usize> {
        let declared = self.read_length()?;
        let element = element_min.max(1);
        let available = self.remaining();
        let needed = u64::from(declared).saturating_mul(element as u64);
        if needed > available as u64 {
            return Err(CdrError::LengthOverflow {
                declared: u64::from(declared),
                available,
                element_size: element,
                context,
            });
        }
        usize::try_from(declared).map_err(|_| CdrError::SizeOverflow("reading a sequence length"))
    }

    // -- strings ------------------------------------------------------------

    /// Read an IDL `string` without copying: the returned `&str` borrows the
    /// input buffer.
    ///
    /// The four-octet length includes the terminating NUL, so a length of `1`
    /// is the empty string and a length of `0` is malformed.
    ///
    /// # Errors
    ///
    /// - [`CdrError::MissingNulTerminator`] for a zero length, or when the
    ///   last octet is not NUL.
    /// - [`CdrError::InteriorNul`] when the body holds a NUL before its
    ///   terminator. OMG CDR strings are C strings; accepting an interior NUL
    ///   would produce a `String` this crate could not re-encode.
    /// - [`CdrError::InvalidUtf8`] — ROS 2 defines `string` as UTF-8.
    /// - [`CdrError::LengthOverflow`] when the length exceeds the buffer.
    pub fn read_str(&mut self) -> CdrResult<&'de str> {
        let len = self.read_sequence_len(1, "string")?;
        if len == 0 {
            return Err(CdrError::MissingNulTerminator);
        }
        let raw = self.read_octets(len)?;
        let (last, body) = raw.split_last().unwrap_or((&0, &[]));
        if *last != 0 {
            return Err(CdrError::MissingNulTerminator);
        }
        if let Some(index) = body.iter().position(|octet| *octet == 0) {
            return Err(CdrError::InteriorNul { index });
        }
        Ok(core::str::from_utf8(body)?)
    }

    /// Read an IDL `string` into an owned `String`.
    ///
    /// # Errors
    ///
    /// Those of [`CdrReader::read_str`].
    pub fn read_string(&mut self) -> CdrResult<String> {
        self.read_str().map(str::to_owned)
    }

    /// Read an IDL `wstring` as its UTF-16 code units.
    ///
    /// The four-octet prefix counts code units, not octets, and there is no
    /// terminator (OMG DDS-XTypes 1.3 §7.4.3.5.1) — both differ from the
    /// plain `string` rule.
    ///
    /// # Errors
    ///
    /// [`CdrError::LengthOverflow`] for a count the buffer cannot hold,
    /// [`CdrError::Truncated`] if it ends early, and
    /// [`CdrError::InvalidUtf16`] when the four-octet compatibility mode
    /// carries a unit above `0xffff`.
    pub fn read_wstring(&mut self) -> CdrResult<Vec<u16>> {
        let width = self.encoding.wchar_width().octets();
        let count = self.read_sequence_len(width, "wstring")?;
        let mut units = Vec::with_capacity(count);
        for index in 0..count {
            let unit = match self.encoding.wchar_width() {
                WCharWidth::Two => self.read_u16()?,
                WCharWidth::Four => {
                    let wide = self.read_u32()?;
                    u16::try_from(wide).map_err(|_| CdrError::InvalidUtf16 { index })?
                }
            };
            units.push(unit);
        }
        Ok(units)
    }

    // -- collections --------------------------------------------------------

    /// Read a `sequence<octet>` without copying.
    ///
    /// # Errors
    ///
    /// [`CdrError::LengthOverflow`] or [`CdrError::Truncated`].
    pub fn read_octet_sequence(&mut self) -> CdrResult<&'de [u8]> {
        let len = self.read_sequence_len(1, "sequence<octet>")?;
        self.read_octets(len)
    }

    /// Deserialize a value through its [`CdrDeserialize`] impl.
    ///
    /// # Errors
    ///
    /// Whatever the impl returns.
    pub fn deserialize<T: CdrDeserialize<'de>>(&mut self) -> CdrResult<T> {
        T::deserialize(self)
    }

    // -- scopes -------------------------------------------------------------

    /// Run `body` with the alignment origin moved to the current position.
    ///
    /// The mirror of [`crate::CdrWriter::scoped_origin`]; see it for why a
    /// `PL_CDR` parameter value needs one.
    ///
    /// # Errors
    ///
    /// Whatever `body` returns. The origin is restored either way.
    pub fn scoped_origin<R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> CdrResult<R>,
    ) -> CdrResult<R> {
        let previous = self.origin;
        self.origin = self.pos;
        let result = body(self);
        self.origin = previous;
        result
    }

    /// Read a DHEADER: the count of octets that follow it.
    ///
    /// # Errors
    ///
    /// [`CdrError::UnsupportedEncapsulation`] on an XCDR1 stream,
    /// [`CdrError::Truncated`] at the end of the scope, or
    /// [`CdrError::DelimiterOverrun`] when the declared length runs past the
    /// enclosing scope.
    pub fn read_dheader(&mut self) -> CdrResult<usize> {
        if !self.encoding.is_v2() {
            return Err(CdrError::UnsupportedEncapsulation(
                "a DHEADER requires an XCDR2 stream",
            ));
        }
        let declared = self.read_length()?;
        let declared =
            usize::try_from(declared).map_err(|_| CdrError::SizeOverflow("reading a DHEADER"))?;
        let available = self.remaining();
        if declared > available {
            return Err(CdrError::DelimiterOverrun {
                declared,
                available,
            });
        }
        Ok(declared)
    }

    /// Run `body` inside an XCDR2 DHEADER scope.
    ///
    /// The reader is clamped to the declared length while `body` runs, so a
    /// short DHEADER cannot let a member read into its successor. Afterwards
    /// the cursor moves to the declared end **even if `body` stopped early**:
    /// that skip is the forward compatibility an appendable type buys, and
    /// treating it as an error would defeat the feature.
    ///
    /// # Errors
    ///
    /// Those of [`CdrReader::read_dheader`], plus whatever `body` returns.
    pub fn delimited<R>(&mut self, body: impl FnOnce(&mut Self) -> CdrResult<R>) -> CdrResult<R> {
        let declared = self.read_dheader()?;
        let start = self.pos;
        let scope_end = start + declared;
        let outer_end = self.end;
        self.end = scope_end;
        let result = body(self);
        self.end = outer_end;
        let result = result?;
        // Forward only: `self.end` was clamped, so the cursor cannot be past
        // `scope_end`, and anything it left behind is a member this reader
        // does not know about.
        self.pos = scope_end;
        Ok(result)
    }

    /// Read a constructed type's body, honouring the DHEADER its
    /// extensibility calls for.
    ///
    /// The mirror of [`crate::CdrWriter::write_struct`], so one generated
    /// impl serves both CDR versions. Like the writer, it consults `T`'s own
    /// extensibility rather than the stream's encapsulation kind: the kind
    /// describes the *top-level* type, and a nested member's framing follows
    /// the member. A caller that wants to check the sender's announcement
    /// against the type it is about to decode compares
    /// [`EncapsulationKind::declared_extensibility`](crate::EncapsulationKind::declared_extensibility)
    /// with [`CdrType::EXTENSIBILITY`] before calling this.
    ///
    /// # Errors
    ///
    /// Whatever `body` returns, plus those of [`CdrReader::delimited`].
    pub fn read_struct<T, R>(
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

    /// Read the EMHEADER (and NEXTINT, if any) of one member of an XCDR2
    /// mutable struct.
    ///
    /// On return the cursor sits at the first octet of the member's **own**
    /// serialization: for length codes 5..=7 the NEXTINT is part of the
    /// member (it is the member's string length, sequence length or DHEADER),
    /// so the reader rewinds over it. Hand the result to
    /// [`CdrReader::read_member_value`] or [`CdrReader::skip_member`].
    ///
    /// # Errors
    ///
    /// [`CdrError::UnsupportedEncapsulation`] on an XCDR1 stream,
    /// [`CdrError::Truncated`], [`CdrError::BadEmHeader`], or
    /// [`CdrError::DelimiterOverrun`] when the member runs past the scope.
    pub fn read_member_header(&mut self) -> CdrResult<MemberHeader> {
        if !self.encoding.is_v2() {
            return Err(CdrError::UnsupportedEncapsulation(
                "an EMHEADER requires an XCDR2 stream",
            ));
        }
        let header = EmHeader::from_bits(self.read_u32()?)?;
        let code = header.length_code;
        let len = if code.has_next_int() {
            let next_int = self.read_u32()?;
            if code.next_int_is_part_of_member() {
                // The NEXTINT *is* the member's first four octets; rewind so
                // the member's own deserializer sees its length prefix.
                self.pos -= DHEADER_LEN;
            }
            code.member_len(next_int)?
        } else {
            code.member_len(0)?
        };
        let available = self.remaining();
        if len > available {
            return Err(CdrError::DelimiterOverrun {
                declared: len,
                available,
            });
        }
        Ok(MemberHeader {
            header,
            body: self.position(),
            len,
        })
    }

    /// Skip a member located by [`CdrReader::read_member_header`].
    ///
    /// # Errors
    ///
    /// [`CdrError::UnknownMustUnderstand`] when the member is flagged
    /// must-understand: the specification requires the whole sample to be
    /// discarded rather than the member. Use
    /// [`CdrReader::skip_member_unchecked`] to skip anyway.
    pub fn skip_member(&mut self, header: &MemberHeader) -> CdrResult<()> {
        if header.must_understand() {
            return Err(CdrError::UnknownMustUnderstand {
                member_id: header.member_id(),
            });
        }
        self.skip_member_unchecked(header)
    }

    /// Skip a member regardless of its must-understand flag.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when the member runs past the scope.
    pub fn skip_member_unchecked(&mut self, header: &MemberHeader) -> CdrResult<()> {
        self.skip_to(header.end()?)
    }

    /// Deserialize a member located by [`CdrReader::read_member_header`].
    ///
    /// The reader is clamped to the member's declared extent while `T` runs,
    /// then moved to the member's end, so a nested type that reads fewer
    /// octets than the EMHEADER declared does not desynchronise the stream.
    ///
    /// # Errors
    ///
    /// Whatever `T` returns, plus [`CdrError::Truncated`].
    pub fn read_member_value<T: CdrDeserialize<'de>>(
        &mut self,
        header: &MemberHeader,
    ) -> CdrResult<T> {
        let end_position = header.end()?;
        let absolute_end = self.origin + end_position;
        let outer_end = self.end;
        self.end = absolute_end.min(outer_end);
        let value = T::deserialize(self);
        self.end = outer_end;
        let value = value?;
        self.skip_to(end_position)?;
        Ok(value)
    }

    /// A reader over the next `len` octets, with a fresh alignment origin.
    ///
    /// The cursor of `self` advances past them. This is how a `PL_CDR`
    /// parameter value is handed to a typed decoder: the value is a CDR
    /// stream in its own right.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when fewer than `len` octets remain.
    pub fn sub_reader(&mut self, len: usize) -> CdrResult<Self> {
        let octets = self.read_octets(len)?;
        Ok(Self::with_encoding(octets, self.encoding.plain()))
    }

    // -- finishing ----------------------------------------------------------

    /// Assert the scope is fully consumed.
    ///
    /// # Errors
    ///
    /// [`CdrError::TrailingBytes`] when anything is left.
    pub fn finish(self) -> CdrResult<()> {
        let remaining = self.remaining();
        if remaining == 0 {
            Ok(())
        } else {
            Err(CdrError::TrailingBytes { remaining })
        }
    }

    /// Assert the scope is consumed apart from at most
    /// [`MAX_ALIGNMENT_SLACK`] octets of padding.
    ///
    /// For a `PL_CDR` parameter value, whose enclosing list rounds every
    /// value up to four octets without saying how many it added.
    ///
    /// # Errors
    ///
    /// [`CdrError::TrailingBytes`] when more than three octets are left.
    pub fn finish_tolerant(self) -> CdrResult<()> {
        let remaining = self.remaining();
        if remaining <= MAX_ALIGNMENT_SLACK {
            Ok(())
        } else {
            Err(CdrError::TrailingBytes { remaining })
        }
    }
}

/// Decode a value from a payload that starts with an encapsulation header,
/// refusing trailing octets.
///
/// # Errors
///
/// Those of [`CdrReader::new`], of `T`'s impl, and
/// [`CdrError::TrailingBytes`].
pub fn from_bytes<'de, T: CdrDeserialize<'de>>(data: &'de [u8]) -> CdrResult<T> {
    let mut reader = CdrReader::new(data)?;
    let value = reader.deserialize()?;
    reader.finish()?;
    Ok(value)
}

/// [`from_bytes`], forgiving up to three octets of trailing alignment
/// padding.
///
/// RTPS pads a `SerializedPayload` to a four-octet multiple; under XCDR1
/// there is nowhere to record how much, so a reader has to be told to expect
/// it.
///
/// # Errors
///
/// Those of [`from_bytes`], with the trailing-octet rule relaxed.
pub fn from_bytes_tolerant<'de, T: CdrDeserialize<'de>>(data: &'de [u8]) -> CdrResult<T> {
    let mut reader = CdrReader::new(data)?;
    let value = reader.deserialize()?;
    reader.finish_tolerant()?;
    Ok(value)
}

/// Decode a value from a body with no encapsulation header.
///
/// # Errors
///
/// Those of `T`'s impl, and [`CdrError::TrailingBytes`].
pub fn from_bytes_headerless<'de, T: CdrDeserialize<'de>>(
    data: &'de [u8],
    encoding: Encoding,
) -> CdrResult<T> {
    let mut reader = CdrReader::with_encoding(data, encoding);
    let value = reader.deserialize()?;
    reader.finish()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::EncapsulationKind;

    fn body(encoding: Encoding, octets: &[u8]) -> CdrReader<'_> {
        CdrReader::with_encoding(octets, encoding)
    }

    #[test]
    fn header_selects_the_encoding_and_the_origin() {
        let bytes = [0x00, 0x03, 0x00, 0x00, 0xaa];
        let reader = CdrReader::new(&bytes).expect("header parses");
        assert_eq!(reader.encoding().kind(), EncapsulationKind::PlCdrLe);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.remaining(), 1);
        assert_eq!(reader.peek_remaining(), &[0xaa]);
    }

    #[test]
    fn options_padding_is_excluded_from_the_scope() {
        // CDR2_LE with options declaring three pad octets.
        let bytes = [0x00, 0x07, 0x00, 0x03, 0xaa, 0x00, 0x00, 0x00];
        let mut reader = CdrReader::new(&bytes).expect("header parses");
        assert_eq!(reader.remaining(), 1);
        assert_eq!(reader.read_u8().expect("read"), 0xaa);
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn options_padding_larger_than_the_body_is_refused() {
        let bytes = [0x00, 0x07, 0x00, 0x03, 0xaa];
        assert_eq!(
            CdrReader::new(&bytes).map(|_| ()),
            Err(CdrError::PaddingOverrun {
                padding: 3,
                available: 1,
            })
        );
    }

    #[test]
    fn short_and_unknown_headers_are_refused() {
        assert_eq!(
            CdrReader::new(&[0x00, 0x01]).map(|_| ()),
            Err(CdrError::Truncated {
                needed: 4,
                available: 2,
                context: "encapsulation header",
            })
        );
        assert_eq!(
            CdrReader::new(&[0xff, 0xff, 0x00, 0x00]).map(|_| ()),
            Err(CdrError::UnknownEncapsulation { identifier: 0xffff })
        );
    }

    #[test]
    fn alignment_consumes_padding_without_inspecting_it() {
        // Non-zero padding is accepted: senders must zero it, receivers gain
        // nothing by refusing it.
        let mut reader = body(Encoding::ROS2, &[0x01, 0xde, 0xad, 0xbe, 0x04, 0, 0, 0]);
        assert_eq!(reader.read_u8().expect("read"), 1);
        assert_eq!(reader.read_u32().expect("read"), 4);
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn alignment_that_runs_off_the_end_is_truncation() {
        // One octet consumed leaves stream position 1; a u32 needs three pad
        // octets to reach position 4, and only one octet is left.
        let mut reader = body(Encoding::ROS2, &[0x01, 0x00]);
        assert_eq!(reader.read_u8().expect("read"), 1);
        assert_eq!(
            reader.read_u32(),
            Err(CdrError::Truncated {
                needed: 3,
                available: 1,
                context: "alignment padding",
            })
        );
    }

    #[test]
    fn trailing_octets_are_fatal_but_slack_is_tolerated() {
        let mut reader = body(Encoding::ROS2, &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(reader.read_u8().expect("read"), 1);
        assert_eq!(
            reader.clone().finish(),
            Err(CdrError::TrailingBytes { remaining: 3 })
        );
        assert_eq!(reader.clone().finish_tolerant(), Ok(()));

        let mut wide = body(Encoding::ROS2, &[0; 8]);
        assert_eq!(wide.read_u32().expect("read"), 0);
        assert_eq!(
            wide.finish_tolerant(),
            Err(CdrError::TrailingBytes { remaining: 4 })
        );
        assert_eq!(MAX_ALIGNMENT_SLACK, 3);
    }

    #[test]
    fn booleans_reject_anything_but_zero_and_one() {
        let mut reader = body(Encoding::ROS2, &[0x00, 0x01, 0x02]);
        assert!(!reader.read_bool().expect("read"));
        assert!(reader.read_bool().expect("read"));
        assert_eq!(reader.read_bool(), Err(CdrError::InvalidBoolean(2)));
    }

    #[test]
    fn strings_are_borrowed_and_must_be_nul_terminated() {
        let octets = [0x03, 0x00, 0x00, 0x00, b'h', b'i', 0x00];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(reader.read_str().expect("read"), "hi");
        assert_eq!(reader.finish(), Ok(()));

        let unterminated = [0x03, 0x00, 0x00, 0x00, b'h', b'i', b'!'];
        let mut reader = body(Encoding::ROS2, &unterminated);
        assert_eq!(reader.read_str(), Err(CdrError::MissingNulTerminator));

        let zero_length = [0x00, 0x00, 0x00, 0x00];
        let mut reader = body(Encoding::ROS2, &zero_length);
        assert_eq!(reader.read_str(), Err(CdrError::MissingNulTerminator));
    }

    #[test]
    fn interior_nul_and_bad_utf8_are_refused() {
        let interior = [0x04, 0x00, 0x00, 0x00, b'a', 0x00, b'b', 0x00];
        let mut reader = body(Encoding::ROS2, &interior);
        assert_eq!(reader.read_str(), Err(CdrError::InteriorNul { index: 1 }));

        let bad_utf8 = [0x03, 0x00, 0x00, 0x00, 0xff, 0xfe, 0x00];
        let mut reader = body(Encoding::ROS2, &bad_utf8);
        assert!(matches!(reader.read_str(), Err(CdrError::InvalidUtf8(_))));
    }

    #[test]
    fn a_hostile_string_length_never_allocates() {
        let hostile = [0xff, 0xff, 0xff, 0xff, b'x'];
        let mut reader = body(Encoding::ROS2, &hostile);
        assert_eq!(
            reader.read_str(),
            Err(CdrError::LengthOverflow {
                declared: 0xffff_ffff,
                available: 1,
                element_size: 1,
                context: "string",
            })
        );
    }

    #[test]
    fn a_hostile_sequence_length_is_scaled_by_the_element_size() {
        let hostile = [0x00, 0x00, 0x00, 0x40, 0x01, 0x02, 0x03, 0x04];
        let mut reader = body(Encoding::ROS2, &hostile);
        assert_eq!(
            reader.read_sequence_len(8, "sequence<double>"),
            Err(CdrError::LengthOverflow {
                declared: 0x4000_0000,
                available: 4,
                element_size: 8,
                context: "sequence<double>",
            })
        );
    }

    #[test]
    fn wstrings_count_code_units_and_have_no_terminator() {
        let octets = [0x02, 0x00, 0x00, 0x00, 0x41, 0x00, 0xe9, 0x00];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(reader.read_wstring().expect("read"), vec![0x0041, 0x00e9]);
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn four_octet_wchar_mode_rejects_units_above_ffff() {
        let encoding = Encoding::ROS2.with_wchar_width(WCharWidth::Four);
        let octets = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00];
        let mut reader = body(encoding, &octets);
        assert_eq!(
            reader.read_wstring(),
            Err(CdrError::InvalidUtf16 { index: 0 })
        );
    }

    #[test]
    fn scoped_origin_restarts_alignment() {
        // One octet, then a u64 at a fresh origin: no padding at all.
        let octets = [0xaa, 1, 0, 0, 0, 0, 0, 0, 0];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(reader.read_u8().expect("read"), 0xaa);
        let value = reader
            .scoped_origin(|inner| {
                assert_eq!(inner.position(), 0);
                inner.read_u64()
            })
            .expect("scope");
        assert_eq!(value, 1);
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn delimited_skips_members_the_reader_does_not_know() {
        // DHEADER = 8, but this reader only knows the first u32.
        let octets = [0x08, 0x00, 0x00, 0x00, 1, 0, 0, 0, 2, 0, 0, 0];
        let mut reader = body(Encoding::new(EncapsulationKind::Cdr2Le), &octets);
        let known = reader
            .delimited(|inner| inner.read_u32())
            .expect("delimited");
        assert_eq!(known, 1);
        // The unknown tail was skipped, not reported as trailing data.
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn delimited_clamps_so_a_short_dheader_cannot_over_read() {
        // DHEADER = 4 but the body tries to read two u32s.
        let octets = [0x04, 0x00, 0x00, 0x00, 1, 0, 0, 0, 2, 0, 0, 0];
        let mut reader = body(Encoding::new(EncapsulationKind::Cdr2Le), &octets);
        let outcome = reader.delimited(|inner| {
            inner.read_u32()?;
            inner.read_u32()
        });
        assert_eq!(
            outcome,
            Err(CdrError::Truncated {
                needed: 4,
                available: 0,
                context: "u32",
            })
        );
    }

    #[test]
    fn a_dheader_past_the_end_of_the_scope_is_refused() {
        let octets = [0xff, 0x00, 0x00, 0x00, 1, 0, 0, 0];
        let mut reader = body(Encoding::new(EncapsulationKind::Cdr2Le), &octets);
        assert_eq!(
            reader.read_dheader(),
            Err(CdrError::DelimiterOverrun {
                declared: 255,
                available: 4,
            })
        );
    }

    #[test]
    fn dheaders_and_emheaders_are_refused_on_xcdr1() {
        let octets = [0x04, 0x00, 0x00, 0x00];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(
            reader.read_dheader(),
            Err(CdrError::UnsupportedEncapsulation(
                "a DHEADER requires an XCDR2 stream"
            ))
        );
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(
            reader.read_member_header().map(|_| ()),
            Err(CdrError::UnsupportedEncapsulation(
                "an EMHEADER requires an XCDR2 stream"
            ))
        );
    }

    #[test]
    fn member_header_rewinds_over_a_nextint_that_belongs_to_the_member() {
        // EMHEADER id=1, LC=5 (0x5000_0001), then a string of length 3.
        let octets = [
            0x01, 0x00, 0x00, 0x50, // EMHEADER, little-endian
            0x03, 0x00, 0x00, 0x00, // NEXTINT == the string's own length
            b'h', b'i', 0x00, 0x00, // body + terminator + one pad octet
        ];
        let mut reader = body(Encoding::new(EncapsulationKind::PlCdr2Le), &octets);
        let header = reader.read_member_header().expect("member header");
        assert_eq!(header.member_id(), 1);
        // The cursor is back at the NEXTINT, which is the string's length.
        assert_eq!(header.body, 4);
        // LC=5: total member length is 4 + NEXTINT.
        assert_eq!(header.len, 7);
        let value: &str = reader.read_member_value(&header).expect("member value");
        assert_eq!(value, "hi");
        assert_eq!(reader.position(), 11);
    }

    #[test]
    fn a_must_understand_member_may_not_be_skipped() {
        // EMHEADER M=1, LC=2 (0xa000_0002), then a u32.
        let octets = [0x02, 0x00, 0x00, 0xa0, 0x09, 0x00, 0x00, 0x00];
        let mut reader = body(Encoding::new(EncapsulationKind::PlCdr2Le), &octets);
        let header = reader.read_member_header().expect("member header");
        assert!(header.must_understand());
        assert_eq!(
            reader.skip_member(&header),
            Err(CdrError::UnknownMustUnderstand { member_id: 2 })
        );
        assert_eq!(reader.skip_member_unchecked(&header), Ok(()));
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn sub_reader_gets_a_fresh_origin_and_plain_rules() {
        let octets = [0xaa, 1, 0, 0, 0, 0, 0, 0, 0];
        let mut reader = body(Encoding::DISCOVERY, &octets);
        assert_eq!(reader.read_u8().expect("read"), 0xaa);
        let mut nested = reader.sub_reader(8).expect("sub reader");
        assert_eq!(nested.encoding().kind(), EncapsulationKind::CdrLe);
        assert_eq!(nested.read_u64().expect("read"), 1);
        assert_eq!(nested.finish(), Ok(()));
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn skip_to_moves_forward_only() {
        let octets = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(reader.read_u8().expect("read"), 1);
        assert_eq!(reader.skip_to(4), Ok(()));
        assert_eq!(reader.read_u8().expect("read"), 5);
        assert!(reader.skip_to(2).is_err());
        assert!(reader.skip_to(999).is_err());
    }

    #[test]
    fn octet_sequences_are_borrowed() {
        let octets = [0x03, 0x00, 0x00, 0x00, 9, 8, 7];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(reader.read_octet_sequence().expect("read"), &[9, 8, 7]);
        assert!(reader.is_empty());
    }

    #[test]
    fn every_signed_and_float_accessor_round_trips_a_known_pattern() {
        let octets = [
            0xff, // position 0: i8 == -1
            0x00, // pad: the i16 must start at an even position
            0xfe, 0xff, // position 2: i16 == -2
            0xfd, 0xff, 0xff, 0xff, // position 4: i32 == -3
            0xfc, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // position 8: i64 == -4
        ];
        let mut reader = body(Encoding::ROS2, &octets);
        assert_eq!(reader.read_i8().expect("read"), -1);
        assert_eq!(reader.read_i16().expect("read"), -2);
        assert_eq!(reader.read_i32().expect("read"), -3);
        assert_eq!(reader.read_i64().expect("read"), -4);
        assert_eq!(reader.finish(), Ok(()));

        let floats = [0x00, 0x00, 0x80, 0x3f];
        let mut reader = body(Encoding::ROS2, &floats);
        assert_eq!(reader.read_f32().expect("read"), 1.0_f32);

        let doubles = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f];
        let mut reader = body(Encoding::ROS2, &doubles);
        assert_eq!(reader.read_f64().expect("read"), 1.0_f64);
    }

    #[test]
    fn char_and_wchar_accessors_are_the_octet_and_the_code_unit() {
        // The wchar is two octets aligned to two, so a pad octet follows the
        // one-octet char.
        let mut reader = body(Encoding::ROS2, &[b'x', 0x00, 0x41, 0x00]);
        assert_eq!(reader.read_char8().expect("read"), b'x');
        assert_eq!(reader.read_wchar().expect("read"), 0x0041);
    }
}
