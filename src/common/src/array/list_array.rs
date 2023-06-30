// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::fmt;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::hash::Hash;
use std::mem::size_of;

use bytes::{Buf, BufMut};
use either::Either;
use itertools::Itertools;
use risingwave_pb::data::{ListArrayData, PbArray, PbArrayType};
use serde::{Deserialize, Serializer};

use super::{Array, ArrayBuilder, ArrayBuilderImpl, ArrayImpl, ArrayResult, RowRef};
use crate::buffer::{Bitmap, BitmapBuilder};
use crate::estimate_size::EstimateSize;
use crate::row::Row;
use crate::types::{
    hash_datum, DataType, Datum, DatumRef, DefaultPartialOrd, Scalar, ScalarRefImpl, ToDatumRef,
    ToText,
};
use crate::util::memcmp_encoding;
use crate::util::value_encoding::estimate_serialize_datum_size;

macro_rules! iter_elems_ref {
    ($self:expr, $it:ident, { $($body:tt)* }) => {
        iter_elems_ref!($self, $it, { $($body)* }, { $($body)* })
    };

    ($self:expr, $it:ident, { $($l_body:tt)* }, { $($r_body:tt)* }) => {
        match $self {
            ListRef::Indexed { arr, idx } => {
                // SAFETY: `offsets` and `value` are both generated by the array builder, so they are always valid.
                let $it = (arr.offsets[idx]..arr.offsets[idx + 1]).map(|o| unsafe { arr.value.value_at_unchecked(o as usize) });
                $($l_body)*
            }
            ListRef::ValueRef { val } => {
                let $it = val.values.iter().map(ToDatumRef::to_datum_ref);
                $($r_body)*
            }
        }
    };
}

#[derive(Debug)]
pub struct ListArrayBuilder {
    bitmap: BitmapBuilder,
    offsets: Vec<u32>,
    value: Box<ArrayBuilderImpl>,
    value_type: DataType,
    len: usize,
}

impl ArrayBuilder for ListArrayBuilder {
    type ArrayType = ListArray;

    #[cfg(not(test))]
    fn new(_capacity: usize) -> Self {
        panic!("Must use with_type.")
    }

    #[cfg(test)]
    fn new(capacity: usize) -> Self {
        Self::with_type(
            capacity,
            // Default datatype
            DataType::List(Box::new(DataType::Int16)),
        )
    }

    fn with_type(capacity: usize, ty: DataType) -> Self {
        let DataType::List(value_type) = ty else {
            panic!("data type must be DataType::List");
        };
        Self {
            bitmap: BitmapBuilder::with_capacity(capacity),
            offsets: vec![0],
            value: Box::new(value_type.create_array_builder(capacity)),
            value_type: *value_type,
            len: 0,
        }
    }

    fn append_n(&mut self, n: usize, value: Option<ListRef<'_>>) {
        match value {
            None => {
                self.bitmap.append_n(n, false);
                let last = *self.offsets.last().unwrap();
                for _ in 0..n {
                    self.offsets.push(last);
                }
            }
            Some(v) => {
                self.bitmap.append_n(n, true);
                for _ in 0..n {
                    let last = *self.offsets.last().unwrap();
                    let elems = v.iter();
                    self.offsets.push(
                        last.checked_add(elems.len() as u32)
                            .expect("offset overflow"),
                    );
                    for elem in elems {
                        self.value.append(elem);
                    }
                }
            }
        }
        self.len += n;
    }

    fn append_array(&mut self, other: &ListArray) {
        self.bitmap.append_bitmap(&other.bitmap);
        let last = *self.offsets.last().unwrap();
        self.offsets
            .append(&mut other.offsets[1..].iter().map(|o| *o + last).collect());
        self.value.append_array(&other.value);
        self.len += other.len();
    }

