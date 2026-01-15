-- Tools for JSON escaping
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

applyPairsSafe = (dict, f) -> (
    try applyPairs(dict, f) else "null"
);

opMapping = new HashTable from {
    "*" => "star", "+" => "plus", "-" => "minus", "/" => "slash", "%" => "percent",
    "^" => "caret", "!" => "bang", "?" => "question", "=" => "eq", "<" => "lt",
    ">" => "gt", "|" => "bar", "&" => "amp", "@" => "at", "#" => "hash",
    "$" => "dollar", "~" => "tilde", "\\" => "bs", ":" => "colon", ";" => "semicolon",
    "," => "comma", "." => "dot", "_" => "underscore"
};

getSafeName = (s) -> (
    s = toString s;
    if s === "" then return "empty";
    if match("^[a-zA-Z0-9_]+$", s) then return s;
    
    safeRes = "op_";
    scan(s, c -> (
        c = toString c;
        if opMapping#?c then safeRes = safeRes | opMapping#c | "_"
        else if match("^[a-zA-Z0-9]$", c) then safeRes = safeRes | c
        else safeRes = safeRes | "x" | (toString first ascii c) | "_"; 
    ));
    if match("_$", safeRes) then safeRes = substring(0, #safeRes - 1, safeRes);
    safeRes
);

stderr << "Starting collection..." << endl;

allNames = apropos "";
stderr << "Found " << #allNames << " names from apropos." << endl;

allObjects = new MutableList;

scan(allNames, n -> (
    v = try value n else null;
    if v =!= null then allObjects#(#allObjects) = v;
));

operators = { "+", "-", "*", "**", "/", "//", "^", "_", "!", "?", "==", "!=", "<", ">", "<=", ">=", "&", "|", "@", "@@", "#", "$", "~", "\\", ":", ";", ",", "." };
scan(operators, op -> (
    s = try value("symbol " | op) else null;
    if s =!= null then allObjects#(#allObjects) = s;
));

allObjects = unique toList allObjects;
stderr << "Resolved to " << #allObjects << " objects." << endl;

print "[";

state = new MutableList from {true};
opAttrs = try OperatorAttributes else null;

typeCount = 0;
funcCount = 0;
methodCount = 0;
opCount = 0;
otherCount = 0;

scan(allObjects, obj -> (
    if obj =!= null then (
        kind = null;
        
        try (
            if instance(obj, Type) then (
                kind = "Type";
                typeCount = typeCount + 1;
            )
            else if instance(obj, Function) then (
                kind = "Function";
                funcCount = funcCount + 1;
            )
            else if instance(obj, Keyword) then (
                kind = "Operator";
                opCount = opCount + 1;
            )
            else if instance(obj, Symbol) and (
                s = toString obj;
                match("^[a-zA-Z0-9_]+$", s) == false
            ) then (
                kind = "Operator";
                opCount = opCount + 1;
            )
            else (
                kind = "Instance";
                otherCount = otherCount + 1;
            );
        ) else ();
        
        if kind =!= null then (
            if not state#0 then print ",";
            state#0 = false;
            
            n = toString obj;
            
            print "{";
            print ("\"name\": " | jq(n) | ",");
            print ("\"kind\": " | jq(kind) | ",");
            print ("\"safeName\": " | jq(getSafeName(n)) | ",");
            
            if kind == "Type" then (
                try (
                    p = parent obj;
                    print ("\"parent\": " | (if p === null then "null" else jq(toString p)) | ",");
                    
                    ms = methods obj;
                    if instance(ms, NumberedVerticalList) then (
                         ml = toList ms;
                         print ("\"methods\": " | jq(apply(ml, m -> apply(toList m, toString))) | ",");
                    );
                    
                    insts = instances obj;
                    if instance(insts, HashTable) then (
                        instKeys = keys insts;
                        print ("\"instances\": " | jq(apply(instKeys, toString)) | ",");
                    );
                ) else ();
            ) else (
                -- For Instances, record their class (which is their Type)
                print ("\"instanceOf\": " | jq(toString class obj) | ",");
            );
            
            if kind == "Function" or kind == "Method" or kind == "Operator" then (
                 try (
                     opts = options obj;
                     if opts =!= null then (
                         optKeys = keys opts;
                         print ("\"options\": [");
                         scan(0 .. #optKeys - 1, i -> (
                             k = optKeys#i;
                             v = opts#k;
                             if i > 0 then print ",";
                             print "{";
                             print ("\"name\": " | jq(toString k) | ",");
                             print ("\"default\": " | jq(toString v));
                             print "}";
                         ));
                         print "],";
                     );
                 ) else ();
                 
                 try (
                     insts = methods obj;
                     if instance(insts, NumberedVerticalList) then (
                         il = toList insts;
                         print ("\"installations\": " | jq(apply(il, i -> apply(toList i, toString))) | ",");
                     );
                 ) else ();
                 
                 try (
                     if opAttrs =!= null and opAttrs#?obj then (
                         attrs = opAttrs#obj;
                         if instance(attrs, HashTable) then (
                              print ("\"operator_attributes\": {");
                              attrKeys = keys attrs;
                              scan(0 .. #attrKeys - 1, i -> (
                                  k = attrKeys#i;
                                  if i > 0 then print ",";
                                  print (jq(toString k) | ": " | jq(toString(attrs#k)));
                              ));
                              print "},";
                         ) else (
                              print ("\"operator_attributes\": " | jq(toString attrs) | ",");
                         );
                     );
                 ) else ();
            );

            hasDocs = false;
            docDescription = "";
            try (
                tag = makeDocumentTag obj;
                if tag =!= null and instance(tag, DocumentTag) then (
                    if tag#?RawDocumentation and tag#RawDocumentation =!= null then (
                        hasDocs = true;
                    )
                    else if tag#?Description and tag#Description =!= null then (
                        hasDocs = true;
                        docDescription = toString tag#Description;
                    );
                );
            ) else ();
            
            print ("\"has_documentation\": " | (if hasDocs then "true" else "false") | ",");
            print ("\"description\": " | jq(docDescription));
            print "}";
        );
    );
));
print "]";

stderr << "Extraction summary:" << endl;
stderr << "  Types: " << typeCount << endl;
stderr << "  Functions: " << funcCount << endl;
stderr << "  Operators: " << opCount << endl;
stderr << "  Instances: " << otherCount << endl;

exit 0;
