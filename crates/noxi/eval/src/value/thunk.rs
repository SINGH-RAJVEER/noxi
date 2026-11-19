//! This module implements the runtime representation of Thunks.
//!
//! Thunks are a special kind of Nix value, similar to a 0-argument
//! closure that yields some value. Thunks are used to implement the
//! lazy evaluation behaviour of Nix:
//!
//! Whenever the compiler determines that an expression should be
//! evaluated lazily, it creates a thunk instead of compiling the
//! expression value directly. At any point in the runtime where the
//! actual value of a thunk is required, it is "forced", meaning that
//! the encompassing computation takes place and the thunk takes on
//! its new value.
//!
//! Thunks have interior mutability to be able to memoise their
//! computation. Once a thunk is evaluated, its internal
//! representation becomes the result of the expression. It is legal
//! for the runtime to replace a thunk object directly with its value
//! object, but when forcing a thunk, the runtime *must* mutate the
//! memoisable slot.

use rustc_hash::FxHashSet;
use std::{fmt::Debug, rc::Rc};

use crate::{
	Value,
	errors::ErrorKind,
	opcode::Op,
	upvalues::Upvalues,
	value::Closure,
	vm::generators::{self, GenCo},
};

use super::{Lambda, TotalDisplay};
use cell::{RcCell, Ref, RefMut};
use codemap::Span;

mod cell;
#[cfg(feature = "eval-diagnostics")]
mod census;
#[cfg(feature = "eval-cycle-collection")]
mod cycles;
#[cfg(feature = "eval-diagnostics")]
mod retention;

/// Internal representation of a suspended native thunk.
pub(super) struct SuspendedNative(Box<dyn Fn() -> Result<Value, ErrorKind>>);

impl Debug for SuspendedNative {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "SuspendedNative({:p})", self.0)
	}
}

/// Internal representation of the different states of a thunk.
///
/// Upvalues must be finalised before leaving the initial state
/// (Suspended or RecursiveClosure).  The [`Thunk::value()`] function may
/// not be called until the thunk is in the final state (Evaluated).
#[derive(Debug)]
pub(super) enum ThunkRepr {
	/// Thunk is closed over some values, suspended and awaiting
	/// execution.
	/// The thunk was suspended at [`Lambda::thunk_span`].
	Suspended {
		lambda: Rc<Lambda>,
		// Captures have one owner until forcing enters bytecode. Store them
		// here, without a separate reference-counted allocation for thunks
		// that remain suspended.
		upvalues: Upvalues,
	},

	/// Thunk is a suspended native computation.
	Native(SuspendedNative),

	/// Thunk currently under-evaluation; encountering a blackhole
	/// value means that infinite recursion has occurred.
	Blackhole {
		/// Span at which the thunk was first forced.
		forced_at: Span,

		/// For a thunk of bytecode, the span at which it was suspended and
		/// the span of the first instruction of its code.
		origin: Option<[Span; 2]>,
	},

	// TODO(amjoseph): consider changing `Value` to `Rc<Value>` to avoid
	// expensive clone()s in Thunk::force().
	/// Fully evaluated thunk.
	Evaluated(Value),
}

// With the one-word header of its cell this is a 40-byte allocation, which
// takes a 48-byte allocator block; a 48-byte allocation would take 64.
const _: () = assert!(std::mem::size_of::<ThunkRepr>() == 32);

impl ThunkRepr {
	fn debug_repr(&self) -> String {
		match self {
			ThunkRepr::Evaluated(v) => format!("thunk(val|{v})"),
			ThunkRepr::Blackhole { .. } => "thunk(blackhole)".to_string(),
			ThunkRepr::Native(_) => "thunk(native)".to_string(),
			ThunkRepr::Suspended { lambda, .. } => format!("thunk({:p})", *lambda),
		}
	}

	/// Return the Value within a fully-evaluated ThunkRepr; panics
	/// if the thunk is not fully-evaluated.
	fn expect(self) -> Value {
		match self {
			ThunkRepr::Evaluated(value) => value,
			ThunkRepr::Blackhole { .. } => panic!("Thunk::expect() called on a black-holed thunk"),
			ThunkRepr::Suspended { .. } | ThunkRepr::Native(_) => {
				panic!("Thunk::expect() called on a suspended thunk")
			}
		}
	}