    fn pop(&mut self) -> Option<()> {
        self.bitmap.pop()?;
        let start = self.offsets.pop().unwrap();
        let end = *self.offsets.last().unwrap();
        self.len -= 1;
        for _ in end..start {
            self.value.pop().unwrap();
        }
        Some(())
    }

    fn len(&self) -> usize {
        self.bitmap.len()
    }

    fn finish(self) -> ListArray {
        ListArray {
            bitmap: self.bitmap.finish(),
            offsets: self.offsets,
            value: Box::new(self.value.finish()),
            value_type: self.value_type,
        }
    }
}

impl ListArrayBuilder {
    pub fn append_row_ref(&mut self, row: RowRef<'_>) {
        self.bitmap.append(true);
        let last = *self.offsets.last().unwrap();
        self.offsets
            .push(last.checked_add(row.len() as u32).expect("offset overflow"));
        self.len += 1;
        for v in row.iter() {
            self.value.append(v);
        }
    }
}

/// Each item of this `ListArray` is a `List<T>`, or called `T[]` (T array).
///
/// * As other arrays, there is a null bitmap, with `1` meaning nonnull and `0` meaning null.
/// * As [`BytesArray`], there is an offsets `Vec` and a value `Array`. The value `Array` has all
///   items concatenated, and the offsets `Vec` stores start and end indices into it for slicing.
///   Effectively, the inner array is the flattened form, and `offsets.len() == n + 1`.
///
/// For example, `values (array[1]), (array[]::int[]), (null), (array[2, 3]);` stores an inner
///  `I32Array` with `[1, 2, 3]`, along with offsets `[0, 1, 1, 1, 3]` and null bitmap `TTFT`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListArray {
    bitmap: Bitmap,
    pub(super) offsets: Vec<u32>,
    pub(super) value: Box<ArrayImpl>,
    pub(super) value_type: DataType,
}

impl EstimateSize for ListArray {
    fn estimated_heap_size(&self) -> usize {
        self.bitmap.estimated_heap_size()
            + self.offsets.capacity() * size_of::<u32>()
            + self.value.estimated_heap_size()
    }
}

impl Array for ListArray {
    type Builder = ListArrayBuilder;
    type OwnedItem = ListValue;
    type RefItem<'a> = ListRef<'a>;

    unsafe fn raw_value_at_unchecked(&self, idx: usize) -> Self::RefItem<'_> {
        ListRef::Indexed { arr: self, idx }
    }

    fn len(&self) -> usize {
        self.bitmap.len()
    }

    fn to_protobuf(&self) -> PbArray {
        let value = self.value.to_protobuf();
        PbArray {
            array_type: PbArrayType::List as i32,
            struct_array_data: None,
            list_array_data: Some(Box::new(ListArrayData {
                offsets: self.offsets.clone(),
                value: Some(Box::new(value)),
                value_type: Some(self.value_type.to_protobuf()),
            })),
            null_bitmap: Some(self.bitmap.to_protobuf()),
            values: vec![],
        }
    }

    fn null_bitmap(&self) -> &Bitmap {
        &self.bitmap
    }

    fn into_null_bitmap(self) -> Bitmap {
        self.bitmap
    }

    fn set_bitmap(&mut self, bitmap: Bitmap) {
        self.bitmap = bitmap;
    }

    fn data_type(&self) -> DataType {
        DataType::List(Box::new(self.value_type.clone()))
    }
}

impl ListArray {
    /// Returns the total number of elements in the flattened array.
    pub fn flatten_len(&self) -> usize {
        self.value.len()
    }

    /// Flatten the list array into a single array.
    pub fn flatten(&self) -> ArrayImpl {
        (*self.value).clone()
    }

