try (
    -- Let's iterate allValues() which returns a list of values in current dictionary?
    
    L = allValues()
    print ("allValues count: " | toString (#L))
    
    -- Check if '+' is in there
    hasPlus = false
    scan(L, val -> (
        if val === symbol + then hasPlus = true;
    ))
    print ("Has + symbol: " | toString hasPlus)
    
) else print "Failed"
exit 0
