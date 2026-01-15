-- Extract actual documentation content from Macaulay2
-- Usage: M2 --script extract_documentation.m2 [instance_name]

-- JSON helpers
escapeString = (s) -> (
    s = toString s;
    s = replace(///\\///, ///\\\\///, s);
    s = replace("\"", "\\\\\"", s);
    s = replace("\n", "\\\\n", s);
    s = replace("\r", "\\\\r", s);
    s = replace("\t", "\\\\t", s);
    s
);

jq = (s) -> (
    if s === null then "null"
    else if instance(s, String) then "\"" | escapeString s | "\""
    else if instance(s, ZZ) then toString s
    else if instance(s, Boolean) then (if s then "true" else "false")
    else if instance(s, List) then "[" | demark(",", apply(s, x -> jq x)) | "]"
    else if instance(s, Sequence) then jq(toList s)
    else "\"" | escapeString s | "\""
);

-- Get instance name from command line
-- scriptCommandLine has [script_path, arg1, arg2...]
instanceName = if #scriptCommandLine > 1 then scriptCommandLine#1 else null;

if instanceName === null then (
    stderr << "Usage: M2 --script extract_documentation.m2 instance_name" << endl;
    exit 1;
);

stderr << "Extracting documentation for: " << instanceName << endl;

-- Try to get the object
obj = try value instanceName else null;

if obj === null then (
    stderr << "Could not resolve: " << instanceName << endl;
    print "null";
    exit 0;
);

print "{";
print ("\"name\": " | jq(instanceName) | ",");

-- Try to get documentation
hasRealDocs = false;
description = "";
fullDoc = "";
headline = "";
usage = "";
exampleCode = "";

try (
    -- Try help command first as it might load things
    h = help obj;
    if h =!= null then (
        -- Convert Hypertext to something useful?
        -- toString h might give us the structure
        fullDoc = toString h; 
        stderr << "Help available, class: " << toString class h << endl;
    );

    tag = makeDocumentTag obj;
    
    if tag =!= null and instance(tag, DocumentTag) then (
        stderr << "Found DocumentTag with keys: " << toString keys tag << endl;
        
        -- Try to get Headline
        if tag#?"Headline" then (
            headline = toString tag#"Headline";
            stderr << "Got headline: " << headline << endl;
        );
        
        -- Try to get Usage
        if tag#?"Usage" then (
            usage = toString tag#"Usage";
            stderr << "Got usage" << endl;
        );
        
        -- Try to get Description
        if tag#?"Description" then (
            description = toString tag#"Description";
            hasRealDocs = true;
            stderr << "Got description: " << (substring(0, min(50, #description), description)) << "..." << endl;
        );
        
        -- Try to get RawDocumentation (which might contain everything)
        if tag#?"RawDocumentation" then (
            rawDoc = tag#"RawDocumentation";
            
            if rawDoc === null then (
                stderr << "RawDocumentation is null" << endl;
            ) else (
                stderr << "RawDocumentation found, class: " << toString class rawDoc << endl;
                hasRealDocs = true;
                
                if description == "" then (
                    try (
                        description = toString rawDoc;
                        stderr << "Extracted description length: " << #description << endl;
                    ) else (
                        description = "Error converting documentation to string";
                        stderr << "Error converting" << endl;
                    );
                );
            );
        );
        
        -- Try to get examples
        if tag#?"ExampleFiles" then (
            exampleCode = toString tag#"ExampleFiles";
            stderr << "Got example files" << endl;
        );
        
        -- Try other fields that might contain documentation
        if tag#?"Inputs" then (
            stderr << "Has Inputs" << endl;
        );
        
        if tag#?"Outputs" then (
            stderr << "Has Outputs" << endl;
        );
    );
) else (
    stderr << "No DocumentTag found or error occurred" << endl;
);

print ("\"hasDocumentation\": " | (if hasRealDocs then "true" else "false") | ",");
print ("\"headline\": " | jq(headline) | ",");
print ("\"usage\": " | jq(usage) | ",");
print ("\"description\": " | jq(description) | ",");
print ("\"full_doc\": " | jq(fullDoc) | ",");
print ("\"examples\": " | jq(exampleCode));
print "}";

exit 0;
