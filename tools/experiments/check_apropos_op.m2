names = apropos ""
print ("+ in names: " | toString(member(symbol +, names)))
print ("'+' in names: " | toString(member("+", names)))
exit 0
