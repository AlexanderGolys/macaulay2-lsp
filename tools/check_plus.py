import json
import sys

def main():
    with open("docs/internal/raw_data.json", 'r') as f:
        data = json.load(f)
    
    plus = next((x for x in data if x['name'] == '+'), None)
    if plus:
        print(json.dumps(plus, indent=4))
    else:
        print("+ not found")

if __name__ == "__main__":
    main()
