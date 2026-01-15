#!/usr/bin/env python3
import json
import re

def clean_m2_doc(text):
    """Clean up M2's DIV/SPAN structure to readable text."""
    if not text:
        return ""
    
    # Remove DIV{...}, SPAN{...}, TT{...}, etc.
    # Keep the content inside the braces
    while True:
        match = re.search(r'[A-Z0-9]+{([^{}]*)}', text)
        if not match:
            break
        text = text[:match.start()] + match.group(1) + text[match.end():]
    
    # Remove remaining structural tags like PARA, HEADER1, etc.
    text = re.sub(r'[A-Z0-9]+{', '', text)
    text = text.replace('}', '')
    
    # Clean up multiple spaces and newlines
    text = re.sub(r' +', ' ', text)
    text = text.strip()
    
    return text

def extract_examples(text):
    """Extract example code from the M2 documentation string."""
    examples = []
    # Look for code blocks inside TABLE{class => examples, ...}
    # These often look like CODE{i1 : ... \n o1 = ...}
    code_matches = re.finditer(r'CODE{([^}]*), class => language-macaulay2}', text)
    for match in code_matches:
        code = match.group(1)
        if 'i1 :' in code or 'i2 :' in code:
            examples.append(code)
    
    return "\n\n".join(examples)

def main():
    print("Loading data...")
    with open("raw_data.json", "r") as f:
        raw_data = json.load(f)
    
    with open("documentation_data.json", "r") as f:
        doc_data = json.load(f)
    
    print(f"Merging docs for {len(raw_data)} items...")
    
    updated_count = 0
    for item in raw_data:
        name = item.get("name")
        if name in doc_data:
            docs = doc_data[name]
            
            # Headline
            headline = docs.get("headline", "").strip()
            if headline:
                item["headline"] = headline
            
            # Full description (cleaned)
            raw_desc = docs.get("description", "")
            if raw_desc:
                item["description"] = clean_m2_doc(raw_desc)
                item["has_documentation"] = True
                
                # Extract examples
                examples = extract_examples(raw_desc)
                if examples:
                    item["examples_extracted"] = examples
            
            updated_count += 1
    
    print(f"Updated {updated_count} items with real documentation.")
    
    # Save merged data
    print("Saving updated raw_data.json...")
    with open("raw_data.json", "w") as f:
        json.dump(raw_data, f, indent=None, separators=(',', ':'))
    
    print("Done. Now run generate_docs.py.")

if __name__ == "__main__":
    main()
