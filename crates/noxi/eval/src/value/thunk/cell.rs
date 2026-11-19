//! A reference-counted cell with a single header word, standing in for
//! `Rc<RefCell<T>>` in thunks.
//!
//! `Rc<RefCell<T>>` puts three words before `T`: the strong count, the weak
//! count and the borrow flag. Evaluations keep millions of thunks alive, and
//! with a 32-byte state those three words take the allocation from a 48-byte
//! to a 64-byte allocator block. [`RcCell`] keeps a 32-bit strong count and a
//! 32-bit borrow state in one word. Only builds with diagnostics or cycle
//! collection, which hold weak references to thunks, add a weak count.
//!
//! The borrow rules and panics are those of `RefCell`: any number of shared
//! borrows or one exclusive borrow, checked at runtime. Counts that would
//! overflow abort the process, as `Rc` does.

use std::{
	cell::{Cell, UnsafeCell},
	fmt,
	marker::PhantomData,
	mem::ManuallyDrop,
	ops::{Deref, DerefMut},
	ptr::NonNull,
};

/// Builds holding weak references to cells. The weak count only exists in
/// them.
macro_rules! with_weak {
	($($item:item)*) => {
		$(
			#[cfg(any(feature = "eval-diagnostics", feature = "eval-cycle-collection"))]
			$item
		)*
	};
}

/// Borrow state: positive for that many shared borrows, `EXCLUSIVE` for one
/// exclusive borrow, zero for none.
const UNUSED: i32 = 0;
const EXCLUSIVE: i32 = -1;

#[repr(C)]
struct Inner<T> {
	/// Number of [`RcCell`] handles. The value is dropped when it reaches zero.
	strong: Cell<u32>,
	borrow: Cell<i32>,
	/// Number of [`Weak`] handles, plus one held jointly by all strong
	/// handles while the strong count is nonzero. The allocation is freed
	/// when it reaches zero.
	#[cfg(any(feature = "eval-diagnostics", feature = "eval-cycle-collection"))]
	weak: Cell<u32>,
	value: UnsafeCell<ManuallyDrop<T>>,
}

/// A shared, mutable `T`, as `Rc<RefCell<T>>`.
///
/// Neither `Send` nor `Sync`, since the counts are not atomic.
pub(crate) struct RcCell<T> {
	ptr: NonNull<Inner<T>>,
	// Owns an `Inner<T>` for drop checking. `Inner` holds an `UnsafeCell<T>`,
	// so this makes the cell invariant in `T`, as `RefCell<T>` is.
	_owns: PhantomData<Inner<T>>,
}

/// Error from [`RcCell::try_borrow`] while the cell is exclusively borrowed.
#[derive(Debug)]
pub(crate) struct BorrowError;

/// Error from [`RcCell::try_borrow_mut`] while the cell is borrowed.
#[derive(Debug)]
pub(crate) struct BorrowMutError;

#[cold]
#[inline(never)]
fn abort_on_overflow() -> ! {
	std::process::abort()
}

#[cold]
#[inline(never)]
#[track_caller]
fn panic_already_mutably_borrowed() -> ! {
	panic!("already mutably borrowed: {:?}", BorrowError)
}

#[cold]
#[inline(never)]
#[track_caller]
fn panic_already_borrowed() -> ! {
	panic!("already borrowed: {:?}", BorrowMutError)
}

impl<T> RcCell<T> {
	#[inline]
	pub(crate) fn new(value: T) -> Self {
		let inner = Box::new(Inner {
			strong: Cell::new(1),
			borrow: Cell::new(UNUSED),
			#[cfg(any(feature = "eval-diagnostics", feature = "eval-cycle-collection"))]
			weak: Cell::new(1),
			value: UnsafeCell::new(ManuallyDrop::new(value)),
		});
		Self {
			ptr: NonNull::from(Box::leak(inner)),
			_owns: PhantomData,
		}
	}

	#[inline]
	fn inner(&self) -> &Inner<T> {
		// SAFETY: a strong handle keeps the allocation alive and the value
		// initialized. The header is only accessed through shared references
		// to its `Cell`s, and the value only through `UnsafeCell::get`.
		unsafe { self.ptr.as_ref() }
	}

