use std::{
    any::TypeId,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use bitflags::bitflags;
use ecow::{EcoString, EcoVec};
use serde::{de::DeserializeOwned, *};

use crate::{
    algorithm::map::{MapKeys, EMPTY_NAN, TOMBSTONE_NAN},
    cowslice::{cowslice, CowSlice},
    fill::Fill,
    grid_fmt::{ElemAlign, GridFmt},
    Boxed, Complex, ExactDoubleIterator, HandleKind, Shape, Value,
};

/// Uiua's array type
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    from = "ArrayRep<T>",
    into = "ArrayRep<T>",
    bound(
        serialize = "T: ArrayValueSer + Serialize",
        deserialize = "T: ArrayValueSer + Deserialize<'de>"
    )
)]
#[repr(C)]
pub struct Array<T> {
    pub(crate) shape: Shape,
    pub(crate) data: CowSlice<T>,
    pub(crate) meta: Option<Arc<ArrayMeta>>,
}

/// Non-shape metadata for an array
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrayMeta {
    /// The label
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<EcoString>,
    /// Flags for the array
    #[serde(default, skip_serializing_if = "ArrayFlags::is_empty")]
    pub flags: ArrayFlags,
    /// The keys of a map array
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_keys: Option<MapKeys>,
    /// The pointer value for FFI
    #[serde(skip)]
    pub pointer: Option<MetaPtr>,
    /// The kind of system handle
    #[serde(skip)]
    pub handle_kind: Option<HandleKind>,
}

/// Array pointer metadata
#[derive(Debug, Clone, Copy)]
pub struct MetaPtr {
    /// The pointer value
    pub ptr: usize,
    /// Whether the pointer should prevent the array's value from being shown
    pub raw: bool,
}

impl MetaPtr {
    /// Get a null metadata pointer
    pub const fn null() -> Self {
        Self { ptr: 0, raw: true }
    }
    /// Create a new metadata pointer
    pub fn new<T: ?Sized>(ptr: *const T, raw: bool) -> Self {
        Self {
            ptr: ptr as *const () as usize,
            raw,
        }
    }
    /// Get the pointer as a raw pointer
    pub fn get<T>(&self) -> *const T {
        self.ptr as *const T
    }
    /// Get the pointer as a raw pointer
    pub fn get_mut<T>(&self) -> *mut T {
        self.ptr as *mut T
    }
}

impl PartialEq for MetaPtr {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl Eq for MetaPtr {}

bitflags! {
    /// Flags for an array
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct ArrayFlags: u8 {
        /// No flags
        const NONE = 0;
        /// The array is boolean
        const BOOLEAN = 1;
        /// The array was *created from* a boolean
        const BOOLEAN_LITERAL = 2;
    }
}

impl ArrayFlags {
    /// Check if the array is boolean
    pub fn is_boolean(self) -> bool {
        self.contains(Self::BOOLEAN)
    }
    /// Reset all flags
    pub fn reset(&mut self) {
        *self = Self::NONE;
    }
}

/// Default metadata for an array
pub static DEFAULT_META: ArrayMeta = ArrayMeta {
    label: None,
    flags: ArrayFlags::NONE,
    map_keys: None,
    pointer: None,
    handle_kind: None,
};

/// Array metadata that can be persisted across operations
#[derive(Clone, Default)]
pub struct PersistentMeta {
    label: Option<EcoString>,
    map_keys: Option<MapKeys>,
}

impl PersistentMeta {
    /// XOR this metadata with another
    pub fn xor(self, other: Self) -> Self {
        Self {
            label: self.label.xor(other.label),
            map_keys: self.map_keys.xor(other.map_keys),
        }
    }
    /// XOR several metadatas
    pub fn xor_all(metas: impl IntoIterator<Item = Self>) -> Self {
        let mut label = None;
        let mut map_keys = None;
        let mut set_label = false;
        let mut set_map_keys = false;
        for meta in metas {
            if let Some(l) = meta.label {
                if set_label {
                    label = None;
                } else {
                    label = Some(l);
                    set_label = true;
                }
            }
            if let Some(keys) = meta.map_keys {
                if set_map_keys {
                    map_keys = None;
                } else {
                    map_keys = Some(keys);
                    set_map_keys = true;
                }
            }
        }
        Self { label, map_keys }
    }
}

impl<T: ArrayValue> Default for Array<T> {
    fn default() -> Self {
        Self {
            shape: 0.into(),
            data: CowSlice::new(),
            meta: None,
        }
    }
}

impl<T: ArrayValue> fmt::Debug for Array<T>
where
    Array<T>: GridFmt,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.grid_string(true))
    }
}

impl<T: ArrayValue> fmt::Display for Array<T>
where
    Array<T>: GridFmt,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rank() {
            0 => write!(f, "{}", self.data[0]),
            1 => {
                let (start, end) = T::format_delims();
                write!(f, "{}", start)?;
                for (i, x) in self.data.iter().enumerate() {
                    if i > 0 {
                        write!(f, "{}", T::format_sep())?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "{}", end)
            }
            _ => {
                write!(f, "\n{}", self.grid_string(false))
            }
        }
    }
}

#[track_caller]
#[inline(always)]
pub(crate) fn validate_shape(shape: &[usize], len: usize) {
    let elems = if shape.contains(&0) {
        0
    } else {
        shape.iter().product()
    };
    debug_assert_eq!(
        elems, len,
        "shape {shape:?} does not match data length {}",
        len
    );
}

