stderr << "Testing operator classes..." << endl;

plusSym = value "symbol +";
stderr << "symbol + class: " << toString class plusSym << endl;
stderr << "symbol + type: " << toString class class plusSym << endl;
stderr << "Is Symbol? " << toString instance(plusSym, Symbol) << endl;
stderr << "Is Keyword? " << toString instance(plusSym, Keyword) << endl;
stderr << "Is Function? " << toString instance(plusSym, Function) << endl;

-- Try a method
stderr << "\nTesting methods..." << endl;
methodObj = value "methods";
stderr << "methods class: " << toString class methodObj << endl;
stderr << "Is Method? " << toString instance(methodObj, Method) << endl;
stderr << "Is Function? " << toString instance(methodObj, Function) << endl;

-- Try an actual function
stderr << "\nTesting functions..." << endl;
factorObj = value "factor";
stderr << "factor class: " << toString class factorObj << endl;
stderr << "Is Function? " << toString instance(factorObj, Function) << endl;

exit 0;
