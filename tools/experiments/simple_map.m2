print "Testing mapping..."
mapping = new HashTable from { "+" => "plus", "*" => "star" }
print ("+ -> " | mapping#("+"))
exit 0