impl<T> Array<T> {
    #[track_caller]
    /// Create an array from a shape and data
    ///
    /// # Panics
    /// Panics in debug mode if the shape does not match the data length
    pub fn new(shape: impl Into<Shape>, data: impl Into<CowSlice<T>>) -> Self {
        let shape = shape.into();
        let data = data.into();
        validate_shape(&shape, data.len());
        Self {
            shape,
            data,
            meta: None,
        }
    }
    #[track_caller]
    #[inline(always)]
    /// Debug-only function to validate that the shape matches the data length
    pub(crate) fn validate_shape(&self) {
        validate_shape(&self.shape, self.data.len());
    }
    /// Get the number of rows in the array
    pub fn row_count(&self) -> usize {
        self.shape.first().copied().unwrap_or(1)
    }
    /// Get the number of elements in the array
    pub fn element_count(&self) -> usize {
        self.data.len()
    }
    /// Get the number of elements in a row
    pub fn row_len(&self) -> usize {
        self.shape.iter().skip(1).product()
    }
    /// Get the rank of the array
    pub fn rank(&self) -> usize {
        self.shape.len()
    }
    /// Get the shape of the array
    pub fn shape(&self) -> &Shape {
        &self.shape
    }
    /// Get a mutable reference to the shape of the array
    pub fn shape_mut(&mut self) -> &mut Shape {
        &mut self.shape
    }
    /// Iterate over the elements of the array
    pub fn elements(&self) -> impl ExactDoubleIterator<Item = &T> {
        self.data.iter()
    }
    /// Get the metadata of the array
    pub fn meta(&self) -> &ArrayMeta {
        self.meta.as_deref().unwrap_or(&DEFAULT_META)
    }
    pub(crate) fn meta_mut_impl(meta: &mut Option<Arc<ArrayMeta>>) -> &mut ArrayMeta {
        let meta = meta.get_or_insert_with(Default::default);
        Arc::make_mut(meta)
    }
    /// Get a mutable reference to the metadata of the array if it exists
    pub fn get_meta_mut(&mut self) -> Option<&mut ArrayMeta> {
        self.meta.as_mut().map(Arc::make_mut)
    }
    /// Get a mutable reference to the metadata of the array
    pub fn meta_mut(&mut self) -> &mut ArrayMeta {
        Self::meta_mut_impl(&mut self.meta)
    }
    /// Take the label from the metadata
    pub fn take_label(&mut self) -> Option<EcoString> {
        self.get_meta_mut().and_then(|meta| meta.label.take())
    }
    /// Take the map keys from the metadata
    pub fn take_map_keys(&mut self) -> Option<MapKeys> {
        self.get_meta_mut().and_then(|meta| meta.map_keys.take())
    }
    /// The the persistent metadata of the array
    pub fn take_per_meta(&mut self) -> PersistentMeta {
        if let Some(meta) = self.get_meta_mut() {
            let label = meta.label.take();
            let map_keys = meta.map_keys.take();
            PersistentMeta { label, map_keys }
        } else {
            PersistentMeta::default()
        }
    }
    /// Set the map keys in the metadata
    pub fn set_per_meta(&mut self, per_meta: PersistentMeta) {
        if let Some(keys) = per_meta.map_keys {
            self.meta_mut().map_keys = Some(keys);
        } else if let Some(meta) = self.get_meta_mut() {
            meta.map_keys = None;
        }
        if let Some(label) = per_meta.label {
            self.meta_mut().label = Some(label);
        } else if let Some(meta) = self.get_meta_mut() {
            meta.label = None;
        }
    }
    /// Get a reference to the map keys
    pub fn map_keys(&self) -> Option<&MapKeys> {
        self.meta().map_keys.as_ref()
    }
    /// Get a mutable reference to the map keys
    pub fn map_keys_mut(&mut self) -> Option<&mut MapKeys> {
        self.get_meta_mut().and_then(|meta| meta.map_keys.as_mut())
    }
    /// Reset all metadata
    pub fn reset_meta(&mut self) {
        self.meta = None;
    }
    /// Reset all metadata flags
    pub fn reset_meta_flags(&mut self) {
        if self.meta.is_some() {
            self.meta_mut().flags.reset();
        }
    }
    /// Get an iterator over the row slices of the array
    pub fn row_slices(
        &self,
    ) -> impl ExactSizeIterator<Item = &[T]> + DoubleEndedIterator + Clone + Send + Sync
    where
        T: Send + Sync,
    {
        (0..self.row_count()).map(move |row| self.row_slice(row))
    }
    /// Get a slice of a row
    #[track_caller]
    pub fn row_slice(&self, row: usize) -> &[T] {
        let row_len = self.row_len();
        &self.data[row * row_len..(row + 1) * row_len]
    }
    /// Combine the metadata of two arrays
    ///
    /// This combines:
    /// - flags
    /// - map keys
    /// - handle kind
    ///
    /// Notably, this does not combine the label, as label
    /// combination should be more nuanced.
    pub fn combine_meta(&mut self, other: &ArrayMeta) {
        if let Some(meta) = self.get_meta_mut() {
            meta.flags &= other.flags;
            meta.map_keys = None;
            if meta.handle_kind != other.handle_kind {
                meta.handle_kind = None;
            }
        }
    }
}

