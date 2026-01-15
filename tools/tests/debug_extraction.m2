try (
    -- Let's check why 'apropos ""' isn't returning operators
    
    L = apropos ""
    print ("Count: " | toString (#L))
    
    -- Check if '+' is in L
    -- apropos returns list of strings.
    -- The symbol + is named "+"
    
    isIn = member("+", L)
    print ("+ in list: " | toString isIn)
    
    -- When we use 'value n' on "+", do we get the symbol +?
    v = value "+"
    print ("Value of +: " | toString v)
    print ("Class of +: " | toString class v)
    
    -- Is it a Keyword?
    print ("Is Keyword: " | toString instance(v, Keyword))

    -- Check if extract_docs is checking correctly
    if instance(v, Type) then print "Is Type"
    else if instance(v, Method) then print "Is Method"
    else if instance(v, Function) then print "Is Function"
    else if instance(v, Keyword) then print "Is Keyword"
    else print "None of the above"
    
) else print "Failed"
exit 0
