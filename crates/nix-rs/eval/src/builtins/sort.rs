//! `builtins.sort` with CppNix's PeekSort.
//!
//! A port of `peeksort` from CppNix 2.35.2's `nix/util/sort.hh`, itself
//! adapted from Sebastian Wild's MIT-licensed reference implementation
//! (<https://github.com/sebawild/powersort>). It is a stable natural merge
//! sort that makes the same comparator calls as CppNix, in the same order, so
//! a comparator with side effects such as `builtins.trace`, or one that is not
//! a strict weak ordering, gives the same result. Lists of up to 16 elements
//! use insertion sort.

use std::future::Future;
use std::pin::Pin;

use crate::errors::{AddContext, ErrorKind};
use crate::value::Value;
use crate::vm::generators::{self, GenCo};

/// The outcome of a comparison: a Boolean, a catchable error to return as
/// the value of `builtins.sort`, or an uncatchable error.
type Step<T> = Result<Result<T, Value>, ErrorKind>;

/// Unwrap a [`Step`], returning early with a catchable value.
macro_rules! step {
	($e:expr) => {
		match $e? {
			Ok(value) => value,
			Err(catchable) => return Ok(Err(catchable)),
		}
	};
}

const INSERTION_SORT_THRESHOLD: usize = 16;

struct Sorter<'a> {
	co: &'a GenCo,
	comparator: &'a Value,
	data: Vec<Value>,
	buffer: Vec<Value>,
}

impl Sorter<'_> {
	/// `comparator a b`, or its negation.
	async fn less(&self, a: usize, b: usize, negate: bool) -> Step<bool> {
		let result = generators::request_force(
			self.co,
			generators::request_call_with(
				self.co,
				self.comparator.clone(),
				[self.data[a].clone(), self.data[b].clone()],
			)
			.await,
		)
		.await;
		if result.is_catchable() {
			return Ok(Err(result));
		}
		let less = result
			.as_bool()
			.context("evaluating the return value of the sorting function passed to builtins.sort")?;
		Ok(Ok(less != negate))
	}

	async fn insertion_sort(&mut self, begin: usize, end: usize) -> Step<()> {
		if begin == end {
			return Ok(Ok(()));
		}
		for current in begin + 1..end {
			let mut point = current;
			while point != begin && step!(self.less(point, point - 1, false).await) {
				self.data.swap(point, point - 1);
				point -= 1;
			}
		}
		Ok(Ok(()))
	}

	/// The largest `i <= end` such that `[begin, i)` is strictly decreasing,
	/// or weakly increasing if `negate`.
	async fn decreasing_prefix(&self, mut begin: usize, end: usize, negate: bool) -> Step<usize> {
		if begin == end {
			return Ok(Ok(begin));
		}
		while begin + 1 != end && step!(self.less(begin + 1, begin, negate).await) {
			begin += 1;
		}
		Ok(Ok(begin + 1))
	}

	/// The smallest `i >= begin` such that `[i, end)` is strictly decreasing,
	/// or weakly increasing if `negate`.
	async fn decreasing_suffix(&self, begin: usize, mut end: usize, negate: bool) -> Step<usize> {
		if begin == end {
			return Ok(Ok(end));
		}
		while end - 1 > begin && step!(self.less(end - 1, end - 2, negate).await) {
			end -= 1;
		}
		Ok(Ok(end - 1))
	}

	/// Merge the sorted runs `[begin, middle)` and `[middle, end)`.
	async fn merge(&mut self, begin: usize, middle: usize, end: usize) -> Step<()> {
		self.buffer.clear();
		self.buffer.extend_from_slice(&self.data[begin..end]);
		let working_middle = middle - begin;
		let working_end = end - begin;
		let (mut left, mut right, mut output) = (0, working_middle, begin);
		while left != working_middle && right != working_end {
			// `!(right < left)` takes the left element on ties, which keeps
			// the merge stable.
			let result = generators::request_force(
				self.co,
				generators::request_call_with(
					self.co,
					self.comparator.clone(),
					[self.buffer[right].clone(), self.buffer[left].clone()],
				)
				.await,
			)
			.await;
			if result.is_catchable() {
				return Ok(Err(result));
			}
			let right_first = result
				.as_bool()
				.context("evaluating the return value of the sorting function passed to builtins.sort")?;
			if right_first {
				self.data[output] = self.buffer[right].clone();
				right += 1;
			} else {
				self.data[output] = self.buffer[left].clone();
				left += 1;
			}
			output += 1;
		}
		for index in (left..working_middle).chain(right..working_end) {
			self.data[output] = self.buffer[index].clone();
			output += 1;
		}
		Ok(Ok(()))
	}

	/// Sort `[begin, end)`, given that `[begin, left_run_end)` and
	/// `[right_run_begin, end)` are sorted.
	fn peeksort<'s>(
		&'s mut self,
		begin: usize,
		end: usize,
		left_run_end: usize,
		right_run_begin: usize,
	) -> Pin<Box<dyn Future<Output = Step<()>> + 's>> {
		Box::pin(async move {
			if left_run_end == end || right_run_begin == begin {
				return Ok(Ok(()));
			}
			let length = end - begin;
			if length <= INSERTION_SORT_THRESHOLD {
				return self.insertion_sort(begin, end).await;
			}
			let middle = begin + length / 2;
			if middle <= left_run_end {
				step!(
					self.peeksort(left_run_end, end, left_run_end + 1, right_run_begin)
						.await
				);
				return self.merge(begin, left_run_end, end).await;
			} else if middle >= right_run_begin {
				step!(
					self.peeksort(begin, right_run_begin, left_run_end, right_run_begin - 1)
						.await
				);
				return self.merge(begin, right_run_begin, end).await;
			}

			// The run containing `middle - 1`.
			let (i, j) = if !step!(self.less(middle, middle - 1, false).await) {
				(
					step!(self.decreasing_suffix(left_run_end, middle, true).await),
					step!(self.decreasing_prefix(middle - 1, right_run_begin, true).await),
				)
			} else {
				let i = step!(self.decreasing_suffix(left_run_end, middle, false).await);
				let j = step!(self.decreasing_prefix(middle - 1, right_run_begin, false).await);
				self.data[i..j].reverse();
				(i, j)
			};
			if i == begin && j == end {
				return Ok(Ok(()));
			}
			if middle - i < j - middle {
				step!(self.peeksort(begin, i, left_run_end, i - 1).await);
				step!(self.peeksort(i, end, j, right_run_begin).await);
				self.merge(begin, i, end).await
			} else {
				step!(self.peeksort(begin, j, left_run_end, i).await);
				step!(self.peeksort(j, end, j + 1, right_run_begin).await);
				self.merge(begin, j, end).await
			}
		})
	}
}

/// Sort `data` with `comparator`, after CppNix's `prim_sort`. The elements
/// must already be forced. A catchable error from the comparator is returned
/// as `Err`.
pub(super) async fn peeksort(
	co: &GenCo,
	comparator: &Value,
	data: Vec<Value>,
) -> Result<Result<Vec<Value>, Value>, ErrorKind> {
	let mut sorter = Sorter {
		co,
		comparator,
		buffer: Vec::with_capacity(data.len()),
		data,
	};
	match sorter.data.len() {
		0 | 1 => {}
		2 => {
			if step!(sorter.less(1, 0, false).await) {
				sorter.data.swap(0, 1);
			}
		}
		length => step!(sorter.peeksort(0, length, 0, length).await),
	}
	Ok(Ok(sorter.data))
}
