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

opMapping = new HashTable from {
    "*" => "star", "+" => "plus", "-" => "minus", "/" => "slash", "%" => "percent",
    "^" => "caret", "!" => "bang", "?" => "question", "=" => "eq", "<" => "lt",
    ">" => "gt", "|" => "bar", "&" => "amp", "@" => "at", "#" => "hash",
    "$" => "dollar", "~" => "tilde", "\\" => "bs", ":" => "colon", ";" => "semicolon",
    "," => "comma", "." => "dot", "_" => "underscore", "'" => "prime"
};

getSafeName = (s) -> (
    s = toString s;
    if s === "" then return "empty";
    if match("^[a-zA-Z][a-zA-Z0-9']*$", s) then (
        res := replace("'", "prime", s);
        return res;
    );
    
    safeRes := "op_";
    scan(s, c -> (
        c = toString c;
        if opMapping#?c then safeRes = safeRes | opMapping#c | "_"
        else if match("^[a-zA-Z0-9]$", c) then safeRes = safeRes | c
        else safeRes = safeRes | "x" | (toString first ascii c) | "_"; 
    ));
    if match("_$", safeRes) then safeRes = substring(0, #safeRes - 1, safeRes);
    safeRes
);

stderr << "Starting collection from Core.Dictionary..." << endl;

allKeys = keys Core.Dictionary;
stderr << "Found " << #allKeys << " keys in Core.Dictionary." << endl;

print "[";
state = new MutableList from {true};
opAttrs = try OperatorAttributes else null;

typeCount = 0;
funcCount = 0;
methodCount = 0;
opCount = 0;
otherCount = 0;

totalCount := 0;
scan(allKeys, n -> (
    totalCount = totalCount + 1;
    if totalCount % 500 == 0 then stderr << "Processed " << totalCount << " / " << #allKeys << " keys..." << endl;

    -- Skip session variables
    if n == "o" or n == "oo" or n == "ooo" or n == "oooo" or match("^o[0-9]+$", n) then (
        return;
    );

    s := try Core.Dictionary#n else null;
    if s =!= null then (
        val := try value s else null;
        if val === null then return;
        
        obj := val;
        displayName := try (if instance(obj, String) then toExternalString obj else toString obj) else toString n;
        codeName := n;
        kind := null;
        isSym := try (class obj === Symbol) else false;
        
        try (
            if instance(obj, Type) then (
                kind = "Type";
                typeCount = typeCount + 1;
            )
            else if instance(obj, MethodFunction) then (
                kind = "Method";
                methodCount = methodCount + 1;
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
                match("^[a-zA-Z0-9_]+$", n) == false
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
            -- Documentation check
            hasDocs := false;
            docDescription := "";
            try (
                tag := makeDocumentTag s; -- Use symbol for tag
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

            -- Filter out basic constants if they don't have docs
            if (instance(obj, String) or instance(obj, ZZ) or instance(obj, RR) or instance(obj, QQ)) and not hasDocs and not instance(obj, Type) then (
                kind = null;
            );
            
            if kind =!= null then (
                if not state#0 then print ",";
                state#0 = false;
                
                print "{";
                print ("\"name\": " | jq(displayName) | ",");
                print ("\"kind\": " | jq(kind) | ",");
                print ("\"isSymbol\": " | (if isSym then "true" else "false") | ",");
                print ("\"safeName\": " | jq(getSafeName(n)) | ",");
                print ("\"codeName\": " | jq(codeName) | ",");
                
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

                print ("\"has_documentation\": " | (if hasDocs then "true" else "false") | ",");
                print ("\"description\": " | jq(docDescription));
                print "}";
            );
        );
    );
));
print "]";

stderr << "Extraction summary:" << endl;
stderr << "  Types: " << typeCount << endl;
stderr << "  Functions: " << funcCount << endl;
stderr << "  Methods: " << methodCount << endl;
stderr << "  Operators: " << opCount << endl;
stderr << "  Instances: " << otherCount << endl;

exit 0;