    pub fn from_protobuf(array: &PbArray) -> ArrayResult<ArrayImpl> {
        ensure!(
            array.values.is_empty(),
            "Must have no buffer in a list array"
        );
        let bitmap: Bitmap = array.get_null_bitmap()?.into();
        let array_data = array.get_list_array_data()?.to_owned();
        let flatten_len = match array_data.offsets.last() {
            Some(&n) => n as usize,
            None => bail!("Must have at least one element in offsets"),
        };
        let value = ArrayImpl::from_protobuf(array_data.value.as_ref().unwrap(), flatten_len)?;
        let arr = ListArray {
            bitmap,
            offsets: array_data.offsets,
            value: Box::new(value),
            value_type: DataType::from(&array_data.value_type.unwrap()),
        };
        Ok(arr.into())
    }

    // Used for testing purposes
    pub fn from_iter(
        values: impl IntoIterator<Item = Option<ArrayImpl>>,
        value_type: DataType,
    ) -> ListArray {
        let values = values.into_iter();
        let size_hint = values.size_hint().0;

        let mut offsets = vec![0u32];
        offsets.reserve(size_hint);
        let mut builder = ArrayBuilderImpl::with_type(size_hint, value_type.clone());
        let mut bitmap = BitmapBuilder::with_capacity(size_hint);
        for v in values {
            bitmap.append(v.is_some());
            let last_offset = *offsets.last().unwrap();
            match v {
                Some(a) => {
                    offsets.push(
                        last_offset
                            .checked_add(a.len() as u32)
                            .expect("offset overflow"),
                    );
                    builder.append_array(&a)
                }
                None => {
                    offsets.push(last_offset);
                }
            }
        }
        ListArray {
            bitmap: bitmap.finish(),
            offsets,
            value: Box::new(builder.finish()),
            value_type,
        }
    }

    #[cfg(test)]
    pub fn values_vec(&self) -> Vec<Option<ListValue>> {
        use crate::types::ScalarRef;

        self.iter()
            .map(|v| v.map(|s| s.to_owned_scalar()))
            .collect_vec()
    }
}

#[derive(Clone, Debug, Eq, Default, PartialEq, Hash)]
pub struct ListValue {
    values: Box<[Datum]>,
}

impl PartialOrd for ListValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_scalar_ref().partial_cmp(&other.as_scalar_ref())
    }
}

impl Ord for ListValue {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

// Used to display ListValue in explain for better readibilty.
pub fn display_for_explain(list: &ListValue) -> String {
    // Example of ListValue display: ARRAY[1, 2, null]
    format!(
        "ARRAY[{}]",
        list.values
            .iter()
            .map(|v| {
                match v.as_ref() {
                    None => "null".into(),
                    Some(scalar) => scalar.as_scalar_ref_impl().to_text(),
                }
            })
            .collect::<Vec<String>>()
            .join(", ")
    )
}

impl From<Vec<Datum>> for ListValue {
    fn from(data: Vec<Datum>) -> Self {
        ListValue::new(data)
    }
}

impl EstimateSize for ListValue {
    fn estimated_heap_size(&self) -> usize {
        // TODO: Try speed up this process.
        self.values
            .iter()
            .map(|datum| datum.estimated_heap_size())
            .sum()
    }
}

impl ListValue {
    pub fn new(values: Vec<Datum>) -> Self {
        Self {
            values: values.into_boxed_slice(),
        }
    }

    pub fn values(&self) -> &[Datum] {
        &self.values
    }

