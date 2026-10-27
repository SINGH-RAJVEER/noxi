let
	add = builtins.add;
	addTen = add 10;
	addTwenty = add 20;
	getX = builtins.getAttr "x";
	getY = builtins.getAttr "y";
in
[
	(addTen 1)
	(addTwenty 2)
	(addTen 3)
	(add 4 5)
	(getX { x = 6; })
	(getY { y = 7; })
	(getX { x = 8; })
]
