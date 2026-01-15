safeValue = (n) -> (
    try value n else null
);

print ("safeValue('Type'): " | toString safeValue("Type"))
print ("safeValue('resolution'): " | toString safeValue("resolution"))
exit 0