impl<T: ArrayValue> Array<T> {
    /// Create a scalar array
    pub fn scalar(data: T) -> Self {
        Self::new(Shape::SCALAR, cowslice![data])
    }
    /// Attempt to convert the array into a scalar
    pub fn into_scalar(self) -> Result<T, Self> {
        if self.shape.is_empty() {
            Ok(self.data.into_iter().next().unwrap())
        } else {
            Err(self)
        }
    }
    /// Attempt to get a reference to the scalar value
    pub fn as_scalar(&self) -> Option<&T> {
        if self.shape.is_empty() {
            Some(&self.data[0])
        } else {
            None
        }
    }
    /// Attempt to get a mutable reference to the scalar value
    pub fn as_scalar_mut(&mut self) -> Option<&mut T> {
        if self.shape.is_empty() {
            Some(&mut self.data.as_mut_slice()[0])
        } else {
            None
        }
    }
    /// Get an iterator over the row arrays of the array
    pub fn rows(&self) -> impl ExactSizeIterator<Item = Self> + DoubleEndedIterator + '_ {
        (0..self.row_count()).map(|row| self.row(row))
    }
    pub(crate) fn row_shaped_slice(&self, index: usize, row_shape: Shape) -> Self {
        let row_len = row_shape.elements();
        let start = index * row_len;
        let end = start + row_len;
        Self::new(row_shape, self.data.slice(start..end))
    }
    /// Get an iterator over the row arrays of the array that have the given shape
    pub fn row_shaped_slices(
        &self,
        row_shape: Shape,
    ) -> impl ExactSizeIterator<Item = Self> + DoubleEndedIterator + '_ {
        let row_len = row_shape.elements();
        let row_count = self.element_count() / row_len;
        (0..row_count).map(move |i| {
            let start = i * row_len;
            let end = start + row_len;
            Self::new(row_shape.clone(), self.data.slice(start..end))
        })
    }
    /// Get an iterator over the row arrays of the array that have the given shape
    pub fn into_row_shaped_slices(self, row_shape: Shape) -> impl DoubleEndedIterator<Item = Self> {
        let row_len = row_shape.elements();
        let zero_count = if row_len == 0 { self.row_count() } else { 0 };
        let row_sh = row_shape.clone();
        let nonzero = self
            .data
            .into_slices(row_len)
            .map(move |data| Self::new(row_sh.clone(), data));
        let zero = (0..zero_count).map(move |_| Self::new(row_shape.clone(), CowSlice::new()));
        nonzero.chain(zero)
    }
    /// Get a row array
    #[track_caller]
    pub fn row(&self, row: usize) -> Self {
        if self.rank() == 0 {
            let mut row = self.clone();
            row.take_map_keys();
            row.take_label();
            return row;
        }
        let row_count = self.row_count();
        if row >= row_count {
            panic!("row index out of bounds: {} >= {}", row, row_count);
        }
        let row_len = self.row_len();
        let start = row * row_len;
        let end = start + row_len;
        let mut row = Self::new(&self.shape[1..], self.data.slice(start..end));
        if self.meta().flags != ArrayFlags::NONE {
            row.meta_mut().flags = self.meta().flags;
        }
        row
    }
    #[track_caller]
    pub(crate) fn depth_row(&self, depth: usize, row: usize) -> Self {
        if self.rank() <= depth {
            let mut row = self.clone();
            row.take_map_keys();
            row.take_label();
            return row;
        }
        let row_count: usize = self.shape[..depth + 1].iter().product();
        if row >= row_count {
            panic!("row index out of bounds: {} >= {}", row, row_count);
        }
        let row_len: usize = self.shape[depth + 1..].iter().product();
        let start = row * row_len;
        let end = start + row_len;
        Self::new(&self.shape[depth + 1..], self.data.slice(start..end))
    }
    #[track_caller]
    pub(crate) fn slice_rows(&self, start: usize, end: usize) -> Self {
        assert!(start <= end);
        assert!(start < self.row_count());
        assert!(end <= self.row_count());
        let row_len = self.row_len();
        let mut shape = self.shape.clone();
        shape[0] = end - start;
        let start = start * row_len;
        let end = end * row_len;
        Self::new(shape, self.data.slice(start..end))
    }
    /// Consume the array and get an iterator over its rows
    pub fn into_rows(self) -> impl ExactSizeIterator<Item = Self> + DoubleEndedIterator {
        (0..self.row_count()).map(move |i| self.row(i))
    }
    pub(crate) fn first_dim_zero(&self) -> Self {
        if self.rank() == 0 {
            return self.clone();
        }
        let mut shape = self.shape.clone();
        shape[0] = 0;
        Array::new(shape, CowSlice::new())
    }
    /// Get a pretty-printed string representing the array
    ///
    /// This is what is printed by the `&s` function
    pub fn show(&self) -> String {
        self.grid_string(true)
    }
    pub(crate) fn pop_row(&mut self) -> Option<Self> {
        if self.row_count() == 0 {
            return None;
        }
        let data = self.data.split_off(self.data.len() - self.row_len());
        self.shape[0] -= 1;
        let shape: Shape = self.shape[1..].into();
        self.validate_shape();
        Some(Self::new(shape, data))
    }
    /// Get a mutable slice of a row
    #[track_caller]
    pub fn row_slice_mut(&mut self, row: usize) -> &mut [T] {
        let row_len = self.row_len();
        &mut self.data.as_mut_slice()[row * row_len..(row + 1) * row_len]
    }
}