    pub fn memcmp_deserialize(
        datatype: &DataType,
        deserializer: &mut memcomparable::Deserializer<impl Buf>,
    ) -> memcomparable::Result<Self> {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        let mut inner_deserializer = memcomparable::Deserializer::new(bytes.as_slice());
        let mut values = Vec::new();
        while inner_deserializer.has_remaining() {
            values.push(memcmp_encoding::deserialize_datum_in_composite(
                datatype,
                &mut inner_deserializer,
            )?)
        }
        Ok(Self::new(values))
    }
}

#[derive(Copy, Clone)]
pub enum ListRef<'a> {
    Indexed { arr: &'a ListArray, idx: usize },
    ValueRef { val: &'a ListValue },
}

impl<'a> ListRef<'a> {
    /// Returns the length of the list.
    pub fn len(&self) -> usize {
        match self {
            ListRef::Indexed { arr, idx } => (arr.offsets[*idx + 1] - arr.offsets[*idx]) as usize,
            ListRef::ValueRef { val } => val.values.len(),
        }
    }

    /// Returns `true` if the list has a length of 0.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn flatten(self) -> Vec<DatumRef<'a>> {
        // XXX: avoid using vector
        iter_elems_ref!(self, it, {
            it.flat_map(|datum_ref| {
                if let Some(ScalarRefImpl::List(list_ref)) = datum_ref {
                    list_ref.flatten()
                } else {
                    vec![datum_ref]
                }
                .into_iter()
            })
            .collect()
        })
    }

    /// Iterates over the elements of the list.
    ///
    /// Prefer using the macro `iter_elems_ref!` if possible to avoid the cost of enum dispatching.
    pub fn iter(self) -> impl ExactSizeIterator<Item = DatumRef<'a>> + 'a {
        iter_elems_ref!(self, it, { Either::Left(it) }, { Either::Right(it) })
    }

    /// Get the element at the given index. Returns `None` if the index is out of bounds.
    pub fn elem_at(self, index: usize) -> Option<DatumRef<'a>> {
        iter_elems_ref!(self, it, {
            let mut it = it;
            it.nth(index)
        })
    }

    pub fn memcmp_serialize(
        self,
        serializer: &mut memcomparable::Serializer<impl BufMut>,
    ) -> memcomparable::Result<()> {
        let mut inner_serializer = memcomparable::Serializer::new(vec![]);
        iter_elems_ref!(self, it, {
            for datum_ref in it {
                memcmp_encoding::serialize_datum_in_composite(datum_ref, &mut inner_serializer)?
            }
        });
        serializer.serialize_bytes(&inner_serializer.into_inner())
    }

    pub fn hash_scalar_inner<H: std::hash::Hasher>(self, state: &mut H) {
        iter_elems_ref!(self, it, {
            for datum_ref in it {
                hash_datum(datum_ref, state);
            }
        })
    }

    /// estimate the serialized size with value encoding
    pub fn estimate_serialize_size_inner(self) -> usize {
        iter_elems_ref!(self, it, {
            it.fold(0, |acc, datum_ref| {
                acc + estimate_serialize_datum_size(datum_ref)
            })
        })
    }
}

impl PartialEq for ListRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        iter_elems_ref!(*self, lhs, {
            iter_elems_ref!(*other, rhs, { lhs.eq(rhs) })
        })
    }
}

impl PartialOrd for ListRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        iter_elems_ref!(*self, lhs, {
            iter_elems_ref!(*other, rhs, {
                lhs.partial_cmp_by(rhs, |lv, rv| lv.default_partial_cmp(&rv))
            })
        })
    }
}

impl Debug for ListRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        iter_elems_ref!(*self, it, {
            for v in it {
                Debug::fmt(&v, f)?;
            }
            Ok(())
        })
    }
}

impl ToText for ListRef<'_> {
    // This function will be invoked when pgwire prints a list value in string.
    // Refer to PostgreSQL `array_out` or `appendPGArray`.
    fn write<W: std::fmt::Write>(&self, f: &mut W) -> std::fmt::Result {
        iter_elems_ref!(*self, it, {
            write!(
                f,
                "{{{}}}",
                it.format_with(",", |datum_ref, f| {
                    let s = datum_ref.to_text();
                    // Never quote null or inner list, but quote empty, verbatim 'null', special
                    // chars and whitespaces.
                    let need_quote = !matches!(datum_ref, None | Some(ScalarRefImpl::List(_)))
                        && (s.is_empty()
                            || s.to_ascii_lowercase() == "null"
                            || s.contains([
                                '"', '\\', '{', '}', ',',
                                // PostgreSQL `array_isspace` includes '\x0B' but rust
                                // [`char::is_ascii_whitespace`] does not.
                                ' ', '\t', '\n', '\r', '\x0B', '\x0C',
                            ]));
                    if need_quote {
                        f(&"\"")?;
                        s.chars().try_for_each(|c| {
                            if c == '"' || c == '\\' {
                                f(&"\\")?;
                            }
                            f(&c)
                        })?;
                        f(&"\"")
                    } else {
                        f(&s)
                    }
                })
            )
        })
    }

    fn write_with_type<W: std::fmt::Write>(&self, ty: &DataType, f: &mut W) -> std::fmt::Result {
        match ty {
            DataType::List { .. } => self.write(f),
            _ => unreachable!(),
        }
    }
}

