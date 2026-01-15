print class Core
D = Core.dictionary
print ("Class of dictionary: " | toString class D)
K = keys D
print ("Key count: " | toString(#K))
print ("Has +: " | toString(member(symbol +, K)))
exit 0
