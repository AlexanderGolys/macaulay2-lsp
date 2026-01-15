try (
    -- OperatorAttributes is a Symbol, but it behaves like a HashTable when accessed?
    -- No, it's a global variable holding a HashTable?
    -- Let's try to access it as a HashTable directly.
    
    opAttrs = value OperatorAttributes
    
    if instance(opAttrs, HashTable) then (
        keysList = keys opAttrs
        print "["
        
        isFirst = true
        scan(keysList, k -> (
            if not isFirst then print ",";
            isFirst = false;
            
            print "{";
            print ("\"name\": " | jq(toString k) | ",");
            
            attrs = opAttrs#k;
            -- attrs is a HashTable or similar
            if instance(attrs, HashTable) then (
                 print ("\"attributes\": " | jq(applyPairsSafe(attrs, (a,v) -> {toString a, toString v})));
            ) else (
                 print ("\"attributes\": " | jq(toString attrs));
            );
            print "}";
        ));
        print "]"
    ) else (
        print "OperatorAttributes is not a HashTable"
        print ("Class: " | toString class opAttrs)
    )
    
) else (
    print "Failed to access OperatorAttributes"
    -- print exception
)
exit 0
