stderr << "Testing operator extraction..." << endl;

plusSym = value "symbol +";
stderr << "Got + symbol: " << toString plusSym << endl;
stderr << "Class: " << toString class plusSym << endl;

-- Check instance tests
if instance(plusSym, Type) then stderr << "Is Type" << endl;
if instance(plusSym, Keyword) then stderr << "Is Keyword" << endl;
if instance(plusSym, Symbol) then (
    s = toString plusSym;
    if match("^[a-zA-Z0-9_]+$", s) then (
        stderr << "Matches alphanumeric pattern" << endl;
    ) else (
        stderr << "Does NOT match alphanumeric pattern (correct for operator)" << endl;
    );
);

-- Check OperatorAttributes
opAttrs = try OperatorAttributes else null;
if opAttrs =!= null and opAttrs#?plusSym then (
    stderr << "Found in OperatorAttributes!" << endl;
    attrs = opAttrs#plusSym;
    stderr << "Attributes: " << toString attrs << endl;
);

exit 0;
