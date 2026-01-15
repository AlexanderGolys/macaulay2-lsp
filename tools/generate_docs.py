import json
import subprocess
import os
import re

def generate_mdx_content(item, all_items_dict, parent_map=None):
    """Generate MDX content for a single item.
    
    Args:
        item: Dictionary with item data
        all_items_dict: Dictionary mapping names to their safe names for linking
        parent_map: Dictionary mapping type names to their parent type names
    """
    name = item.get("name", "Unnamed")
    kind = item.get("kind", "Instance")
    safe_name = item.get("safeName", name)
    parent = item.get("parent")
    subtypes = item.get("subtypes", [])
    methods = item.get("methods", [])
    instances = item.get("instances", [])
    installations = item.get("installations", [])
    options = item.get("options", [])
    operator_attributes = item.get("operator_attributes", {})
    has_documentation = item.get("has_documentation", False)
    description = item.get("description", "")
    headline = item.get("headline", "")
    examples = item.get("examples", "")
    instance_of = item.get("instanceOf")
    used_in_methods = item.get("usedInMethods", [])
    used_in_functions = item.get("usedInFunctions", [])
    works_with_types = item.get("worksWithTypes", [])
    is_stub = item.get("stub", False)

    def get_link(target_name):
        """Generate a relative link to another doc page."""
        if target_name in all_items_dict:
            target_kind, target_safe = all_items_dict[target_name]
            return f"../{target_kind}/{target_safe}.mdx"
        # If not found, still generate a link (might be created later)
        return f"../{kind}/{target_name}.mdx"

    # Helper for type refinement
    def is_ancestor(ancestor, node):
        if not parent_map:
            return False
        curr = node
        # Simple cycle detection limit
        for _ in range(20):
            if curr not in parent_map:
                break
            curr = parent_map[curr]
            if curr == ancestor:
                return True
        return False

    # Refine instance_of if it's a list
    if isinstance(instance_of, list) and parent_map:
        # Filter out types that are ancestors of other types in the list
        refined = []
        for t1 in instance_of:
            is_redundant = False
            for t2 in instance_of:
                if t1 != t2 and is_ancestor(t1, t2):
                    is_redundant = True
                    break
            if not is_redundant:
                refined.append(t1)
        instance_of = refined
        
        # If reduced to 1, make it a string
        if len(instance_of) == 1:
            instance_of = instance_of[0]

    # Frontmatter
    frontmatter = f"""---
title: "{name}"
kind: {kind}
"""
    if parent:
        frontmatter += f"parent: {parent}\n"
    frontmatter += "---\n\n"

    # Main content
    content = f"# {name}\n\n"
    
    if headline:
        content += f"**{headline}**\n\n"
    
    # Add note if documentation is missing or if it's a stub
    if is_stub:
        content += ":::warning[Stub Documentation]\n"
        content += "This is a placeholder documentation entry. The full documentation for this instance needs to be extracted from Macaulay2.\n"
        content += ":::\n\n"
    elif not has_documentation:
        content += ":::warning[Missing Documentation]\n"
        content += "This documentation is automatically generated and currently lacks detailed information from the source. Please contribute!\n"
        content += ":::\n\n"
    
    # Type badge
    # Use the actual class (instanceOf) as the Type if available
    type_display = kind
    if instance_of:
        if isinstance(instance_of, list):
            type_refs = ", ".join([f"[{t}]({get_link(t)})" for t in instance_of])
            content += f"**Type:** {type_refs}\n\n"
        else:
            content += f"**Type:** [{instance_of}]({get_link(instance_of)})\n\n"
    else:
        content += f"**Type:** `{kind}`\n\n"
    
    # Instance of (bidirectional reference) - REMOVED as it is now merged into Type
    # if instance_of:
    #    ...

    # Description
    if description:
        content += f"## Description\n\n{description}\n\n"
    else:
        if instance_of and not isinstance(instance_of, list):
             content += f"## Description\n\nAn instance of the type [{instance_of}]({get_link(instance_of)}).\n\n"
        else:
             content += f"## Description\n\nA {kind} in the Macaulay2 system.\n\n"

    # Type-specific fields
    if kind == "Type":
            # Parent type (bidirectional with subtypes)
        if parent:
            content += f"## Parent Type\n\n[{parent}]({get_link(parent)})\n\n"
        
        # Subtypes (bidirectional with parent)
        if subtypes:
            content += f"## Subtypes\n\nDirect subtypes of this type:\n\n"
            for subtype_name in subtypes:
                content += f"- [{subtype_name}]({get_link(subtype_name)})\n"
            content += "\n"
        
        # Instances
        if instances:
            content += f"## Instances\n\nDirect instances of this type:\n\n"
            for inst_name in instances[:20]:  # Limit to first 20
                content += f"- `{inst_name}`\n"
            if len(instances) > 20:
                content += f"\n*... and {len(instances) - 20} more*\n"
            content += "\n"
        
        # Methods
        if methods:
            content += f"## Methods\n\nMethods that can be used with this type:\n\n"
            method_groups = {}
            for method_sig in methods[:50]:  # Limit display
                method_name = method_sig[0]
                if method_name not in method_groups:
                    method_groups[method_name] = []
                method_types = ", ".join(method_sig[1:])
                method_groups[method_name].append(method_types)
            
            for method_name, type_lists in sorted(method_groups.items())[:30]:
                content += f"### `{method_name}`\n\n"
                for types in type_lists[:5]:
                    content += f"- `{method_name}({types})`\n"
                if len(type_lists) > 5:
                    content += f"- *... and {len(type_lists) - 5} more signatures*\n"
                content += "\n"
            
            if len(methods) > 50:
                content += f"\n*Showing 50 of {len(methods)} methods*\n\n"
        
        # Used in methods (bidirectional reference)
        if used_in_methods:
            content += f"## Used In\n\nThis type is used in the following methods/operators:\n\n"
            for method_name in used_in_methods[:30]:
                content += f"- [{method_name}]({get_link(method_name)})\n"
            if len(used_in_methods) > 30:
                content += f"\n*... and {len(used_in_methods) - 30} more*\n"
            content += "\n"

    # Function/Method/Operator specific fields
    if kind in ["Function", "Method", "Operator"]:
        if usage:
            content += f"## Usage\n\n```macaulay2\n{usage}\n```\n\n"
            
        if options:
            content += f"## Options\n\n"
            for opt in options:
                content += f"- **`{opt['name']}`**: Default value `{opt['default']}`\n"
            content += "\n"
        
        if installations:
            # Group installations by type for operators
            if kind == "Operator":
                binary_insts = []
                unary_insts = []
                augmented_insts = []
                other_insts = []
                
                for inst_sig in installations:
                    inst_name = inst_sig[0]
                    if "," in inst_name and "=" in inst_name:
                        # Augmented assignment like (+,=)
                        augmented_insts.append(inst_sig)
                    elif len(inst_sig) == 3:
                        # Binary operator (name, type1, type2)
                        binary_insts.append(inst_sig)
                    elif len(inst_sig) == 2:
                        # Unary operator (name, type)
                        unary_insts.append(inst_sig)
                    else:
                        other_insts.append(inst_sig)
                
                content += f"## Operator Installations\n\n"
                
                if binary_insts:
                    content += f"### Binary Operator\n\n"
                    content += f"Usage: `x {name} y`\n\n"
                    for inst_sig in binary_insts[:20]:
                        inst_types = ", ".join(inst_sig[1:])
                        content += f"- `{name}({inst_types})`\n"
                    if len(binary_insts) > 20:
                        content += f"\n*Showing 20 of {len(binary_insts)} binary signatures*\n"
                    content += "\n"
                
                if unary_insts:
                    content += f"### Unary Operator\n\n"
                    content += f"Usage: `{name} x` or `x {name}`\n\n"
                    for inst_sig in unary_insts[:10]:
                        inst_types = ", ".join(inst_sig[1:])
                        content += f"- `{name}({inst_types})`\n"
                    if len(unary_insts) > 10:
                        content += f"\n*Showing 10 of {len(unary_insts)} unary signatures*\n"
                    content += "\n"
                
                if augmented_insts:
                    content += f"### Augmented Assignment\n\n"
                    # Extract the augmented operator name
                    aug_op = name + "="
                    content += f"Usage: `x {aug_op} y` (modifies `x` in place)\n\n"
                    content += ":::info[Special Installation Syntax]\n"
                    content += f"Augmented assignment methods require special installation using `installAssignmentMethod`. "
                    content += f"See [Macaulay2 documentation](https://macaulay2.com/doc/Macaulay2/share/doc/Macaulay2/Macaulay2Doc/html/_installing_spaugmented_spassignment_spmethods.html) for details.\n"
                    content += ":::\n\n"
                    for inst_sig in augmented_insts[:10]:
                        inst_name = inst_sig[0]
                        inst_types = ", ".join(inst_sig[1:])
                        content += f"- `{inst_name}({inst_types})`\n"
                    if len(augmented_insts) > 10:
                        content += f"\n*Showing 10 of {len(augmented_insts)} augmented assignment signatures*\n"
                    content += "\n"
            else:
                # Regular function/method installations
                content += f"## Installations\n\nType signatures for this {kind.lower()}:\n\n"
                for inst_sig in installations[:30]:  # Limit display
                    inst_name = inst_sig[0]
                    inst_types = ", ".join(inst_sig[1:])
                    content += f"- `{inst_name}({inst_types})`\n"
                if len(installations) > 30:
                    content += f"\n*... and {len(installations) - 30} more*\n"
                content += "\n"
        
        if operator_attributes:
            content += f"## Operator Attributes\n\n"
            for attr_name, attr_value in operator_attributes.items():
                content += f"- **{attr_name}**: `{attr_value}`\n"
            content += "\n"
        
        # Works with types (bidirectional reference)
        if works_with_types:
            content += f"## Works With Types\n\nThis {kind.lower()} works with the following types:\n\n"
            for type_name in works_with_types[:30]:
                content += f"- [{type_name}]({get_link(type_name)})\n"
            if len(works_with_types) > 30:
                content += f"\n*... and {len(works_with_types) - 30} more*\n"
            content += "\n"

    # Option specific fields
    if kind == "Option":
        if used_in_functions:
            content += f"## Used in Functions\n\nThis option is used in the following functions:\n\n"
            for func_name in used_in_functions[:30]:
                content += f"- [{func_name}]({get_link(func_name)})\n"
            if len(used_in_functions) > 30:
                content += f"\n*... and {len(used_in_functions) - 30} more*\n"
            content += "\n"

    # Examples section
    content += f"## Examples\n\n"
    if examples:
        content += f"```macaulay2\n{examples}\n```\n\n"
    elif has_documentation:
        content += "*Examples would be extracted from M2 documentation here*\n\n"
    else:
        content += "*No examples available yet. Please contribute!*\n\n"

    # See Also section
    content += f"## See Also\n\n"
    if parent:
        content += f"- [{parent}]({get_link(parent)}) (parent type)\n"
    
    # Full Documentation
    if item.get("full_doc"):
        content += "## Full Documentation\n\n"
        content += "```macaulay2\n"
        content += str(item["full_doc"]) + "\n"
        content += "```\n\n"

    return frontmatter + content

