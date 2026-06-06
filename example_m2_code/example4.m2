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

R = ring ideal(2_ZZ)

h = x -> [x, x]
a = h(5)
