import json

def main():
    with open("docs/internal/raw_data.json", 'r') as f:
        data = json.load(f)
    
    option_names = set()
    for item in data:
        options = item.get("options")
        if options:
            if isinstance(options, list):
                for opt in options:
                    if isinstance(opt, dict) and "name" in opt:
                        option_names.add(opt["name"])
            elif isinstance(options, dict):
                for opt_name in options.keys():
                    option_names.add(opt_name)

    potential_options = []
    for item in data:
        name = item.get("name")
        full_doc = item.get("full_doc", "")
        if "an optional argument" in full_doc and name not in option_names:
            potential_options.append(name)
            
    print(f"Found {len(potential_options)} potential options from full_doc: {potential_options[:20]}")

if __name__ == "__main__":
    main()
