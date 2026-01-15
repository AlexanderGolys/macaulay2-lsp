-- Extract detailed information about a specific instance by name
-- Usage: M2 --script extract_instances.m2 [instance_name]

-- JSON helpers (same as main extraction)
escapeString = (s) -> (
    s = toString s;
    s = replace(///\\///, ///\\\\///, s);
    s = replace("\"", "\\\"", s);
    s = replace("\n", "\\n", s);
    s = replace("\r", "\\r", s);
    s = replace("\t", "\\t", s);
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

-- Get instance name from command line or stdin
instanceName = if #scriptCommandLine > 0 then scriptCommandLine#0 else null;

if instanceName === null then (
    stderr << "Usage: M2 --script extract_instances.m2 instance_name" << endl;
    exit 1;
);

stderr << "Extracting instance: " << instanceName << endl;

-- Try to get the object
obj = try value instanceName else null;

if obj === null then (
    stderr << "Could not resolve: " << instanceName << endl;
    print "null";
    exit 0;
);

-- Determine kind
kind = null;
try (
    if instance(obj, Type) then kind = "Type"
    else if instance(obj, Function) then kind = "Function"
    else if instance(obj, Keyword) then kind = "Operator"
    else if instance(obj, Symbol) and (
        s = toString obj;
        match("^[a-zA-Z0-9_]+$", s) == false
    ) then kind = "Operator";
) else ();

if kind === null then (
    stderr << "Unknown kind for: " << instanceName << endl;
    print "null";
    exit 0;
);

-- Extract data
print "{";
print ("\"name\": " | jq(instanceName) | ",");
print ("\"kind\": " | jq(kind) | ",");
print ("\"safeName\": " | jq(getSafeName(instanceName)) | ",");
print ("\"instanceOf\": " | jq(toString class obj) | ",");

-- Type-specific fields
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
);

-- Function/Operator fields
if kind == "Function" or kind == "Operator" then (
    try (
        opts = options obj;
        if opts =!= null then (
            optList = toList opts;
            print ("\"options\": " | jq(apply(optList, o -> {
                "name" => toString(o#0),
                "default" => toString(o#1)
            })) | ",");
        );
    ) else ();
    
    if kind == "Operator" then (
        try (
            insts = methods obj;
            if instance(insts, NumberedVerticalList) then (
                il = toList insts;
                print ("\"installations\": " | jq(apply(il, i -> apply(toList i, toString))) | ",");
            );
        ) else ();
    );
);

-- Documentation check
hasDocs = false;
docDescription = "";
try (
    tag = makeDocumentTag obj;
    if tag =!= null and instance(tag, DocumentTag) then (
        if tag#?RawDocumentation and tag#RawDocumentation =!= null then hasDocs = true
        else if tag#?Description and tag#Description =!= null then (
            hasDocs = true;
            docDescription = toString tag#Description;
        );
    );
) else ();

print ("\"has_documentation\": " | (if hasDocs then "true" else "false") | ",");
print ("\"description\": " | jq(docDescription));
print "}";

exit 0;
