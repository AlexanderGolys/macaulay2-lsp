#!/usr/bin/env python3
"""
Selectively extract documentation for important instances.
Focuses on Functions and Commands rather than simple data instances.
"""

import json
import subprocess
import sys

def extract_instance(instance_name):
    """Extract data for a single instance using M2."""
    try:
        result = subprocess.run(
            ["M2", "--script", "tools/extract_instances.m2", instance_name],
            capture_output=True,
            text=True,
            timeout=5  # Shorter timeout per instance
        )
        if result.returncode != 0:
            return None
        
        # Parse JSON
        try:
            data = json.loads(result.stdout)
            if data == "null" or not data:
                return None
            return data
        except json.JSONDecodeError:
            return None
    except (subprocess.TimeoutExpired, Exception):
        return None

def is_interesting_instance(inst_name):
    """Determine if an instance is worth documenting separately."""
    # Skip configuration variables (usually lowercase or camelCase starting with lowercase)
    if inst_name and inst_name[0].islower():
        # But keep important commands
        important_lowercase = {
            'help', 'exit', 'quit', 'restart', 'clearAll', 'clearOutput',
            'load', 'needs', 'use', 'examples', 'code', 'methods', 'options',
            'scan', 'apply', 'select', 'toString', 'toList', 'value',
            'parent', 'class', 'instances', 'keys', 'values', 'pairs'
        }
        if inst_name not in important_lowercase:
            return False
    
    # Skip internal/debug variables
    skip_patterns = ['Count', 'state', 'Attrs', 'Keys', 'Names', 'Objects']
    if any(pattern in inst_name for pattern in skip_patterns):
        return False
    
    return True

def main():
    # Load existing data
    print("Loading existing data...")
    try:
        with open("raw_data.json", "r") as f:
            existing_data = json.load(f)
    except FileNotFoundError:
        print("Error: raw_data.json not found. Run extract_docs.m2 first.")
        return 1
    
    print(f"Loaded {len(existing_data)} existing items.")
    
    # Build a set of already-extracted names to avoid infinite loops
    existing_names = {item["name"] for item in existing_data if "name" in item}
    print(f"Already have {len(existing_names)} unique names.")
    
    # Collect interesting instances from Types
    instances_to_extract = set()
    for item in existing_data:
        if item.get("kind") == "Type" and "instances" in item:
            for inst_name in item["instances"]:
                # Avoid recursion: don't re-extract existing items
                if inst_name not in existing_names and is_interesting_instance(inst_name):
                    instances_to_extract.add(inst_name)
    
    print(f"Found {len(instances_to_extract)} interesting instances to extract.")
    
    if not instances_to_extract:
        print("No new instances to extract!")
        return 0
    
    # Limit to a reasonable number for first pass
    max_instances = 100
    if len(instances_to_extract) > max_instances:
        print(f"Limiting to first {max_instances} instances for safety.")
        instances_to_extract = set(sorted(instances_to_extract)[:max_instances])
    
    # Extract instances
    new_data = []
    failed = []
    total = len(instances_to_extract)
    
    for i, inst_name in enumerate(sorted(instances_to_extract), 1):
        print(f"[{i}/{total}] Extracting: {inst_name}", end="... ")
        
        inst_data = extract_instance(inst_name)
        if inst_data:
            new_data.append(inst_data)
            existing_names.add(inst_name)  # Prevent re-extraction
            print("✓")
        else:
            failed.append(inst_name)
            print("✗")
    
    print(f"\nSuccessfully extracted {len(new_data)} new instances.")
    print(f"Failed: {len(failed)} instances")
    
    if failed and len(failed) <= 20:
        print(f"Failed instances: {', '.join(failed)}")
    
    if not new_data:
        print("No new data to add.")
        return 0
    
    # Merge data
    merged_data = existing_data + new_data
    print(f"Total items: {len(merged_data)}")
    
    # Save merged data
    print("Saving merged data...")
    with open("raw_data.json", "w") as f:
        json.dump(merged_data, f, indent=None, separators=(',', ':'))
    
    with open("docs/internal/raw_data.json", "w") as f:
        json.dump(merged_data, f, indent=None, separators=(',', ':'))
    
    print("Done! Run generate_docs.py to create MDX files for new instances.")
    
    # Report on what types of things were extracted
    kinds = {}
    for item in new_data:
        kind = item.get("kind", "Unknown")
        kinds[kind] = kinds.get(kind, 0) + 1
    
    print("\nExtracted by kind:")
    for kind, count in sorted(kinds.items()):
        print(f"  {kind}: {count}")
    
    return 0

if __name__ == "__main__":
    sys.exit(main())
