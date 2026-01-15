import json
import sys

def main():
    with open("docs/internal/raw_data.json", 'r') as f:
        data = json.load(f)
    
    items = [x for x in data if "CC" in x['name']]
    for item in items:
        print(f"Name: {item['name']}, Kind: {item['kind']}, SafeName: {item.get('safeName')}")

if __name__ == "__main__":
    main()
