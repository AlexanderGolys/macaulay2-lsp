print "Debugging value access..."
try (
    v = value "Type"
    print ("value 'Type' = " | toString v)
) else print "value 'Type' failed"

try (
    v = value "resolution"
    print ("value 'resolution' = " | toString v)
) else print "value 'resolution' failed"

exit 0
