-- Generate src/data/builtins.details.jsonl and src/data/builtins.names from Macaulay2's installed documentation
-- database and live runtime symbol table.
--
-- Usage from m2_ls/:
--   M2 --script scripts/extract_builtins.m2 src/data/builtins.details.jsonl
--   M2 --script scripts/extract_builtins.m2 /tmp/builtins-debug.details.jsonl + % Ring
--   M2 --script scripts/extract_builtins.m2 --rich /tmp/builtins-rich.details.jsonl + % Ring

needsPackage "Macaulay2Doc"
needsPackage "JSON"

args := drop(scriptCommandLine, 1)
richDocs := member("--rich", args)
args = select(args, arg -> arg =!= "--rich" and arg =!= "--compact")
outputPath := if #args > 0 then args#0 else "src/data/builtins.details.jsonl"
namesPath := replace("\\.json$", ".names", replace("\\.jsonl$", ".names", replace("\\.details\\.jsonl$", ".names", outputPath)))
debugNames := if #args > 1 then drop(args, 1) else {}
db := Macaulay2Doc#"raw documentation database"
runtimeDictionary := Core.Dictionary
operatorNames := sort apply(keys operatorAttributes, toString)

recordNameFromDocKey := key -> (
    parts := separate("\\(", key);
    name := first parts;
    packageParts := separate("::", name);
    if #packageParts > 1 then name = last packageParts;
    name)

rawDocSource := key -> if key =!= null and db#?key then db#key else null
rawDoc := key -> (
    source := rawDocSource key;
    if source === null then null else try value source else null)

docKeyFor := (name, symName) -> if db#?name then name else if db#?symName then symName else null

