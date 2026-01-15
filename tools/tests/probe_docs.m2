try (
    -- Load Macaulay2Doc if not loaded (usually preloaded)
    -- Try to make a document tag for a well-known type
    
    T = Type
    tag = try makeDocumentTag T else null
    
    if tag =!= null then (
        print "DocumentTag created for Type"
        print ("Class: " | toString class tag)
        -- peek tag
        -- Tag is a HashTable?
        if instance(tag, HashTable) then (
             print ("Keys: " | toString keys tag)
             if tag#?RawDocumentation then print "Has RawDocumentation"
             else print "No RawDocumentation"
             
             if tag#?Description then print "Has Description"
             else print "No Description"
             
             if tag#?Headline then (
                 print ("Headline: " | toString(tag#Headline))
             )
        )
    ) else print "makeDocumentTag failed"
    
    -- Try for a symbol
    s = symbol +
    tag2 = try makeDocumentTag s else null
    if tag2 =!= null then (
        print "DocumentTag created for +"
        if instance(tag2, HashTable) and tag2#?Headline then (
             print ("Headline for +: " | toString(tag2#Headline))
        )
    )

) else print "Crash"
exit 0
