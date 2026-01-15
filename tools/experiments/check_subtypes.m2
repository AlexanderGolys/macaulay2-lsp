try (
    L = subtypes Thing
    print ("Subtypes of Thing: " | toString (#L))
    print ("First few: " | toString (take(L, 5)))
) else (
    print "subtypes function not found or failed"
)
exit 0