def generate_option_table(data, all_items_dict):
    """Generate a single MDX file listing all Option Keys and Values."""
    keys = []
    values = []
    
    for item in data:
        if item.get("kind") == "Option":
            otype = item.get("optionType", "Unknown")
            if otype == "Key":
                keys.append(item)
            else:
                values.append(item)
    
    keys.sort(key=lambda x: x["name"])
    values.sort(key=lambda x: x.get("relatedKey", "") + x["name"])
    
    content = """---
title: Option Keywords
---

# Option Keywords

This page lists reserved keywords used as options (Keys) and their possible values.

## Option Keys

| Name | Used in Functions |
|---|---|
"""
    
    def get_link_md(name):
        if name in all_items_dict:
            k, s = all_items_dict[name]
            return f"[{name}](../{k}/{s}.mdx)"
        return name

    # Keys table
    for item in keys:
        name = item["name"]
        used_in = item.get("usedInFunctions", [])
        
        name_cell = f"`{name}`"
        
        # Used In cell
        used_links = []
        for func in used_in[:10]:
            used_links.append(get_link_md(func))
        if len(used_in) > 10:
            used_links.append(f"and {len(used_in)-10} more...")
        
        used_cell = ", ".join(used_links) if used_links else "-"
        
        content += f"| {name_cell} | {used_cell} |\n"
        
    content += """

## Option Values

| Name | For Key |
|---|---|
"""

    # Values table
    for item in values:
        name = item["name"]
        key = item.get("relatedKey", "-")
        
        name_cell = f"`{name}`"
        key_cell = f"`{key}`"
        
        content += f"| {name_cell} | {key_cell} |\n"
        
    # Write file
    path = "docs/reference/OptionKeywords.mdx"
    with open(path, "w") as f:
        f.write(content)
    print(f"Generated {path}")

