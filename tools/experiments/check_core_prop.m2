try (
    D = Core.dictionary
    print ("Core.dictionary: " | toString class D)
) else (
    print "Core.dictionary access failed"
)
exit 0
