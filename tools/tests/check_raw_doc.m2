try (
    T = Type;
    tag = makeDocumentTag T;
    
    raw = tag#RawDocumentation;
    if raw === null then print "RawDocumentation is null"
    else (
        print "RawDocumentation type: " | toString class raw
        -- It might be a list or hypertext
    );
    
) else print "Crash"
exit 0