	fn expect_ref(&self) -> &Value {
		match self {
			ThunkRepr::Evaluated(value) => value,
			ThunkRepr::Blackhole { .. } => panic!("Thunk::expect() called on a black-holed thunk"),
			ThunkRepr::Suspended { .. } | ThunkRepr::Native(_) => {
				panic!("Thunk::expect() called on a suspended thunk")
			}
		}
	}

	pub fn is_forced(&self) -> bool {
		match self {
			ThunkRepr::Evaluated(Value::Thunk(_)) => false,
			ThunkRepr::Evaluated(_) => true,
			_ => false,
		}
	}
}

/// A thunk is created for any value which requires non-strict
/// evaluation due to self-reference or lazy semantics (or both).
/// Every reference cycle involving `Value`s will contain at least
/// one `Thunk`.
#[derive(Clone, Debug)]
pub struct Thunk(pub(super) RcCell<ThunkRepr>);

#[cfg(feature = "eval-diagnostics")]
struct RetentionSamples {
	rng: u32,
	sample_mask: u32,
	sample_limit: usize,
	entries: Vec<(&'static str, cell::Weak<ThunkRepr>)>,
	truncated: bool,
}

#[cfg(feature = "eval-diagnostics")]
thread_local! {
	static RETENTION_SAMPLES: std::cell::RefCell<RetentionSamples> = {
		let (sample_mask, sample_limit) = if std::env::var_os("NOXI_EVAL_OWNERSHIP_CENSUS_ALL")
			.is_some_and(|setting| setting == "1")
		{
			(0, 22_000_000)
		} else if std::env::var_os("NOXI_EVAL_OWNERSHIP_CENSUS_DENSE")
			.is_some_and(|setting| setting == "1")
		{
			(63, 524_288)
		} else {
			(1023, 65_536)
		};
		std::cell::RefCell::new(RetentionSamples {
			rng: 0x2395_1aa9,
			sample_mask,
			sample_limit,
			entries: Vec::new(),
			truncated: false,
		})
	};
}

impl Thunk {
	#[cfg(feature = "eval-diagnostics")]
	fn record_creation(&self, kind: &'static str) {
		crate::perf_counters::thunk_created(kind);
		crate::perf_counters::layout("ThunkRepr", std::mem::size_of::<ThunkRepr>());
		crate::perf_counters::layout("Upvalues", std::mem::size_of::<Upvalues>());
		crate::perf_counters::layout(
			"FrameUpvalues",
			std::mem::size_of::<crate::upvalues::FrameUpvalues>(),
		);
		crate::perf_counters::allocation("thunk_rc", cell::allocation_size::<ThunkRepr>());
		RETENTION_SAMPLES.with_borrow_mut(|samples| {
			// A deterministic xorshift avoids sampling a fixed phase of repeated allocations.
			samples.rng ^= samples.rng << 13;
			samples.rng ^= samples.rng >> 17;
			samples.rng ^= samples.rng << 5;
			if samples.rng & samples.sample_mask == 0 {
				if samples.entries.len() < samples.sample_limit {
					samples.entries.push((kind, RcCell::downgrade(&self.0)));
				} else {
					samples.truncated = true;
				}
			}
		});
	}

	#[cfg(feature = "eval-diagnostics")]
	pub(crate) fn reset_retention_samples() {
		RETENTION_SAMPLES.with_borrow_mut(|samples| {
			samples.entries.clear();
			samples.truncated = false;
			samples.rng = 0x2395_1aa9;
		});
	}