	/// Number of strong handles to this cell.
	#[inline]
	pub(crate) fn strong_count(this: &Self) -> usize {
		this.inner().strong.get() as usize
	}

	/// Whether both handles refer to the same cell.
	#[inline]
	pub(crate) fn ptr_eq(this: &Self, other: &Self) -> bool {
		this.ptr == other.ptr
	}

	/// The address of the value, which identifies the cell while it lives.
	#[inline]
	pub(crate) fn as_ptr(this: &Self) -> *const T {
		this.inner().value.get().cast_const().cast::<T>()
	}

	/// The value, if this is the only strong handle; otherwise the handle
	/// itself. Outstanding weak handles do not prevent unwrapping, and fail
	/// to upgrade afterwards.
	pub(crate) fn try_unwrap(this: Self) -> Result<T, Self> {
		let inner = this.inner();
		if inner.strong.get() != 1 {
			return Err(this);
		}
		// Borrows are tied to a strong handle; the only one is `this`, which
		// is owned here, so none can be live.
		debug_assert_eq!(inner.borrow.get(), UNUSED);
		inner.strong.set(0);
		// SAFETY: the strong count was one, so the value is initialized, and
		// with no borrows nothing else refers to it. Setting the strong count
		// to zero first means no handle reads it again: weak handles fail to
		// upgrade, and `this` is forgotten below instead of dropped.
		let value = unsafe { ManuallyDrop::take(&mut *inner.value.get()) };
		let ptr = this.ptr;
		std::mem::forget(this);
		// SAFETY: `ptr` was the last strong handle, and its value has moved out.
		unsafe { Self::release_allocation(ptr) };
		Ok(value)
	}

	/// Give up the strong handles' claim on an allocation whose value has
	/// been dropped or moved out.
	///
	/// # Safety
	///
	/// The strong count is zero and the value is no longer initialized.
	#[inline]
	unsafe fn release_allocation(ptr: NonNull<Inner<T>>) {
		#[cfg(any(feature = "eval-diagnostics", feature = "eval-cycle-collection"))]
		{
			// SAFETY: the joint weak reference of the strong handles keeps the
			// allocation alive until here.
			let weak = unsafe { &ptr.as_ref().weak };
			let remaining = weak.get() - 1;
			weak.set(remaining);
			if remaining != 0 {
				return;
			}
		}
		// SAFETY: the allocation came from `Box::new` in `new`, and no handle
		// remains to use it. The value is wrapped in `ManuallyDrop`, so
		// dropping the box frees the memory without touching the value.
		drop(unsafe { Box::from_raw(ptr.as_ptr()) });
	}

	/// Borrow the value, panicking if it is exclusively borrowed.
	#[inline]
	#[track_caller]
	pub(crate) fn borrow(&self) -> Ref<'_, T> {
		match self.try_borrow() {
			Ok(borrow) => borrow,
			Err(_) => panic_already_mutably_borrowed(),
		}
	}

	#[inline]
	pub(crate) fn try_borrow(&self) -> Result<Ref<'_, T>, BorrowError> {
		let inner = self.inner();
		let state = inner.borrow.get();
		// A negative state is an exclusive borrow. At the maximum, another
		// shared borrow would overflow; `RefCell` reports the same error then.
		if state < UNUSED || state == i32::MAX {
			return Err(BorrowError);
		}
		inner.borrow.set(state + 1);
		Ok(Ref {
			// SAFETY: the value is initialized while a strong handle lives,
			// and the borrow state now records this shared borrow, so no
			// exclusive reference to it is handed out until the guard drops.
			value: NonNull::from(unsafe { &**inner.value.get() }),
			guard: SharedGuard(&inner.borrow),
		})
	}

	/// Borrow the value exclusively, panicking if it is borrowed.
	#[inline]
	#[track_caller]
	pub(crate) fn borrow_mut(&self) -> RefMut<'_, T> {
		match self.try_borrow_mut() {
			Ok(borrow) => borrow,
			Err(_) => panic_already_borrowed(),
		}
	}

	#[inline]
	pub(crate) fn try_borrow_mut(&self) -> Result<RefMut<'_, T>, BorrowMutError> {
		let inner = self.inner();
		if inner.borrow.get() != UNUSED {
			return Err(BorrowMutError);
		}
		inner.borrow.set(EXCLUSIVE);
		Ok(RefMut {
			// SAFETY: the value is initialized while a strong handle lives,
			// and the borrow state was unused and now records this exclusive
			// borrow, so no other reference to the value exists or is handed
			// out until the guard drops.
			value: NonNull::from(unsafe { &mut **inner.value.get() }),
			guard: ExclusiveGuard(&inner.borrow),
			_exclusive: PhantomData,
		})
	}

	/// Replace the value, returning the old one, as `RefCell::replace`.
	/// Panics if the value is borrowed.
	#[inline]
	#[track_caller]
	pub(crate) fn replace(&self, value: T) -> T {
		// The guard is released before the old value is returned, so its drop
		// may borrow this cell again.
		std::mem::replace(&mut *self.borrow_mut(), value)
	}
}

