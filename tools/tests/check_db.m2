-- We need to access the documentation database.
-- The global variable 'DocumentationDatabase' might hold it?
try (
    if globalAssignment#?("DocumentationDatabase") then (
        db = value "DocumentationDatabase";
        print ("DocumentationDatabase class: " | toString class db);
    ) else print "DocumentationDatabase not found";
    
    -- Or try accessing document via key?
    -- The key for 'Type' is (Type) or just Type?
    
    -- There is a function 'fetchDocumentation' maybe?
    
    -- Let's try to find where 'help' gets its data.
    -- help X calls help(X) -> help(DocumentTag)
    
    -- We can try to replicate help logic.
    -- help(tag) usually formats it.
    
    -- Try 'documentation' function?
    -- documentation key
) else print "Crash"
exit 0
