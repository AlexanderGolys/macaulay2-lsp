print OperatorAttributes
try (
    K = keys OperatorAttributes
    print ("OperatorAttributes keys: " | toString(#K))
    print ("First key: " | toString K#0)
    print ("Value: " | toString(OperatorAttributes#(K#0)))
) else print "Failed to inspect OperatorAttributes"
exit 0
