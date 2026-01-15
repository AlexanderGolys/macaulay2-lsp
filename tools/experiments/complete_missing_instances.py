#!/usr/bin/env python3
"""
Add documentation for all missing instances.
Creates complete documentation with all bidirectional references.
"""

import json
import subprocess
from collections import defaultdict

def extract_instance_info(inst_name):
    """Try to extract basic info about an instance from M2."""
    try:
        result = subprocess.run(
            ["M2", "--script", "tools/extract_instances.m2", inst_name],
            capture_output=True,
            text=True,
            timeout=3
        )
        if result.returncode == 0:
            try:
                return json.loads(result.stdout)
            except:
                return None
    except:
        pass
    return None

def main():
    print("Loading existing documentation data...")
    with open("raw_data.json", "r") as f:
        data = json.load(f)
    
    items_by_name = {item["name"]: item for item in data if "name" in item}
    print(f"Loaded {len(data)} items.\n")
    
    # Find all missing instances
    missing_instances = set()
    instance_to_types = defaultdict(list)  # Track which types list each instance
    
    for item in data:
        if item.get("kind") == "Type" and "instances" in item:
            type_name = item["name"]
            for inst_name in item["instances"]:
                if inst_name not in items_by_name:
                    missing_instances.add(inst_name)
                    instance_to_types[inst_name].append(type_name)
    
    print(f"Found {len(missing_instances)} missing instances.")
    
    if not missing_instances:
        print("No missing instances to add!")
        return 0
    
    # Try to extract info for missing instances
    print("\nExtracting information for missing instances...")
    new_items = []
    failed = []
    
    for i, inst_name in enumerate(sorted(missing_instances), 1):
        if i % 20 == 0:
            print(f"  [{i}/{len(missing_instances)}] {inst_name}")
        
        # Try extraction
        inst_data = extract_instance_info(inst_name)
        
        if inst_data and inst_data != "null":
            # Successfully extracted
            new_items.append(inst_data)
        else:
            # Create stub with minimal info
            types = instance_to_types[inst_name]
            safe_name = inst_name
            
            # Check if it needs a safe name transformation
            if not inst_name.replace("_", "").replace("'", "").isalnum():
                safe_name = "inst_" + "".join(c if c.isalnum() else "_" for c in inst_name)
            
            stub = {
                "name": inst_name,
                "kind": "Instance",
                "safeName": safe_name,
                "instanceOf": types[0] if len(types) == 1 else types,
                "has_documentation": False,
                "description": "",
                "stub": True
            }
            new_items.append(stub)
            failed.append(inst_name)
    
    print(f"\nAdded {len(new_items)} new items ({len(new_items) - len(failed)} extracted, {len(failed)} stubs)")
    
    # Merge with existing data
    all_data = data + new_items
    
    # Now rebuild ALL bidirectional references
    print("\nRebuilding bidirectional references...")
    
    # Build relationship maps
    items_by_name = {item["name"]: item for item in all_data if "name" in item}
    
    parent_of = defaultdict(list)
    instance_of = defaultdict(list)
    used_in_methods = defaultdict(set)
    method_uses_types = defaultdict(set)
    
    for item in all_data:
        name = item.get("name")
        kind = item.get("kind")
        
        # Parent/Subtype relationships
        if kind == "Type" and "parent" in item and item["parent"]:
            parent = item["parent"]
            parent_of[parent].append(name)
        
        # Instance relationships
        if kind == "Type" and "instances" in item:
            for inst_name in item["instances"]:
                instance_of[inst_name].append(name)
        
        # Method/Type relationships
        if "methods" in item and kind == "Type":
            for method_sig in item["methods"]:
                if method_sig:
                    method_name = method_sig[0]
                    used_in_methods[name].add(method_name)
                    method_uses_types[method_name].add(name)
        
        if "installations" in item and kind in ["Function", "Operator"]:
            for inst_sig in item["installations"]:
                if len(inst_sig) > 1:
                    for type_name in inst_sig[1:]:
                        if type_name != inst_sig[0]:
                            used_in_methods[type_name].add(name)
                            method_uses_types[name].add(type_name)
    
    # Apply bidirectional references
    updates = 0
    for item in all_data:
        name = item.get("name")
        kind = item.get("kind")
        
        # Add subtypes (reverse of parent)
        if kind == "Type" and name in parent_of:
            item["subtypes"] = sorted(parent_of[name])
            updates += 1
        
        # Add instanceOf (reverse of instances)
        if name in instance_of:
            types = instance_of[name]
            item["instanceOf"] = types[0] if len(types) == 1 else types
            updates += 1
        
        # Add usedInMethods (for Types)
        if kind == "Type" and name in used_in_methods:
            item["usedInMethods"] = sorted(list(used_in_methods[name]))[:50]
            updates += 1
        
        # Add worksWithTypes (for Functions/Operators)
        if kind in ["Function", "Operator"] and name in method_uses_types:
            item["worksWithTypes"] = sorted(list(method_uses_types[name]))[:30]
            updates += 1
    
    print(f"Updated {updates} bidirectional reference fields.")
    
    # Save
    print("\nSaving complete data...")
    with open("raw_data.json", "w") as f:
        json.dump(all_data, f, indent=None, separators=(',', ':'))
    
    with open("docs/internal/raw_data.json", "w") as f:
        json.dump(all_data, f, indent=None, separators=(',', ':'))
    
    # Report
    kinds = defaultdict(int)
    for item in all_data:
        kinds[item.get("kind", "Unknown")] += 1
    
    print(f"\nTotal items: {len(all_data)}")
    print("\nBy kind:")
    for kind, count in sorted(kinds.items()):
        print(f"  {kind}: {count}")
    
    print("\nNow run: python3 tools/generate_docs.py")
    return 0

if __name__ == "__main__":
    import sys
    sys.exit(main())
