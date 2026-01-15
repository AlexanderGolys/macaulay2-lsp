try (
    -- Access the "value" of OperatorAttributes which is the symbol itself?
    -- No, OperatorAttributes is a symbol. Its "value" is the same symbol because it's a self-evaluating symbol or protected?
    -- Wait, if it holds a database, maybe we need to peek at it?
    
    op = OperatorAttributes
    print ("Class: " | toString class op)
    
    -- In M2, some system dictionaries are accessible via value()
    -- But we tried that.
    
    -- Let's check if 'attributes' function returns anything for a symbol
    -- attributes symbol +
    -- or attributes(symbol +)
    
    attrs = attributes(symbol +)
    if attrs =!= null then (
         print "Attributes found via attributes():"
         print toString attrs
    ) else print "attributes() returned null"
    
) else print "Failed"
exit 0
