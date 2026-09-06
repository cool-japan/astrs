//! Property-based tests for the columnar core.
//!
//! The unit tests pin the cases we thought of; these pin the ones we did not.
//! Each property is checked against a *reference model* written in the
//! obvious, slow way — `Vec<bool>` for a bitmap, `Vec<Option<String>>` for a
//! string column — so a divergence points at the fast implementation rather
//! than at a second copy of the same mistake.
//!
//! Four areas, chosen because they are where a columnar layout actually goes
//! wrong: bit numbering, offset arithmetic, UTF-8 boundaries, and the URN
//! grammar.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use astrs_data::array::{
    Array, BinaryArray, BooleanArray, Int32Array, LargeStringArray, StringArray,
};
use astrs_data::buffer::{ALIGNMENT, AlignedBuf, Bitmap, BitmapBuilder, Buffer, ScalarBuffer};
use astrs_data::builder::{BinaryBuilder, BooleanBuilder, Int32Builder, StringBuilder};
use astrs_data::urn::TypeUrn;
use astrs_data::{DataError, RecordBatch};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// A run of bits of a length that straddles byte and word boundaries.
fn bits() -> impl Strategy<Value = Vec<bool>> {
    prop::collection::vec(any::<bool>(), 0..=200)
}

/// Two bit runs of the *same* length, which the binary operators require.
fn bit_pairs() -> impl Strategy<Value = (Vec<bool>, Vec<bool>)> {
    (0usize..=200).prop_flat_map(|len| {
        (
            prop::collection::vec(any::<bool>(), len..=len),
            prop::collection::vec(any::<bool>(), len..=len),
        )
    })
}

/// Builds a bitmap from a reference model, through the public builder.
fn bitmap_of(model: &[bool]) -> Bitmap {
    let mut builder = BitmapBuilder::with_capacity(model.len());
    builder.append_slice(model);
    builder.finish()
}

/// An arbitrary URN, assembled from valid parts.
///
/// Parameters come from a `BTreeMap` strategy rather than a `Vec` one so the
/// keys are unique by construction — duplicates are a *rejection* case, tested
/// separately, not something this generator should stumble into.
fn type_urns() -> impl Strategy<Value = TypeUrn> {
    let category = "[a-z][a-z0-9_]{0,10}";
    let name = "[A-Z][A-Za-z0-9]{0,10}";
    let params =
        prop::collection::btree_map("[a-z][a-z0-9_]{0,6}", "[A-Za-z0-9_.:+*-]{1,8}", 0..4usize);
    (category, any::<u16>(), name, params).prop_map(|(category, version, name, params)| {
        TypeUrn::try_with_params(&category, version, &name, params)
            .expect("every part came from the grammar")
    })
}

// ---------------------------------------------------------------------------
// Bitmaps
// ---------------------------------------------------------------------------