impl<T> Clone for RcCell<T> {
	#[inline]
	fn clone(&self) -> Self {
		let strong = &self.inner().strong;
		let count = strong.get();
		if count == u32::MAX {
			abort_on_overflow();
		}
		strong.set(count + 1);
		Self {
			ptr: self.ptr,
			_owns: PhantomData,
		}
	}
}

impl<T> Drop for RcCell<T> {
	#[inline]
	fn drop(&mut self) {
		let strong = &self.inner().strong;
		let remaining = strong.get() - 1;
		strong.set(remaining);
		if remaining != 0 {
			return;
		}
		// SAFETY: this was the last strong handle, so the value is initialized
		// and, borrows being tied to strong handles, unborrowed. The strong
		// count is already zero, so a weak handle reached from the value's
		// drop fails to upgrade and cannot observe it partly dropped. If the
		// drop unwinds, the allocation leaks, which is safe.
		unsafe {
			ManuallyDrop::drop(&mut *self.inner().value.get());
			Self::release_allocation(self.ptr);
		}
	}
}

/// Formats as `RefCell` does, since thunks used to hold one.
impl<T: fmt::Debug> fmt::Debug for RcCell<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		struct BorrowedPlaceholder;
		impl fmt::Debug for BorrowedPlaceholder {
			fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
				f.write_str("<borrowed>")
			}
		}
		let mut d = f.debug_struct("RefCell");
		match self.try_borrow() {
			Ok(borrow) => d.field("value", &&*borrow),
			Err(_) => d.field("value", &BorrowedPlaceholder),
		};
		d.finish()
	}
}

/// Releases one shared borrow.
struct SharedGuard<'a>(&'a Cell<i32>);

impl Drop for SharedGuard<'_> {
	#[inline]
	fn drop(&mut self) {
		self.0.set(self.0.get() - 1);
	}
}

/// Releases the exclusive borrow.
struct ExclusiveGuard<'a>(&'a Cell<i32>);

impl Drop for ExclusiveGuard<'_> {
	#[inline]
	fn drop(&mut self) {
		debug_assert_eq!(self.0.get(), EXCLUSIVE);
		self.0.set(UNUSED);
	}
}

/// A shared borrow of an [`RcCell`] value, or of part of it.
pub struct Ref<'a, T> {
	// Not a reference: the guard could be released while `value` still
	// exists, and a reference must stay valid for as long as it exists.
	value: NonNull<T>,
	guard: SharedGuard<'a>,
}

impl<'a, T> Ref<'a, T> {
	/// A borrow of part of the value, as `std::cell::Ref::map`.
	#[inline]
	pub(crate) fn map<U, F: FnOnce(&T) -> &U>(orig: Self, f: F) -> Ref<'a, U> {
		let Ref { value, guard } = orig;
		// SAFETY: `guard` holds the shared borrow while `f` runs and moves to
		// the result, so `value` stays valid for both. If `f` panics, the guard
		// is dropped during unwinding and releases the borrow.
		let value = NonNull::from(f(unsafe { value.as_ref() }));
		Ref { value, guard }
	}
}

impl<T> Deref for Ref<'_, T> {
	type Target = T;

	#[inline]
	fn deref(&self) -> &T {
		// SAFETY: the guard holds a shared borrow for as long as `self` lives.
		unsafe { self.value.as_ref() }
	}
}

impl<T: fmt::Debug> fmt::Debug for Ref<'_, T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		(**self).fmt(f)
	}
}

