try (
    attrs = OperatorAttributes#(symbol +)
    print ("Attributes for +: " | toString attrs)
) else print "Failed"
exit 0