impl<T: Clone> Array<T> {
    /// Convert the elements of the array
    #[inline(always)]
    pub fn convert<U>(self) -> Array<U>
    where
        T: Into<U> + 'static,
        U: Clone + 'static,
    {
        if TypeId::of::<T>() == TypeId::of::<U>() {
            unsafe { std::mem::transmute::<Array<T>, Array<U>>(self) }
        } else {
            self.convert_with(Into::into)
        }
    }
    /// Convert the elements of the array with a function
    pub fn convert_with<U: Clone>(self, f: impl FnMut(T) -> U) -> Array<U> {
        Array {
            shape: self.shape,
            data: self.data.into_iter().map(f).collect(),
            meta: self.meta,
        }
    }
    /// Convert the elements of the array with a fallible function
    pub fn try_convert_with<U: Clone, E>(
        self,
        f: impl FnMut(T) -> Result<U, E>,
    ) -> Result<Array<U>, E> {
        Ok(Array {
            shape: self.shape,
            data: self.data.into_iter().map(f).collect::<Result<_, _>>()?,
            meta: self.meta,
        })
    }
    /// Convert the elements of the array without consuming it
    pub fn convert_ref<U>(&self) -> Array<U>
    where
        T: Into<U>,
        U: Clone,
    {
        self.convert_ref_with(Into::into)
    }
    /// Convert the elements of the array with a function without consuming it
    pub fn convert_ref_with<U: Clone>(&self, f: impl FnMut(T) -> U) -> Array<U> {
        Array {
            shape: self.shape.clone(),
            data: self.data.iter().cloned().map(f).collect(),
            meta: self.meta.clone(),
        }
    }
}

impl Array<u8> {
    pub(crate) fn json_bool(b: bool) -> Self {
        let mut arr = Self::from(b);
        arr.meta_mut().flags |= ArrayFlags::BOOLEAN_LITERAL;
        arr
    }
}

impl Array<Boxed> {
    /// Attempt to unbox a scalar box array
    pub fn into_unboxed(self) -> Result<Value, Self> {
        match self.into_scalar() {
            Ok(v) => Ok(v.0),
            Err(a) => Err(a),
        }
    }
    /// Attempt to unbox a scalar box array
    pub fn as_unboxed(&self) -> Option<&Value> {
        self.as_scalar().map(|v| &v.0)
    }
    /// Attempt to unbox a scalar box array
    pub fn as_unboxed_mut(&mut self) -> Option<&mut Value> {
        self.as_scalar_mut().map(|v| &mut v.0)
    }
}

impl<T: ArrayValue + ArrayCmp<U>, U: ArrayValue> PartialEq<Array<U>> for Array<T> {
    fn eq(&self, other: &Array<U>) -> bool {
        if self.shape() != other.shape() {
            return false;
        }
        if self.map_keys() != other.map_keys() {
            return false;
        }
        self.data
            .iter()
            .zip(&other.data)
            .all(|(a, b)| a.array_eq(b))
    }
}

impl<T: ArrayValue> Eq for Array<T> {}

impl<T: ArrayValue + ArrayCmp<U>, U: ArrayValue> PartialOrd<Array<U>> for Array<T> {
    fn partial_cmp(&self, other: &Array<U>) -> Option<Ordering> {
        let rank_cmp = self.rank().cmp(&other.rank());
        if rank_cmp != Ordering::Equal {
            return Some(rank_cmp);
        }
        let cmp = self
            .data
            .iter()
            .zip(&other.data)
            .map(|(a, b)| a.array_cmp(b))
            .find(|o| o != &Ordering::Equal)
            .unwrap_or_else(|| self.shape.cmp(&other.shape));
        Some(cmp)
    }
}

impl<T: ArrayValue> Ord for Array<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl<T: ArrayValue> Hash for Array<T> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        if let Some(keys) = self.map_keys() {
            keys.hash(hasher);
        }
        if let Some(scalar) = self.as_scalar() {
            if let Some(value) = scalar.nested_value() {
                value.hash(hasher);
                return;
            }
        }
        T::TYPE_ID.hash(hasher);
        self.shape.hash(hasher);
        self.data.iter().for_each(|x| x.array_hash(hasher));
    }
}

impl<T: ArrayValue> From<T> for Array<T> {
    fn from(data: T) -> Self {
        Self::scalar(data)
    }
}

impl<T: ArrayValue> From<EcoVec<T>> for Array<T> {
    fn from(data: EcoVec<T>) -> Self {
        Self::new(data.len(), data)
    }
}

impl<T: ArrayValue> From<CowSlice<T>> for Array<T> {
    fn from(data: CowSlice<T>) -> Self {
        Self::new(data.len(), data)
    }
}

impl<'a, T: ArrayValue> From<&'a [T]> for Array<T> {
    fn from(data: &'a [T]) -> Self {
        Self::new(data.len(), data)
    }
}

impl<T: ArrayValue> FromIterator<T> for Array<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<CowSlice<T>>())
    }
}

impl From<String> for Array<char> {
    fn from(s: String) -> Self {
        Self::new(s.len(), s.chars().collect::<CowSlice<_>>())
    }
}

impl From<Vec<bool>> for Array<u8> {
    fn from(data: Vec<bool>) -> Self {
        Self::new(
            data.len(),
            data.into_iter().map(u8::from).collect::<CowSlice<_>>(),
        )
    }
}

impl From<bool> for Array<u8> {
    fn from(data: bool) -> Self {
        let mut arr = Self::new(Shape::SCALAR, cowslice![u8::from(data)]);
        arr.meta_mut().flags |= ArrayFlags::BOOLEAN;
        arr
    }
}

