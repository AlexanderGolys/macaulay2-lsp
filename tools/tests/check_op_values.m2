try (
    -- Let's manually inject operators into the scan list
    -- apropos "" returns strings.
    -- "+" is likely not in it.
    
    opList = { "+", "-", "*", "/", "^", "_", "!", "?", "=", "<", ">", "&", "|", "@", "#", "$", "~", "\\", ":", ";", ",", "." }
    
    scan(opList, op -> (
        print ("Checking " | op | "...")
        v = try value op else null
        if v =!= null then print ("Found value for " | op | ": " | toString v)
        else print ("No value for " | op)
    ))
) else print "Failed"
exit 0
