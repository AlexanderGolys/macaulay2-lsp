#!/usr/bin/env python3
"""
Cleanup raw_data.json:
1. Remove numeric instances.
2. Remove instances that are just numbers.
"""

import json
import re
import os

def main():
    print("Loading data...")
    with open("raw_data.json", "r") as f:
        data = json.load(f)
    
    print(f"Loaded {len(data)} items.")
    
    clean_data = []
    removed_count = 0
    
    for item in data:
        name = item.get("name", "")
        
        # Check if name is purely numeric
        if re.match(r"^\d+$", name):
            print(f"Removing numeric instance: {name}")
            removed_count += 1
            
            # Remove MDX file if it exists
            kind = item.get("kind", "Instance")
            safe_name = item.get("safeName", name)
            path = f"docs/reference/{kind}/{safe_name}.mdx"
            if os.path.exists(path):
                os.remove(path)
            continue
            
        clean_data.append(item)
    
    print(f"Removed {removed_count} items.")
    print(f"Remaining items: {len(clean_data)}")
    
    print("Saving cleaned data...")
    with open("raw_data.json", "w") as f:
        json.dump(clean_data, f, indent=None, separators=(',', ':'))
    
    with open("docs/internal/raw_data.json", "w") as f:
        json.dump(clean_data, f, indent=None, separators=(',', ':'))

if __name__ == "__main__":
    main()
