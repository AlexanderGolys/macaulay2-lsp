try (
    -- Let's try to access Core dictionary via 'Core' package
    
    pkg = Core
    print ("Package class: " | toString class pkg)
    
    -- dictionary(Package) -> Dictionary
    d = dictionary pkg
    print ("Dictionary class: " | toString class d)
    
    -- dictionary(String) -> Dictionary?
    d2 = dictionary "Core"
    print ("Dictionary(String) class: " | toString class d2)
) else print "Failed"
exit 0
