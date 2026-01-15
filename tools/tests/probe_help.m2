stderr << "Probing help logic..." << endl;
-- Try to get the RawDocumentation for ZZ
try (
    tag = makeDocumentTag ZZ;
    doc = help(tag);
    stderr << "Class of help(tag): " << toString class doc << endl;
    stderr << "Keys: " << toString keys doc << endl;
) else (
    stderr << "help(tag) failed" << endl;
);

exit 0;
