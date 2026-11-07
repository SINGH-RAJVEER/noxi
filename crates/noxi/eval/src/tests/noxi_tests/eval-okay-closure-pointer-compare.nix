# Checks evaluator value pointer equality behavior
let
  g = x:
    owo: "th" + x;
in
[
  (
    { q = g "ia"; } == { q = g ("i" + "a"); }
  )

  (
    [ (g "ia") ] == [ (g ("i" + "a")) ]
  )
]
