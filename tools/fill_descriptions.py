#!/usr/bin/env python3
"""
Extract full documentation (descriptions, examples) for items.
Updates raw_data.json with rich content.
"""

import json
import subprocess
import sys
import time
import os
import re
from concurrent.futures import ThreadPoolExecutor, as_completed

def extract_doc(name):
    """Extract documentation for a single item."""
    try:
        # Run M2 script
        result = subprocess.run(
            ["M2", "--script", "tools/extract_documentation.m2", name],
            capture_output=True,
            text=True,
            timeout=10
        )
        
        if result.returncode != 0:
            return None
            
        # Parse output
        # M2 script prints JSON to stdout
        try:
            # Find the JSON part (it might have M2 startup text before it)
            output = result.stdout
            if "{" in output:
                json_str = output[output.find("{"):]
                data = json.loads(json_str)
                return data
            return None
        except json.JSONDecodeError:
            return None
            
    except Exception as e:
        return None

def main():
    print("Loading data...")
    # Prefer internal data path
    data_path = "docs/internal/raw_data.json"
    if not os.path.exists(data_path):
        data_path = "raw_data.json"
        
    with open(data_path, "r") as f:
        data = json.load(f)
    
    print(f"Loaded {len(data)} items from {data_path}.")
    
    # Items to process: those that are marked as having docs, or stubs
    to_process = []
    for i, item in enumerate(data):
        # Process if we haven't extracted full docs yet
        # We check for 'headline' as a marker of full extraction
        # Also check if 'full_doc' is missing (for updates)
        if "headline" not in item or "full_doc" not in item:
            to_process.append((i, item["name"]))
    
    print(f"Found {len(to_process)} items to process.")
    
    if not to_process:
        print("All items processed.")
        return
        
    # Limit for testing? No, let's try to run all but save periodically
    # Parallel processing
    updated_count = 0
    
    # i9-14900K allows for higher concurrency
    max_workers = 16
    print(f"Starting extraction (using {max_workers} threads)...")
    
    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_to_idx = {
            executor.submit(extract_doc, name): idx 
            for idx, name in to_process
        }
        
        processed = 0
        total = len(to_process)
        
        for future in as_completed(future_to_idx):
            idx = future_to_idx[future]
            name = data[idx]["name"]
            processed += 1
            
            if processed % 50 == 0:
                print(f"[{processed}/{total}] Processing...")
                
            try:
                doc_data = future.result()
                if doc_data:
                    # Update item
                    item = data[idx]
                    if doc_data.get("headline"):
                        item["headline"] = doc_data["headline"]
                    if doc_data.get("full_doc"):
                        item["full_doc"] = doc_data["full_doc"]

                    if doc_data.get("description"):
                        desc = doc_data["description"]
                        # Clean prefix like "Name -- " or "Name, -- "
                        # Regex matches: Start, optional word (name), optional comma/space, --, whitespace
                        clean_desc = re.sub(r'^\s*[\w\']+\s*(?:,|\s)\s*--\s*', '', desc)
                        item["description"] = clean_desc
                        
                    if doc_data.get("usage"):
                        item["usage"] = doc_data["usage"]
                    if doc_data.get("examples"):
                        item["examples"] = doc_data["examples"]
                    
                    # Update has_documentation flag
                    has_docs = doc_data.get("hasDocumentation", False)
                    item["has_documentation"] = has_docs
                    
                    # If we found documentation, it's no longer a stub
                    if has_docs:
                        item["stub"] = False
                        
                    updated_count += 1
            except Exception as e:
                print(f"Error processing {name}: {e}")
                
    print(f"Updated {updated_count} items with documentation.")
    
    print("Saving updated data...")
    with open("raw_data.json", "w") as f:
        json.dump(data, f, indent=None, separators=(',', ':'))
        
    with open("docs/internal/raw_data.json", "w") as f:
        json.dump(data, f, indent=None, separators=(',', ':'))
        
    print("Done! Run generate_docs.py to update MDX files.")

if __name__ == "__main__":
    main()