	#[cfg(feature = "eval-diagnostics")]
	pub(crate) fn retention_snapshot() -> (
		std::collections::BTreeMap<&'static str, crate::perf_counters::ThunkRetention>,
		bool,
	) {
		RETENTION_SAMPLES.with_borrow(|samples| {
			let mut counts: std::collections::BTreeMap<
				&'static str,
				crate::perf_counters::ThunkRetention,
			> = Default::default();
			let total_live = samples
				.entries
				.iter()
				.filter(|(_, weak)| weak.strong_count() != 0)
				.count();
			let stride = total_live.div_ceil(512).max(1);
			let mut live_index = 0;
			for (kind, weak) in &samples.entries {
				let count = counts.entry(*kind).or_default();
				count.sampled += 1;
				let Some(cell) = weak.upgrade() else { continue };
				count.live += 1;
				let state = match cell.try_borrow().as_deref() {
					Ok(ThunkRepr::Suspended { .. }) => "suspended",
					Ok(ThunkRepr::Evaluated(_)) => "evaluated",
					Ok(ThunkRepr::Blackhole { .. }) => "blackhole",
					Ok(ThunkRepr::Native(_)) => "native",
					Err(_) => "borrowed",
				};
				*count.live_by_state.entry(state).or_default() += 1;
				if live_index % stride == 0 {
					count.cycle_probes += 1;
					let probe = retention::probe(&cell);
					count.cycle_probes_exhausted += u64::from(probe.exhausted);
					if let Some(edges) = probe.witness {
						*count.cycle_witnesses.entry(edges).or_default() += 1;
					}
				}
				live_index += 1;
			}
			(counts, samples.truncated)
		})
	}

	/// Classify everything reachable from the surviving samples by ownership,
	/// when `NOXI_EVAL_OWNERSHIP_CENSUS` requests it. Read-only.
	#[cfg(feature = "eval-diagnostics")]
	pub(crate) fn ownership_census() -> Option<serde_json::Value> {
		let limits = census::Limits::from_env()?;
		let (roots, sample_rate_inverse) = RETENTION_SAMPLES.with_borrow(|samples| {
			let roots = samples
				.entries
				.iter()
				.filter_map(|(kind, weak)| Some((*kind, weak.upgrade()?)))
				.collect();
			(roots, u64::from(samples.sample_mask) + 1)
		});
		serde_json::to_value(census::run(roots, limits, sample_rate_inverse)).ok()
	}

	#[inline]
	fn from_repr(repr: ThunkRepr, _kind: &'static str) -> Self {
		let thunk = Self(RcCell::new(repr));
		#[cfg(feature = "eval-diagnostics")]
		thunk.record_creation(_kind);
		thunk
	}

	/// Preserve an already computed value in a memoizable cell. A nested thunk
	/// still follows the ordinary forcing path, including alias updates.
	pub(crate) fn new_evaluated(value: Value) -> Self {
		Self::from_repr(ThunkRepr::Evaluated(value), "evaluated")
	}

	pub fn new_closure(lambda: Rc<Lambda>) -> Self {
		let upvalues = Upvalues::with_capacity(lambda.upvalue_count as usize);
		Self::new_closure_with_upvalues(lambda, upvalues)
	}

	pub(crate) fn new_closure_with_upvalues(lambda: Rc<Lambda>, upvalues: Upvalues) -> Self {
		Self::from_repr(
			ThunkRepr::Evaluated(Value::Closure(Rc::new(Closure {
				upvalues: Rc::new(upvalues),
				lambda,
			}))),
			"closure",
		)
	}

	/// A thunk of `lambda` suspended at `span`. Thunks take this span from
	/// their lambda, so it is recorded there; a lambda that already has other
	/// owners must already record it.
	pub fn new_suspended(mut lambda: Rc<Lambda>, span: Span) -> Self {
		if lambda.suspended_span != Some(span) {
			let lambda = Rc::get_mut(&mut lambda);
			debug_assert!(
				lambda.is_some(),
				"a shared lambda must already record the span of its thunks"
			);
			if let Some(lambda) = lambda {
				lambda.suspended_span = Some(span);
			}
		}
		let upvalues = Upvalues::with_capacity(lambda.upvalue_count as usize);
		Self::new_suspended_with_upvalues(lambda, upvalues)
	}

	/// A thunk of `lambda` suspended at [`Lambda::thunk_span`].
	pub(crate) fn new_suspended_with_upvalues(lambda: Rc<Lambda>, upvalues: Upvalues) -> Self {
		Self::from_repr(ThunkRepr::Suspended { upvalues, lambda }, "suspended")
	}

	pub fn new_suspended_native(native: Box<dyn Fn() -> Result<Value, ErrorKind>>) -> Self {
		Self::from_repr(ThunkRepr::Native(SuspendedNative(native)), "native")
	}

