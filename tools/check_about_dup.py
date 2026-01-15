import json
import sys

def main():
    print("Loading raw data...")
    with open("docs/internal/raw_data.json", 'r') as f:
        data = json.load(f)
    
    abouts = [x for x in data if x['name'] == 'about']
    print(f"Found {len(abouts)} items named 'about'")
    for a in abouts:
        print(f"Kind: {a.get('kind')}")
        print(f"InstanceOf: {a.get('instanceOf')}")

if __name__ == "__main__":
    main()
