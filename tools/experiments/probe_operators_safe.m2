op = symbol +
print ("Class of +: " | toString(class op))
-- OperatorAttributes is a HashTable
-- Keys are symbols?
try (
    if OperatorAttributes#?op then (
        print "Found attributes for +"
        print toString(OperatorAttributes#op)
    ) else (
        print "No attributes for +"
    )
) else print "OperatorAttributes access failed"
exit 0
