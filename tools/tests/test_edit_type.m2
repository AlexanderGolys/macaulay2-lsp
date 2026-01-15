stderr << "Testing edit..." << endl;
e = value "edit";
stderr << "class edit: " << toString class e << endl;
stderr << "instance(edit, Type): " << toString instance(e, Type) << endl;
stderr << "instance(edit, Command): " << toString instance(e, Command) << endl;
stderr << "parent class edit: " << toString parent class e << endl;

stderr << "\nTesting Command..." << endl;
c = Command;
stderr << "class Command: " << toString class c << endl;
stderr << "instance(Command, Type): " << toString instance(c, Type) << endl;

exit 0;