	/// Helper function to create a [`Thunk`] that calls a function given as the
	/// [`Value`] `callee` with the argument `arg` when it is forced. This is
	/// particularly useful in builtin implementations if the result of calling
	/// a function does not need to be forced immediately, because e.g. it is
	/// stored in an attribute set.
	pub fn new_suspended_call(callee: Value, arg: Value, span: Span) -> Self {
		Self::new_suspended_call_with_lambda(callee, arg, Self::suspended_call_lambda(span))
	}

	/// Builtins producing many lazy calls can share this bytecode. Arguments
	/// belong to each thunk's captures, so forcing one call cannot change another.
	pub(crate) fn suspended_call_lambda(span: Span) -> Rc<Lambda> {
		Self::suspended_call_lambda_at(span, false, false)
	}

	pub(crate) fn suspended_call_lambda_at(
		span: Span,
		generated_call_position: bool,
		omit_call_position: bool,
	) -> Rc<Lambda> {
		let mut lambda = Lambda::default();
		lambda.upvalue_count = 2;
		lambda.synthetic_call = true;
		// The first span of the chunk, as every thunk of this lambda records.
		lambda.suspended_span = Some(span);
		lambda.generated_call_position = generated_call_position;
		lambda.omit_call_position = omit_call_position;

		// This is basically a recreation of compile_apply():
		// We need to push the argument onto the stack and then the function.
		// The function (not the argument) needs to be forced before calling.
		lambda.chunk.push_op(Op::GetUpvalue, span);
		lambda.chunk.push_uvarint(0);
		lambda.chunk.push_op(Op::GetUpvalue, span);
		lambda.chunk.push_uvarint(1);
		lambda.chunk.push_op(Op::Force, span);
		lambda.chunk.push_op(Op::Call, span);

		// Inform the VM that the chunk has ended
		lambda.chunk.push_op(Op::Return, span);

		Rc::new(lambda)
	}

	pub(crate) fn new_suspended_call_with_lambda(
		callee: Value,
		arg: Value,
		lambda: Rc<Lambda>,
	) -> Self {
		Self::new_suspended_with_upvalues(lambda, Upvalues::from_raw_parts(vec![arg, callee], None))
	}

	fn prepare_blackhole(&self, forced_at: Span) -> ThunkRepr {
		match &*self.0.borrow() {
			ThunkRepr::Suspended { lambda, .. } => ThunkRepr::Blackhole {
				forced_at,
				origin: Some([lambda.thunk_span(), lambda.chunk.first_span()]),
			},

			_ => ThunkRepr::Blackhole {
				forced_at,
				origin: None,
			},
		}
	}

