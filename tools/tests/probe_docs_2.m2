try (
    T = Type;
    tag = try makeDocumentTag T else null;
    
    if tag =!= null then (
        print "DocumentTag created for Type";
        print ("Class: " | toString class tag);
        
        if instance(tag, HashTable) then (
             K = keys tag;
             print ("Keys count: " | toString(#K));
             
             -- Check for interesting keys
             -- Usually: Key, Package, Format, maybe Headline?
             
             -- Let's try to access documentation content directly?
             -- It seems documentation is stored in 'RawDocumentation' if accessed?
             -- Or maybe we need to call 'help tag'?
        );
    ) else print "makeDocumentTag failed";
) else print "Crash"
exit 0
