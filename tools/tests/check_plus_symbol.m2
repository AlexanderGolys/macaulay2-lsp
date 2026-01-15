try (
    s = symbol +
    print ("Symbol +: " | toString s)
    v = value s
    print ("Value of +: " | toString v)
) else print "Failed"
exit 0
