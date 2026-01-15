stderr << "Accessing ZZ keys..." << endl;
tag = makeDocumentTag ZZ;
ks = keys tag;
scan(ks, k -> (
    stderr << "Key: " << toString k << endl;
    if toString k == "RawDocumentation" then (
        stderr << "Found RawDocumentation!" << endl;
        rd = tag#k;
        stderr << "Class: " << toString class rd << endl;
        stderr << "Content: " << (substring(0, min(100, # toString rd), toString rd)) << "..." << endl;
    );
));
exit 0;
