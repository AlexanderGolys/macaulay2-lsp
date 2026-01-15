names = apropos ""
scan(names, n -> (
   if n === symbol + then print "Found + as Symbol"
   else if n === "+" then print "Found + as String"
))
exit 0
