try (
    opAttrs = value OperatorAttributes;
    print ("Class: " | toString class opAttrs);
    
    if instance(opAttrs, HashTable) then (
        print "It is a HashTable";
        K = keys opAttrs;
        print ("Count: " | toString(#K));
    )
) else print "Failed"
exit 0
