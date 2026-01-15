print "Searching for 'plus' in all symbols..."
allSyms = apropos ""
hasPlus = member("plus", allSyms)
print ("Has 'plus': " | toString hasPlus)

hasMinus = member("minus", allSyms)
print ("Has 'minus': " | toString hasMinus)

hasStar = member("star", allSyms)
print ("Has 'star': " | toString hasStar)

exit 0
