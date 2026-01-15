import json
import os

def main():
    with open("docs/internal/raw_data.json", 'r') as f:
        old_data = json.load(f)
    
    with open("docs/internal/raw_data_new.json", 'r') as f:
        new_data = json.load(f)
    
    old_map = {item['name']: item for item in old_data}
    
    merged = []
    for item in new_data:
        name = item['name']
        if name in old_map:
            old_item = old_map[name]
            # Preserve scraped details
            for key in ["full_doc", "headline", "description", "examples", "usage", "additional_info", "has_documentation", "stub"]:
                if key in old_item:
                    item[key] = old_item[key]
        merged.append(item)
    
    with open("docs/internal/raw_data.json", 'w') as f:
        json.dump(merged, f, indent=4)
    
    print(f"Merged {len(merged)} items into raw_data.json.")

if __name__ == "__main__":
    main()
