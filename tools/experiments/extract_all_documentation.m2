-- Extract actual documentation content from Macaulay2
-- Usage: M2 --script extract_all_documentation.m2 > documentation_data.json

-- JSON helpers
escapeString = (s) -> (
    s = toString s;
    s = replace(///\\///, ///\\\\///, s);
    s = replace(///\"///, ///\\\"///, s);
    s = replace(///\n///, ///\\n///, s);
    s = replace(///\r///, ///\\r///, s);
    s = replace(///\t///, ///\\t///, s);
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

stderr << "Starting comprehensive documentation extraction..." << endl;

allNames = apropos "";
stderr << "Found " << #allNames << " names." << endl;

-- Limit for testing
-- allNames = take(allNames, 100);

print "{";
isFirst = true;

scan(allNames, n -> (
    obj = try value n else null;
    if obj =!= null then (
        description = "";
        headline = "";
        hasRealDocs = false;
        
        -- Try about for headline
        try (
            a = about obj;
            if a =!= null then (
                s = toString a;
                -- about output looks like: Name\n--\nHeadline
                lines = separate(///\n///, s);
                if #lines >= 3 then (
                    headline = lines#2;
                );
            );
        ) else ();
        
        -- Try help for full description
        try (
            h = help obj;
            if h =!= null then (
                description = toString h;
                if #description > 0 then hasRealDocs = true;
            );
        ) else ();
        
        if hasRealDocs or headline != "" then (
            if not isFirst then print ",";
            isFirst = false;
            
            print (jq(n) | ": {");
            print ("\"headline\": " | jq(headline) | ",");
            print ("\"description\": " | jq(description));
            print "}";
        );
    );
));

print "}";
stderr << "Done." << endl;
exit 0;