/// An exclusive borrow of an [`RcCell`] value, or of part of it.
pub struct RefMut<'a, T> {
	value: NonNull<T>,
	guard: ExclusiveGuard<'a>,
	// Invariant in `T`, as `&'a mut T` is.
	_exclusive: PhantomData<&'a mut T>,
}

impl<'a, T> RefMut<'a, T> {
	/// An exclusive borrow of part of the value, as `std::cell::RefMut::map`.
	#[inline]
	pub(crate) fn map<U, F: FnOnce(&mut T) -> &mut U>(orig: Self, f: F) -> RefMut<'a, U> {
		let RefMut {
			mut value, guard, ..
		} = orig;
		// SAFETY: as in `Ref::map`, with the guard holding the exclusive
		// borrow and `orig` consumed, so the result is the only access.
		let value = NonNull::from(f(unsafe { value.as_mut() }));
		RefMut {
			value,
			guard,
			_exclusive: PhantomData,
		}
	}
}

impl<T> Deref for RefMut<'_, T> {
	type Target = T;

	#[inline]
	fn deref(&self) -> &T {
		// SAFETY: the guard holds the exclusive borrow for as long as `self`
		// lives.
		unsafe { self.value.as_ref() }
	}
}

impl<T> DerefMut for RefMut<'_, T> {
	#[inline]
	fn deref_mut(&mut self) -> &mut T {
		// SAFETY: as in `deref`, and `&mut self` makes this the only access.
		unsafe { self.value.as_mut() }
	}
}

impl<T: fmt::Debug> fmt::Debug for RefMut<'_, T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		(**self).fmt(f)
	}
}

with_weak! {
	/// A handle that does not keep the value alive, as `std::rc::Weak`.
	pub(crate) struct Weak<T> {
		ptr: NonNull<Inner<T>>,
		_owns: PhantomData<Inner<T>>,
	}

	impl<T> RcCell<T> {
		pub(crate) fn downgrade(this: &Self) -> Weak<T> {
			let weak = &this.inner().weak;
			let count = weak.get();
			if count == u32::MAX {
				abort_on_overflow();
			}
			weak.set(count + 1);
			Weak {
				ptr: this.ptr,
				_owns: PhantomData,
			}
		}
	}

	impl<T> Weak<T> {
		#[inline]
		fn header(&self) -> &Inner<T> {
			// SAFETY: a weak handle keeps the allocation alive. Only the
			// header `Cell`s are read through this reference; the value is
			// reached only after checking the strong count.
			unsafe { self.ptr.as_ref() }
		}

		/// A strong handle, unless the value has been dropped or moved out.
		pub(crate) fn upgrade(&self) -> Option<RcCell<T>> {
			let strong = &self.header().strong;
			let count = strong.get();
			if count == 0 {
				return None;
			}
			if count == u32::MAX {
				abort_on_overflow();
			}
			strong.set(count + 1);
			Some(RcCell {
				ptr: self.ptr,
				_owns: PhantomData,
			})
		}

		pub(crate) fn strong_count(&self) -> usize {
			self.header().strong.get() as usize
		}
	}

	impl<T> Clone for Weak<T> {
		fn clone(&self) -> Self {
			let weak = &self.header().weak;
			let count = weak.get();
			if count == u32::MAX {
				abort_on_overflow();
			}
			weak.set(count + 1);
			Self {
				ptr: self.ptr,
				_owns: PhantomData,
			}
		}
	}

	impl<T> Drop for Weak<T> {
		fn drop(&mut self) {
			let weak = &self.header().weak;
			let remaining = weak.get() - 1;
			weak.set(remaining);
			if remaining == 0 {
				// The strong handles' joint weak reference is gone, so the
				// strong count is zero and the value already dropped or
				// moved out.
				debug_assert_eq!(self.header().strong.get(), 0);
				// SAFETY: the allocation came from `Box::new` and this was
				// its last handle. The value is not initialized and sits in
				// `ManuallyDrop`, so freeing the box does not touch it.
				drop(unsafe { Box::from_raw(self.ptr.as_ptr()) });
			}
		}
	}

	impl<T> fmt::Debug for Weak<T> {
		fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
			f.write_str("(Weak)")
		}
	}
}

/// The allocation size of a cell holding `T`.
#[cfg(feature = "eval-diagnostics")]
pub(crate) const fn allocation_size<T>() -> usize {
	std::mem::size_of::<Inner<T>>()
}

