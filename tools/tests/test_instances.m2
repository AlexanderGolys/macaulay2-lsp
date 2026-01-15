stderr << "Testing instances..." << endl;
insts = instances ZZ;
stderr << "Class: " << toString class insts << endl;
if instance(insts, HashTable) then (
    ks = keys insts;
    stderr << "Keys class: " << toString class ks << endl;
    stderr << "Keys: " << toString ks << endl;
);
exit 0;
