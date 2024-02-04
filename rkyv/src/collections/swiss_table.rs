//! An archived hash map implementation based on Google's high-performance
//! SwissTable hash map.
//!
//! Notable differences from other implementations:
//!
//! - The number of control bytes is rounded up to a maximum group width (16)
//!   instead of the next power of two. This reduces the number of empty buckets
//!   on the wire. Since this collection is immutable after writing, we'll never
//!   benefit from having more buckets than we need.
//! - Because the bucket count is not a power of two, the triangular probing
//!   sequence simply skips any indices larger than the actual size of the
//!   buckets array.
//! - Instead of the final control bytes always being marked EMPTY, the last
//!   control bytes repeat the first few. This helps reduce the number of
//!   lookups when probing at the end of the control bytes.
//! - Because the available SIMD group width may be less than the maximum group
//!   width, each probe reads N groups before striding where N is the maximum
//!   group width divided by the SIMD group width.

use core::{
    borrow::Borrow, fmt, hash::Hash, iter::FusedIterator, marker::PhantomData,
    ops::Index, pin::Pin,
};

use rancor::{Error, Fallible};

use crate::{
    collections::raw_swiss_table::{
        ArchivedRawSwissTable, RawIter, RawSwissTableResolver,
    },
    hash::hash_value,
    ser::{Allocator, Writer},
    Archive, Serialize,
};

struct EntryAdapter<'a, K, V> {
    key: &'a K,
    value: &'a V,
}

struct EntryResolver<K, V> {
    key: K,
    value: V,
}

impl<K: Archive, V: Archive> Archive for EntryAdapter<'_, K, V> {
    type Archived = Entry<K::Archived, V::Archived>;
    type Resolver = EntryResolver<K::Resolver, V::Resolver>;

    unsafe fn resolve(
        &self,
        pos: usize,
        resolver: Self::Resolver,
        out: *mut Self::Archived,
    ) {
        let (fp, fo) = out_field!(out.key);
        K::resolve(self.key, pos + fp, resolver.key, fo);
        let (fp, fo) = out_field!(out.value);
        V::resolve(self.value, pos + fp, resolver.value, fo);
    }
}

impl<S, K, V> Serialize<S> for EntryAdapter<'_, K, V>
where
    S: Fallible + ?Sized,
    K: Serialize<S>,
    V: Serialize<S>,
{
    fn serialize(
        &self,
        serializer: &mut S,
    ) -> Result<Self::Resolver, S::Error> {
        Ok(EntryResolver {
            key: self.key.serialize(serializer)?,
            value: self.value.serialize(serializer)?,
        })
    }
}

#[cfg_attr(feature = "stable_layout", repr(C))]
#[cfg_attr(feature = "bytecheck", derive(bytecheck::CheckBytes))]
struct Entry<K, V> {
    key: K,
    value: V,
}

/// An archived SwissTable hash map.
#[cfg_attr(feature = "stable_layout", repr(C))]
#[cfg_attr(feature = "bytecheck", derive(bytecheck::CheckBytes))]
pub struct ArchivedSwissTable<K, V> {
    raw: ArchivedRawSwissTable<Entry<K, V>>,
}

impl<K, V> ArchivedSwissTable<K, V> {
    /// Returns the key-value pair corresponding to the supplied key.
    #[inline]
    pub fn get_key_value_with<Q, C>(&self, key: &Q, cmp: C) -> Option<(&K, &V)>
    where
        Q: Hash + Eq + ?Sized,
        C: Fn(&Q, &K) -> bool,
    {
        let entry = self.raw.get_with(hash_value(key), |e| cmp(key, &e.key))?;
        Some((&entry.key, &entry.value))
    }

