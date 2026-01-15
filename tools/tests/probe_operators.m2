op = symbol +
print ("Class of +: " | toString(class op))
print ("Value of +: " | toString(value op))
print ("Class of value +: " | toString(class(value op)))

op2 = symbol !
print ("Class of !: " | toString(class op2))

try (
    attrs = OperatorAttributes#op
    print ("Attributes for +: " | toString attrs)
) else print "No attributes for +"

exit 0
