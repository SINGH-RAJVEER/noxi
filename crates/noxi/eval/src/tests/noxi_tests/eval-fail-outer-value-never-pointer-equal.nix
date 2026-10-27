# Checks evaluator value pointer equality behavior
let
  x = { foo = throw "foo"; };
in

# while `builtins.seq x null` would succeed, this fails!
x == x
