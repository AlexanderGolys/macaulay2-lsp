try (
    -- OperatorAttributes is a Symbol.
    -- The values are stored in a database or dictionary associated with it?
    -- Try 'value OperatorAttributes' again.
    
    op = value OperatorAttributes
    print ("Value of OperatorAttributes: " | toString op)
    
    -- In M2 source, OperatorAttributes is a GlobalDictionary or something similar?
    -- Let's try to access it via property on a symbol?
    -- Or maybe it's a HashTable attached to the symbol 'OperatorAttributes'?
    
    -- Try accessing a property on '+'
    plusSym = symbol +
    -- 'OperatorAttributes' might be the key in the property list of '+'?
    -- No, 'attributes' usually work the other way or via property/value.
    
    -- Let's try iterating apropos to see if we missed the real variable name
    -- print "Searching apropos..."
    -- scan(apropos "Operator", print)
    
    -- Try to see if it is a MutableHashTable
    if instance(op, MutableHashTable) then print "It is MutableHashTable"
    else print "Not MutableHashTable"

) else print "Failed"
exit 0
