import itertools
import random

def generate(root):
	cases = []
	seen = set()

	def add(category, expression, flags=None):
		key = (expression, tuple(flags or []))
		if key not in seen:
			seen.add(key)
			cases.append({'category': category, 'expression': expression, 'flags': flags or []})

	numbers = ['(-9223372036854775807 - 1)', '-9223372036854775807', '-7', '-1', '0', '1', '2', '7', '9223372036854775806', '9223372036854775807', '-0.5', '0.0', '1.5', '1.0e20']
	for a, b, op in itertools.product(numbers, numbers, ['+', '-', '*', '/', '==', '!=', '<', '<=', '>', '>=']):
		add('numeric-operator', f'({a}) {op} ({b})')
	for name, a, b in itertools.product(['div', 'bitAnd', 'bitOr', 'bitXor'], numbers, numbers):
		add('numeric-builtin', f'builtins.{name} ({a}) ({b})')

	atoms = ['null', 'true', 'false', '0', '1', '1.0', '""', '"a"', '[]', '[ 1 ]', '{}', '{ a = 1; }', '(x: x)']
	for a, b, op in itertools.product(atoms, atoms, ['==', '<']):
		add('mixed-equality-order', f'({a}) {op} ({b})')

	for n in range(12):
		for expr in [
			f'let x = {n}; in (x: x + 1) (x + 2)',
			f'let x = {n}; f = y: x + y; in with {{ x = 999; }}; f 3',
			f'(rec {{ a = {n}; b = a + 1; nested = {{ inherit a b; }}; }}).nested',
			f'let a = {n}; in ({{ a ? a + 1, b ? a + 2 }}: [ a b ]) {{}}',
			f'({{ a ? {n}, b ? a + 1, ... }}@args: [ a b (builtins.attrNames args) ]) {{ z = 3; }}',
			f'let f = {{ __functor = self: x: self.n + x; n = {n}; }}; in f 2',
			f'let x = {n}; in (rec {{ x = x + 1; }}).x',
			f'let x = {n}; in with {{ x = 900; y = x + 1; }}; [ x y ]',
			f'let a = {{ x = {n}; }}; b = a // {{ y = a.x + 1; }}; in [ a b (b // {{ x = null; }}) ]',
			f'let xs = builtins.genList (x: x) {n}; in [ (builtins.foldl\' (a: b: a + b) 0 xs) (builtins.filter (x: x < 4) xs) (builtins.map (x: x * x) xs) ]',
			f'let f = x: if x == 0 then 0 else f (x - 1) + 1; in f {n}',
			f'let f = x: if x == 0 then [] else [x] ++ f (x - 1); in f {n}',
		]:
			add('scopes-closures-lists', expr)

	traps = ['throw "trap"', 'abort "trap"', '(1 / 0)', '(builtins.head [])', '(let x = x; in x)']
	for trap in traps:
		for expr in [
			f'if true then 3 else ({trap})', f'if false then ({trap}) else 4',
			f'false && ({trap})', f'true || ({trap})',
			f'let x = {trap}; in 1', f'builtins.seq 1 ({{ x = {trap}; }} ? x)',
			f'(x: 1) ({trap})', f'builtins.length [ ({trap}) ]',
			f'builtins.attrNames {{ x = {trap}; }}', f'({{ x = {trap}; }} // {{ x = 1; }}).x',
			f'builtins.tryEval ({trap})', f'builtins.tryEval (builtins.deepSeq [ ({trap}) ] 1)',
			f'builtins.tryEval (builtins.seq [ ({trap}) ] 1)',
			f'builtins.tryEval (builtins.addErrorContext "outer" ({trap}))',
		]:
			add('laziness-catchability', expr)

	for expr in [
		'let f = x: x; in f == f',
		'let a = { f = x: x; }; in a == a',
		'let a = [ (x: x) ]; in a == a',
		'let a = rec { x = a; }; in a == a',
		'let a = [ 1 ]; in builtins.deepSeq a [ a a ]',
		'builtins.sort (a: b: a < b) [ 2 1.0 0 (-1) 1 ]',
		'builtins.genericClosure { startSet = [ { key = 1; } ]; operator = x: if x.key < 4 then [{ key = x.key + 1; }] else []; }',
		'builtins.genericClosure { startSet = [{ key = null; } { key = null; }]; operator = _: []; }',
		'builtins.zipAttrsWith (name: vals: [ name (builtins.length vals) ]) [{ a = 1; b = 2; } { a = 3; }]',
		'builtins.intersectAttrs { a = throw "left"; } { a = 1; b = throw "right"; }',
		'builtins.removeAttrs { a = throw "removed"; b = 2; } [ "a" ]',
		'builtins.catAttrs "a" [ {} { a = 2; } { a = 3; } ]',
		'({ x ? 1 }: x) { y = throw "unused"; }',
		'let a.b = 1; a.c = 2; in a',
		'let a = {}; inherit (a) missing; in 1',
		'with { a = 1; }; with { a = 2; }; a',
		'let a = 1; in with (throw "unused"); a',
	]:
		add('structural-semantics', expr)

	rng = random.Random(497629)
	def integer(depth):
		if depth <= 0:
			return str(rng.randrange(-10, 11))
		choice = rng.randrange(6)
		if choice == 0: return f'({integer(depth-1)}) + ({integer(depth-1)})'
		if choice == 1: return f'({integer(depth-1)}) * ({integer(depth-1)})'
		if choice == 2: return f'if {boolean(depth-1)} then ({integer(depth-1)}) else ({integer(depth-1)})'
		if choice == 3: return f'let x = ({integer(depth-1)}); in (y: x - y) ({integer(depth-1)})'
		if choice == 4: return f'builtins.foldl\' (a: b: a + b) 0 [ ({integer(depth-1)}) ({integer(depth-1)}) ]'
		return f'({{ a = ({integer(depth-1)}); b = throw "unused"; }}).a'
	def boolean(depth):
		if depth <= 0: return rng.choice(['true', 'false'])
		return f'({integer(depth-1)}) {rng.choice(["<", "==", "<=", "!="])} ({integer(depth-1)})'
	for i in range(400):
		add('generated-typed-composition', integer(3))

	# Error traces are compared explicitly for a bounded representative subset.
	for expr in ['builtins.div 1 0', '(x: x + 1) "x"', '({ a, ... }: a) {}',
		'builtins.head []', '1 < null', '1 == (throw "bad")',
		'let a = throw "bad"; in builtins.seq (builtins.tryEval a) a',
		'let x = 1; in x.y', 'builtins.elemAt [] 0']:
		add('explicit-trace-diagnostics', expr, ['--show-trace'])
	return cases
