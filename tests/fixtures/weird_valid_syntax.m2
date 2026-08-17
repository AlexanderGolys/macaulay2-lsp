Thing Thing := (a, b) -> "call(" | toString a | ", " | toString b | ")";
Thing .. Thing := (a, b) -> "[" | toString a | " .. " | toString b | "]";
Thing _ Thing := (a, b) -> "(" | toString a | ")_(" | toString b | "]";

keyStr := t -> k -> (toString k | "=>" | (toString (t#k)))
p := t -> keyStr(t) \ keys t
f := (t) -> toString p t
toString Tally := f
-- e2 := e
-- new AtomicInt from 2
[e_0..e_5]
e2 = tally {e2}
e_2 = tally {e2}
e_2. = tally {e2}

2..e2
2..2
2...2
-- ERROR: 2....2
e2..e2
e2.e2
e2.e2..e2
2. ..e2
2. ...2
2..e2..2
e2.e2...2e-2
2. e2. e2 2..e2
-- M2 numeric syntax allows extra precision letters in floats; the trailing
-- symbol-like tail is then parsed through adjacency/SPACE, which is installed
-- above for Thing Thing.
2x2
.2x.2
x.2
2e2
2 e2
2 e 2
2 ex
2p2e2e2
2p2e2p2e2
-- ERROR: 2e
-- ERROR: 2ex
e_2.e2
--e_2 . e2
keys e_2.
e_2. e2
disassemble (() -> 2. .e2) -- keyError, but syntax ok:
s#"1".a
"1".2
