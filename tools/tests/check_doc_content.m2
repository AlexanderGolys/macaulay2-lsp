try (
    T = Type;
    tag = makeDocumentTag T;
    
    -- Check if we can get description directly
    -- In M2, descriptions are usually just items in the list that are strings/hypertext.
    
    -- Let's inspect the RawDocumentation content if it exists
    if tag#?RawDocumentation then (
        raw = tag#RawDocumentation;
        if instance(raw, List) then (
             -- Scan for strings that might be description
             print ("RawDocumentation is a List of length " | toString(#raw));
             
             -- Usually the description is the first non-key element?
             -- Or elements without keys?
             
             scan(raw, item -> (
                 if instance(item, String) then print ("String item: " | item)
                 else if instance(item, Sequence) then print ("Sequence item (Key/Value?)")
                 else print ("Item type: " | toString class item)
             ));
        ) else (
             print ("RawDocumentation is " | toString class raw)
        )
    ) else print "No RawDocumentation";
    
) else print "Crash"
exit 0
