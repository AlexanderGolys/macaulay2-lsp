o = options gb;
print("Keys: " | toString keys o);
k = (keys o)#0;
print("First key: " | toString k);
print("First value: " | toString(o#k));
