#!/usr/bin/env python3
"""
Add bidirectional references to documentation.
Ensures that if A references B, then B references A back.
"""

import json
import os
from collections import defaultdict

def main():
    print("Loading existing documentation data...")
    # Use consistent path
    data_path = "docs/internal/raw_data.json"
    try:
        with open(data_path, "r") as f:
            data = json.load(f)
    except FileNotFoundError:
        print(f"Error: {data_path} not found.")
        return 1
    
    print(f"Loaded {len(data)} items.")
    
    # Build indices
    items_by_name = {item["name"]: item for item in data if "name" in item}
    
    # Track all relationships (both directions)
    instance_of = defaultdict(list)  # instance_name -> [type_names]
    instances_list = defaultdict(list)  # type_name -> [instance_names]
    used_in_methods = defaultdict(set)  # type_name -> {method_names}
    method_uses_types = defaultdict(set)  # method_name -> {type_names}
    parent_of = defaultdict(list)  # parent_type -> [child_types] (REVERSE of parent)
    child_has_parent = {}  # child_type -> parent_type (FORWARD)
    
    # New: Options
    used_in_functions = defaultdict(set) # option_name -> {function_names}
    
    print("\nAnalyzing relationships...")
    
    # Build relationship maps
    for item in data:
        name = item.get("name")
        kind = item.get("kind")
        
        # Parent/Subtype bidirectional relationship
        if kind == "Type" and "parent" in item and item["parent"]:
            parent = item["parent"]
            child_has_parent[name] = parent
            parent_of[parent].append(name)
        
        if kind == "Type" and "instances" in item:
            # Type lists instances
            for inst_name in item["instances"]:
                instances_list[name].append(inst_name)
                instance_of[inst_name].append(name)
        
        if "methods" in item and kind == "Type":
            # Type has methods - track which methods use this type
            for method_sig in item["methods"]:
                if method_sig:
                    method_name = method_sig[0]
                    used_in_methods[name].add(method_name)
                    method_uses_types[method_name].add(name)
        
        if "installations" in item and kind in ["Function", "Operator", "Method"]:
            # Method/Operator has installations - track which types it works with
            for inst_sig in item["installations"]:
                if len(inst_sig) > 1:
                    for type_name in inst_sig[1:]:
                        if type_name != inst_sig[0]:  # Skip the method name itself
                            used_in_methods[type_name].add(name)
                            method_uses_types[name].add(type_name)
                            
        # Options usage
        if "options" in item and kind in ["Function", "Method", "Operator"]:
            for opt in item["options"]:
                if "name" in opt:
                    opt_name = opt["name"]
                    used_in_functions[opt_name].add(name)
    
    # Add reverse references to data
    print("\nAdding bidirectional references...")
    updates = 0
    
    for item in data:
        name = item.get("name")
        kind = item.get("kind")
        modified = False
        
        # Add subtypes reference (bidirectional to parent)
        if kind == "Type" and name in parent_of:
            subtypes_list = sorted(parent_of[name])
            if "subtypes" not in item:
                item["subtypes"] = subtypes_list
                modified = True
        
        # Add instanceOf reference
        if name in instance_of and "instanceOf" not in item:
            item["instanceOf"] = instance_of[name][0] if len(instance_of[name]) == 1 else instance_of[name]
            modified = True
        
        # Add usedInMethods reference
        if name in used_in_methods and kind == "Type":
            methods_list = sorted(list(used_in_methods[name]))[:50]  # Limit to 50
            if "usedInMethods" not in item:
                item["usedInMethods"] = methods_list
                modified = True
        
        # Add worksWithTypes reference for methods/operators
        if name in method_uses_types and kind in ["Function", "Operator", "Method"]:
            types_list = sorted(list(method_uses_types[name]))[:30]  # Limit to 30
            if "worksWithTypes" not in item:
                item["worksWithTypes"] = types_list
                modified = True
                
        # Add usedInFunctions reference for Options
        if kind == "Option" and name in used_in_functions:
            funcs_list = sorted(list(used_in_functions[name]))
            if "usedInFunctions" not in item:
                item["usedInFunctions"] = funcs_list
                modified = True
        
        if modified:
            updates += 1
    
    print(f"Updated {updates} items with bidirectional references.")
    
    # Create stub entries for missing instances
    print("\nCreating stubs for missing instances...")
    stubs_created = 0
    
    all_instance_names = set(instance_of.keys())
    existing_names = set(items_by_name.keys())
    missing = all_instance_names - existing_names
    
    print(f"Found {len(missing)} missing instances to create stubs for.")
    
    # Removed limit for full coverage
    # if len(missing) > 200:
    #     print(f"Limiting to first 200 stubs for safety.")
    #     missing = set(sorted(missing)[:200])
    
    # Load Core symbols for filtering stubs
    core_syms_path = "docs/internal/core_symbols.json"
    core_syms = set()
    if os.path.exists(core_syms_path):
        with open(core_syms_path) as f:
            core_syms = set(json.load(f))
    else:
        print(f"Warning: {core_syms_path} not found. Stubs won't be filtered by Core symbols.")

    for inst_name in sorted(missing):
        # Filter: only create stub if it's in Core symbols (if list available)
        if core_syms and inst_name not in core_syms:
            continue

        # Create a stub entry
        stub = {
            "name": inst_name,
            "kind": "Instance",  # Generic kind for unknown items
            "safeName": inst_name if inst_name.replace("_", "").replace("'", "").isalnum() else f"inst_{inst_name}",
            "instanceOf": instance_of[inst_name][0] if len(instance_of[inst_name]) == 1 else instance_of[inst_name],
            "has_documentation": False,
            "description": "",
            "stub": True  # Mark as stub for later completion
        }
        data.append(stub)
        stubs_created += 1
    
    print(f"Created {stubs_created} stub entries.")
    
    # Save updated data
    print("\nSaving updated data...")
    # Save to docs/internal only
    with open(data_path, "w") as f:
        json.dump(data, f, indent=None, separators=(',', ':'))
    
    print(f"\nTotal items now: {len(data)}")
    
    # Report statistics
    print("\nStatistics:")
    print(f"  Types with subtypes: {len(parent_of)}")
    print(f"  Types with parent: {len(child_has_parent)}")
    print(f"  Types with instances: {len(instances_list)}")
    print(f"  Total instance relationships: {len(instance_of)}")
    print(f"  Types used in methods: {len([t for t in used_in_methods if t in items_by_name and items_by_name[t].get('kind') == 'Type'])}")
    print(f"  Methods/operators with type info: {len([m for m in method_uses_types if m in items_by_name])}")
    print(f"  Options used in functions: {len(used_in_functions)}")
    
    return 0

if __name__ == "__main__":
    import sys
    sys.exit(main())
