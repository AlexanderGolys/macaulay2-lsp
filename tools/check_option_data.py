import json
import sys

def main():
    with open("docs/internal/raw_data.json", 'r') as f:
        data = json.load(f)
    
    body = next((x for x in data if x['name'] == 'Body'), None)
    if body:
        print(json.dumps(body, indent=4))
    else:
        print("Body not found")

if __name__ == "__main__":
    main()
