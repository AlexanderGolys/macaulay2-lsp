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

validSymbols = select(keys Core.Dictionary, k -> #k > 5 and substring(0, 5, k) == "Core$") / (k -> substring(5, k));
print(jq(validSymbols));
exit 0;
