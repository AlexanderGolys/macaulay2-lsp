import re

def clean_text(text):
    if not text: return ""
    text = re.sub(r' +', ' ', text)
    text = re.sub(r'\s+([.,:;])', r'\1', text)
    return text.strip()

def parse_m2_smart(text):
    if not isinstance(text, str): return ""
    if not text: return ""
    
    match = re.match(r'^\s*([a-zA-Z0-9_]+)\s*\{(.*)\}\s*$', text, re.DOTALL)
    if match:
        tag = match.group(1)
        content = match.group(2)
        children = []
        depth = 0
        start = 0
        for i, char in enumerate(content):
            if char == '{': depth += 1
            elif char == '}': depth -= 1
            elif char == ',' and depth == 0:
                chunk = content[start:i]
                children.append(parse_m2_smart(chunk))
                start = i + 1
        chunk = content[start:]
        if chunk.strip(): children.append(parse_m2_smart(chunk))
        return {'tag': tag, 'children': children}
    if '=>' in text:
        parts = text.split('=>', 1)
        return {'key': parts[0].strip(), 'value': parse_m2_smart(parts[1])}
    return text

def render_text_only(tree):
    if isinstance(tree, str): return tree
    if isinstance(tree, dict):
        if 'tag' in tree:
            return "".join(render_text_only(c) for c in tree['children'])
        if 'key' in tree:
            return "" 
    return ""

def extract_headline_from_tree(tree):
    if isinstance(tree, dict):
        if tree.get('tag') == 'HEADER1':
            return render_text_only(tree).strip()
        if 'children' in tree:
            for child in tree['children']:
                res = extract_headline_from_tree(child)
                if res: return res
    return None

def extract_description_body(tree):
    if isinstance(tree, dict) and 'children' in tree:
        if tree.get('tag') == 'DIV':
            if tree['children']:
                c0 = tree['children'][0]
                if isinstance(c0, dict) and c0.get('tag') == 'HEADER2':
                    txt = render_text_only(c0)
                    if "Description" in txt:
                        return tree
        for child in tree['children']:
            res = extract_description_body(child)
            if res: return res
    return None

def extract_examples_from_tree(tree):
    blocks = []
    def traverse(node):
        if isinstance(node, dict) and 'children' in node:
            if node.get('tag') == 'TABLE':
                is_ex = False
                for child in node['children']:
                    if isinstance(child, dict) and child.get('key') == 'class':
                         val = render_text_only(child['value']).strip()
                         if val == 'examples': is_ex = True
                if is_ex:
                    for child in node['children']:
                         if isinstance(child, dict) and child.get('tag') == 'TR':
                             for td in child.get('children', []):
                                 if isinstance(td, dict) and td.get('tag') == 'TD':
                                     for pre in td.get('children', []):
                                          if isinstance(pre, dict) and pre.get('tag') == 'PRE':
                                              text = render_text_only(pre).strip()
                                              if text:
                                                  blocks.append(text)
                    return 
            for child in node['children']:
                traverse(child)
    traverse(tree)
    return blocks

def remove_examples_from_tree(tree):
    if isinstance(tree, dict):
        if tree.get('tag') == 'TABLE':
             is_ex = False
             for child in tree.get('children', []):
                 if isinstance(child, dict) and child.get('key') == 'class':
                      val = render_text_only(child['value']).strip()
                      if val == 'examples': is_ex = True
             if is_ex: return None
        
        if 'children' in tree:
            new_children = []
            for c in tree['children']:
                res = remove_examples_from_tree(c)
                if res: new_children.append(res)
            tree['children'] = new_children
            return tree
    return tree

def remove_waystouse_from_tree(tree):
    if isinstance(tree, dict):
        if tree.get('tag') == 'DIV':
             is_way = False
             for child in tree.get('children', []):
                 if isinstance(child, dict) and child.get('key') == 'class':
                      val = render_text_only(child['value']).strip()
                      if val == 'waystouse': is_way = True
             if is_way: return None
        
        if 'children' in tree:
            new_children = []
            for c in tree['children']:
                res = remove_waystouse_from_tree(c)
                if res: new_children.append(res)
            tree['children'] = new_children
            return tree
    return tree

