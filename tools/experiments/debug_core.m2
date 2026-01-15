print ("Class of Core: " | toString class Core)
try (
    K = keys Core
    print ("Keys of Core: " | toString(#K))
) else (
    print "Core has no keys"
)
exit 0
