print "Debugging dictionary access..."
dict = dictionary "Core"
print ("dictionary 'Core' class: " | toString class dict)
if instance(dict, Dictionary) then (
    print "Is Dictionary"
    -- Try 'values dict' which works
    vals = values dict
    print ("Count: " | toString(#vals))
) else (
    print "Not Dictionary"
)
exit 0