impl Eq for ListRef<'_> {}

impl Ord for ListRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // The order between two lists is deterministic.
        self.partial_cmp(other).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use more_asserts::{assert_gt, assert_lt};

    use super::*;
    use crate::try_match_expand;

    #[test]
    fn test_list_with_values() {
        use crate::array::*;
        let arr = ListArray::from_iter(
            [
                Some(I32Array::from_iter([Some(12), Some(-7), Some(25)]).into()),
                None,
                Some(I32Array::from_iter([Some(0), Some(-127), Some(127), Some(50)]).into()),
                Some(I32Array::from_iter([0; 0]).into()),
            ],
            DataType::Int32,
        );
        let actual = ListArray::from_protobuf(&arr.to_protobuf()).unwrap();
        let tmp = ArrayImpl::List(arr);
        assert_eq!(tmp, actual);

        let arr = try_match_expand!(actual, ArrayImpl::List).unwrap();
        let list_values = arr.values_vec();
        assert_eq!(
            list_values,
            vec![
                Some(ListValue::new(vec![
                    Some(ScalarImpl::Int32(12)),
                    Some(ScalarImpl::Int32(-7)),
                    Some(ScalarImpl::Int32(25)),
                ])),
                None,
                Some(ListValue::new(vec![
                    Some(ScalarImpl::Int32(0)),
                    Some(ScalarImpl::Int32(-127)),
                    Some(ScalarImpl::Int32(127)),
                    Some(ScalarImpl::Int32(50)),
                ])),
                Some(ListValue::new(vec![])),
            ]
        );

        let mut builder = ListArrayBuilder::with_type(4, DataType::List(Box::new(DataType::Int32)));
        list_values.iter().for_each(|v| {
            builder.append(v.as_ref().map(|s| s.as_scalar_ref()));
        });
        let arr = builder.finish();
        assert_eq!(arr.values_vec(), list_values);

        let part1 = ListArray::from_iter(
            [
                Some(I32Array::from_iter([Some(12), Some(-7), Some(25)]).into()),
                None,
            ],
            DataType::Int32,
        );

        let part2 = ListArray::from_iter(
            [
                Some(I32Array::from_iter([Some(0), Some(-127), Some(127), Some(50)]).into()),
                Some(I32Array::from_iter([0; 0]).into()),
            ],
            DataType::Int32,
        );

        let mut builder = ListArrayBuilder::with_type(4, DataType::List(Box::new(DataType::Int32)));
        builder.append_array(&part1);
        builder.append_array(&part2);

        assert_eq!(arr.values_vec(), builder.finish().values_vec());
    }

    // Ensure `create_builder` exactly copies the same metadata.
    #[test]
    fn test_list_create_builder() {
        use crate::array::*;
        let arr = ListArray::from_iter(
            [Some(
                F32Array::from_iter([Some(2.0), Some(42.0), Some(1.0)]).into(),
            )],
            DataType::Float32,
        );
        let builder = arr.create_builder(0);
        let arr2 = builder.finish();
        assert_eq!(arr.data_type(), arr2.data_type());
    }

    #[test]
    fn test_builder_pop() {
        use crate::array::*;

        {
            let mut builder =
                ListArrayBuilder::with_type(1, DataType::List(Box::new(DataType::Int32)));
            let val = ListValue::new(vec![Some(1.into()), Some(2.into()), Some(3.into())]);
            builder.append(Some(ListRef::ValueRef { val: &val }));
            assert!(builder.pop().is_some());
            assert!(builder.pop().is_none());
            let arr = builder.finish();
            assert!(arr.is_empty());
        }

        {
            let meta = DataType::List(Box::new(DataType::List(Box::new(DataType::Int32))));
            let mut builder = ListArrayBuilder::with_type(2, meta);
            let val1 = ListValue::new(vec![Some(1.into()), Some(2.into()), Some(3.into())]);
            let val2 = ListValue::new(vec![Some(1.into()), Some(2.into()), Some(3.into())]);
            let list1 = ListValue::new(vec![Some(val1.into()), Some(val2.into())]);
            builder.append(Some(ListRef::ValueRef { val: &list1 }));

            let val3 = ListValue::new(vec![Some(1.into()), Some(2.into()), Some(3.into())]);
            let val4 = ListValue::new(vec![Some(1.into()), Some(2.into()), Some(3.into())]);
            let list2 = ListValue::new(vec![Some(val3.into()), Some(val4.into())]);

            builder.append(Some(ListRef::ValueRef { val: &list2 }));

            assert!(builder.pop().is_some());

            let arr = builder.finish();
            assert_eq!(arr.len(), 1);

            let val = arr.value_at(0).unwrap();

            let datums = val.iter().map(ToOwnedDatum::to_owned_datum).collect_vec();
            assert_eq!(datums, list1.values.to_vec());
        }
    }

    #[test]
    fn test_list_nested_layout() {
        use crate::array::*;

        let listarray1 = ListArray::from_iter(
            [
                Some(I32Array::from_iter([Some(1), Some(2)]).into()),
                Some(I32Array::from_iter([Some(3), Some(4)]).into()),
            ],
            DataType::Int32,
        );

        let listarray2 = ListArray::from_iter(
            [
                Some(I32Array::from_iter([Some(5), Some(6), Some(7)]).into()),
                None,
                Some(I32Array::from_iter([Some(8)]).into()),
            ],
            DataType::Int32,
        );

        let listarray3 = ListArray::from_iter(
            [Some(I32Array::from_iter([Some(9), Some(10)]).into())],
            DataType::Int32,
        );

        let nestarray = ListArray::from_iter(
            [
                Some(listarray1.into()),
                Some(listarray2.into()),
                Some(listarray3.into()),
            ],
            DataType::List(Box::new(DataType::Int32)),
        );
        let actual = ListArray::from_protobuf(&nestarray.to_protobuf()).unwrap();
        assert_eq!(ArrayImpl::List(nestarray), actual);

        let nestarray = try_match_expand!(actual, ArrayImpl::List).unwrap();
        let nested_list_values = nestarray.values_vec();
        assert_eq!(
            nested_list_values,
            vec![
                Some(ListValue::new(vec![
                    Some(ScalarImpl::List(ListValue::new(vec![
                        Some(ScalarImpl::Int32(1)),
                        Some(ScalarImpl::Int32(2)),
                    ]))),
                    Some(ScalarImpl::List(ListValue::new(vec![
                        Some(ScalarImpl::Int32(3)),
                        Some(ScalarImpl::Int32(4)),
                    ]))),
                ])),
                Some(ListValue::new(vec![
                    Some(ScalarImpl::List(ListValue::new(vec![
                        Some(ScalarImpl::Int32(5)),
                        Some(ScalarImpl::Int32(6)),
                        Some(ScalarImpl::Int32(7)),
                    ]))),
                    None,
                    Some(ScalarImpl::List(ListValue::new(vec![Some(
                        ScalarImpl::Int32(8)
                    ),]))),
                ])),
                Some(ListValue::new(vec![Some(ScalarImpl::List(
                    ListValue::new(vec![
                        Some(ScalarImpl::Int32(9)),
                        Some(ScalarImpl::Int32(10)),
                    ])
                )),])),
            ]
        );

        let mut builder = ListArrayBuilder::with_type(
            3,
            DataType::List(Box::new(DataType::List(Box::new(DataType::Int32)))),
        );
        for v in &nested_list_values {
            builder.append(v.as_ref().map(|s| s.as_scalar_ref()));
        }
        let nestarray = builder.finish();
        assert_eq!(nestarray.values_vec(), nested_list_values);
    }

    #[test]
    fn test_list_value_cmp() {
        // ARRAY[1, 1] < ARRAY[1, 2, 1]
        assert_lt!(
            ListValue::new(vec![Some(1.into()), Some(1.into())]),
            ListValue::new(vec![Some(1.into()), Some(2.into()), Some(1.into())]),
        );
        // ARRAY[1, 2] < ARRAY[1, 2, 1]
        assert_lt!(
            ListValue::new(vec![Some(1.into()), Some(2.into())]),
            ListValue::new(vec![Some(1.into()), Some(2.into()), Some(1.into())]),
        );
        // ARRAY[1, 3] > ARRAY[1, 2, 1]
        assert_gt!(
            ListValue::new(vec![Some(1.into()), Some(3.into())]),
            ListValue::new(vec![Some(1.into()), Some(2.into()), Some(1.into())]),
        );
        // null > 1
        assert_gt!(
            ListValue::new(vec![None]),
            ListValue::new(vec![Some(1.into())]),
        );
        // ARRAY[1, 2, null] > ARRAY[1, 2, 1]
        assert_gt!(
            ListValue::new(vec![Some(1.into()), Some(2.into()), None]),
            ListValue::new(vec![Some(1.into()), Some(2.into()), Some(1.into())]),
        );
        // Null value in first ARRAY results into a Greater ordering regardless of the smaller ARRAY
        // length. ARRAY[1, null] > ARRAY[1, 2, 3]
        assert_gt!(
            ListValue::new(vec![Some(1.into()), None]),
            ListValue::new(vec![Some(1.into()), Some(2.into()), Some(3.into())]),
        );
        // ARRAY[1, null] == ARRAY[1, null]
        assert_eq!(
            ListValue::new(vec![Some(1.into()), None]),
            ListValue::new(vec![Some(1.into()), None]),
        );
    }

    #[test]
    fn test_list_ref_display() {
        let v = ListValue::new(vec![Some(1.into()), None]);
        let r = ListRef::ValueRef { val: &v };
        assert_eq!("{1,NULL}".to_string(), format!("{}", r.to_text()));
    }

    #[test]
    fn test_serialize_deserialize() {
        let value = ListValue::new(vec![
            Some("abcd".into()),
            Some("".into()),
            None,
            Some("a".into()),
        ]);
        let list_ref = ListRef::ValueRef { val: &value };
        let mut serializer = memcomparable::Serializer::new(vec![]);
        serializer.set_reverse(true);
        list_ref.memcmp_serialize(&mut serializer).unwrap();
        let buf = serializer.into_inner();
        let mut deserializer = memcomparable::Deserializer::new(&buf[..]);
        deserializer.set_reverse(true);
        assert_eq!(
            ListValue::memcmp_deserialize(&DataType::Varchar, &mut deserializer).unwrap(),
            value
        );

        let mut builder =
            ListArrayBuilder::with_type(0, DataType::List(Box::new(DataType::Varchar)));
        builder.append(Some(list_ref));
        let array = builder.finish();
        let list_ref = array.value_at(0).unwrap();
        let mut serializer = memcomparable::Serializer::new(vec![]);
        list_ref.memcmp_serialize(&mut serializer).unwrap();
        let buf = serializer.into_inner();
        let mut deserializer = memcomparable::Deserializer::new(&buf[..]);
        assert_eq!(
            ListValue::memcmp_deserialize(&DataType::Varchar, &mut deserializer).unwrap(),
            value
        );
    }

    #[test]
    fn test_memcomparable() {
        let cases = [
            (
                ListValue::new(vec![
                    Some(123.to_scalar_value()),
                    Some(456.to_scalar_value()),
                ]),
                ListValue::new(vec![
                    Some(123.to_scalar_value()),
                    Some(789.to_scalar_value()),
                ]),
                DataType::Int32,
                Ordering::Less,
            ),
            (
                ListValue::new(vec![
                    Some(123.to_scalar_value()),
                    Some(456.to_scalar_value()),
                ]),
                ListValue::new(vec![Some(123.to_scalar_value())]),
                DataType::Int32,
                Ordering::Greater,
            ),
            (
                ListValue::new(vec![None, Some("".into())]),
                ListValue::new(vec![None, None]),
                DataType::Varchar,
                Ordering::Less,
            ),
            (
                ListValue::new(vec![Some(2.to_scalar_value())]),
                ListValue::new(vec![
                    Some(1.to_scalar_value()),
                    None,
                    Some(3.to_scalar_value()),
                ]),
                DataType::Int32,
                Ordering::Greater,
            ),
        ];

        for (lhs, rhs, datatype, order) in cases {
            let lhs_serialized = {
                let mut serializer = memcomparable::Serializer::new(vec![]);
                ListRef::ValueRef { val: &lhs }
                    .memcmp_serialize(&mut serializer)
                    .unwrap();
                serializer.into_inner()
            };
            let rhs_serialized = {
                let mut serializer = memcomparable::Serializer::new(vec![]);
                ListRef::ValueRef { val: &rhs }
                    .memcmp_serialize(&mut serializer)
                    .unwrap();
                serializer.into_inner()
            };
            assert_eq!(lhs_serialized.cmp(&rhs_serialized), order);

            let mut builder = ListArrayBuilder::with_type(0, DataType::List(Box::new(datatype)));
            builder.append(Some(ListRef::ValueRef { val: &lhs }));
            builder.append(Some(ListRef::ValueRef { val: &rhs }));
            let array = builder.finish();
            let lhs_serialized = {
                let mut serializer = memcomparable::Serializer::new(vec![]);
                array
                    .value_at(0)
                    .unwrap()
                    .memcmp_serialize(&mut serializer)
                    .unwrap();
                serializer.into_inner()
            };
            let rhs_serialized = {
                let mut serializer = memcomparable::Serializer::new(vec![]);
                array
                    .value_at(1)
                    .unwrap()
                    .memcmp_serialize(&mut serializer)
                    .unwrap();
                serializer.into_inner()
            };
            assert_eq!(lhs_serialized.cmp(&rhs_serialized), order);
        }
    }

    #[test]
    fn test_listref() {
        use crate::array::*;
        use crate::types;
        let arr = ListArray::from_iter(
            [
                Some(I32Array::from_iter([Some(1), Some(2), Some(3)]).into()),
                None,
                Some(I32Array::from_iter([Some(4), Some(5), Some(6), Some(7)]).into()),
            ],
            DataType::Int32,
        );

        // get 3rd ListRef from ListArray
        let list_ref = arr.value_at(2).unwrap();
        assert_eq!(
            list_ref,
            ListRef::ValueRef {
                val: &ListValue::new(vec![
                    Some(4.to_scalar_value()),
                    Some(5.to_scalar_value()),
                    Some(6.to_scalar_value()),
                    Some(7.to_scalar_value()),
                ]),
            }
        );

        // Get 2nd value from ListRef
        let scalar = list_ref.elem_at(1).unwrap();
        assert_eq!(scalar, Some(types::ScalarRefImpl::Int32(5)));
    }
}
