import os
import sys
import json
import re
from typing import List, Dict

# Ensure tools in path
sys.path.append(os.path.dirname(__file__))

import docs_loader
from structure_docs import Instance, Type, Method, Function, Option, Installation

DOCS_DIR = "docs/reference"

def get_kind(obj: Instance) -> str:
    # Use class name as kind, but handle special cases
    if obj.extra.get("kind") == "Operator":
        return "Operator"
    return obj.__class__.__name__

def get_safe_filename(name: str) -> str:
    if not name: return "empty"
    import re
    # M2 identifiers can contain letters and ' but not _
    if re.match(r"^[a-zA-Z][a-zA-Z0-9']*$", name):
        return name.replace("'", "prime")
    
    mapping = {
        "*": "star", "+": "plus", "-": "minus", "/": "slash", "%": "percent",
        "^": "caret", "!": "bang", "?": "question", "=": "eq", "<": "lt",
        ">": "gt", "|": "bar", "&": "amp", "@": "at", "#": "hash",
        "$": "dollar", "~": "tilde", "\\": "bs", ":": "colon", ";": "semicolon",
        ",": "comma", ".": "dot", "_": "underscore", "'": "prime"
    }
    
    res = "op_"
    for char in name:
        if char in mapping:
            res += mapping[char] + "_"
        elif char.isalnum():
            res += char
        else:
            res += f"x{ord(char)}_"
    
    return res.rstrip("_")

def get_output_path(obj: Instance) -> str:
    kind = get_kind(obj)
    safe_name = get_safe_filename(obj.name)
    
    # Map kind to folder
    folder = kind
    if kind == "Option": folder = "Option"
    elif kind == "Instance": folder = "Instance"
    
    return os.path.join(DOCS_DIR, folder, f"{safe_name}.mdx")

def get_link(obj: Instance) -> str:
    kind = get_kind(obj)
    folder = kind
    safe_name = get_safe_filename(obj.name)
    return f"[{obj.name}](../{folder}/{safe_name}.mdx)"

def generate_mdx(obj: Instance, registry: Dict[str, Instance]) -> str:
    # Frontmatter
    kind = get_kind(obj)
    content = f"---\ntitle: \"{obj.name}\"\nkind: {kind}\n---\n\n"
    
    # Headline
    headline = obj.extra.get("parsed_headline", "")
    content += f"# {obj.name}\n\n"
    if headline:
        content += f"**{headline}**\n\n"
    
    # Type Info
    if obj.type:
        type_link = get_link(obj.type)
        content += f"**Type:** {type_link}\n\n"
    
    # Description
    if obj.description:
        content += f"## Description\n\n{obj.description}\n\n"
    
    # Installations (for Methods)
    if isinstance(obj, Method) and obj.installations:
        content += f"## Installed For\n\n"
        sorted_insts = sorted(obj.installations, key=lambda x: str(x))
        
        limit = 30
        for inst in sorted_insts[:limit]:
            sig_types = []
            for t in inst.domain:
                 sig_types.append(get_link(t))
            sig_str = ", ".join(sig_types)
            header = f"### {obj.name}({sig_str})"
            if inst.codomain:
                header += f" -> {get_link(inst.codomain)}"
            content += f"{header}\n\n"
            
            if inst.examples:
                content += "\n".join(inst.examples) + "\n\n"
        
        if len(sorted_insts) > limit:
            content += f"\n*... and {len(sorted_insts) - limit} more*\n\n"
            
    # Options (for Methods/Functions)
    if isinstance(obj, (Method, Function)) and obj.options:
        option_lines = []
        options = obj.options
        if isinstance(options, dict):
            for opt_name, opt_val in options.items():
                if opt_name in registry:
                    opt_obj = registry[opt_name]
                    link = get_link(opt_obj)
                    val_type = ""
                    if isinstance(opt_obj, Option) and opt_obj.value_type:
                        vt = opt_obj.value_type
                        if vt in registry:
                            val_type = f" (type: {get_link(registry[vt])})"
                        else:
                            val_type = f" (type: {vt})"
                    option_lines.append(f"- {link}{val_type} => {opt_val}")
                else:
                    option_lines.append(f"- {opt_name} => {opt_val}")
        elif isinstance(options, list):
            for opt in options:
                if isinstance(opt, dict) and "name" in opt:
                    name = opt["name"]
                    val = opt.get("default", "null")
                    if name in registry:
                        opt_obj = registry[name]
                        link = get_link(opt_obj)
                        val_type = ""
                        if isinstance(opt_obj, Option) and opt_obj.value_type:
                            vt = opt_obj.value_type
                            if vt in registry:
                                val_type = f" (type: {get_link(registry[vt])})"
                            else:
                                val_type = f" (type: {vt})"
                        option_lines.append(f"- {link}{val_type} => {val}")
                    else:
                        option_lines.append(f"- {name} => {val}")
                elif isinstance(opt, str):
                    if opt in registry:
                        link = get_link(registry[opt])
                        option_lines.append(f"- {link}")
                    else:
                        option_lines.append(f"- {opt}")

        if option_lines:
            content += f"## Options\n\n"
            content += "\n".join(option_lines) + "\n\n"

    # Examples
    if obj.examples:
        content += f"## Examples\n\n"
        content += "\n\n".join(obj.examples) + "\n\n"
        
    # Additional Info
    if obj.additional_info:
        content += f"## Additional Information\n\n{obj.additional_info}\n\n"

    # See Also
    see_also = obj.extra.get("seeAlso")
    if see_also and isinstance(see_also, list):
         content += f"## See Also\n\n"
         for s in see_also:
             name = s.split("::")[-1]
             if name in registry:
                 link = get_link(registry[name])
                 content += f"- {link}\n"
             else:
                 content += f"- {s}\n"
         content += "\n"
    
    # Debug Info
    content += f"## Debug info\n\n```python\n"
    debug_data = {
        "obj_type": obj.__class__.__name__,
        "name": obj.name,
        "safe_name": obj.safe_name,
        "type": obj.type.name if obj.type else None,
        "description": obj.description[:100] + "..." if obj.description else "",
    }
    if isinstance(obj, Type):
        debug_data["parent"] = obj.parent.name if obj.parent else None
        debug_data["subtypes"] = [s.name for s in obj.subtypes]
    if isinstance(obj, Method):
        debug_data["installations"] = [str(inst) for inst in obj.installations]
    if isinstance(obj, (Method, Function)):
        debug_data["options"] = obj.options
    if isinstance(obj, Option):
        debug_data["value_type"] = obj.value_type
    
    extra_debug = obj.extra.copy()
    if "full_doc" in extra_debug:
        extra_debug["full_doc"] = f"... ({len(extra_debug['full_doc'])} bytes) ..."
    debug_data["extra"] = extra_debug
    
    content += json.dumps(debug_data, indent=4)
    content += "\n```\n"
        
    return content

