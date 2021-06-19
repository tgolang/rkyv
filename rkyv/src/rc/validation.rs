//! Validation implementations for shared pointers.

use super::{ArchivedRc, ArchivedRcWeak, ArchivedRcWeakTag, ArchivedRcWeakVariantSome};
use crate::{
    validation::{ArchiveBoundsContext, LayoutMetadata, SharedArchiveContext},
    ArchivePointee, RelPtr,
};
use bytecheck::{CheckBytes, Error};
use core::{any::TypeId, convert::Infallible, fmt, ptr};
use ptr_meta::Pointee;

/// Errors that can occur while checking archived shared pointers.
#[derive(Debug)]
pub enum SharedPointerError<T, R, C> {
    /// An error occurred while checking the bytes of a shared value
    PointerCheckBytesError(T),
    /// An error occurred while checking the bytes of a shared reference
    ValueCheckBytesError(R),
    /// A context error occurred
    ContextError(C),
}

impl<T: fmt::Display, R: fmt::Display, C: fmt::Display> fmt::Display
    for SharedPointerError<T, R, C>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SharedPointerError::PointerCheckBytesError(e) => e.fmt(f),
            SharedPointerError::ValueCheckBytesError(e) => e.fmt(f),
            SharedPointerError::ContextError(e) => e.fmt(f),
        }
    }
}

#[cfg(feature = "std")]
const _: () = {
    use std::error::Error;

    impl<T, R, C> Error for SharedPointerError<T, R, C>
    where
        T: Error + 'static,
        R: Error + 'static,
        C: Error + 'static,
    {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            match self {
                SharedPointerError::PointerCheckBytesError(e) => Some(e as &dyn Error),
                SharedPointerError::ValueCheckBytesError(e) => Some(e as &dyn Error),
                SharedPointerError::ContextError(e) => Some(e as &dyn Error),
            }
        }
    }
};

/// Errors that can occur while checking archived weak pointers.
#[derive(Debug)]
pub enum WeakPointerError<T, R, C> {
    /// The weak pointer had an invalid tag
    InvalidTag(u8),
    /// An error occurred while checking the underlying shared pointer
    CheckBytes(SharedPointerError<T, R, C>),
}

impl<T: fmt::Display, R: fmt::Display, C: fmt::Display> fmt::Display for WeakPointerError<T, R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WeakPointerError::InvalidTag(tag) => {
                write!(f, "archived weak had invalid tag: {}", tag)
            }
            WeakPointerError::CheckBytes(e) => e.fmt(f),
        }
    }
}

#[cfg(feature = "std")]
const _: () = {
    use std::error::Error;

    impl<T, R, C> Error for WeakPointerError<T, R, C>
    where
        T: Error + 'static,
        R: Error + 'static,
        C: Error + 'static,
    {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            match self {
                WeakPointerError::InvalidTag(_) => None,
                WeakPointerError::CheckBytes(e) => Some(e as &dyn Error),
            }
        }
    }
};

impl<T, R, C> From<Infallible> for WeakPointerError<T, R, C> {
    fn from(_: Infallible) -> Self {
        unsafe { core::hint::unreachable_unchecked() }
    }
}

impl<T, C> CheckBytes<C> for ArchivedRc<T>
where
    T: ArchivePointee + CheckBytes<C> + Pointee + ?Sized + 'static,
    C: ArchiveBoundsContext + SharedArchiveContext + ?Sized,
    T::ArchivedMetadata: CheckBytes<C>,
    C::Error: Error,
    <T as Pointee>::Metadata: LayoutMetadata<T>,
{
    type Error =
        SharedPointerError<<T::ArchivedMetadata as CheckBytes<C>>::Error, T::Error, C::Error>;

    unsafe fn check_bytes<'a>(
        value: *const Self,
        context: &mut C,
    ) -> Result<&'a Self, Self::Error> {
        let rel_ptr = RelPtr::<T>::manual_check_bytes(value.cast(), context)
            .map_err(SharedPointerError::PointerCheckBytesError)?;
        if let Some(ptr) = context
            .claim_shared_ptr(rel_ptr, TypeId::of::<ArchivedRc<T>>())
            .map_err(SharedPointerError::ContextError)?
        {
            T::check_bytes(ptr, context).map_err(SharedPointerError::ValueCheckBytesError)?;
        }
        Ok(&*value)
    }
}

impl ArchivedRcWeakTag {
    const TAG_NONE: u8 = ArchivedRcWeakTag::None as u8;
    const TAG_SOME: u8 = ArchivedRcWeakTag::Some as u8;
}

impl<T, C> CheckBytes<C> for ArchivedRcWeak<T>
where
    T: ArchivePointee + CheckBytes<C> + Pointee + ?Sized + 'static,
    C: ArchiveBoundsContext + SharedArchiveContext + ?Sized,
    T::ArchivedMetadata: CheckBytes<C>,
    C::Error: Error,
    <T as Pointee>::Metadata: LayoutMetadata<T>,
{
    type Error =
        WeakPointerError<<T::ArchivedMetadata as CheckBytes<C>>::Error, T::Error, C::Error>;

    unsafe fn check_bytes<'a>(
        value: *const Self,
        context: &mut C,
    ) -> Result<&'a Self, Self::Error> {
        let tag = *u8::check_bytes(value.cast::<u8>(), context)?;
        match tag {
            ArchivedRcWeakTag::TAG_NONE => (),
            ArchivedRcWeakTag::TAG_SOME => {
                let value = value.cast::<ArchivedRcWeakVariantSome<T>>();
                ArchivedRc::<T>::check_bytes(ptr::addr_of!((*value).1), context)
                    .map_err(WeakPointerError::CheckBytes)?;
            }
            _ => return Err(WeakPointerError::InvalidTag(tag)),
        }
        Ok(&*value)
    }
}