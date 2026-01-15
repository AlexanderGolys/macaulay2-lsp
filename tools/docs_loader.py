import json
import sys
import os
import re
from typing import Dict, Any, List

sys.path.append(os.path.dirname(__file__))

from structure_docs import Instance, Type, Function, Method, Installation, Option
import m2_parser

def clean_extra(item: Dict[str, Any]) -> Dict[str, Any]:
    """Remove fields from extra that are now primary attributes of the objects."""
    keys_to_remove = [
        "name", "kind", "instanceOf", "description", 
        "installations", "worksWithTypes", "safeName", "filename",
        "options", "headline", "has_documentation"
    ]
    extra = item.copy()
    for key in keys_to_remove:
        if key in extra:
            del extra[key]
    return extra

def load_data(json_path: str) -> Dict[str, Instance]:
    print(f"Loading data from {json_path}...")
    with open(json_path, 'r') as f:
        data = json.load(f)
    
    registry: Dict[str, Instance] = {}
    raw_map: Dict[str, Dict[str, Any]] = {}
    
    # Pre-pass: Identify Option objects from Function/Method options or full_doc
    option_keys = set()
    option_values = set()
    
    # 1. From explicit options fields
    for item in data:
        options = item.get("options")
        if options:
            if isinstance(options, list):
                for opt in options:
                    if isinstance(opt, dict) and "name" in opt:
                        option_keys.add(opt["name"])
            elif isinstance(options, dict):
                for opt_name in options.keys():
                    option_keys.add(opt_name)
    
    # 2. From full_doc heuristics
    for item in data:
        name = item.get("name")
        full_doc = item.get("full_doc", "").lower()
        if "an optional argument" in full_doc or "an option" in full_doc or "the option" in full_doc:
            option_keys.add(name)
        elif "option value" in full_doc or "strategy value" in full_doc or "strategy element" in full_doc or "strategy used with" in full_doc or "a value for the option" in full_doc or "used as a value for" in full_doc or "permissible values for the strategy" in full_doc or "symbol used as the value" in full_doc or "a result indicating" in full_doc or "a symbol indicating" in full_doc:
            option_values.add(name)

    # Pass 1: Create Stub Objects
    for item in data:
        name = item.get("name")
        kind = item.get("kind")
        if not name: continue
        
        # Filter out session variables, paths, and raw numbers
        if name in ["o", "oo", "ooo", "oooo"] or re.match(r"^o\d+$", name):
            continue
        if re.match(r"^-?\d+$", name):
            continue
        if not name.strip():
            continue
        if "/" in name or "\\" in name or name.startswith("."):
            # Check if it's an operator first
            if kind != "Operator" and name not in ["..", "...", "..-", "..<"]:
                continue
            
    # Mark as Option if it's a key or value
    for item in data:
        name = item.get("name")
        kind = item.get("kind")
        if not name: continue
        
        # Mark as Option if it's a key or value or is a pure Symbol
        is_sym = item.get("isSymbol", False)
        if (name in option_keys or name in option_values or is_sym) and kind == "Instance":
            kind = "Option"
            item["kind"] = "Option"
            if name in option_keys:
                item["is_option_key"] = True
            if name in option_values:
                item["is_option_value"] = True
            if is_sym and not item.get("is_option_key") and not item.get("is_option_value"):
                # Heuristic: If it has "optional argument" in full_doc, it's a key
                # If it has "strategy value" or similar, it's a value
                # Otherwise, it might just be a protected symbol
                item["is_protected_symbol"] = True

        # Fix classification: MethodFunctionWithOptions etc. should be Method
        inst_of = item.get("instanceOf")
        if inst_of and "Method" in str(inst_of):
            kind = "Method"
            item["kind"] = "Method"

        # Filter invalid names
        if len(name) > 100 or "{" in name or "}" in name:
            # print(f"Skipping invalid/huge name: {name[:50]}...")
            continue

        raw_map[name] = item
        
        safe_name = item.get("filename") or item.get("safeName") or name
        options = item.get("options")
        extra = clean_extra(item)
        
        # Init with empty lists/strings
        if kind == "Type":
            obj = Type(name, safe_name, None, "", [], "", None, extra)
        elif kind == "Method" or kind == "Operator":
            obj = Method(name, safe_name, None, "", [], "", None, None, None, options, extra)
        elif kind == "Function":
            obj = Function(name, safe_name, None, "", [], "", None, None, None, options, extra)
        elif kind == "Option":
            obj = Option(name, safe_name, "", None, extra)
        else:
            obj = Instance(name, safe_name, None, "", [], "", extra)
        registry[name] = obj
        
    print(f"Pass 1 complete: Created {len(registry)} objects.")
    
    # Pass 2: Link Objects (Type hierarchy & Installations)
    for name, obj in registry.items():
        raw = raw_map[name]
        
        # Link Type (class of the instance)
        type_name = raw.get("type")
        if not type_name:
             val = raw.get("instanceOf")
             if isinstance(val, str):
                 type_name = val
        
        if type_name and type_name in registry:
             t = registry[type_name]
             if isinstance(t, Type):
                 obj.type = t
                 t.instances.append(obj)
        
        # Link Parent (for Type)
        if isinstance(obj, Type):
             instance_of = raw.get("instanceOf")
             if instance_of and isinstance(instance_of, list) and len(instance_of) > 1:
                 parent_name = instance_of[1]
                 if parent_name in registry:
                     p = registry[parent_name]
                     if isinstance(p, Type):
                         obj.parent = p
                         p.subtypes.add(obj)

        # Link Installations (for Method)
        if isinstance(obj, Method):
             installations = raw.get("installations")
             if installations:
                 for inst_sig in installations:
                     domain_names = inst_sig[1:]
                     domain_types = []
                     for tn in domain_names:
                         if tn in registry and isinstance(registry[tn], Type):
                             domain_types.append(registry[tn])
                     
                     if domain_types:
                         Installation(obj, domain_types, None, None, [], None, {})

    print("Pass 2 complete: Linked objects.")
    
    # Pass 3: Process Content
    for name, obj in registry.items():
        raw = raw_map[name]
        full_doc = raw.get("full_doc")
        headline = raw.get("headline", "")
        
        description_md = ""
        valid_examples = []
        additional_info_md = ""
        
        if full_doc:
            parsed = m2_parser.parse_m2_smart(full_doc)
            
            # Helper for links
            def resolver(target_name):
                if target_name in registry:
                    t = registry[target_name]
                    k = t.extra.get("kind") or t.__class__.__name__
                    if k == "Option": k = "Instance"
                    return f"../{k}/{t.safe_name}.mdx"
                return None
            
            # Extract Headline
            hl = m2_parser.extract_headline_from_tree(parsed)
            if hl: headline = hl
            
            # 1. Process Options Info
            if isinstance(obj, (Method, Function)):
                opt_info = m2_parser.extract_options_info(parsed)
                for opt_name, info in opt_info.items():
                    if opt_name in registry and isinstance(registry[opt_name], Option):
                        # Extract type from info
                        m = re.search(r'a (?:\[)?([a-zA-Z0-9_ ]+)(?:\])? value', info)
                        if m:
                            registry[opt_name].value_type = m.group(1).strip()
                        elif "reserved Symbol" in info:
                            m_sym = re.search(r'Symbol ([a-zA-Z0-9_]+)', info)
                            if m_sym:
                                registry[opt_name].value_type = m_sym.group(1)
                        
                        if not registry[opt_name].description:
                            registry[opt_name].description = info

            # 2. Extract Examples
            ex_blocks = m2_parser.extract_examples_from_tree(parsed)
            if ex_blocks:
                for block in ex_blocks:
                    calls = m2_parser.analyze_example_calls(block, obj.name)
                    if not calls:
                        continue
                    
                    formatted = m2_parser.parse_and_format_example(block)
                    valid_examples.append(formatted)

            # 3. Process Input/Output Info for Codomain
            if isinstance(obj, Method):
                inputs_raw, outputs_raw = m2_parser.extract_inputs_outputs(parsed)
                if inputs_raw and outputs_raw:
                    input_types = []
                    for li in inputs_raw:
                        t = m2_parser.extract_type_from_li(li)
                        if t: input_types.append(t)
                    
                    output_types = []
                    for li in outputs_raw:
                        t = m2_parser.extract_type_from_li(li)
                        if t: output_types.append(t)
                    
                    if input_types and output_types:
                        # Case 1: Inputs match a single installation's domain
                        for inst in obj.installations:
                            if [t.name for t in inst.domain] == input_types:
                                codom_name = output_types[0]
                                if codom_name in registry and isinstance(registry[codom_name], Type):
                                    inst.codomain = registry[codom_name]
                        
                        # Case 2: Inputs are specific cases (like exp)
                        if len(input_types) == len(output_types):
                            for i, in_t_name in enumerate(input_types):
                                for inst in obj.installations:
                                    if len(inst.domain) == 1 and inst.domain[0].name == in_t_name:
                                        codom_name = output_types[i]
                                        if codom_name in registry and isinstance(registry[codom_name], Type):
                                            inst.codomain = registry[codom_name]

            # 4. Description
            desc_tree = m2_parser.extract_description_body(parsed)
            if desc_tree:
                desc_tree = m2_parser.remove_examples_from_tree(desc_tree)
                desc_tree = m2_parser.remove_waystouse_from_tree(desc_tree)
                desc_tree = m2_parser.remove_usage_from_tree(desc_tree)
                if desc_tree and desc_tree.get('children'):
                     content_tree = {'tag': 'DIV', 'children': desc_tree['children'][1:]}
                     description_md = m2_parser.render_to_markdown(content_tree, resolver)
            
            if not description_md and headline:
                 description_md = headline
            
            # 4. Usage (added as first example)
            usage_str = m2_parser.extract_usage_from_tree(parsed)
            if usage_str:
                usage_block = f"```macaulay2\n{usage_str}\n```"
                valid_examples.insert(0, usage_block)
            
            # 5. Additional Info
            add_info_tree = m2_parser.collect_additional_info_tree(parsed)
            if add_info_tree:
                additional_info_md = m2_parser.render_to_markdown(add_info_tree, resolver)
                additional_info_md = additional_info_md.strip()
        
        if not description_md:
            description_md = raw.get("description", "")
            
        obj.description = description_md
        obj.examples = valid_examples
        obj.additional_info = additional_info_md
        obj.extra["parsed_headline"] = headline

    print("Pass 3 complete: Processed content.")
    return registry

if __name__ == "__main__":
    reg = load_data("docs/internal/raw_data.json")
    print(f"Loaded {len(reg)} objects.")
    options = [x for x in reg.values() if isinstance(x, Option) and x.value_type]
    print(f"Options with value type: {len(options)}")
