#!/usr/bin/env python3
"""
Verify that all instances have corresponding documentation files.
For each Type, check that all its instances have MDX files.
"""

import json
import os
from collections import defaultdict

def get_safe_name(name, kind="Instance"):
    """Approximate the safeName logic from generate_docs.py"""
    if name.replace("_", "").replace("'", "").isalnum():
        return name
    # For operators
    if any(c in name for c in "!@#$%^&*()+-=[]{}|\\:;\"'<>,.?/~`"):
        return f"op_{name}"  # Simplified
    return name

def main():
    print("Loading documentation data...")
    # Use internal data path
    data_path = "docs/internal/raw_data.json"
    if not os.path.exists(data_path):
        data_path = "raw_data.json"
        
    with open(data_path, "r") as f:
        data = json.load(f)
    
    # Build indices
    items_by_name = {item["name"]: item for item in data if "name" in item}
    items_by_safe_name = {item.get("safeName", item["name"]): item for item in data if "name" in item}
    
    print(f"Loaded {len(data)} items.\n")
    
    # Check each Type and its instances
    types_with_instances = [item for item in data if item.get("kind") == "Type" and "instances" in item]
    
    print(f"Checking {len(types_with_instances)} Types that have instances...\n")
    
    total_instances = 0
    documented_instances = 0
    missing_by_type = defaultdict(list)
    
    for type_item in types_with_instances:
        type_name = type_item["name"]
        instances = type_item.get("instances", [])
        
        if not instances:
            continue
        
        total_instances += len(instances)
        
        for inst_name in instances:
            # Check if instance exists in our data
            if inst_name in items_by_name:
                documented_instances += 1
            else:
                missing_by_type[type_name].append(inst_name)
    
    # Report results
    print("=" * 70)
    print("VERIFICATION REPORT")
    print("=" * 70)
    print(f"\nTotal Types with instances: {len(types_with_instances)}")
    print(f"Total instance references: {total_instances}")
    print(f"Documented instances: {documented_instances}")
    print(f"Missing instances: {total_instances - documented_instances}")
    print(f"Coverage: {100.0 * documented_instances / total_instances:.1f}%\n")
    
    if missing_by_type:
        print("\nTypes with missing instance documentation:")
        print("-" * 70)
        
        # Sort by number of missing instances
        sorted_types = sorted(missing_by_type.items(), key=lambda x: len(x[1]), reverse=True)
        
        for type_name, missing_insts in sorted_types[:10]:  # Show top 10
            print(f"\n{type_name}: {len(missing_insts)} missing instances")
            if len(missing_insts) <= 10:
                for inst in missing_insts:
                    print(f"  - {inst}")
            else:
                for inst in missing_insts[:5]:
                    print(f"  - {inst}")
                print(f"  ... and {len(missing_insts) - 5} more")
        
        if len(sorted_types) > 10:
            print(f"\n... and {len(sorted_types) - 10} more types with missing instances")
    
    # Check MDX file existence
    print("\n" + "=" * 70)
    print("Checking MDX file existence...")
    print("=" * 70)
    
    doc_dirs = {
        "Type": "docs/reference/Type",
        "Function": "docs/reference/Function",
        "Method": "docs/reference/Method",
        "Operator": "docs/reference/Operator",
        "Instance": "docs/reference/Instance",
        # "Option": "docs/reference/Option", # Options are in OptionKeywords.mdx
    }
    
    missing_files = []
    
    for item in data:
        if "name" not in item or "kind" not in item:
            continue
        
        kind = item["kind"]
        safe_name = item.get("safeName", item["name"])
        
        if kind in doc_dirs:
            expected_path = f"{doc_dirs[kind]}/{safe_name}.mdx"
            if not os.path.exists(expected_path):
                missing_files.append((item["name"], kind, expected_path))
    
    if missing_files:
        print(f"\nFound {len(missing_files)} items without MDX files:")
        for name, kind, path in missing_files[:20]:
            print(f"  - {name} ({kind}): expected at {path}")
        if len(missing_files) > 20:
            print(f"  ... and {len(missing_files) - 20} more")
    else:
        print("\n✓ All items have corresponding MDX files!")
    
    # Summary
    print("\n" + "=" * 70)
    print("SUMMARY")
    print("=" * 70)
    kinds = defaultdict(int)
    for item in data:
        kinds[item.get("kind", "Unknown")] += 1
    
    print("\nItems by kind:")
    for kind, count in sorted(kinds.items()):
        print(f"  {kind}: {count}")
    
    return 0

if __name__ == "__main__":
    import sys
    sys.exit(main())
