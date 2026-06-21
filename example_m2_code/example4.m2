opts = {Slope => 1, Intercept => 1}
g = opts >> o -> x -> x*o.Slope + o.Intercept

p = method(Binary => true, TypicalValue => List)
p(ZZ, ZZ) := p(List, ZZ) := (i, j) -> {i, j}
p(CC, CC) := Array => (i, j) -> [i, j]

f = method()
f ZZ := x -> -x;
f(ZZ, String) := (n, s) -> concatenate(n:s);
f(String, ZZ, String) := (s, n, t) -> concatenate(s, " : ", toString n, " : ", t);

Cu = new Type of List
w = new Cu from {1, -2}
expression Cu := z -> (expression z#0 + expression z#1*expression "i");
toString Qu := z -> toString expression z;

R = ring ideal 2_ZZ

h = x -> [x, x]
a = h(5)

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
a = 2

x = 1
f := () -> (
    y := 3;
    z = 4;
    g := () -> (x:=5;y=6;x);
    h := () -> (x:=7;z=8;x);
    (x, y, z, g(), h(), x, y, z)
)