impl From<Vec<usize>> for Array<f64> {
    fn from(data: Vec<usize>) -> Self {
        Self::new(
            data.len(),
            data.into_iter().map(|u| u as f64).collect::<CowSlice<_>>(),
        )
    }
}

impl FromIterator<String> for Array<Boxed> {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Array::from(
            iter.into_iter()
                .map(Value::from)
                .map(Boxed)
                .collect::<CowSlice<_>>(),
        )
    }
}

impl<'a> FromIterator<&'a str> for Array<Boxed> {
    fn from_iter<I: IntoIterator<Item = &'a str>>(iter: I) -> Self {
        Array::from(
            iter.into_iter()
                .map(Value::from)
                .map(Boxed)
                .collect::<CowSlice<_>>(),
        )
    }
}

/// A trait for types that can be used as array elements
#[allow(unused_variables)]
pub trait ArrayValue:
    Default + Clone + fmt::Debug + fmt::Display + GridFmt + ArrayCmp + Send + Sync + 'static
{
    /// The type name
    const NAME: &'static str;
    /// A glyph indicating the type
    const SYMBOL: char;
    /// An ID for the type
    const TYPE_ID: u8;
    /// Get the scalar fill value from the environment
    fn get_scalar_fill(fill: &Fill) -> Result<Self, &'static str>;
    /// Get the array fill value from the environment
    fn get_array_fill(fill: &Fill) -> Result<Array<Self>, &'static str>;
    /// Hash the value
    fn array_hash<H: Hasher>(&self, hasher: &mut H);
    /// Get the proxy value
    fn proxy() -> Self;
    /// Delimiters for formatting
    fn format_delims() -> (&'static str, &'static str) {
        ("[", "]")
    }
    /// Marker for empty lists in grid formatting
    fn empty_list_inner() -> &'static str {
        ""
    }
    /// Separator for formatting
    fn format_sep() -> &'static str {
        " "
    }
    /// Delimiters for grid formatting
    fn grid_fmt_delims(boxed: bool) -> (char, char) {
        if boxed {
            ('⟦', '⟧')
        } else {
            ('[', ']')
        }
    }
    /// Whether to compress all items of a list when grid formatting
    fn compress_list_grid() -> bool {
        false
    }
    /// Get a nested value
    fn nested_value(&self) -> Option<&Value> {
        None
    }
    /// Summarize the elements of an array of this type
    fn summarize(elems: &[Self]) -> String {
        String::new()
    }
    /// The minimum number of elements that require a summary
    fn summary_min_elems() -> usize {
        3600
    }
    /// How to align elements when formatting
    fn alignment() -> ElemAlign {
        ElemAlign::Left
    }
    /// How to determine the maximum width of a formatted column
    fn max_col_width<'a>(rows: impl Iterator<Item = &'a [char]> + Clone) -> usize {
        rows.map(|row| row.len()).max().unwrap_or(0)
    }
}

impl ArrayValue for f64 {
    const NAME: &'static str = "number";
    const SYMBOL: char = 'ℝ';
    const TYPE_ID: u8 = 0;
    fn get_scalar_fill(fill: &Fill) -> Result<Self, &'static str> {
        fill.num_scalar()
    }
    fn get_array_fill(fill: &Fill) -> Result<Array<Self>, &'static str> {
        fill.num_array()
    }
    fn array_hash<H: Hasher>(&self, hasher: &mut H) {
        let v = if self.to_bits() == EMPTY_NAN.to_bits() {
            EMPTY_NAN
        } else if self.to_bits() == TOMBSTONE_NAN.to_bits() {
            TOMBSTONE_NAN
        } else if self.is_nan() {
            f64::NAN
        } else if *self == 0.0 && self.is_sign_negative() {
            0.0
        } else {
            *self
        };
        v.to_bits().hash(hasher)
    }
    fn proxy() -> Self {
        0.0
    }
    fn summarize(elems: &[Self]) -> String {
        if elems.is_empty() {
            return String::new();
        }
        if elems.iter().all(|&n| n.is_nan()) {
            return "all NaN".into();
        }
        let mut min = f64::NAN;
        let mut max = f64::NAN;
        for &elem in elems {
            min = min.min(elem);
            max = max.max(elem);
        }
        let mut nan_count = elems.iter().take_while(|n| n.is_nan()).count();
        let mut mean = elems[nan_count];
        let mut i = 1;
        for elem in &elems[nan_count + 1..] {
            if elem.is_nan() {
                nan_count += 1;
            } else {
                mean += (elem - mean) / (i + 1) as f64;
                i += 1;
            }
        }
        if min == max {
            format!("all {}", min.grid_string(false))
        } else {
            let mut s = format!(
                "{}-{} x̄{}",
                min.grid_string(false),
                max.grid_string(false),
                mean.grid_string(false)
            );
            if nan_count > 0 {
                s.push_str(&format!(
                    " ({nan_count} NaN{})",
                    if nan_count > 1 { "s" } else { "" }
                ));
            }
            s
        }
    }
    fn alignment() -> ElemAlign {
        ElemAlign::DelimOrRight(".")
    }
    fn max_col_width<'a>(rows: impl Iterator<Item = &'a [char]>) -> usize {
        let mut max_whole_len = 0;
        let mut max_dec_len: Option<usize> = None;
        for row in rows {
            if let Some(dot_pos) = row.iter().position(|&c| c == '.') {
                max_whole_len = max_whole_len.max(dot_pos);
                max_dec_len = max_dec_len.max(Some(row.len() - dot_pos - 1));
            } else {
                max_whole_len = max_whole_len.max(row.len());
            }
        }
        if let Some(dec_len) = max_dec_len {
            max_whole_len + dec_len + 1
        } else {
            max_whole_len
        }
    }
}

