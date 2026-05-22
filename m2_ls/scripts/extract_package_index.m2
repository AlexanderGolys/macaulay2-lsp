-- Generate exported-symbol package indexes for macaulay2-lsp.
--
-- Usage from m2_ls/:
--   M2 --script scripts/extract_package_index.m2 /tmp/m2-package-index Graphs Text
--
-- For each package, writes:
--   <output-dir>/<Package>.names
--   <output-dir>/<Package>.details.jsonl

needsPackage "JSON"

args := drop(scriptCommandLine, 1)
if #args < 2 then (
    printerr "usage: M2 --script scripts/extract_package_index.m2 <output-dir> <package> [package...]";
    exit 1
)

outputDir := first args
packageNames := drop(args, 1)

safeString := x -> if x === null then null else try toString x else null
safeHtml := x -> if x === null then null else try html x else null
stringList := values -> apply(values, value -> toString value)
signatureItems := sig -> if instance(sig, Sequence) then apply(toList sig, item -> toString item) else {toString sig}

docField := (doc, field) -> if doc =!= null and doc#?field then doc#field else null
docStatus := (docKey, headline) -> if docKey === null then "missing" else "upstream"
docSummary := (docKey, doc, headline, usage) -> hashTable {
    "status" => docStatus(docKey, headline),
    "doc_key" => docKey,
    "source_file" => docField(doc, "filename"),
    "source_line" => docField(doc, "linenum"),
    "upstream_description_short" => headline,
    "upstream_description_long" => safeHtml usage,
    "upstream_inputs" => null,
    "upstream_outputs" => null
}

methodRecords := fn -> (
    ms := try methods fn else null;
    if ms === null or length ms == 0 then {} else apply(0..(length ms - 1), i -> hashTable {
        "signature" => signatureItems(ms#i)
    }))

recordForExport := (pkg, sym) -> (
    name := toString sym;
    val := try value sym else null;
    dataType := if val === null then "Symbol" else safeString class val;
    db := pkg#"raw documentation database";
    docKey := if db#?name then name else null;
    doc := if docKey === null then null else try value db#docKey else null;
    headline := docField(doc, Headline);
    usage := docField(doc, Usage);
    extra := hashTable {
        "symbol_name" => name,
        "package" => pkg#"pkgname",
        "package_source_file" => pkg#"source file",
        "package_source_directory" => pkg#"source directory",
        "exported" => true
    };
    record := new MutableHashTable from {
        "name" => name,
        "data_type" => dataType,
        "description_short" => if docKey === null then null else headline,
        "description_long" => if docKey === null then null else usage,
        "documentation" => docSummary(docKey, doc, headline, usage),
        "examples" => {},
        "extra" => extra
    };
    if val =!= null and instance(val, Function) then record#"function_info" = hashTable {
        "methods" => methodRecords val
    };
    new HashTable from record)

writePackageIndex := packageName -> (
    needsPackage packageName;
    pkg := package packageName;
    exports := sort unique join(pkg#"exported symbols", pkg#"exported mutable symbols");
    records := new MutableHashTable;
    scan(exports, sym -> (
        rec := try recordForExport(pkg, sym) else null;
        if rec =!= null then records#(toString sym) = rec;
    ));

    sortedNames := sort keys records;
    namesPath := outputDir | "/" | packageName | ".names";
    detailsPath := outputDir | "/" | packageName | ".details.jsonl";
    namesOut := openOut namesPath;
    scan(sortedNames, name -> namesOut << name << endl);
    namesOut << close;

    out := openOut detailsPath;
    scan(sortedNames, name -> out << toJSON(records#name, Sort => true, ValueSeparator => ",") << endl);
    out << close;

    printerr("wrote package " | packageName | " with " | toString length sortedNames | " exported symbols to " | outputDir);
)

scan(packageNames, writePackageIndex)
exit 0
