f = resolution
print "Options:"
try print toString options f else print "No options"

print "Methods:"
try print toString methods f else print "No methods"

L = methods f
if instance(L, Sequence) then L = toList L
if #L > 0 then (
    m = L#0
    print ("First method: " | toString m)
    print ("Class of method: " | toString class m)
)
exit 0