docField := (doc, field) -> if doc =!= null and doc#?field then doc#field else null
stringList := values -> apply(values, value -> toString value)
docFields := doc -> if doc === null then {} else sort stringList keys doc
safeString := x -> if x === null then null else try toString x else null
safeNet := x -> if x === null then null else try toString net x else null
safeHtml := x -> if x === null then null else try html x else null
docMarkup := (x, depth) -> if x === null then null else if depth <= 0 then hashTable {
    "kind" => "truncated",
    "class" => toString class x,
    "string" => safeString x
} else if instance(x, String) then hashTable {
    "kind" => "text",
    "text" => x
} else if instance(x, Option) then hashTable {
    "kind" => "option",
    "name" => safeString first x,
    "value" => docMarkup(last x, depth - 1)
} else if instance(x, Sequence) then hashTable {
    "kind" => "sequence",
    "class" => toString class x,
    "children" => apply(toList x, item -> docMarkup(item, depth - 1))
} else if instance(x, BasicList) then (
    entries := toList pairs x;
    optionEntries := select(entries, entry -> instance(entry#1, Option));
    childEntries := select(entries, entry -> not instance(entry#1, Option));
    hashTable {
        "kind" => "node",
        "class" => toString class x,
        "options" => new HashTable from apply(optionEntries, entry -> safeString first entry#1 => docMarkup(last entry#1, depth - 1)),
        "children" => apply(childEntries, entry -> docMarkup(entry#1, depth - 1))
    }
) else hashTable {
    "kind" => "scalar",
    "class" => toString class x,
    "string" => safeString x
}
docFieldInfo := (doc, field) -> if doc === null or not doc#?field then null else (
    value := doc#field;
    hashTable {
        "class" => toString class value,
        "string" => safeString value,
        "net" => safeNet value,
        "html" => safeHtml value,
        "markup" => docMarkup(value, 16)
    })
docFieldInfos := doc -> if doc === null then hashTable {} else new HashTable from apply(keys doc, field -> toString field => docFieldInfo(doc, field))
docSummary := (docKey, doc, status, headline, usage, docSource) -> (
    base := new MutableHashTable from {
        "status" => status,
        "doc_key" => docKey,
        "source_file" => docField(doc, "filename"),
        "source_line" => docField(doc, "linenum"),
        "upstream_description_short" => headline,
        "upstream_description_long" => safeHtml usage,
        "upstream_inputs" => docFieldInfo(doc, Inputs),
        "upstream_outputs" => docFieldInfo(doc, Outputs)
    };
    if richDocs then (
        base#"upstream_eval_status" = if docKey === null then "missing" else if doc === null then "failed" else "ok";
        base#"upstream_raw" = docSource;
        base#"upstream_fields" = docFields doc;
        base#"upstream_field_data" = docFieldInfos doc;
        base#"upstream_description_body" = docFieldInfo(doc, Description);
        base#"upstream_usage" = docFieldInfo(doc, Usage);
        base#"upstream_see_also" = docFieldInfo(doc, SeeAlso);
        base#"upstream_key" = safeString docField(doc, Key);
        base#"upstream_document_tag" = safeString docField(doc, symbol DocumentTag);
    );
    new HashTable from base)
genericHeadline := headline -> headline =!= null and (
    headline === "a binary operator"
    or headline === "a unary operator"
    or match("^a binary operator, usually used for ", headline)
    or match("^a binary operator, used for ", headline)
    or match("^augmented assignment for ", headline))
docStatus := (docKey, headline) -> if docKey === null or genericHeadline headline then "missing" else "upstream"

signatureItems := sig -> if instance(sig, Sequence) then apply(toList sig, item -> toString item) else {toString sig}

methodRecords := fn -> (
    ms := try methods fn else null;
    if ms === null or length ms == 0 then {} else apply(0..(length ms - 1), i -> hashTable {
        "signature" => signatureItems(ms#i)
    }))

symbolForName := name -> if runtimeDictionary#?name then runtimeDictionary#name else if isGlobalSymbol name then getGlobalSymbol name else null

operatorInfoForSymbol := sym -> if operatorAttributes#?sym then (
    attrs := operatorAttributes#sym;
    forms := sort stringList keys attrs;
    flagsByForm := new HashTable from apply(keys attrs, kind -> toString kind => sort stringList toList attrs#kind);
    hashTable {
        "method_lookup" => "symbol",
        "method_symbol" => toString sym,
        "forms" => forms,
        "flags" => flagsByForm,
        "attributes" => flagsByForm,
        "flexible" => any(values flagsByForm, flags -> member("Flexible", flags))
    }
) else null

allRuntimeNames := sort unique join(
    join(apply(keys runtimeDictionary, toString), apply(keys runtimeDictionary, key -> toString(runtimeDictionary#key))),
    join(operatorNames, select(apply(keys db, recordNameFromDocKey), name -> isGlobalSymbol name)))
runtimeNames := if length debugNames == 0 then allRuntimeNames else sort unique debugNames
includeName := name -> length debugNames == 0 or member(name, debugNames)

parentOf := new MutableHashTable;
ancestorsOf := new MutableHashTable;
classOf := new MutableHashTable;
classAncestorsOf := new MutableHashTable;
parentChildren := new MutableHashTable;
classChildren := new MutableHashTable;

scan(runtimeNames, name -> (
    sym := symbolForName name;
    if sym =!= null then (
        val := value sym;
        p := try toString parent val else null;
        a := try stringList ancestors val else {};
        c := try toString class val else null;
        ca := try stringList ancestors class val else {};
        parentOf#name = p;
        ancestorsOf#name = a;
        classOf#name = c;
        classAncestorsOf#name = ca;
        if p =!= null then parentChildren#p = append(parentChildren#p ?? {}, name);
        if c =!= null then classChildren#c = append(classChildren#c ?? {}, name);
    )
));

relationInfo := name -> hashTable {
    "parent" => parentOf#name,
    "ancestors" => ancestorsOf#name,
    "children" => sort(parentChildren#name ?? {}),
    "class" => classOf#name,
    "class_ancestors" => classAncestorsOf#name,
    "instances" => sort(classChildren#name ?? {})
}

typeInfoForName := (name, val) -> if instance(val, Type) then hashTable {
    "parent_type" => parentOf#name,
    "ancestors" => ancestorsOf#name,
    "subtypes" => sort(parentChildren#name ?? {}),
    "instances" => sort(classChildren#name ?? {})
} else null

recordForSymbol := (name, sym) -> (
    symName := toString sym;
    val := value sym;
    docKey := docKeyFor(name, symName);
    docSource := rawDocSource docKey;
    doc := if docKey === null then null else rawDoc docKey;
    headline := docField(doc, Headline);
    usage := docField(doc, Usage);
    status := docStatus(docKey, headline);
    extra := hashTable {
        "symbol_name" => symName
    };
    documentation := docSummary(docKey, doc, status, headline, usage, docSource);
    record := new MutableHashTable from {
        "name" => name,
        "data_type" => toString class val,
        "description_short" => if status === "upstream" then headline else null,
        "description_long" => if status === "upstream" then usage else null,
        "documentation" => documentation,
        "examples" => {},
        "extra" => extra,
        "relation_info" => relationInfo name
    };
    isOperator := operatorAttributes#?sym;
    methodsForValue := if instance(val, Function) or isOperator then methodRecords val else {};
    if instance(val, Function) or isOperator then record#"function_info" = hashTable {
        "methods" => methodsForValue
    };
    ti := typeInfoForName(name, val);
    if ti =!= null then record#"type_info" = ti;
    oi := operatorInfoForSymbol sym;
    if oi =!= null then record#"operator_info" = oi;
    new HashTable from record)

records := new MutableHashTable;
skipped := 0;
scan(runtimeNames, name -> (
    if not (records#?name) then (
        sym := symbolForName name;
        rec := if sym === null then null else try recordForSymbol(name, sym) else null;
        if rec === null then skipped = skipped + 1 else records#name = rec;
    )
));

scan(keys db, key -> (
    name := recordNameFromDocKey key;
    if includeName name and isGlobalSymbol name then (
        if not (records#?name) then (
            rec := try recordForSymbol(name, getGlobalSymbol name) else null;
            if rec === null then skipped = skipped + 1 else records#name = rec;
        );
    )
));

sortedNames := sort(keys records);
namesOut := openOut namesPath;
scan(sortedNames, name -> namesOut << name << endl);
namesOut << close;

out := openOut outputPath;
scan(sortedNames, name -> out << toJSON(records#name, Sort => true, ValueSeparator => ",") << endl);
out << close;

printerr("wrote " | toString(length(keys records)) | " names to " | namesPath | " and records to " | outputPath | "; skipped " | toString skipped);
exit 0