def extract_usage_from_tree(tree):
    if isinstance(tree, dict) and 'children' in tree:
        if tree.get('tag') == 'DL':
            usages = []
            capturing = False
            for child in tree['children']:
                tag = child.get('tag')
                if tag == 'DT':
                    text = render_text_only(child)
                    if "Usage" in text:
                        capturing = True
                    else:
                        capturing = False
                elif tag == 'DD' and capturing:
                    usages.append(render_text_only(child).strip())
            if usages:
                return "\n".join(usages)
        for child in tree['children']:
            res = extract_usage_from_tree(child)
            if res: return res
    return None

def remove_usage_from_tree(tree):
    if isinstance(tree, dict):
        if tree.get('tag') == 'DL':
            new_children = []
            skip_dds = False
            for child in tree.get('children', []):
                tag = child.get('tag')
                if tag == 'DT':
                    text = render_text_only(child)
                    if "Usage" in text:
                        skip_dds = True
                        continue
                    else:
                        skip_dds = False
                
                if tag == 'DD' and skip_dds:
                    continue
                
                res = remove_usage_from_tree(child)
                if res: new_children.append(res)
            tree['children'] = new_children
            if not new_children: return None
            return tree
        if 'children' in tree:
            new_children = []
            for c in tree['children']:
                res = remove_usage_from_tree(c)
                if res: new_children.append(res)
            tree['children'] = new_children
            return tree
    return tree

def collect_additional_info_tree(tree):
    if not isinstance(tree, dict) or 'children' not in tree: return None
    
    sections = []
    for child in tree['children']:
        if child.get('tag') == 'HEADER1': continue
        
        if child.get('tag') == 'DIV' and child.get('children'):
             c0 = child['children'][0]
             if isinstance(c0, dict) and c0.get('tag') == 'HEADER2':
                 txt = render_text_only(c0)
                 if "Description" in txt: continue

        pruned = remove_examples_from_tree(child)
        if not pruned: continue
        
        pruned = remove_waystouse_from_tree(pruned)
        if not pruned: continue

        pruned = remove_usage_from_tree(pruned)
        if not pruned: continue
        
        sections.append(pruned)
    
    if sections:
        return {'tag': 'DIV', 'children': sections}
    return None

def parse_and_format_example(code):
    lines = code.split('\n')
    result = []
    re_in = re.compile(r'^i(\d+)\s*:\s*(.*)$')
    re_out = re.compile(r'^o(\d+)\s*[=:]\s*(.*)$')
    current_type = None 
    current_lines = []
    suppress_output = False
    
    def flush():
        nonlocal current_type, current_lines
        if not current_lines: return
        
        if current_type == 'output':
            is_doc_output = any("**********" in line for line in current_lines)
            if is_doc_output or len(current_lines) > 15:
                limit = 10 if is_doc_output else 15
                if len(current_lines) > limit:
                     current_lines = current_lines[:limit] + ["...", "   (output truncated)"]

        content = "\n".join(current_lines)
        if not content.strip(): current_lines = [] ; return
        
        if current_type == 'input':
            result.append(f"```macaulay2\n{content}\n```")
        elif current_type == 'output':
            result.append(f"```text\n{content}\n```")
        else:
            result.append(content)
        current_lines = []

    for line in lines:
        m_in = re_in.match(line)
        m_out = re_out.match(line)
        
        if m_in:
            flush()
            current_type = 'input'
            cmd = m_in.group(2)
            current_lines = [cmd]
            if cmd.startswith("help ") or cmd.startswith("viewHelp "):
                 suppress_output = True
            else:
                 suppress_output = False
                 
        elif m_out:
            if suppress_output: continue
            if current_type == 'output':
                current_lines.append(m_out.group(2))
            else:
                flush()
                current_type = 'output'
                current_lines = [m_out.group(2)]
        else:
            if suppress_output: continue
            if current_type:
                current_lines.append(line)
            else:
                if line.strip():
                    current_type = 'text'
                    current_lines = [line]
    flush()
    return "\n\n".join(result)

