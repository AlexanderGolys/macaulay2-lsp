probe = (name) -> (
    obj = value name;
    tag = makeDocumentTag obj;
    
    checkKey = (key) -> (
        if tag#?key then (
            val = tag#key;
            stderr << "=== " << name << " " << key << " ===" << endl;
            stderr << "Class: " << toString class val << endl;
            if instance(val, List) then (
                stderr << "Length: " << #val << endl;
                scan(0 .. min(3, #val - 1), i -> (
                    elem = val#i;
                    stderr << "Elem " << i << " class: " << toString class elem << endl;
                    stderr << "Elem " << i << " content: " << toString elem << endl;
                ));
            ) else (
                stderr << "Content: " << toString val << endl;
            );
        ) else (
            stderr << name << ": No " << key << endl;
        );
    );
    
    checkKey "Description";
    checkKey "RawDocumentation";
);

probe "abs";
exit 0;