#[cfg(test)]
#[test]
fn f64_summarize() {
    assert_eq!(f64::summarize(&[2.0, 6.0, 1.0]), "1-6 x̄3");
}

impl ArrayValue for u8 {
    const NAME: &'static str = "number";
    const SYMBOL: char = 'ℝ';
    const TYPE_ID: u8 = 0;
    fn get_scalar_fill(fill: &Fill) -> Result<Self, &'static str> {
        fill.byte_scalar()
    }
    fn get_array_fill(fill: &Fill) -> Result<Array<Self>, &'static str> {
        fill.byte_array()
    }
    fn array_hash<H: Hasher>(&self, hasher: &mut H) {
        (*self as f64).to_bits().hash(hasher)
    }
    fn proxy() -> Self {
        0
    }
    fn summarize(elems: &[Self]) -> String {
        if elems.is_empty() {
            return String::new();
        }
        let mut min = u8::MAX;
        let mut max = 0;
        for &elem in elems {
            min = min.min(elem);
            max = max.max(elem);
        }
        let mut mean = elems[0] as f64;
        for (i, &elem) in elems.iter().enumerate().skip(1) {
            mean += (elem as f64 - mean) / (i + 1) as f64;
        }
        if min == max {
            format!("all {}", min.grid_string(false))
        } else {
            format!(
                "{}-{} x̄{}",
                min.grid_string(false),
                max.grid_string(false),
                mean.grid_string(false)
            )
        }
    }
    fn alignment() -> ElemAlign {
        ElemAlign::Right
    }
}

impl ArrayValue for char {
    const NAME: &'static str = "character";
    const SYMBOL: char = '@';
    const TYPE_ID: u8 = 1;
    fn get_scalar_fill(fill: &Fill) -> Result<Self, &'static str> {
        fill.char_scalar()
    }
    fn get_array_fill(fill: &Fill) -> Result<Array<Self>, &'static str> {
        fill.char_array()
    }
    fn format_delims() -> (&'static str, &'static str) {
        ("", "")
    }
    fn format_sep() -> &'static str {
        ""
    }
    fn array_hash<H: Hasher>(&self, hasher: &mut H) {
        self.hash(hasher)
    }
    fn proxy() -> Self {
        ' '
    }
    fn grid_fmt_delims(boxed: bool) -> (char, char) {
        if boxed {
            ('⌜', '⌟')
        } else {
            ('"', '"')
        }
    }
    fn compress_list_grid() -> bool {
        true
    }
    fn summarize(elems: &[Self]) -> String {
        if elems.is_empty() {
            return String::new();
        }
        let mut parts = Vec::new();
        let lowercase = elems.iter().any(|c| c.is_lowercase());
        let uppercase = elems.iter().any(|c| c.is_uppercase());
        let writing = elems
            .iter()
            .any(|c| c.is_alphabetic() && !(c.is_lowercase() || c.is_uppercase()));
        let numeric = elems.iter().any(|c| c.is_numeric() && !c.is_ascii_digit());
        let digit = elems.iter().any(|c| c.is_ascii_digit());
        let punct = elems.iter().any(|c| c.is_ascii_punctuation());
        let whitespace = elems.iter().any(|c| c.is_whitespace());
        let control = elems.iter().any(|c| c.is_control());
        let other = (elems.iter()).any(|c| {
            !(c.is_lowercase()
                || c.is_uppercase()
                || c.is_alphabetic()
                || c.is_numeric()
                || c.is_ascii_punctuation()
                || c.is_whitespace()
                || c.is_control())
        });
        if writing {
            parts.push("writing");
        } else if lowercase && uppercase {
            parts.push("letters");
        } else if lowercase {
            parts.push("lower");
        } else if uppercase {
            parts.push("upper");
        }
        if numeric {
            parts.push("nums");
        }
        if digit {
            parts.push("digits");
        }
        if punct {
            parts.push("punct");
        }
        if whitespace {
            parts.push("whitespace");
        }
        if control {
            parts.push("control");
        }
        if other {
            parts.push("other");
        }
        match parts.len() {
            0 => String::new(),
            1 => parts[0].to_string(),
            2 => format!("{} and {}", parts[0], parts[1]),
            _ => {
                let mut s = String::new();
                for (i, &part) in parts.iter().enumerate() {
                    if i > 0 {
                        s.push_str(", ");
                    }
                    s.push_str(part);
                }
                s
            }
        }
    }
}

