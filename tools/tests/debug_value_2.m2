print "Debugging value access..."
try (
    v = value "Type";
    print ("value 'Type' = " | toString v);
) else print "value 'Type' failed"
exit 0