	pub async fn force(myself: Thunk, co: GenCo, span: Span) -> Result<Value, ErrorKind> {
		Self::force_(myself, &co, span).await
	}
	pub async fn force_(mut myself: Thunk, co: &GenCo, span: Span) -> Result<Value, ErrorKind> {
		// This vector of "thunks which point to the thunk-being-forced", to
		// be updated along with it, is necessary in order to write this
		// function in iterative (and later, mutual-tail-call) form.
		let mut also_update: Vec<RcCell<ThunkRepr>> = vec![];

		loop {
			// If the current thunk is already fully evaluated, return its evaluated
			// value. The VM will continue running the code that landed us here.
			if myself.is_forced() {
				#[cfg(feature = "eval-diagnostics")]
				crate::perf_counters::thunk_force(false);
				let val = Self::copy_memoized_error(myself.unwrap_or_clone());
				for other_thunk in also_update.into_iter() {
					other_thunk.replace(ThunkRepr::Evaluated(val.clone()));
				}
				return Ok(val);
			}

			// Begin evaluation of this thunk by marking it as a blackhole, meaning
			// that any other forcing frame encountering this thunk before its
			// evaluation is completed detected an evaluation cycle.
			let inner = myself.0.replace(myself.prepare_blackhole(span));

			let value = match inner {
				// If there was already a blackhole in the thunk, this is an
				// evaluation cycle.
				ThunkRepr::Blackhole { forced_at, origin } => {
					return Err(ErrorKind::InfiniteRecursion {
						first_force: forced_at,
						suspended_at: origin.map(|[suspended_at, _]| suspended_at),
						content_span: origin.map(|[_, content_span]| content_span),
					});
				}

				// Evaluate native computations without entering bytecode.
				ThunkRepr::Native(native) => {
					#[cfg(feature = "eval-diagnostics")]
					crate::perf_counters::thunk_force(true);
					native.0()?
				}

				// When encountering a suspended thunk, request that the VM enters
				// it and produces the result.
				ThunkRepr::Suspended { lambda, upvalues } => {
					#[cfg(feature = "eval-diagnostics")]
					crate::perf_counters::thunk_force(true);
					// TODO(amjoseph): use #[tailcall::mutual] here.  This can
					// be turned into a tailcall to vm::execute_bytecode() by
					// passing `also_update` to it.
					let span = lambda.thunk_span();
					generators::request_enter_lambda(co, lambda, upvalues, span).await
				}

				// nested thunks -- try to flatten before forcing
				ThunkRepr::Evaluated(Value::Thunk(inner_thunk)) => {
					match RcCell::try_unwrap(inner_thunk.0) {
						Ok(repr) => {
							// we are the only reference to the inner thunk,
							// so steal it
							myself.0.replace(repr);
							continue;
						}
						Err(rc) => {
							let inner_thunk = Thunk(rc);
							if inner_thunk.is_forced() {
								// tail call to force the inner thunk; note that
								// this means the outer thunk remains unforced
								// even after calling force() on it; however the
								// next time it is forced we will be one
								// thunk-forcing closer to it being
								// fully-evaluated.
								myself
									.0
									.replace(ThunkRepr::Evaluated(inner_thunk.value().clone()));
								continue;
							}
							also_update.push(myself.0.clone());
							myself = inner_thunk;
							continue;
						}
					}
				}

				ThunkRepr::Evaluated(val) => {
					return Ok(val);
				}
			};

			if matches!(value, Value::Thunk(_)) {
				myself.0.replace(ThunkRepr::Evaluated(value));
				continue;
			}

			// Return the completed value directly. Shared thunks and aliases still
			// memoize it, while a uniquely owned thunk can simply be dropped. This
			// avoids storing, testing and borrowing the result again on another lap.
			if RcCell::strong_count(&myself.0) > 1 {
				myself.0.replace(ThunkRepr::Evaluated(value.clone()));
			}
			for other_thunk in also_update {
				other_thunk.replace(ThunkRepr::Evaluated(value.clone()));
			}
			return Ok(value);
		}
	}

	/// Begin forcing a suspended bytecode thunk as [`Thunk::force`] would,
	/// returning the code the VM enters for it. Other states return `None`
	/// and keep needing the generator.
	pub(crate) fn begin_direct_force(
		&self,
		forced_at: Span,
	) -> Option<(Rc<Lambda>, Upvalues, Span)> {
		if !matches!(*self.0.borrow(), ThunkRepr::Suspended { .. }) {
			return None;
		}
		match self.0.replace(self.prepare_blackhole(forced_at)) {
			ThunkRepr::Suspended { lambda, upvalues } => {
				#[cfg(feature = "eval-diagnostics")]
				crate::perf_counters::thunk_force(true);
				let span = lambda.thunk_span();
				Some((lambda, upvalues, span))
			}
			_ => unreachable!("the thunk was just checked to be suspended"),
		}
	}

	/// Store the result of code entered by [`Thunk::begin_direct_force`],
	/// as [`Thunk::force`] does. For a result that is itself a thunk, this
	/// thunk is left pointing to it and `None` returned: it still needs
	/// the generator to finish forcing.
	pub(crate) fn finish_direct_force(&self, value: Value) -> Option<Value> {
		if matches!(value, Value::Thunk(_)) {
			self.0.replace(ThunkRepr::Evaluated(value));
			return None;
		}
		if RcCell::strong_count(&self.0) > 1 {
			self.0.replace(ThunkRepr::Evaluated(value.clone()));
		}
		Some(value)
	}