impl ArrayValue for Boxed {
    const NAME: &'static str = "box";
    const SYMBOL: char = '□';
    const TYPE_ID: u8 = 2;
    fn get_scalar_fill(fill: &Fill) -> Result<Self, &'static str> {
        fill.box_scalar()
    }
    fn get_array_fill(fill: &Fill) -> Result<Array<Self>, &'static str> {
        fill.box_array()
    }
    fn array_hash<H: Hasher>(&self, hasher: &mut H) {
        self.0.hash(hasher);
    }
    fn proxy() -> Self {
        Boxed(Array::<f64>::new(0, []).into())
    }
    fn empty_list_inner() -> &'static str {
        "□"
    }
    fn nested_value(&self) -> Option<&Value> {
        Some(&self.0)
    }
    fn summarize(elems: &[Self]) -> String {
        if elems.is_empty() {
            return String::new();
        }
        let smallest_rank = elems.iter().map(|e| e.0.rank()).min().unwrap();
        let largest_rank = elems.iter().map(|e| e.0.rank()).max().unwrap();
        let smallest_shape = (elems.iter().map(|e| e.0.shape()))
            .min_by_key(|s| s.elements())
            .unwrap();
        let largest_shape = (elems.iter().map(|e| e.0.shape()))
            .max_by_key(|s| s.elements())
            .unwrap();
        let rank_summary = if smallest_rank == largest_rank {
            format!("all rank {smallest_rank}")
        } else {
            format!("ranks {smallest_rank}-{largest_rank}")
        };
        let shape_summary = if smallest_shape == largest_shape {
            format!("all shape {smallest_shape}")
        } else {
            format!("shapes {smallest_shape}-{largest_shape}")
        };
        format!("{rank_summary}, {shape_summary}")
    }
    fn summary_min_elems() -> usize {
        1000
    }
    fn alignment() -> ElemAlign {
        ElemAlign::DelimOrLeft(": ")
    }
    fn max_col_width<'a>(rows: impl Iterator<Item = &'a [char]>) -> usize {
        let mut max_val_len = 0;
        let mut max_label_len: Option<usize> = None;
        for row in rows {
            if let Some(delim_pos) = (0..row.len()).find(|&i| row[i..].starts_with(&[':', ' '])) {
                max_val_len = max_val_len.max(row.len() - delim_pos - 2);
                max_label_len = max_label_len.max(Some(delim_pos));
            } else {
                max_val_len = max_val_len.max(row.len());
            }
        }
        if let Some(label_len) = max_label_len {
            max_val_len + label_len + 2
        } else {
            max_val_len
        }
    }
}

impl ArrayValue for Complex {
    const NAME: &'static str = "complex";
    const SYMBOL: char = 'ℂ';
    const TYPE_ID: u8 = 3;
    fn get_scalar_fill(fill: &Fill) -> Result<Self, &'static str> {
        fill.complex_scalar()
    }
    fn get_array_fill(fill: &Fill) -> Result<Array<Self>, &'static str> {
        fill.complex_array()
    }
    fn array_hash<H: Hasher>(&self, hasher: &mut H) {
        for n in [self.re, self.im] {
            n.array_hash(hasher);
        }
    }
    fn proxy() -> Self {
        Complex::new(0.0, 0.0)
    }
    fn empty_list_inner() -> &'static str {
        "ℂ"
    }
    fn summarize(elems: &[Self]) -> String {
        if elems.is_empty() {
            return String::new();
        }
        let mut min = Complex::new(f64::INFINITY, f64::INFINITY);
        let mut max = Complex::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &elem in elems {
            min = min.min(elem);
            max = max.max(elem);
        }
        let mut mean = elems[0];
        for (i, &elem) in elems.iter().enumerate().skip(1) {
            mean = mean + (elem - mean) / (i + 1) as f64;
        }
        if min == max {
            format!("all {}", min.grid_string(false))
        } else {
            format!(
                "{} - {} x̄{}",
                min.grid_string(false),
                max.grid_string(false),
                mean.grid_string(false)
            )
        }
    }
}

/// Trait for [`ArrayValue`]s that are real numbers
pub trait RealArrayValue: ArrayValue + Copy {
    /// Whether the value is an integer
    fn is_int(&self) -> bool;
    /// Convert the value to an `f64`
    fn to_f64(&self) -> f64;
}

impl RealArrayValue for f64 {
    fn is_int(&self) -> bool {
        self.fract().abs() < f64::EPSILON
    }
    fn to_f64(&self) -> f64 {
        *self
    }
}

impl RealArrayValue for u8 {
    fn is_int(&self) -> bool {
        true
    }
    fn to_f64(&self) -> f64 {
        *self as f64
    }
}

/// Trait for comparing array elements
pub trait ArrayCmp<U = Self> {
    /// Compare two elements
    fn array_cmp(&self, other: &U) -> Ordering;
    /// Check if two elements are equal
    fn array_eq(&self, other: &U) -> bool {
        self.array_cmp(other) == Ordering::Equal
    }
}

impl ArrayCmp for f64 {
    fn array_eq(&self, other: &Self) -> bool {
        self == other
    }
    fn array_cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other)
            .unwrap_or(self.is_nan().cmp(&other.is_nan()))
    }
}

impl ArrayCmp for u8 {
    fn array_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl ArrayCmp for Complex {
    fn array_cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or_else(|| {
            (self.re.is_nan(), self.im.is_nan()).cmp(&(other.re.is_nan(), other.im.is_nan()))
        })
    }
}

impl ArrayCmp for char {
    fn array_eq(&self, other: &Self) -> bool {
        self == other
    }
    fn array_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl ArrayCmp for Boxed {
    fn array_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl ArrayCmp<f64> for u8 {
    fn array_cmp(&self, other: &f64) -> Ordering {
        (*self as f64).array_cmp(other)
    }
}

impl ArrayCmp<u8> for f64 {
    fn array_cmp(&self, other: &u8) -> Ordering {
        self.array_cmp(&(*other as f64))
    }
}

/// A formattable shape
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FormatShape<'a, T = usize>(pub &'a [T]);

impl<'a, T: fmt::Display> fmt::Debug for FormatShape<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl<'a, T: fmt::Display> fmt::Display for FormatShape<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, dim) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, " × ")?;
            }
            write!(f, "{dim}")?;
        }
        write!(f, "]")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(bound(
    serialize = "T: ArrayValueSer + Serialize",
    deserialize = "T: ArrayValueSer + Deserialize<'de>"
))]
enum ArrayRep<T: ArrayValueSer> {
    List(T::Collection),
    Scalar(T::Scalar),
    Map(Shape, Value, T::Collection),
    Metaless(Shape, T::Collection),
    Full(Shape, T::Collection, ArrayMeta),
}

