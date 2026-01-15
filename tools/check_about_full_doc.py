import json
import sys

def main():
    with open("docs/internal/raw_data.json", 'r') as f:
        data = json.load(f)
    
    about = next((x for x in data if x['name'] == 'about'), None)
    if about:
        print(about.get('full_doc'))
    else:
        print("about not found")

if __name__ == "__main__":
    main()