#[cfg(not(any(feature = "eval-diagnostics", feature = "eval-cycle-collection")))]
const _: () = {
	// One header word before the value.
	assert!(std::mem::size_of::<Inner<u64>>() == 16);
	assert!(std::mem::size_of::<Inner<[u64; 4]>>() == 40);
};

#[cfg(test)]
mod tests {
	use super::*;
	use std::{panic, rc::Rc};

	/// Counts drops into a shared counter.
	struct Tracked(Rc<Cell<u32>>);

	impl Drop for Tracked {
		fn drop(&mut self) {
			self.0.set(self.0.get() + 1);
		}
	}

	#[test]
	fn shared_borrows_nest_and_exclude_exclusive_ones() {
		let cell = RcCell::new(5);
		let a = cell.borrow();
		let b = cell.borrow();
		assert_eq!(*a + *b, 10);
		assert!(cell.try_borrow_mut().is_err());
		drop(a);
		assert!(cell.try_borrow_mut().is_err());
		drop(b);
		*cell.borrow_mut() = 6;
		assert_eq!(*cell.borrow(), 6);
	}

	#[test]
	fn exclusive_borrow_excludes_all_others() {
		let cell = RcCell::new(1);
		let alias = cell.clone();
		let guard = cell.borrow_mut();
		assert!(alias.try_borrow().is_err());
		assert!(alias.try_borrow_mut().is_err());
		drop(guard);
		assert!(alias.try_borrow().is_ok());
	}

	#[test]
	fn borrow_failures_panic_with_refcell_messages() {
		let cell = RcCell::new(0);
		let guard = cell.borrow_mut();
		let shared = panic::catch_unwind(panic::AssertUnwindSafe(|| {
			let _ = cell.borrow();
		}))
		.unwrap_err();
		assert_eq!(
			shared.downcast_ref::<String>().map(String::as_str),
			Some("already mutably borrowed: BorrowError")
		);
		drop(guard);
		let shared = cell.borrow();
		let exclusive = panic::catch_unwind(panic::AssertUnwindSafe(|| {
			let _ = cell.borrow_mut();
		}))
		.unwrap_err();
		assert_eq!(
			exclusive.downcast_ref::<String>().map(String::as_str),
			Some("already borrowed: BorrowMutError")
		);
		let replace = panic::catch_unwind(panic::AssertUnwindSafe(|| cell.replace(1)));
		assert!(replace.is_err());
		drop(shared);
		assert_eq!(cell.replace(2), 0);
	}