def render_to_markdown(tree, link_resolver=None):
    if isinstance(tree, str): return tree
    if isinstance(tree, dict):
        if 'tag' in tree:
            tag = tree['tag']
            children = [render_to_markdown(c, link_resolver) for c in tree['children']]
            content = "".join(children)
            
            if tag in ['HEADER1']: return "" 
            if tag == 'HEADER2': return f"\n### {clean_text(content)}\n\n"
            if tag == 'HEADER3': return f"\n#### {clean_text(content)}\n\n"
            if tag == 'PAR': return f"\n{clean_text(content)}\n\n"
            if tag == 'UL': return f"\n{content}\n"
            if tag == 'LI':
                c = clean_text(content)
                if not c: return ""
                return f"- {c}\n"
            if tag == 'DL': return f"\n{content}\n"
            if tag == 'DT': return f"\n**{clean_text(content)}** "
            if tag == 'DD':
                c = content.strip()
                if c.startswith(":"): c = c[1:].strip()
                return f"{clean_text(c)}\n"
            if tag == 'TT': return f"`{content}`"
            if tag == 'CODE': return f"`{content}`"
            if tag == 'PRE': return f"\n```\n{content}\n```\n"
            if tag in ['TO', 'TO2']:
                target = ""
                text = content
                if tree['children']:
                    c0 = tree['children'][0]
                    if isinstance(c0, str):
                        target = c0
                        if "::" in target: target = target.split("::")[-1].strip()
                
                if tag == 'TO2' and len(tree['children']) > 1:
                     text = render_to_markdown(tree['children'][1], link_resolver)
                else:
                     text = target if target else text
                
                if link_resolver:
                    link = link_resolver(target)
                    if link:
                        return f"[{text}]({link})"
                return f"[{text}](../Instance/{target}.mdx)" 
            
            if tag == 'DIV': return content
            if tag == 'SPAN': return content
            if tag == 'EM': return f"*{content}*"
            if tag == 'BF': return f"**{content}**"
            if tag == 'SUB': return f"_{{{content}}}"
            if tag == 'SUP': return f"^{{{content}}}"
            if tag == 'TABLE': return content 
            if tag == 'TR': return content
            if tag == 'TD': return content
            
            return content
        if 'key' in tree: return ""
    return ""

def extract_options_info(tree):
    """Find 'Optional inputs' DL section and extract type info."""
    results = {}
    def traverse(node):
        if not isinstance(node, dict): return
        if node.get('tag') == 'DL':
            for i, child in enumerate(node.get('children', [])):
                if child.get('tag') == 'DT':
                    name = render_text_only(child).strip().strip("`")
                    if i + 1 < len(node['children']):
                        dd = node['children'][i+1]
                        if dd.get('tag') == 'DD':
                            info = render_text_only(dd).strip()
                            if "default value" in info:
                                results[name] = info
        for c in node.get('children', []):
            traverse(c)
    traverse(tree)
    return results

def extract_inputs_outputs(tree):
    """Find 'Inputs' and 'Outputs' lists and return their raw structures."""
    inputs = []
    outputs = []
    
    def traverse(node):
        nonlocal inputs, outputs
        if not isinstance(node, dict) or 'children' not in node: return
        
        # Look for Inputs: or Outputs: in an LI
        if node.get('tag') == 'LI':
            text = render_text_only(node)
            if "Inputs:" in text:
                for child in node.get('children', []):
                    if isinstance(child, dict) and child.get('tag') == 'UL':
                        inputs = child.get('children', [])
            elif "Outputs:" in text:
                for child in node.get('children', []):
                    if isinstance(child, dict) and child.get('tag') == 'UL':
                        outputs = child.get('children', [])
        
        for c in node.get('children', []):
            traverse(c)
            
    traverse(tree)
    return inputs, outputs

def extract_type_from_li(li_node):
    """Extract the first type reference (TO/TO2) from an LI node."""
    def find_type(node):
        if not isinstance(node, dict): return None
        if node.get('tag') in ['TO', 'TO2']:
            if node.get('children'):
                c0 = node['children'][0]
                if isinstance(c0, str):
                    target = c0
                    if "::" in target: target = target.split("::")[-1].strip()
                    return target
        for c in node.get('children', []):
            res = find_type(c)
            if res: return res
        return None
    return find_type(li_node)

def analyze_example_calls(code, method_name):
    """Find calls to method_name in example code and extract return type if present."""
    lines = code.split('\n')
    calls = []
    current_call = None
    
    re_in = re.compile(r'^i\d+\s*:\s*(.*)$')
    re_out_type = re.compile(r'^o\d+\s*:\s*([a-zA-Z0-9_]+)')
    
    for line in lines:
        m_in = re_in.match(line)
        if m_in:
            cmd = m_in.group(1)
            # Match method_name(args) or method_name arg
            if re.search(r'\b' + re.escape(method_name) + r'\b', cmd):
                current_call = {'code': cmd, 'res_type': None}
                calls.append(current_call)
        
        m_out = re_out_type.match(line)
        if m_out and current_call:
            current_call['res_type'] = m_out.group(1)
            
    return calls
