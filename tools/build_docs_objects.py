import json
import sys
import os
from typing import Dict, Any

# Ensure we can import from the same directory
sys.path.append(os.path.dirname(__file__))

from structure_docs import Instance, Type, Function, Method, Installation, Option

def load_data(json_path: str) -> Dict[str, Instance]:
    print(f"Loading data from {json_path}...")
    with open(json_path, 'r') as f:
        data = json.load(f)
    
    registry: Dict[str, Instance] = {}
    
    # Pass 1: Create Instances
    for item in data:
        name = item.get("name")
        kind = item.get("kind")
        if not name: continue
        
        # Prepare common args
        description = item.get("description", "")
        examples = "" # Placeholder, to be populated from full_doc or separate extraction
        extra = item # Store raw data
        
        # Determine class based on kind
        if kind == "Type":
            obj = Type(name, None, description, examples, None, extra)
        elif kind == "Method":
            obj = Method(name, None, description, examples, None, None, None, item.get("options"), extra)
        elif kind == "Function":
            obj = Function(name, None, description, examples, None, None, None, extra)
        elif kind == "Option":
            obj = Option(name, description, extra)
        else:
            obj = Instance(name, None, description, examples, extra)
        
        registry[name] = obj
        
    print(f"Pass 1 complete: Created {len(registry)} objects.")

    # Pass 2: Link
    for name, obj in registry.items():
        raw = obj.extra
        
        # Link Type (class of the instance)
        type_name = raw.get("type")
        if type_name:
             # Some type names might be complex (e.g. "List of ...")
             # For now, exact match
             if type_name in registry:
                 t = registry[type_name]
                 if isinstance(t, Type):
                     obj.type = t
                     t.instances.append(obj)
        
        # Link Parent (for Type)
        if isinstance(obj, Type):
             # We need to find parent info.
             # raw_data.json has "ancestors"? Or "instanceOf"?
             # "instanceOf" usually lists types it belongs to.
             # The first one might be parent?
             # Let's check "instanceOf" field in raw data.
             pass

        # Link Installations (for Method)
        if isinstance(obj, Method):
             installations = raw.get("installations")
             if installations:
                 for inst_sig in installations:
                     # inst_sig: [name, type1, type2...]
                     domain_names = inst_sig[1:]
                     domain_types = []
                     for tn in domain_names:
                         if tn in registry and isinstance(registry[tn], Type):
                             domain_types.append(registry[tn])
                         else:
                             # Create stub type or warn?
                             # For now, just skip if not found or not a Type
                             pass
                     
                     # Create Installation
                     if domain_types:
                         Installation(obj, domain_types, None, None, None, None, {})

    print("Pass 2 complete: Linked objects.")
    return registry

if __name__ == "__main__":
    reg = load_data("docs/internal/raw_data.json")
    
    methods = [x for x in reg.values() if isinstance(x, Method)]
    print(f"Methods: {len(methods)}")
    
    insts_count = sum(len(m.installations) for m in methods)
    print(f"Total Installations: {insts_count}")
    
    types = [x for x in reg.values() if isinstance(x, Type)]
    print(f"Types: {len(types)}")
    
    # Check bidirectional links
    linked_insts = sum(len(t.arg_type_of) for t in types)
    print(f"Installations linked to Types (arg_type_of): {linked_insts}")
    # Note: linked_insts might be > insts_count * avg_arity because one installation links to multiple types
    
    print("Verification complete.")
