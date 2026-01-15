import json
import os

data_path = "docs/internal/raw_data.json"

def classify_options():
    print("Loading data...")
    try:
        with open(data_path, 'r') as f:
            data = json.load(f)
    except FileNotFoundError:
        print(f"Error: {data_path} not found.")
        return

    # Collect all option names
    option_names = set()
    for item in data:
        if "options" in item and item["options"]:
            for opt in item["options"]:
                # opt is a dict {name, default}
                if "name" in opt:
                    option_names.add(opt["name"])

    print(f"Found {len(option_names)} unique option names used in functions/methods.")

    # Update items
    updated_count = 0
    instances_checked = 0
    
    for item in data:
        if item.get("kind") == "Instance":
            instances_checked += 1
            # Check if name is in option_names
            # Also check if it is an instance of Symbol (if we have that info)
            # item["instanceOf"] might be "Symbol" or similar
            
            is_symbol = False
            if "instanceOf" in item and item["instanceOf"] == "Symbol":
                is_symbol = True
            
            # Note: Some options might be Keywords or other things, but usually Symbols.
            # If it's used as an option name, let's classify it as Option.
            # However, we should be careful not to reclassify things that are primarily something else
            # but used as an option? (Unlikely for symbols).
            
            if item["name"] in option_names:
                if is_symbol:
                    item["kind"] = "Option"
                    updated_count += 1
                else:
                    # It's in option_names but not marked as Symbol. 
                    # It might be that instanceOf is missing or different.
                    # Let's check what it is.
                    # If it's "Thing" or similar generic, maybe safe to update.
                    # But if it's "Type", definitely not.
                    if item.get("kind") == "Type":
                        continue
                        
                    # If we don't know it's a symbol, but it's used as an option...
                    # Most options ARE symbols.
                    # Let's be aggressive if it matches exactly.
                    item["kind"] = "Option"
                    updated_count += 1

    print(f"Checked {instances_checked} instances.")
    print(f"Updated {updated_count} items to kind 'Option'.")

    print("Saving updated data...")
    with open(data_path, 'w') as f:
        json.dump(data, f, indent=2)
    print("Done.")

if __name__ == "__main__":
    classify_options()
