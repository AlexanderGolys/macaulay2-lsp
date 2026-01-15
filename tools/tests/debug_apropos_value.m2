stderr << "Debugging apropos/value" << endl;
L = apropos "";
stderr << "Apropos count: " << #L << endl;

count = 0;
scan(L, n -> (
    v = try value n else null;
    if v =!= null then count = count + 1;
    if count < 5 and v =!= null then (
        stderr << "Found: " << n << " -> " << toString v << endl;
    );
));
stderr << "Total resolved: " << count << endl;
exit 0