proptest! {
    /// Reading a bitmap back gives exactly what was written, bit for bit.
    #[test]
    fn bitmap_round_trips_through_the_builder(model in bits()) {
        let bitmap = bitmap_of(&model);

        prop_assert_eq!(bitmap.len(), model.len());
        prop_assert_eq!(bitmap.is_empty(), model.is_empty());
        for (index, expected) in model.iter().enumerate() {
            prop_assert_eq!(bitmap.value(index), *expected, "bit {}", index);
            prop_assert_eq!(bitmap.get(index), Some(*expected));
        }
        prop_assert_eq!(bitmap.get(model.len()), None);
        prop_assert_eq!(bitmap.iter().collect::<Vec<_>>(), model);
    }

    /// Population counts agree with the model.
    #[test]
    fn bitmap_counts_match_the_model(model in bits()) {
        let bitmap = bitmap_of(&model);
        let set = model.iter().filter(|bit| **bit).count();

        prop_assert_eq!(bitmap.count_set(), set);
        prop_assert_eq!(bitmap.count_unset(), model.len() - set);
        prop_assert_eq!(bitmap.all_set(), model.iter().all(|bit| *bit));
        prop_assert_eq!(bitmap.none_set(), model.iter().all(|bit| !*bit));
        prop_assert_eq!(
            bitmap.set_indices().collect::<Vec<_>>(),
            model.iter().enumerate().filter_map(|(i, b)| b.then_some(i)).collect::<Vec<_>>()
        );
        prop_assert_eq!(
            bitmap.unset_indices().collect::<Vec<_>>(),
            model.iter().enumerate().filter_map(|(i, b)| (!b).then_some(i)).collect::<Vec<_>>()
        );
    }

    /// AND, OR, XOR and NOT are the pointwise boolean operators.
    #[test]
    fn bitmap_boolean_ops_are_pointwise((left, right) in bit_pairs()) {
        let a = bitmap_of(&left);
        let b = bitmap_of(&right);

        let and = a.and(&b).unwrap();
        let or = a.or(&b).unwrap();
        let xor = a.xor(&b).unwrap();
        let not = a.not();

        prop_assert_eq!(and.len(), left.len());
        prop_assert_eq!(or.len(), left.len());
        prop_assert_eq!(xor.len(), left.len());
        prop_assert_eq!(not.len(), left.len());

        for index in 0..left.len() {
            let (x, y) = (left[index], right[index]);
            prop_assert_eq!(and.value(index), x && y, "AND bit {}", index);
            prop_assert_eq!(or.value(index), x || y, "OR bit {}", index);
            prop_assert_eq!(xor.value(index), x != y, "XOR bit {}", index);
            prop_assert_eq!(not.value(index), !x, "NOT bit {}", index);
        }
    }

    /// The identities every boolean algebra owes us.
    #[test]
    fn bitmap_ops_obey_boolean_algebra((left, right) in bit_pairs()) {
        let a = bitmap_of(&left);
        let b = bitmap_of(&right);
        let ones = Bitmap::new_set(left.len());
        let zeros = Bitmap::new_unset(left.len());

        let equal = |x: &Bitmap, y: &Bitmap| -> bool {
            x.len() == y.len() && (0..x.len()).all(|i| x.value(i) == y.value(i))
        };

        prop_assert!(equal(&a.and(&b).unwrap(), &b.and(&a).unwrap()), "AND commutes");
        prop_assert!(equal(&a.or(&b).unwrap(), &b.or(&a).unwrap()), "OR commutes");
        prop_assert!(equal(&a.and(&ones).unwrap(), &a), "AND identity");
        prop_assert!(equal(&a.or(&zeros).unwrap(), &a), "OR identity");
        prop_assert!(equal(&a.and(&zeros).unwrap(), &zeros), "AND annihilator");
        prop_assert!(equal(&a.or(&ones).unwrap(), &ones), "OR annihilator");
        prop_assert!(equal(&a.not().not(), &a), "double negation");
        prop_assert!(equal(&a.xor(&a).unwrap(), &zeros), "self XOR is empty");
        // De Morgan.
        prop_assert!(
            equal(&a.and(&b).unwrap().not(), &a.not().or(&b.not()).unwrap()),
            "De Morgan"
        );
    }

    /// Mismatched lengths are rejected, never silently truncated.
    #[test]
    fn bitmap_ops_reject_length_mismatches(left in 0usize..64, right in 0usize..64) {
        prop_assume!(left != right);
        let a = Bitmap::new_set(left);
        let b = Bitmap::new_set(right);
        prop_assert_eq!(
            a.and(&b),
            Err(DataError::BitmapLengthMismatch { left, right })
        );
        prop_assert!(a.or(&b).is_err());
        prop_assert!(a.xor(&b).is_err());
    }

    /// Slicing a bitmap is a pure window: same bits, shifted origin.
    #[test]
    fn bitmap_slices_are_windows(model in bits(), offset in 0usize..200, len in 0usize..200) {
        let bitmap = bitmap_of(&model);
        let clamped_offset = offset.min(model.len());
        let clamped_len = len.min(model.len() - clamped_offset);

        let window = bitmap.slice(offset, len);
        prop_assert_eq!(window.len(), clamped_len);
        for index in 0..clamped_len {
            prop_assert_eq!(
                window.value(index),
                model[clamped_offset + index],
                "bit {}",
                index
            );
        }
        prop_assert_eq!(
            window.count_set(),
            model[clamped_offset..clamped_offset + clamped_len]
                .iter()
                .filter(|bit| **bit)
                .count()
        );

        // Canonicalising re-bases without changing the meaning.
        let canonical = window.to_canonical();
        prop_assert_eq!(canonical.bit_offset(), 0);
        prop_assert_eq!(canonical.len(), window.len());
        for index in 0..clamped_len {
            prop_assert_eq!(canonical.value(index), window.value(index));
        }
    }

    /// Slicing composes: a window of a window is a window of the original.
    #[test]
    fn bitmap_slices_compose(model in bits(), a in 0usize..64, b in 0usize..64) {
        let bitmap = bitmap_of(&model);
        let once = bitmap.slice(a, model.len());
        let twice = once.slice(b, once.len());
        let direct = bitmap.slice(a.saturating_add(b).min(model.len()), model.len());

        prop_assert_eq!(twice.len(), direct.len());
        for index in 0..twice.len() {
            prop_assert_eq!(twice.value(index), direct.value(index), "bit {}", index);
        }
    }

    /// `try_slice` reports exactly the requests `slice` would have clamped.
    #[test]
    fn bitmap_checked_slicing_agrees_with_clamping(
        model in bits(),
        offset in 0usize..250,
        len in 0usize..250,
    ) {
        let bitmap = bitmap_of(&model);
        let fits = offset.checked_add(len).is_some_and(|end| end <= model.len());
        prop_assert_eq!(bitmap.try_slice(offset, len).is_ok(), fits);
        if fits {
            prop_assert_eq!(bitmap.try_slice(offset, len).unwrap().len(), len);
        }
        // The infallible form never fails, whatever it is asked.
        prop_assert!(bitmap.slice(offset, len).len() <= model.len());
    }

    /// A boolean column and a bitmap tell the same story.
    #[test]
    fn boolean_arrays_agree_with_their_bitmap(model in bits()) {
        let array = BooleanArray::from_values(model.iter().copied());
        prop_assert_eq!(array.len(), model.len());
        prop_assert_eq!(array.null_count(), 0);
        prop_assert_eq!(array.true_count(), model.iter().filter(|b| **b).count());
        prop_assert_eq!(array.false_count(), model.iter().filter(|b| !**b).count());
        for (index, expected) in model.iter().enumerate() {
            prop_assert_eq!(array.value(index), Some(*expected), "row {}", index);
        }
    }

    /// `repeat_each` widens a bitmap the way a fixed-size list needs.
    #[test]
    fn bitmap_repeat_each_widens_uniformly(model in bits(), factor in 1usize..6) {
        let bitmap = bitmap_of(&model);
        let widened = bitmap.repeat_each(factor);
        prop_assert_eq!(widened.len(), model.len() * factor);
        for (index, expected) in model.iter().enumerate() {
            for step in 0..factor {
                prop_assert_eq!(
                    widened.value(index * factor + step),
                    *expected,
                    "row {} step {}",
                    index,
                    step
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UTF-8 and variable-length layouts
// ---------------------------------------------------------------------------

proptest! {
    /// A string column round-trips whatever text is put in it.
    #[test]
    fn string_columns_round_trip(model in prop::collection::vec(any::<Option<String>>(), 0..40)) {
        let mut builder = StringBuilder::with_capacity(model.len(), 64);
        for value in &model {
            builder.append_option(value.as_deref());
        }
        let array = builder.finish();

        prop_assert_eq!(array.len(), model.len());
        prop_assert_eq!(
            array.null_count(),
            model.iter().filter(|v| v.is_none()).count()
        );
        for (index, expected) in model.iter().enumerate() {
            prop_assert_eq!(array.get(index), expected.as_deref(), "row {}", index);
        }
        prop_assert_eq!(
            array.iter().collect::<Vec<_>>(),
            model.iter().map(Option::as_deref).collect::<Vec<_>>()
        );
    }

    /// Slicing a string column keeps the values that fall inside the window.
    #[test]
    fn string_slices_keep_their_values(
        model in prop::collection::vec(any::<Option<String>>(), 0..40),
        offset in 0usize..40,
        len in 0usize..40,
    ) {
        let array = StringArray::from_opt_iter(model.iter().map(Option::as_deref));
        let start = offset.min(model.len());
        let count = len.min(model.len() - start);

        let window = array.slice(offset, len);
        prop_assert_eq!(window.len(), count);
        for index in 0..count {
            prop_assert_eq!(
                window.get(index),
                model[start + index].as_deref(),
                "row {}",
                index
            );
        }
    }

    /// The 64-bit offset variant behaves identically to the 32-bit one.
    #[test]
    fn large_string_columns_match_small_ones(
        model in prop::collection::vec(any::<Option<String>>(), 0..30),
    ) {
        let small = StringArray::from_opt_iter(model.iter().map(Option::as_deref));
        let large = LargeStringArray::from_opt_iter(model.iter().map(Option::as_deref));

        prop_assert_eq!(small.len(), large.len());
        prop_assert_eq!(small.null_count(), large.null_count());
        prop_assert_eq!(small.total_value_bytes(), large.total_value_bytes());
        for index in 0..model.len() {
            prop_assert_eq!(small.get(index), large.get(index), "row {}", index);
        }
    }

    /// Valid UTF-8 with honest offsets is always accepted, and gives back what
    /// went in.
    #[test]
    fn well_formed_utf8_is_always_accepted(
        model in prop::collection::vec(any::<String>(), 0..25),
    ) {
        let mut values = Vec::new();
        let mut offsets = vec![0i32];
        for text in &model {
            values.extend_from_slice(text.as_bytes());
            offsets.push(i32::try_from(values.len()).expect("test data stays small"));
        }

        let array = StringArray::try_new(
            ScalarBuffer::<i32>::from_slice(&offsets),
            Buffer::from_slice(&values),
            None,
        )
        .expect("honest offsets over valid UTF-8");

        prop_assert_eq!(array.len(), model.len());
        for (index, expected) in model.iter().enumerate() {
            prop_assert_eq!(array.get(index), Some(expected.as_str()), "row {}", index);
        }
    }

    /// Arbitrary bytes never sneak past validation: either the constructor
    /// rejects them, or every slot it produces really is UTF-8.
    #[test]
    fn utf8_validation_never_admits_bad_bytes(
        values in prop::collection::vec(any::<u8>(), 0..48),
        cuts in prop::collection::vec(0usize..48, 0..8),
    ) {
        // Build monotonic offsets inside the value region from arbitrary cuts.
        let mut points: Vec<usize> = cuts.into_iter().map(|cut| cut.min(values.len())).collect();
        points.push(0);
        points.push(values.len());
        points.sort_unstable();
        let offsets: Vec<i32> = points
            .iter()
            .map(|point| i32::try_from(*point).expect("test data stays small"))
            .collect();

        let result = StringArray::try_new(
            ScalarBuffer::<i32>::from_slice(&offsets),
            Buffer::from_slice(&values),
            None,
        );

        match result {
            Ok(array) => {
                // Accepted: every slot must decode.
                for index in 0..array.len() {
                    prop_assert!(
                        array.value(index).is_some(),
                        "row {} was accepted but does not decode",
                        index
                    );
                }
                // And the concatenation is valid UTF-8 as a whole.
                prop_assert!(std::str::from_utf8(&values).is_ok());
            }
            Err(err) => {
                // Rejected: it must be a UTF-8 or boundary complaint, not a
                // panic and not an unrelated error.
                prop_assert!(
                    matches!(
                        err,
                        DataError::InvalidUtf8 { .. } | DataError::OffsetNotCharBoundary { .. }
                    ),
                    "unexpected error: {err}"
                );
            }
        }
    }

    /// The same bytes are always fine as a binary column, whatever they say.
    #[test]
    fn binary_columns_accept_anything(
        model in prop::collection::vec(prop::option::of(prop::collection::vec(any::<u8>(), 0..16)), 0..30),
    ) {
        let mut builder = BinaryBuilder::with_capacity(model.len(), 64);
        for value in &model {
            builder.append_option(value.as_deref());
        }
        let array = builder.finish();

        prop_assert_eq!(array.len(), model.len());
        for (index, expected) in model.iter().enumerate() {
            prop_assert_eq!(array.get(index), expected.as_deref(), "row {}", index);
        }
        prop_assert_eq!(
            array.total_value_bytes(),
            model.iter().flatten().map(Vec::len).sum::<usize>()
        );
    }

    /// A binary column built directly matches one built through the builder.
    #[test]
    fn binary_construction_paths_agree(
        model in prop::collection::vec(prop::option::of(prop::collection::vec(any::<u8>(), 0..12)), 0..25),
    ) {
        let direct = BinaryArray::from_opt_iter(model.iter().map(Option::as_deref));
        let mut builder = BinaryBuilder::new();
        for value in &model {
            builder.append_option(value.as_deref());
        }
        let built = builder.finish();

        prop_assert!(Array::equals(&direct, &built), "{:?} vs {:?}", direct, built);
    }
}

// ---------------------------------------------------------------------------
// Buffers and alignment
// ---------------------------------------------------------------------------

proptest! {
    /// However a buffer is grown, it never leaves its 64-byte boundary.
    #[test]
    fn aligned_bufs_stay_aligned(steps in prop::collection::vec(0usize..300, 0..40)) {
        let mut buf = AlignedBuf::new();
        prop_assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);

        let mut expected_len = 0usize;
        for step in steps {
            buf.extend_zeroed(step);
            expected_len += step;
            prop_assert_eq!(
                buf.as_ptr() as usize % ALIGNMENT,
                0,
                "misaligned after growing to {}",
                expected_len
            );
            prop_assert_eq!(buf.len(), expected_len);
            prop_assert!(buf.capacity() >= expected_len);
        }
        prop_assert!(buf.as_slice().iter().all(|byte| *byte == 0));
    }

    /// A clone owns its bytes and is aligned in its own right.
    #[test]
    fn aligned_buf_clones_are_independent(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let original = AlignedBuf::from_slice(&bytes);
        let mut copy = original.clone();

        prop_assert_eq!(original.as_ptr() as usize % ALIGNMENT, 0);
        prop_assert_eq!(copy.as_ptr() as usize % ALIGNMENT, 0);
        prop_assert_eq!(original.as_slice(), copy.as_slice());

        copy.push(0xff);
        prop_assert_eq!(original.len(), bytes.len(), "the original is untouched");
        prop_assert_eq!(original.as_slice(), bytes.as_slice());
    }

    /// A buffer window reports the bytes it covers and nothing else.
    #[test]
    fn buffer_windows_are_faithful(
        bytes in prop::collection::vec(any::<u8>(), 0..200),
        offset in 0usize..200,
        len in 0usize..200,
    ) {
        let buffer = Buffer::from_slice(&bytes);
        let start = offset.min(bytes.len());
        let count = len.min(bytes.len() - start);

        let window = buffer.slice(offset, len);
        prop_assert_eq!(window.len(), count);
        prop_assert_eq!(window.as_slice(), &bytes[start..start + count]);
        prop_assert_eq!(window.backing_len(), bytes.len());

        // Realigning copies the same bytes onto a fresh 64-byte boundary.
        let realigned = window.realigned();
        prop_assert!(realigned.is_aligned_to(ALIGNMENT));
        prop_assert_eq!(realigned.as_slice(), window.as_slice());
    }

    /// A typed view over a whole buffer sees exactly the scalars written.
    #[test]
    fn scalar_buffers_round_trip(values in prop::collection::vec(any::<i32>(), 0..60)) {
        let buffer = ScalarBuffer::<i32>::from_slice(&values);
        prop_assert_eq!(buffer.len(), values.len());
        prop_assert_eq!(buffer.as_slice(), values.as_slice());
        prop_assert!(buffer.inner().is_aligned_to(ALIGNMENT));

        let typed = buffer.inner().typed::<i32>().expect("aligned and sized");
        prop_assert_eq!(typed.as_slice(), values.as_slice());
    }
}

// ---------------------------------------------------------------------------
// Arrays, batches and URNs
// ---------------------------------------------------------------------------

proptest! {
    /// A primitive column round-trips through its builder, nulls included.
    #[test]
    fn primitive_columns_round_trip(model in prop::collection::vec(any::<Option<i32>>(), 0..64)) {
        let mut builder = Int32Builder::with_capacity(model.len());
        for value in &model {
            builder.append_option(*value);
        }
        let array = builder.finish();

        prop_assert_eq!(array.len(), model.len());
        prop_assert_eq!(array.null_count(), model.iter().filter(|v| v.is_none()).count());
        prop_assert_eq!(array.iter().collect::<Vec<_>>(), model.clone());

        // The same data built directly compares equal.
        let direct = Int32Array::from_opt_iter(model.iter().copied());
        prop_assert!(Array::equals(&direct, &array));
    }

    /// A boolean column round-trips through its builder.
    #[test]
    fn boolean_columns_round_trip(model in prop::collection::vec(any::<Option<bool>>(), 0..64)) {
        let mut builder = BooleanBuilder::with_capacity(model.len());
        for value in &model {
            builder.append_option(*value);
        }
        let array = builder.finish();
        prop_assert_eq!(array.iter().collect::<Vec<_>>(), model);
    }

    /// Slicing a column is the same as slicing the model it was built from.
    #[test]
    fn column_slices_match_the_model(
        model in prop::collection::vec(any::<Option<i32>>(), 0..50),
        offset in 0usize..50,
        len in 0usize..50,
    ) {
        let array = Int32Array::from_opt_iter(model.iter().copied());
        let start = offset.min(model.len());
        let count = len.min(model.len() - start);

        let window = array.slice(offset, len);
        prop_assert_eq!(window.len(), count);
        prop_assert_eq!(
            window.iter().collect::<Vec<_>>(),
            model[start..start + count].to_vec()
        );

        // And a window compares equal to the same values built directly.
        let direct = Int32Array::from_opt_iter(model[start..start + count].iter().copied());
        prop_assert!(Array::equals(&direct, &window));
    }

    /// A payload batch always mirrors the column it was built from.
    #[test]
    fn payload_batches_mirror_their_column(
        model in prop::collection::vec(any::<Option<i32>>(), 0..40),
        offset in 0usize..40,
        len in 0usize..40,
    ) {
        let array = Int32Array::from_opt_iter(model.iter().copied());
        let batch = RecordBatch::from_payload(std::sync::Arc::new(array));

        prop_assert_eq!(batch.num_rows(), model.len());
        prop_assert_eq!(batch.num_columns(), 1);
        prop_assert!(batch.payload_column().is_some());

        let start = offset.min(model.len());
        let count = len.min(model.len() - start);
        let window = batch.slice(offset, len);
        prop_assert_eq!(window.num_rows(), count);
        prop_assert_eq!(
            window.columns().first().map(|column| column.len()),
            Some(count)
        );

        // `try_slice` succeeds exactly when the window fits.
        let fits = offset.checked_add(len).is_some_and(|end| end <= model.len());
        prop_assert_eq!(batch.try_slice(offset, len).is_ok(), fits);
    }

    /// The canonical round-trip: re-parsing a URN's own text gives it back.
    #[test]
    fn type_urns_survive_reparsing(urn in type_urns()) {
        let text = urn.as_str().to_owned();
        let again = TypeUrn::parse(&text).expect("canonical text always reparses");

        prop_assert_eq!(&again, &urn);
        prop_assert_eq!(again.as_str(), text.as_str());
        prop_assert_eq!(again.category(), urn.category());
        prop_assert_eq!(again.version(), urn.version());
        prop_assert_eq!(again.name(), urn.name());
        prop_assert_eq!(again.param_count(), urn.param_count());
        for (key, value) in urn.params() {
            prop_assert_eq!(again.param(key), Some(&**value));
        }
    }

    /// Parsing normalises parameter order: any permutation of the same set
    /// yields the same URN.
    #[test]
    fn urn_parameter_order_does_not_matter(
        category in "[a-z][a-z0-9_]{0,8}",
        version in any::<u16>(),
        name in "[A-Z][A-Za-z0-9]{0,8}",
        params in prop::collection::btree_map("[a-z][a-z0-9_]{0,5}", "[A-Za-z0-9_.:+*-]{1,6}", 0..5),
    ) {
        let forward: Vec<(String, String)> = params
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut backward = forward.clone();
        backward.reverse();

        let a = TypeUrn::try_with_params(&category, version, &name, forward).unwrap();
        let b = TypeUrn::try_with_params(&category, version, &name, backward).unwrap();
        prop_assert_eq!(&a, &b);

        // The map the URN reports is exactly the map that went in.
        let reported: BTreeMap<String, String> = a
            .params()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        prop_assert_eq!(reported, params);
    }

    /// Serde is the canonical text, both ways.
    #[test]
    fn type_urns_survive_serde(urn in type_urns()) {
        let json = serde_json::to_string(&urn).expect("a URN serialises");
        prop_assert_eq!(&json, &format!("\"{}\"", urn.as_str()));
        let back: TypeUrn = serde_json::from_str(&json).expect("and deserialises");
        prop_assert_eq!(back, urn);
    }

    /// Arbitrary text never panics the parser, and is either a URN or a typed
    /// syntax error.
    #[test]
    fn arbitrary_text_never_panics_the_parser(text in ".{0,80}") {
        match TypeUrn::parse(&text) {
            Ok(urn) => {
                // Anything accepted must be canonical and reparse to itself.
                let again = TypeUrn::parse(urn.as_str());
                prop_assert_eq!(again.as_ref(), Ok(&urn));
            }
            Err(err) => prop_assert!(err.is_syntax(), "non-syntax error from parse: {}", err),
        }
    }

    /// Adding and removing a parameter is the identity.
    #[test]
    fn urn_parameters_add_and_remove_cleanly(
        urn in type_urns(),
        key in "[a-z][a-z0-9_]{0,5}",
        value in "[A-Za-z0-9_.:+*-]{1,6}",
    ) {
        prop_assume!(!urn.params().contains_key(key.as_str()));
        prop_assume!(urn.param_count() < TypeUrn::MAX_PARAMS);

        let with = urn.clone().with_param(&key, &value).expect("a valid parameter");
        prop_assert_eq!(with.param(&key), Some(value.as_str()));
        prop_assert_eq!(with.param_count(), urn.param_count() + 1);

        let without = with.without_param(&key).expect("removal never lengthens");
        prop_assert_eq!(&without, &urn);
    }

    /// Stripping parameters is idempotent and preserves identity.
    #[test]
    fn urn_bases_are_stable(urn in type_urns()) {
        let base = urn.base();
        prop_assert!(!base.has_params());
        prop_assert_eq!(base.base(), base.clone());
        prop_assert_eq!(base.as_str(), urn.base_str());
        prop_assert!(base.base_eq(&urn));
        prop_assert_eq!(base.category(), urn.category());
        prop_assert_eq!(base.name(), urn.name());
        prop_assert_eq!(base.version(), urn.version());
    }
}
