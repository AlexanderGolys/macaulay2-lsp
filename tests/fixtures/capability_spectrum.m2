-- `localValue` is reused by the function below.
needsPackage "JSON"
localValue=1
double = value -> value + localValue
result=double(2)
encoded=toJSON(result)
conditional = if result == 3 then (
encoded
) else null
ZZ
toJ