impl<T: ArrayValueSer> From<ArrayRep<T>> for Array<T> {
    fn from(rep: ArrayRep<T>) -> Self {
        match rep {
            ArrayRep::Scalar(data) => Self::new([], [data.into()]),
            ArrayRep::List(data) => {
                let data = T::make_data(data);
                Self::new(data.len(), data)
            }
            ArrayRep::Map(shape, keys, data) => {
                let data = T::make_data(data);
                let mut arr = Self::new(shape, data);
                _ = arr.map(keys, &());
                arr
            }
            ArrayRep::Metaless(shape, data) => {
                let data = T::make_data(data);
                Self::new(shape, data)
            }
            ArrayRep::Full(shape, data, meta) => {
                let data = T::make_data(data);
                Self {
                    shape,
                    data,
                    meta: Some(meta.into()),
                }
            }
        }
    }
}

impl<T: ArrayValueSer> From<Array<T>> for ArrayRep<T> {
    fn from(mut arr: Array<T>) -> Self {
        if let Some(meta) = arr.meta.take().filter(|meta| **meta != DEFAULT_META) {
            let mut meta = Arc::unwrap_or_clone(meta);
            let map_keys = meta.map_keys.take();
            if meta == DEFAULT_META {
                if let Some(map_keys) = map_keys {
                    let keys = map_keys.normalized();
                    return ArrayRep::Map(arr.shape, keys, T::make_collection(arr.data));
                }
            } else {
                meta.map_keys = map_keys;
            }
            meta.flags &= !ArrayFlags::BOOLEAN;
            if meta != DEFAULT_META {
                return ArrayRep::Full(arr.shape, T::make_collection(arr.data), meta);
            }
        }
        match arr.rank() {
            0 if !T::no_scalar() => ArrayRep::Scalar(arr.data[0].clone().into()),
            1 => ArrayRep::List(T::make_collection(arr.data)),
            _ => ArrayRep::Metaless(arr.shape, T::make_collection(arr.data)),
        }
    }
}

trait ArrayValueSer: ArrayValue + fmt::Debug {
    type Scalar: Serialize + DeserializeOwned + fmt::Debug + From<Self> + Into<Self>;
    type Collection: Serialize + DeserializeOwned + fmt::Debug;
    fn make_collection(data: CowSlice<Self>) -> Self::Collection;
    fn make_data(collection: Self::Collection) -> CowSlice<Self>;
    /// Do not use the [`ArrayRep::Scalar`] variant
    fn no_scalar() -> bool {
        false
    }
}

macro_rules! array_value_ser {
    ($ty:ty) => {
        impl ArrayValueSer for $ty {
            type Scalar = $ty;
            type Collection = CowSlice<$ty>;
            fn make_collection(data: CowSlice<Self>) -> Self::Collection {
                data
            }
            fn make_data(collection: Self::Collection) -> CowSlice<Self> {
                collection
            }
        }
    };
}

array_value_ser!(u8);
array_value_ser!(Boxed);
array_value_ser!(Complex);

impl ArrayValueSer for f64 {
    type Scalar = F64Rep;
    type Collection = Vec<F64Rep>;
    fn make_collection(data: CowSlice<Self>) -> Self::Collection {
        data.iter().map(|&n| n.into()).collect()
    }
    fn make_data(collection: Self::Collection) -> CowSlice<Self> {
        collection.into_iter().map(f64::from).collect()
    }
}

impl ArrayValueSer for char {
    type Scalar = char;
    type Collection = String;
    fn make_collection(data: CowSlice<Self>) -> Self::Collection {
        data.iter().collect()
    }
    fn make_data(collection: Self::Collection) -> CowSlice<Self> {
        collection.chars().collect()
    }
    fn no_scalar() -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum F64Rep {
    #[serde(rename = "NaN")]
    NaN,
    #[serde(rename = "empty")]
    MapEmpty,
    #[serde(rename = "tomb")]
    MapTombstone,
    #[serde(rename = "∞")]
    Infinity,
    #[serde(rename = "-∞")]
    NegInfinity,
    #[serde(untagged)]
    Num(f64),
}

impl From<f64> for F64Rep {
    fn from(n: f64) -> Self {
        if n.is_nan() {
            if n.to_bits() == EMPTY_NAN.to_bits() {
                Self::MapEmpty
            } else if n.to_bits() == TOMBSTONE_NAN.to_bits() {
                Self::MapTombstone
            } else {
                Self::NaN
            }
        } else if n.is_infinite() {
            if n.is_sign_positive() {
                Self::Infinity
            } else {
                Self::NegInfinity
            }
        } else {
            Self::Num(n)
        }
    }
}

impl From<F64Rep> for f64 {
    fn from(rep: F64Rep) -> Self {
        match rep {
            F64Rep::NaN => f64::NAN,
            F64Rep::MapEmpty => EMPTY_NAN,
            F64Rep::MapTombstone => TOMBSTONE_NAN,
            F64Rep::Infinity => f64::INFINITY,
            F64Rep::NegInfinity => f64::NEG_INFINITY,
            F64Rep::Num(n) => n,
        }
    }
}
