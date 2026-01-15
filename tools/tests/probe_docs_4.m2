stderr << "Probing ZZ..." << endl;
tagZZ = makeDocumentTag ZZ;
if tagZZ =!= null then (
    stderr << "Keys for ZZ: " << toString keys tagZZ << endl;
    if tagZZ#?Headline then stderr << "Headline: " << toString tagZZ#Headline << endl;
    if tagZZ#?Description then stderr << "Description: " << toString tagZZ#Description << endl;
    if tagZZ#?RawDocumentation then (
        rd = tagZZ#RawDocumentation;
        stderr << "RawDocumentation class: " << toString class rd << endl;
        stderr << "RawDocumentation content: " << (substring(0, min(100, # toString rd), toString rd)) << "..." << endl;
    );
);

stderr << "\nProbing edit..." << endl;
tagEdit = makeDocumentTag edit;
if tagEdit =!= null then (
    stderr << "Keys for edit: " << toString keys tagEdit << endl;
    if tagEdit#?Headline then stderr << "Headline: " << toString tagEdit#Headline << endl;
    if tagEdit#?Description then stderr << "Description: " << toString tagEdit#Description << endl;
);

exit 0;
