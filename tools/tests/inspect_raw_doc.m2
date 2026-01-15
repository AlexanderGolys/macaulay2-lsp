stderr << "Inspecting ZZ RawDocumentation..." << endl;
tag = makeDocumentTag ZZ;
rd = tag#RawDocumentation;
stderr << "Class: " << toString class rd << endl;
stderr << "Content: " << toString rd << endl;

stderr << "\nKeys if HashTable: " << (if instance(rd, HashTable) then toString keys rd else "Not HashTable") << endl;

-- Let's try to see if it's a List of things
if instance(rd, List) then (
    stderr << "List length: " << toString #rd << endl;
    scan(rd, i -> stderr << "  Item class: " << toString class i << endl);
);

exit 0;