    /// Returns the key-value pair corresponding to the supplied key.
    #[inline]
    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_key_value_with(key, |q, k| q == k.borrow())
    }

    /// Returns a reference to the value corresponding to the supplied key.
    #[inline]
    pub fn get_with<Q, C>(&self, key: &Q, cmp: C) -> Option<&V>
    where
        Q: Hash + Eq + ?Sized,
        C: Fn(&Q, &K) -> bool,
    {
        Some(self.get_key_value_with(key, cmp)?.1)
    }

    /// Returns a reference to the value corresponding to the supplied key.
    #[inline]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        Some(self.get_key_value(key)?.1)
    }

    /// Returns the mutable key-value pair corresponding to the supplied key.
    #[inline]
    pub fn get_key_value_mut_with<Q, C>(
        self: Pin<&mut Self>,
        key: &Q,
        cmp: C,
    ) -> Option<(&K, Pin<&mut V>)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        C: Fn(&Q, &K) -> bool,
    {
        let raw = unsafe { Pin::map_unchecked_mut(self, |s| &mut s.raw) };
        let entry = raw.get_with_mut(hash_value(key), |e| cmp(key, &e.key))?;
        let entry = unsafe { Pin::into_inner_unchecked(entry) };
        let key = &entry.key;
        let value = unsafe { Pin::new_unchecked(&mut entry.value) };
        Some((key, value))
    }

    /// Returns the mutable key-value pair corresponding to the supplied key.
    #[inline]
    pub fn get_key_value_mut<Q>(
        self: Pin<&mut Self>,
        key: &Q,
    ) -> Option<(&K, Pin<&mut V>)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_key_value_mut_with(key, |q, k| q == k.borrow())
    }

    /// Returns a mutable reference to the value corresponding to the supplied key.
    #[inline]
    pub fn get_mut_with<Q, C>(
        self: Pin<&mut Self>,
        key: &Q,
        cmp: C,
    ) -> Option<Pin<&mut V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        C: Fn(&Q, &K) -> bool,
    {
        Some(self.get_key_value_mut_with(key, cmp)?.1)
    }

    /// Returns a mutable reference to the value corresponding to the supplied key.
    #[inline]
    pub fn get_mut<Q>(self: Pin<&mut Self>, key: &Q) -> Option<Pin<&mut V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        Some(self.get_key_value_mut(key)?.1)
    }

    /// Returns whether the SwissTable contains the given key.
    #[inline]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get(key).is_some()
    }

    /// Returns whether the SwissTable is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Returns the number of elements in the SwissTable.
    #[inline]
    pub const fn len(&self) -> usize {
        self.raw.len()
    }

    /// Returns the total capacity of the SwissTable.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    /// Returns an iterator over the key-value entries in the SwissTable.
    #[inline]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            raw: self.raw.raw_iter(),
            _phantom: PhantomData,
        }
    }

    /// Returns an iterator over the mutable key-value entries in the
    /// SwissTable.
    #[inline]
    pub fn iter_mut(self: Pin<&mut Self>) -> IterMut<'_, K, V> {
        IterMut {
            raw: self.raw.raw_iter(),
            _phantom: PhantomData,
        }
    }

    /// Returns an iterator over the keys in the SwissTable.
    #[inline]
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys {
            raw: self.raw.raw_iter(),
            _phantom: PhantomData,
        }
    }

    /// Returns an iterator over the values in the SwissTable.
    #[inline]
    pub fn values(&self) -> Values<'_, K, V> {
        Values {
            raw: self.raw.raw_iter(),
            _phantom: PhantomData,
        }
    }

    /// Returns an iterator over the mutable values in the SwissTable.
    #[inline]
    pub fn values_mut(self: Pin<&mut Self>) -> ValuesMut<'_, K, V> {
        ValuesMut {
            raw: self.raw.raw_iter(),
            _phantom: PhantomData,
        }
    }

    /// Serializes an iterator of key-value pairs as a hash map.
    pub fn serialize_from_iter<'a, KU, VU, I, S>(
        iter: I,
        load_factor: (usize, usize),
        serializer: &mut S,
    ) -> Result<SwissTableResolver, S::Error>
    where
        KU: 'a + Serialize<S, Archived = K> + Hash + Eq,
        VU: 'a + Serialize<S, Archived = V>,
        S: Fallible + Writer + Allocator + ?Sized,
        S::Error: Error,
        I: Clone + ExactSizeIterator<Item = (&'a KU, &'a VU)>,
    {
        ArchivedRawSwissTable::<Entry<K, V>>::serialize_from_iter(
            iter.map(|(key, value)| EntryAdapter { key, value }),
            |e| hash_value(e.key),
            load_factor,
            serializer,
        )
        .map(SwissTableResolver)
    }

    /// Resolves an archived hash map from a given length and parameters.
    ///
    /// # Safety
    ///
    /// `out` must point to a `Self` that properly aligned and valid for writes.
    pub unsafe fn resolve_from_len(
        len: usize,
        load_factor: (usize, usize),
        pos: usize,
        resolver: SwissTableResolver,
        out: *mut Self,
    ) {
        ArchivedRawSwissTable::<Entry<K, V>>::resolve_from_len(
            len,
            load_factor,
            pos,
            resolver.0,
            out.cast(),
        )
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for ArchivedSwissTable<K, V> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Hash + Eq, V: Eq> Eq for ArchivedSwissTable<K, V> {}

impl<K: Hash + Eq, V: PartialEq> PartialEq for ArchivedSwissTable<K, V> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            false
        } else {
            self.iter().all(|(key, value)| {
                other.get(key).map_or(false, |v| *value == *v)
            })
        }
    }
}

