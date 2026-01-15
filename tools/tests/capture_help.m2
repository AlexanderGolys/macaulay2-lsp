stderr << "Capturing help ZZ..." << endl;
-- help returns a Net or Hypertext object
h = help ZZ;
stderr << "Class: " << toString class h << endl;
-- toString h gives the rendered documentation
rendered = toString h;
stderr << "Length: " << toString #rendered << endl;
stderr << "First 200 chars: " << (substring(0, min(200, #rendered), rendered)) << endl;
exit 0;