	pub(crate) fn determining_span(&self) -> Option<Span> {
		match &*self.0.borrow() {
			ThunkRepr::Suspended { lambda, .. } => {
				lambda.span.or_else(|| Some(lambda.thunk_span()))
			}
			ThunkRepr::Evaluated(Value::Thunk(thunk)) => thunk.determining_span(),
			_ => None,
		}
	}

	fn copy_memoized_error(value: Value) -> Value {
		match value {
			Value::Catchable(kind) => {
				if let crate::errors::CatchableErrorKind::Traced { info, kind: cause } = &*kind
					&& let Some(prefix) = info.memoized_prefix.get()
				{
					let traces = info.traces.borrow();
					return Value::Catchable(Box::new(crate::errors::CatchableErrorKind::Traced {
						kind: cause.clone(),
						info: Rc::new(crate::errors::CatchableTrace {
							traces: traces[prefix.min(traces.len())..].to_vec().into(),
							memoized_prefix: Default::default(),
							span: info.span,
							message: info.message.clone(),
							relative: true,
						}),
					}));
				}
				Value::Catchable(kind)
			}
			value => value,
		}
	}

	pub fn finalise(&self, stack: &[Value]) {
		let mut upvalues = self.upvalues_mut();
		#[cfg(feature = "eval-cycle-collection")]
		let candidate = (0..upvalues.len()).any(|i| {
			matches!(
				&upvalues[crate::opcode::UpvalueIdx(i)],
				Value::DeferredUpvalue(index) if stack[index.0].is_thunk()
			)
		});
		upvalues.resolve_deferred_upvalues(stack);
		#[cfg(feature = "eval-cycle-collection")]
		if candidate {
			cycles::register(self);
		}
	}

	/// Try a bounded number of deferred candidates at a VM scheduler boundary.
	#[cfg(feature = "eval-cycle-collection")]
	pub(crate) fn collect_cycles() {
		cycles::collect();
	}

	pub fn is_evaluated(&self) -> bool {
		matches!(*self.0.borrow(), ThunkRepr::Evaluated(_))
	}

	pub fn is_suspended(&self) -> bool {
		matches!(
			*self.0.borrow(),
			ThunkRepr::Suspended { .. } | ThunkRepr::Native(_)
		)
	}

	/// Returns true if forcing this thunk will not change it.
	pub fn is_forced(&self) -> bool {
		self.0.borrow().is_forced()
	}

	/// Read a memoized result without scheduling a forcing generator. An
	/// evaluated thunk pointing to another thunk still needs normal forcing.
	pub(crate) fn force_if_evaluated(self) -> Result<Value, Self> {
		if self.is_forced() {
			#[cfg(feature = "eval-diagnostics")]
			crate::perf_counters::thunk_force(false);
			Ok(Self::copy_memoized_error(self.unwrap_or_clone()))
		} else {
			Err(self)
		}
	}

	/// The value [`force_if_evaluated`](Self::force_if_evaluated) yields for
	/// a memoized thunk, without taking ownership of the thunk.
	pub(crate) fn value_if_evaluated(&self) -> Option<Value> {
		let repr = self.0.borrow();
		if !repr.is_forced() {
			return None;
		}
		#[cfg(feature = "eval-diagnostics")]
		crate::perf_counters::thunk_force(false);
		Some(Self::copy_memoized_error(repr.expect_ref().clone()))
	}

	/// Returns a reference to the inner evaluated value of a thunk.
	/// It is an error to call this on a thunk that has not been
	/// forced, or is not otherwise known to be fully evaluated.
	// Note: Due to the interior mutability of thunks this is
	// difficult to represent in the type system without impacting the
	// API too much.
	pub fn value(&self) -> Ref<'_, Value> {
		Ref::map(self.0.borrow(), |thunk| match thunk {
			ThunkRepr::Evaluated(value) => value,
			ThunkRepr::Blackhole { .. } => panic!("Thunk::value called on a black-holed thunk"),
			ThunkRepr::Suspended { .. } | ThunkRepr::Native(_) => {
				panic!("Thunk::value called on a suspended thunk")
			}
		})
	}

