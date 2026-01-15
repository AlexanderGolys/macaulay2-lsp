try (
    -- Let's check if we can inspect a specific method installation for an operator
    -- and see if we can deduce attributes or at least verify it's an operator
    
    op = symbol +
    -- List all methods for +
    L = methods op
    
    print ("Methods count for +: " | toString (#L))
    
    -- Pick one
    m = L#0
    print ("Method: " | toString m)
    
    -- Is there a property on the symbol '+' itself?
    -- 'attributes' command?
    -- No.
    
    -- Try to see if 'OperatorAttributes' is a database
    if instance(OperatorAttributes, Database) then print "Is Database"
    else print "Not Database"
    
) else print "Failed"
exit 0