def generate_consolidated_options(registry: Dict[str, Instance]) -> str:
    content = "---\ntitle: Option Keywords\n---\n\n# Option Keywords\n\n"
    content += "This page lists reserved keywords used as options (Keys), their possible values (Values), and other protected symbols.\n\n"
    
    keys = [obj for obj in registry.values() if isinstance(obj, Option) and obj.extra.get("is_option_key")]
    values = [obj for obj in registry.values() if isinstance(obj, Option) and obj.extra.get("is_option_value")]
    others = [obj for obj in registry.values() if isinstance(obj, Option) and obj.extra.get("is_protected_symbol")]
    
    keys.sort(key=lambda x: x.name)
    values.sort(key=lambda x: x.name)
    others.sort(key=lambda x: x.name)
    
    content += "## Option Keys\n\n"
    content += "| Name | Description | Used in Functions |\n"
    content += "|---|---|---|\n"
    
    # Map from option name to list of functions using it
    opt_to_funcs = {}
    for obj in registry.values():
        if isinstance(obj, (Function, Method)) and obj.options:
            if isinstance(obj.options, list):
                for opt in obj.options:
                    if isinstance(opt, dict) and "name" in opt:
                        opt_name = opt["name"]
                        if opt_name not in opt_to_funcs:
                            opt_to_funcs[opt_name] = []
                        opt_to_funcs[opt_name].append(obj)
            elif isinstance(obj.options, dict):
                for opt_name in obj.options.keys():
                    if opt_name not in opt_to_funcs:
                        opt_to_funcs[opt_name] = []
                    opt_to_funcs[opt_name].append(obj)
    
    for obj in keys:
        funcs = opt_to_funcs.get(obj.name, [])
        funcs_str = ", ".join([get_link(f) for f in funcs[:10]]) # Limit to 10
        if len(funcs) > 10: funcs_str += "..."
        desc = obj.description.replace("\n", " ") if obj.description else "No description"
        content += f"| {obj.name} | {desc} | {funcs_str} |\n"
        
    content += "\n## Option Values\n\n"
    content += "| Name | Description | For Key |\n"
    content += "|---|---|---|\n"
    
    for obj in values:
        # Try to find which key it's for from full_doc
        related_key = "Unknown"
        match = re.search(r"for , TO\{[^:]+ :: ([^}]+)\}", obj.extra.get("full_doc", ""))
        if match:
            related_key = match.group(1)
            # Try to link if in registry
            if related_key in registry:
                related_key = get_link(registry[related_key])
        
        desc = obj.description.replace("\n", " ") if obj.description else "No description"
        content += f"| {obj.name} | {desc} | {related_key} |\n"
        
    content += "\n## Other Protected Symbols\n\n"
    content += "| Name | Description | Type |\n"
    content += "|---|---|---|\n"
    
    for obj in others:
        desc = obj.description.replace("\n", " ") if obj.description else "No description"
        type_str = obj.type.name if obj.type else "Symbol"
        content += f"| {obj.name} | {desc} | {type_str} |\n"
        
    return content

def main():
    print("Loading data...")
    registry = docs_loader.load_data("docs/internal/raw_data.json")
    print("Generating MDX...")
    
    generated_paths = set()
    count = 0
    for obj in registry.values():
        # Skip individual Option files if they are consolidated
        if isinstance(obj, Option):
            continue
            
        mdx = generate_mdx(obj, registry)
        path = get_output_path(obj)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, 'w') as f:
            f.write(mdx)
        generated_paths.add(os.path.abspath(path))
        count += 1
    
    # Generate consolidated options
    options_mdx = generate_consolidated_options(registry)
    options_path = os.path.join(DOCS_DIR, "OptionKeywords.mdx")
    with open(options_path, 'w') as f:
        f.write(options_mdx)
    generated_paths.add(os.path.abspath(options_path))
    
    print(f"Generated {count} files + consolidated options.")
    
    # Cleanup stale files
    print("Cleaning up stale files...")
    removed = 0
    for root, dirs, files in os.walk(DOCS_DIR):
        for file in files:
            if file.endswith(".mdx"):
                full_path = os.path.abspath(os.path.join(root, file))
                if full_path not in generated_paths:
                    rel_dir = os.path.relpath(root, DOCS_DIR)
                    if rel_dir == ".": continue
                    os.remove(full_path)
                    removed += 1
    print(f"Removed {removed} stale files.")

if __name__ == "__main__":
    main()