	/// Returns the inner evaluated value of a thunk, cloning it if
	/// the Rc has more than one strong reference.  It is an error
	/// to call this on a thunk that has not been forced, or is not
	/// otherwise known to be fully evaluated.
	fn unwrap_or_clone(self) -> Value {
		match RcCell::try_unwrap(self.0) {
			Ok(repr) => repr.expect(),
			Err(rc) => Ref::map(rc.borrow(), |thunkrepr| thunkrepr.expect_ref()).clone(),
		}
	}

	pub fn upvalues(&self) -> Ref<'_, Upvalues> {
		Ref::map(self.0.borrow(), |thunk| match thunk {
			ThunkRepr::Suspended { upvalues, .. } => upvalues,
			ThunkRepr::Evaluated(Value::Closure(c)) => &c.upvalues,
			_ => panic!("upvalues() on non-suspended thunk"),
		})
	}

	pub fn upvalues_mut(&self) -> RefMut<'_, Upvalues> {
		RefMut::map(self.0.borrow_mut(), |thunk| match thunk {
			ThunkRepr::Suspended { upvalues, .. } => upvalues,
			ThunkRepr::Evaluated(Value::Closure(c)) => {
				Rc::get_mut(&mut Rc::get_mut(c).unwrap().upvalues).expect(
					"upvalues_mut() was called on a thunk which already had multiple references to it",
				)
			}
			thunk => panic!("upvalues() on non-suspended thunk: {thunk:?}"),
		})
	}

	/// Do not use this without first reading and understanding
	/// `noxi/docs/value-pointer-equality.md`.
	pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
		RcCell::ptr_eq(&self.0, &other.0)
	}

	/// The identity of the thunk, for detecting repeated values.
	pub(super) fn as_ptr(&self) -> *const () {
		RcCell::as_ptr(&self.0).cast()
	}

	/// Helper function to format thunks in observer output.
	pub(crate) fn debug_repr(&self) -> String {
		self.0.borrow().debug_repr()
	}
}

impl TotalDisplay for Thunk {
	fn total_fmt(&self, f: &mut std::fmt::Formatter<'_>, set: &mut ThunkSet) -> std::fmt::Result {
		if !set.insert(self) {
			return f.write_str("<CYCLE>");
		}

		match &*self.0.borrow() {
			ThunkRepr::Evaluated(v) => v.total_fmt(f, set),
			ThunkRepr::Suspended { .. } | ThunkRepr::Native(_) => f.write_str("<CODE>"),
			other => write!(f, "internal[{}]", other.debug_repr()),
		}
	}
}

/// A wrapper type for tracking which thunks have already been seen
/// in a context. This is necessary for printing and deeply forcing
/// cyclic non-diverging data structures like `rec { f = [ f ]; }`.
/// This is separate from the ThunkRepr::Blackhole mechanism, which
/// detects diverging data structures like `(rec { f = f; }).f`.
///
/// The inner `HashSet` is not available on the outside, as it would be
/// potentially unsafe to interact with the pointers in the set.
#[derive(Default)]
pub struct ThunkSet(FxHashSet<*const ThunkRepr>);

