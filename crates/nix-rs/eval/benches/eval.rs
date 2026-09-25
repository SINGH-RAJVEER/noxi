use criterion::{Criterion, black_box, criterion_group, criterion_main};
use itertools::Itertools;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn interpret(code: &str) {
	nix_rs_eval::Evaluation::builder_pure()
		.build()
		.evaluate(code, None);
}

fn eval_literals(c: &mut Criterion) {
	c.bench_function("int", |b| {
		b.iter(|| {
			interpret(black_box("42"));
		})
	});
}

fn eval_merge_attrs(c: &mut Criterion) {
	c.bench_function("merge small attrs", |b| {
		b.iter(|| {
			interpret(black_box("{ a = 1; b = 2; } // { c = 3; }"));
		})
	});

	c.bench_function("merge large attrs with small attrs", |b| {
		let large_attrs = format!(
			"{{{}}}",
			(0..10000).map(|n| format!("a{n} = {n};")).join(" ")
		);
		let expr = format!("{large_attrs} // {{ c = 3; }}");
		b.iter(move || {
			interpret(black_box(&expr));
		})
	});

	c.bench_function("merge small attrs with large attrs", |b| {
		let large_attrs = format!(
			"{{{}}}",
			(0..10000).map(|n| format!("a{n} = {n};")).join(" ")
		);
		let expr = format!("{{ c = 3 }} // {large_attrs}");
		b.iter(move || {
			interpret(black_box(&expr));
		})
	});
}

fn eval_intersect_attrs(c: &mut Criterion) {
	c.bench_function("intersect small attrs", |b| {
		b.iter(|| {
			interpret(black_box(
				"builtins.intersectAttrs { a = 1; b = 2; } { c = 3; }",
			));
		})
	});

	c.bench_function("intersect large attrs with small attrs", |b| {
		let large_attrs = format!(
			"{{{}}}",
			(0..10000).map(|n| format!("a{n} = {n};")).join(" ")
		);
		let expr = format!("builtins.intersectAttrs {large_attrs} {{ c = 3; }}");
		b.iter(move || {
			interpret(black_box(&expr));
		})
	});

	c.bench_function("intersect large attrs with large attrs", |b| {
		// the intersection is 2n=3m, which is about ~⅓ of the union, and ~¼ of the elements < 2*1e4

		let left_attrs = format!(
			"{{{}}}",
			(0..10000)
				.map(|n| {
					let i = 2 * n;
					format!("a{i} = {i};")
				})
				.join(" ")
		);

		let right_attrs = format!(
			"{{{}}}",
			(0..10000)
				.map(|m| {
					let j = 3 * m;
					format!("a{j} = {j};")
				})
				.join(" ")
		);

		let expr = format!("builtins.intersectAttrs {left_attrs} {right_attrs}");
		b.iter(move || {
			interpret(black_box(&expr));
		})
	});
}

criterion_group!(
	benches,
	eval_literals,
	eval_merge_attrs,
	eval_intersect_attrs
);
criterion_main!(benches);