	#[test]
	fn panicking_projections_release_the_borrow() {
		let cell = RcCell::new((1, 2));
		let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
			Ref::map(cell.borrow(), |_: &(i32, i32)| -> &i32 {
				panic!("projection")
			});
		}));
		assert!(result.is_err());
		assert!(cell.try_borrow_mut().is_ok());
		let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
			RefMut::map(cell.borrow_mut(), |_: &mut (i32, i32)| -> &mut i32 {
				panic!("projection")
			});
		}));
		assert!(result.is_err());
		let mut second = RefMut::map(cell.borrow_mut(), |pair| &mut pair.1);
		*second = 3;
		drop(second);
		let first = Ref::map(cell.borrow(), |pair| &pair.0);
		assert!(cell.try_borrow_mut().is_err());
		assert_eq!(*first, 1);
		drop(first);
		assert_eq!(*cell.borrow(), (1, 3));
	}

	#[test]
	fn value_drops_once_with_the_last_handle() {
		let drops = Rc::new(Cell::new(0));
		let cell = RcCell::new(Tracked(drops.clone()));
		let alias = cell.clone();
		assert_eq!(RcCell::strong_count(&cell), 2);
		assert!(RcCell::ptr_eq(&cell, &alias));
		drop(cell);
		assert_eq!(drops.get(), 0);
		assert_eq!(RcCell::strong_count(&alias), 1);
		drop(alias);
		assert_eq!(drops.get(), 1);
	}

	#[test]
	fn replaced_value_may_reenter_the_cell_from_its_drop() {
		// The old value is dropped after the exclusive borrow ends.
		struct Reenter(RcCell<Option<Reenter>>, Rc<Cell<bool>>);
		impl Drop for Reenter {
			fn drop(&mut self) {
				self.1.set(self.0.try_borrow().is_ok());
			}
		}
		let cell = RcCell::new(None);
		let observed = Rc::new(Cell::new(false));
		cell.replace(Some(Reenter(cell.clone(), observed.clone())));
		// Break the cycle: the old `Reenter` holds a handle to `cell`.
		drop(cell.replace(None));
		assert!(observed.get());
		assert_eq!(RcCell::strong_count(&cell), 1);
	}

	#[test]
	fn unwrap_needs_the_only_strong_handle() {
		let drops = Rc::new(Cell::new(0));
		let cell = RcCell::new(Tracked(drops.clone()));
		let alias = cell.clone();
		let cell = RcCell::try_unwrap(cell).err().unwrap();
		drop(alias);
		let value = RcCell::try_unwrap(cell).ok().unwrap();
		assert_eq!(drops.get(), 0);
		drop(value);
		assert_eq!(drops.get(), 1);
	}

	#[test]
	fn panicking_drop_leaks_without_double_drop() {
		struct Bomb(Rc<Cell<u32>>);
		impl Drop for Bomb {
			fn drop(&mut self) {
				self.0.set(self.0.get() + 1);
				panic!("drop");
			}
		}
		let drops = Rc::new(Cell::new(0));
		let cell = RcCell::new(Bomb(drops.clone()));
		assert!(panic::catch_unwind(panic::AssertUnwindSafe(move || drop(cell))).is_err());
		assert_eq!(drops.get(), 1);
	}

	#[test]
	fn debug_matches_refcell() {
		let cell = RcCell::new(vec![1]);
		let reference = std::cell::RefCell::new(vec![1]);
		assert_eq!(format!("{cell:?}"), format!("{reference:?}"));
		assert_eq!(format!("{cell:#?}"), format!("{reference:#?}"));
		let _guard = cell.borrow_mut();
		let _reference_guard = reference.borrow_mut();
		assert_eq!(format!("{cell:?}"), format!("{reference:?}"));
		drop(_guard);
		let _shared = cell.borrow();
		assert_eq!(format!("{cell:?}"), "RefCell { value: [1] }");
	}

	with_weak! {
		#[test]
		fn weak_handles_outlive_the_value() {
			let drops = Rc::new(Cell::new(0));
			let cell = RcCell::new(Tracked(drops.clone()));
			let weak = RcCell::downgrade(&cell);
			let second = weak.clone();
			assert_eq!(weak.strong_count(), 1);
			let upgraded = weak.upgrade().unwrap();
			assert_eq!(RcCell::strong_count(&cell), 2);
			drop(upgraded);
			drop(cell);
			assert_eq!(drops.get(), 1);
			assert!(weak.upgrade().is_none());
			assert_eq!(second.strong_count(), 0);
			drop(weak);
			drop(second);
		}

		#[test]
		fn unwrap_succeeds_with_outstanding_weak_handles() {
			let drops = Rc::new(Cell::new(0));
			let cell = RcCell::new(Tracked(drops.clone()));
			let weak = RcCell::downgrade(&cell);
			let value = RcCell::try_unwrap(cell).ok().unwrap();
			assert!(weak.upgrade().is_none());
			assert_eq!(weak.strong_count(), 0);
			drop(weak);
			assert_eq!(drops.get(), 0);
			drop(value);
			assert_eq!(drops.get(), 1);
		}

		#[test]
		fn weak_upgrade_fails_during_the_value_drop() {
			struct Probe(RefCellWeak, Rc<Cell<bool>>);
			type RefCellWeak = std::rc::Rc<std::cell::RefCell<Option<Weak<Probe>>>>;
			impl Drop for Probe {
				fn drop(&mut self) {
					let upgraded = self.0.borrow().as_ref().unwrap().upgrade();
					self.1.set(upgraded.is_none());
				}
			}
			let slot: RefCellWeak = Default::default();
			let failed = Rc::new(Cell::new(false));
			let cell = RcCell::new(Probe(slot.clone(), failed.clone()));
			*slot.borrow_mut() = Some(RcCell::downgrade(&cell));
			drop(cell);
			assert!(failed.get());
			// The weak handle in the slot still owns the allocation.
			drop(slot);
		}
	}
}
