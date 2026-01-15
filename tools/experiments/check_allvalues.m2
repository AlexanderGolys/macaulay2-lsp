try (
    L = allValues()
    print ("Found " | toString(#L) | " values")
) else (
    print "allValues failed"
)
exit 0