def main():
    # Create directories
    os.makedirs("docs/internal", exist_ok=True)
    os.makedirs("docs/reference/Type", exist_ok=True)
    os.makedirs("docs/reference/Function", exist_ok=True)
    os.makedirs("docs/reference/Method", exist_ok=True)
    os.makedirs("docs/reference/Operator", exist_ok=True)
    os.makedirs("docs/reference/Option", exist_ok=True)
    os.makedirs("docs/reference/Instance", exist_ok=True)

    # Try to use existing data first, otherwise run extraction
    print("Checking for existing data...")
    data = None
    try:
        # Prefer the internal data file
        data_path = "docs/internal/raw_data.json"
        if not os.path.exists(data_path):
            data_path = "raw_data.json"
            
        with open(data_path, "r") as f:
            data = json.load(f)
        print(f"Using existing {data_path} with {len(data)} items.")
    except FileNotFoundError:
        print("No existing data found. Running M2 extraction script...")
        try:
            result = subprocess.run(
                ["M2", "--script", "tools/extract_docs.m2"],
                capture_output=True,
                text=True,
                check=True
            )
            raw_data = result.stdout
            print(f"Extraction stderr:\n{result.stderr}")
            
            with open("docs/internal/raw_data.json", "w") as f:
                f.write(raw_data)
            
            data = json.loads(raw_data)
            print(f"Loaded {len(data)} items from extraction.")
        except subprocess.CalledProcessError as e:
            print(f"Error running M2 script: {e}")
            print(f"Stdout: {e.stdout}")
            print(f"Stderr: {e.stderr}")
            return
        except json.JSONDecodeError as e:
            print(f"Error decoding JSON from M2 script output: {e}")
            print(f"Raw output (first 1000 chars): {raw_data[:1000]}")
            return

    # Build lookup dictionary for linking
    all_items_dict = {}
    parent_map = {}
    for item in data:
        name = item.get("name")
        kind = item.get("kind")
        safe_name = item.get("safeName")
        if name and kind and safe_name:
            all_items_dict[name] = (kind, safe_name)
        
        # Build parent map
        if kind == "Type" and "parent" in item and item["parent"]:
            parent_map[name] = item["parent"]

    print(f"Generating docs for {len(data)} items...")
    generated_count = 0
    for item in data:
        kind = item.get("kind", "Instance")
        name = item.get("name")
        safe_name = item.get("safeName")

        if not name or not safe_name:
            print(f"Skipping item due to missing name or safeName: {item}")
            continue
            
        # Skip Options (generated in table)
        if kind == "Option":
            continue

        # Determine output directory based on kind
        output_dir = f"docs/reference/{kind}"
        
        # Ensure the directory exists
        os.makedirs(output_dir, exist_ok=True)

        # Generate MDX content
        try:
            mdx_content = generate_mdx_content(item, all_items_dict, parent_map)

            # Write to file
            output_path = os.path.join(output_dir, f"{safe_name}.mdx")
            with open(output_path, "w") as f:
                f.write(mdx_content)
            generated_count += 1
        except Exception as e:
            print(f"Error generating docs for {name}: {e}")
            continue

    # Generate Option Table
    generate_option_table(data, all_items_dict)

    print(f"Done. Generated {generated_count} documentation files.")

if __name__ == "__main__":
    main()
