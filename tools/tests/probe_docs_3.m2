try (
    T = Type;
    tag = makeDocumentTag T;
    print ("Keys: " | toString keys tag);
    
    -- Print values for debugging
    scan(keys tag, k -> (
        print ("Key: " | toString k);
        -- print ("Value: " | toString(tag#k)); -- Value might be huge or complex
    ));
    
    -- Try to find where the text is
    -- Maybe in another lookup?
    
) else print "Crash"
exit 0
