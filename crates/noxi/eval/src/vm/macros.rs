/// This module provides macros which are used in the implementation
/// of the VM for the implementation of repetitive operations.
///
/// This macro simplifies the implementation of arithmetic operations,
/// correctly handling the behaviour on different pairings of number
/// types.
#[macro_export]
macro_rules! arithmetic_op {
	( $self:ident, $op:tt ) => {{ // TODO: remove
		let b = $self.pop();
		let a = $self.pop();
		let result = fallible!($self, arithmetic_op!(&a, &b, $op));
		$self.push(result);
	}};

	( $a:expr, $b:expr, + ) => {
		$crate::arithmetic_op!(@checked $a, $b, +, checked_add, "adding")
	};
	( $a:expr, $b:expr, - ) => {
		$crate::arithmetic_op!(@checked $a, $b, -, checked_sub, "subtracting")
	};
	( $a:expr, $b:expr, * ) => {
		$crate::arithmetic_op!(@checked $a, $b, *, checked_mul, "multiplying")
	};
	( $a:expr, $b:expr, / ) => {{
		let (a, b) = ($a, $b);
		// The reference checks the divisor before choosing integer or float arithmetic.
		match b {
			$crate::Value::Integer(0) => Err($crate::ErrorKind::DivisionByZero),
			$crate::Value::Float(f) if *f == 0.0 => Err($crate::ErrorKind::DivisionByZero),
			_ => $crate::arithmetic_op!(@checked a, b, /, checked_div, "dividing"),
		}
	}};

	( @checked $a:expr, $b:expr, $op:tt, $checked:ident, $operation:literal ) => {{
		match ($a, $b) {
			($crate::Value::Integer(i1), $crate::Value::Integer(i2)) => i1
				.$checked(*i2)
				.map($crate::Value::Integer)
				.ok_or($crate::ErrorKind::IntegerOverflow {
					operation: $operation,
					operator: stringify!($op),
					lhs: *i1,
					rhs: *i2,
				}),
			($crate::Value::Float(f1), $crate::Value::Float(f2)) => Ok($crate::Value::Float(f1 $op f2)),
			($crate::Value::Integer(i1), $crate::Value::Float(f2)) => Ok($crate::Value::Float(*i1 as f64 $op f2)),
			($crate::Value::Float(f1), $crate::Value::Integer(i2)) => Ok($crate::Value::Float(f1 $op *i2 as f64)),

			(v1, v2) => {
				let found = if v1.is_number() { v2 } else { v1 };
				Err($crate::ErrorKind::TypeError {
					expected: "number (either int or float)",
					actual: found.type_of(),
					value: Some(Box::new(found.clone())),
				})
			}
		}
	}};
}

/// This macro simplifies the implementation of comparison operations.
#[macro_export]
macro_rules! cmp_op {
	( $vm:ident, $frame:ident, $span:ident, $op:tt ) => {{
		lifted_pop! {
			$vm(b, a) => {
				if let Some(ordering) = $crate::vm::native::compare(&a, &b) {
					$vm.stack.push(Value::Bool(cmp_op!(@order $op ordering)));
					continue;
				}
				async fn compare(a: Value, b: Value, co: GenCo) -> Result<Value, ErrorKind> {
					let a = generators::request_force(&co, a).await;
					let b = generators::request_force(&co, b).await;
					let span = generators::request_span(&co).await;
					let ordering = a.nix_cmp_ordering(b, co, span).await?;
					match ordering {
						Err(cek) => Ok(Value::from(cek)),
						Ok(ordering) => Ok(Value::Bool(cmp_op!(@order $op ordering))),
					}
				}

				let gen_span = $frame.current_span();
				$vm.push_bytecode_frame($span, $frame);
				$vm.enqueue_generator("compare", gen_span, |co| compare(a, b, co));
				return Ok(false);
			}
		}
	}};

	(@order < $ordering:expr) => {
		$ordering == Ordering::Less
	};

	(@order > $ordering:expr) => {
		$ordering == Ordering::Greater
	};

	(@order <= $ordering:expr) => {
		matches!($ordering, Ordering::Equal | Ordering::Less)
	};

	(@order >= $ordering:expr) => {
		matches!($ordering, Ordering::Equal | Ordering::Greater)
	};
}

#[macro_export]
macro_rules! lifted_pop {
    ($vm:ident ($($bind:ident),+) => $body:expr) => {
        {
            $(
                let $bind = $vm.stack_pop();
            )+
            $(
                if $bind.is_catchable() {
                    $vm.stack.push($bind);
                    continue;
                }
            )+
            $body
        }
    }
}
