try (
    -- methods is a keyword/command, likely not a function in the standard sense
    -- To get methods of a type, we use 'methods Type'
    -- 'methods X' where X is a type returns a NumberedVerticalList
    
    L = methods Type
    print ("methods Type class: " | toString class L)
    
    -- Can we convert to list?
    LL = toList L
    print ("First item: " | toString LL#0)
    
    -- methods Function
    F = methods Function
    FF = toList F
    print ("Function methods count: " | toString(#FF))
    
    -- methods for a specific method?
    -- 'methods resolution'
    try (
       R = methods resolution
       RR = toList R
       print ("resolution methods count: " | toString(#RR))
       print ("First resolution method: " | toString RR#0)
    ) else print "Failed to get methods resolution"
    
) else print "Overall failure"
exit 0
