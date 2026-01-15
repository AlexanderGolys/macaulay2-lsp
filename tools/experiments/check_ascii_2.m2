try (
    -- Maybe it's not a global function but a method?
    -- No, usually it's `toASCII string`?
    -- Maybe it is spelled differently? `ascii`?
    
    s = "A"
    print ("ascii s: " | toString ascii s)
) else print "ascii failed"
exit 0