impl<K, Q, V> Index<&'_ Q> for ArchivedSwissTable<K, V>
where
    K: Eq + Hash + Borrow<Q>,
    Q: Eq + Hash + ?Sized,
{
    type Output = V;

    #[inline]
    fn index(&self, key: &Q) -> &V {
        self.get(key).unwrap()
    }
}

/// The resolver for archived [SwissTables](ArchivedSwissTable).
pub struct SwissTableResolver(RawSwissTableResolver);

/// An iterator over the key-value pairs of a SwissTable.
pub struct Iter<'a, K, V> {
    raw: RawIter<Entry<K, V>>,
    _phantom: PhantomData<&'a ArchivedSwissTable<K, V>>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.raw.next().map(|entry| {
            let entry = unsafe { entry.as_ref() };
            (&entry.key, &entry.value)
        })
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {
    #[inline]
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for Iter<'_, K, V> {}

/// An iterator over the mutable key-value pairs of a SwissTable.
pub struct IterMut<'a, K, V> {
    raw: RawIter<Entry<K, V>>,
    _phantom: PhantomData<&'a ArchivedSwissTable<K, V>>,
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, Pin<&'a mut V>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.raw.next().map(|mut entry| {
            let entry = unsafe { entry.as_mut() };
            let value = unsafe { Pin::new_unchecked(&mut entry.value) };
            (&entry.key, value)
        })
    }
}

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for IterMut<'_, K, V> {}

/// An iterator over the keys of a SwissTable.
pub struct Keys<'a, K, V> {
    raw: RawIter<Entry<K, V>>,
    _phantom: PhantomData<&'a ArchivedSwissTable<K, V>>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.raw.next().map(|entry| {
            let entry = unsafe { entry.as_ref() };
            &entry.key
        })
    }
}

impl<K, V> ExactSizeIterator for Keys<'_, K, V> {
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for Keys<'_, K, V> {}

/// An iterator over the values of a SwissTable.
pub struct Values<'a, K, V> {
    raw: RawIter<Entry<K, V>>,
    _phantom: PhantomData<&'a ArchivedSwissTable<K, V>>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.raw.next().map(|entry| {
            let entry = unsafe { entry.as_ref() };
            &entry.value
        })
    }
}

impl<K, V> ExactSizeIterator for Values<'_, K, V> {
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for Values<'_, K, V> {}

/// An iterator over the mutable values of a SwissTable.
pub struct ValuesMut<'a, K, V> {
    raw: RawIter<Entry<K, V>>,
    _phantom: PhantomData<&'a ArchivedSwissTable<K, V>>,
}

impl<'a, K, V> Iterator for ValuesMut<'a, K, V> {
    type Item = Pin<&'a mut V>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.raw.next().map(|mut entry| {
            let entry = unsafe { entry.as_mut() };
            unsafe { Pin::new_unchecked(&mut entry.value) }
        })
    }
}

impl<K, V> ExactSizeIterator for ValuesMut<'_, K, V> {
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for ValuesMut<'_, K, V> {}