impl ThunkSet {
	/// Check whether the given thunk has already been seen. Will mark the thunk
	/// as seen otherwise.
	pub fn insert(&mut self, thunk: &Thunk) -> bool {
		let ptr: *const ThunkRepr = RcCell::as_ptr(&thunk.0);
		self.0.insert(ptr)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn forcing_moves_captures_out_of_the_thunk_cell() {
		let captured = Rc::new(());
		let weak = Rc::downgrade(&captured);
		let captured = Thunk::new_suspended_native(Box::new(move || {
			let _keep_alive = &captured;
			Ok(Value::Null)
		}));
		let span = crate::test_utils::dummy_span();
		let mut lambda = Lambda::default();
		lambda.upvalue_count = 1;
		lambda.chunk.push_op(Op::Return, span);
		let thunk = Thunk::new_suspended_with_upvalues(
			Rc::new(lambda),
			Upvalues::from_raw_parts(vec![Value::Thunk(captured)], None),
		);
		let alias = thunk.clone();
		let (_, captures, _) = thunk.begin_direct_force(span).unwrap();
		assert!(weak.upgrade().is_some());
		drop(captures);
		assert!(weak.upgrade().is_none());
		assert!(matches!(
			thunk.finish_direct_force(Value::Integer(42)),
			Some(Value::Integer(42))
		));
		assert_eq!(alias.value().as_int().unwrap(), 42);
	}

	#[test]
	fn forcing_releases_shared_with_captures_after_last_owner() {
		let resource = Rc::new(());
		let weak = Rc::downgrade(&resource);
		let captured = Thunk::new_suspended_native(Box::new(move || {
			let _keep_alive = &resource;
			Ok(Value::Null)
		}));
		let shared: Rc<[Value]> = vec![Value::Thunk(captured)].into();
		let span = crate::test_utils::dummy_span();
		let mut lambda = Lambda::default();
		lambda.chunk.push_op(Op::Return, span);
		let thunk = Thunk::new_suspended_with_upvalues(
			Rc::new(lambda),
			Upvalues::from_raw_parts(vec![], Some(shared.clone())),
		);
		let (_, captures, _) = thunk.begin_direct_force(span).unwrap();
		assert!(Rc::ptr_eq(captures.shared_with_stack().unwrap(), &shared));
		drop(captures);
		assert!(weak.upgrade().is_some());
		drop(shared);
		assert!(weak.upgrade().is_none());
	}

	#[test]
	fn finalising_deferred_capture_preserves_thunk_identity() {
		let span = crate::test_utils::dummy_span();
		let captured = Thunk::new_suspended_native(Box::new(|| Ok(Value::Integer(42))));
		let mut lambda = Lambda::default();
		lambda.upvalue_count = 1;
		lambda.chunk.push_op(Op::Return, span);
		let thunk = Thunk::new_suspended_with_upvalues(
			Rc::new(lambda),
			Upvalues::from_raw_parts(
				vec![Value::DeferredUpvalue(crate::opcode::StackIdx(0))],
				None,
			),
		);
		thunk.finalise(&[Value::Thunk(captured.clone())]);
		let (_, captures, _) = thunk.begin_direct_force(span).unwrap();
		let Value::Thunk(finalised) = &captures[crate::opcode::UpvalueIdx(0)] else {
			panic!("deferred capture was not finalised");
		};
		assert!(finalised.ptr_eq(&captured));
	}

	#[test]
	fn evaluated_wrapper_preserves_nested_identity_and_memoized_failure() {
		fn force(thunk: Thunk) -> Value {
			let mut generator = generators::Gen::new(|co| {
				generators::pin_generator(Thunk::force(thunk, co, crate::test_utils::dummy_span()))
			});
			match generator.resume_with(generators::VMResponse::Empty) {
				genawaiter::GeneratorState::Complete(Ok(value)) => value,
				_ => panic!("native thunk force should complete immediately"),
			}
		}

		let calls = Rc::new(std::cell::Cell::new(0));
		let counter = calls.clone();
		let inner = Thunk::new_suspended_native(Box::new(move || {
			counter.set(counter.get() + 1);
			Ok(Value::Catchable(Box::new(
				crate::errors::CatchableErrorKind::Throw("cached failure".into()),
			)))
		}));
		let wrapper = Thunk::new_evaluated(Value::Thunk(inner.clone()));
		{
			let value = wrapper.value();
			let Value::Thunk(nested) = &*value else {
				panic!("wrapper should retain its nested thunk");
			};
			assert!(nested.ptr_eq(&inner));
		}
		assert!(!wrapper.is_forced());
		assert!(matches!(force(wrapper.clone()), Value::Catchable(_)));
		assert!(matches!(force(wrapper), Value::Catchable(_)));
		assert!(matches!(force(inner), Value::Catchable(_)));
		assert_eq!(calls.get(), 1);
	}

	#[test]
	fn unique_native_thunk_completes_without_yielding() {
		let thunk = Thunk::new_suspended_native(Box::new(|| Ok(Value::Integer(42))));
		let mut generator = generators::Gen::new(|co| {
			generators::pin_generator(Thunk::force(thunk, co, crate::test_utils::dummy_span()))
		});
		assert!(matches!(
			generator.resume_with(generators::VMResponse::Empty),
			genawaiter::GeneratorState::Complete(Ok(Value::Integer(42)))
		));
	}
}
