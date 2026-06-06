#!/usr/bin/env python3
"""Generate first-pass Obsidian notes from macaulay2-lsp builtin metadata."""

from __future__ import annotations

import argparse
import html
import json
import re
import shutil
from pathlib import Path
from typing import Any


OPERATOR_WORDS = {
    "!": "bang",
    "#": "hash",
    "%": "percent",
    "&": "amp",
    "*": "star",
    "+": "plus",
    "-": "minus",
    ".": "dot",
    "/": "slash",
    ":": "colon",
    "<": "less",
    "=": "equal",
    ">": "greater",
    "?": "question",
    "@": "at",
    "^": "caret",
    "|": "bar",
    "~": "tilde",
    "_": "underscore",
    "(": "lparen",
    ")": "rparen",
    "[": "lbracket",
    "]": "rbracket",
    "{": "lbrace",
    "}": "rbrace",
    ",": "comma",
    ";": "semicolon",
    "$": "dollar",
    "\\": "backslash",
}

NOTE_LINKS: dict[str, str] = {}
ALL_NAMES: set[str] = set()

CATEGORY_TITLES = {
    "classes": "Classes",
    "functions": "Functions",
    "key-symbols": "Key Symbols",
    "key-values": "Key Values",
    "keywords": "Keywords",
    "objects": "Objects",
}

CANONICAL_ALIASES = {
    r"\mathbb C": "CC",
    r"\mathbb Q": "QQ",
    r"\mathbb R": "RR",
    r"\mathbb Z": "ZZ",
    "ℂ": "CC",
    "ℚ": "QQ",
    "ℝ": "RR",
    "ℤ": "ZZ",
    "CC'": "CC",
    "RR'": "RR",
    "RRi'": "RRi",
    "←": "<-",
    "→": "->",
    "⇒": "=>",
    "∀": "all",
    "∃": "any",
    "∈": "isMember",
    "∏": "product",
    "∑": "sum",
    "√": "sqrt",
    "∞": "infinity",
    "∧": "and",
    "∨": "or",
    "∫": "integrate",
    "≠": "!=",
    "≤": "<=",
    "≥": ">=",
    "≪": "<<",
    "≫": ">>",
    "⊕": "++",
    "⊗": "**",
    "⊢": "|-",
}


def yaml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def yaml_list(values: list[str]) -> list[str]:
    return [f"  - {yaml_string(value)}" for value in values]


def link(name: str | None) -> str:
    if not name:
        return ""
    display = canonical_name(name, ALL_NAMES) if ALL_NAMES else name
    target = NOTE_LINKS.get(name) or NOTE_LINKS.get(display)
    if target:
        slug = target.rsplit("/", 1)[-1]
        if slug != display:
            return f"[[{target}|{display}]]"
    return f"[[{display}]]"


def unique_names(names: list[str]) -> list[str]:
    seen = set()
    out = []
    for name in names:
        canonical = canonical_name(name, ALL_NAMES) if ALL_NAMES else name
        if canonical in seen:
            continue
        seen.add(canonical)
        out.append(canonical)
    return out


def plain_doc(value: Any) -> str:
    if not isinstance(value, dict):
        return ""
    text = value.get("net") or value.get("string") or ""
    text = str(text).strip()
    if text.startswith("{") and text.endswith("}"):
        text = text[1:-1].strip()
    text = text.replace('\\"', '"')
    text = re.sub(r"\s+", " ", text)
    return html.unescape(text)


def description(record: dict[str, Any]) -> str:
    doc = record.get("documentation") or {}
    text = plain_doc(doc.get("upstream_description_body"))
    if text:
        return text
    return ""


def usage(record: dict[str, Any]) -> str:
    doc = record.get("documentation") or {}
    long = record.get("description_long") or doc.get("upstream_description_long") or ""
    codes = re.findall(r"<code[^>]*>(.*?)</code>", str(long), re.S)
    return "\n".join(html.unescape(strip_tags(code)).strip() for code in codes if strip_tags(code).strip())


def html_to_markdownish(text: str) -> str:
    text = re.sub(r"<br\s*/?>", "\n", text)
    text = re.sub(r"</(p|div|dd|dt|dl|li|tr|table)>", "\n", text)
    text = re.sub(r"<code[^>]*>(.*?)</code>", lambda m: f"`{html.unescape(strip_tags(m.group(1))).strip()}`", text, flags=re.S)
    text = strip_tags(text)
    text = html.unescape(text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def strip_tags(text: str) -> str:
    return re.sub(r"<[^>]+>", "", text)


def library_name(record: dict[str, Any]) -> str:
    name = record.get("name") or ""
    if "$" in name:
        return name.split("$", 1)[0]
    return record.get("library") or "Core"


def category(record: dict[str, Any]) -> str:
    if record.get("type_info") is not None:
        return "classes"
    roles = option_role(record)
    if "key" in roles:
        return "key-symbols"
    if "value" in roles:
        return "key-values"
    if record.get("operator_info") is not None or record.get("data_type") == "Keyword":
        return "keywords"
    if record.get("function_info") is not None:
        return "functions"
    return "objects"


def kind_for_category(category_name: str) -> str:
    if category_name == "classes":
        return "class"
    if category_name == "functions":
        return "function"
    if category_name == "key-symbols":
        return "key-symbol"
    if category_name == "key-values":
        return "key-value"
    if category_name == "keywords":
        return "keyword"
    return "object"


def slug_for_name(name: str, category_name: str) -> str:
    if category_name == "keywords" and any(ch in OPERATOR_WORDS for ch in name):
        if "$" in name:
            prefix, op = name.split("$", 1)
            suffix = magic_operator_slug(op) if any(ch in OPERATOR_WORDS for ch in op) else slug_for_name(op, "objects")
            return f"{slug_for_name(prefix, 'objects')}--{suffix}"
        return magic_operator_slug(name)

    out = []
    for ch in name:
        if ch.isalnum() or ch in {"_", "-"}:
            out.append(ch)
        elif ch == "$":
            out.append("--")
        elif ch == "'":
            out.append("-prime")
        else:
            out.append(f"_x{ord(ch):02x}_")
    slug = "".join(out).strip(". ")
    return slug or "_empty_"


def magic_operator_slug(name: str) -> str:
    parts = []
    alnum = []
    for ch in name:
        if ch.isalnum():
            alnum.append(ch)
            continue
        if alnum:
            parts.append("".join(alnum))
            alnum = []
        parts.append(OPERATOR_WORDS.get(ch, f"x{ord(ch):02x}"))
    if alnum:
        parts.append("".join(alnum))
    return "__" + "_".join(part for part in parts if part) + "__"


def option_role(record: dict[str, Any]) -> list[str]:
    needles = []
    for value in [
        record.get("description_short"),
        ((record.get("documentation") or {}).get("upstream_description_short")),
    ]:
        if isinstance(value, str):
            needles.append(value)
    text = "\n".join(needles).lower()
    roles = []
    if "option value" in text or "value of an optional argument" in text:
        roles.append("value")
    elif "an optional argument" in text:
        roles.append("key")
    return roles


def effective_methods(record: dict[str, Any]) -> list[dict[str, Any]]:
    function_info = record.get("function_info") or {}
    methods = function_info.get("methods") or []
    documented = function_info.get("documented_methods") or []
    general = function_info.get("general_signature") or {}
    general_outputs = general.get("output_types") or []
    by_domain: dict[tuple[str, ...], list[str]] = {}
    for method in documented:
        outputs = method.get("output_types") or []
        signature = method.get("signature") or []
        if outputs and len(signature) > 1:
            by_domain[tuple(signature[1:])] = outputs

    rows = []
    for method in methods:
        signature = method.get("signature") or []
        if not signature:
            continue
        domain = signature[1:]
        key = tuple(domain)
        if key in by_domain:
            rows.append({"domain": domain, "codomain": by_domain[key], "provenance": "specialized"})
        elif general_outputs:
            rows.append({"domain": domain, "codomain": general_outputs, "provenance": "inherited"})
        else:
            rows.append({"domain": domain, "codomain": [], "provenance": "installed"})
    return rows


def section(name: str, body: str) -> str:
    return body.rstrip() + "\n"


def frontmatter(record: dict[str, Any], category_name: str, slug: str, tags: list[str]) -> str:
    doc = record.get("documentation") or {}
    relation = record.get("relation_info") or {}
    lines = ["---"]
    fields = {
        "m2_name": record.get("name") or "",
        "kind": kind_for_category(category_name),
        "class": link(relation.get("class") or record.get("data_type")),
        "parent": link(relation.get("parent")),
        "runtime_class": record.get("data_type") or "",
        "library": library_name(record),
        "doc_key": (doc.get("doc_key") or ""),
        "status": (doc.get("status") or ""),
        "source_file": (doc.get("source_file") or ""),
        "source_line": doc.get("source_line"),
        "slug": slug,
        "generated": True,
    }
    for key, value in fields.items():
        if value is None or value == "":
            continue
        if isinstance(value, bool):
            lines.append(f"{key}: {'true' if value else 'false'}")
        elif isinstance(value, int):
            lines.append(f"{key}: {value}")
        else:
            lines.append(f"{key}: {yaml_string(str(value))}")
    roles = option_role(record)
    if roles:
        lines.append("option_roles:")
        lines.extend(yaml_list(roles))
    lines.append("tags:")
    lines.extend(yaml_list(tags))
    lines.append("---")
    return "\n".join(lines) + "\n"


def build_tags(record: dict[str, Any], category_name: str) -> list[str]:
    tags = ["m2", f"m2/{kind_for_category(category_name)}"]
    if record.get("function_info") is not None:
        tags.append("m2/callable")
    if record.get("operator_info") is not None:
        tags.append("m2/operator")
    for role in option_role(record):
        tags.append(f"m2/option-{role}")
    if not record.get("description_short"):
        tags.append("missing-brief")
    if not description(record):
        tags.append("missing-description")
    if not record.get("examples"):
        tags.append("missing-examples")
    if record.get("function_info") is not None and any(not row["codomain"] for row in effective_methods(record)):
        tags.append("missing-codomain")
    if record.get("documentation", {}).get("source_file") is None:
        tags.append("missing-source")
    return sorted(set(tags))


def markdown_for_record(
    record: dict[str, Any],
    category_name: str,
    slug: str,
    option_usages: dict[str, list[str]],
) -> str:
    name = record.get("name") or ""
    tags = build_tags(record, category_name)
    parts = [frontmatter(record, category_name, slug, tags), f"# {name}\n\n"]
    brief = record.get("description_short") or ""
    parts.append(section("brief", f"## Brief\n\n{brief}\n"))

    usage_text = usage(record)
    if usage_text:
        parts.append(section("usage", f"## Usage\n\n```macaulay2\n{usage_text}\n```\n"))

    typical = ((record.get("function_info") or {}).get("general_signature") or {}).get("output_types") or []
    typical_body = "## Typical Value\n\n" + (", ".join(link(item) for item in typical) if typical else "") + "\n"
    if typical or record.get("function_info") is not None:
        parts.append(section("typical-value", typical_body))

    if record.get("function_info") is not None:
        rows = effective_methods(record)
        method_parts = ["## Installed Methods", ""]
        if rows:
            for row in rows:
                domain = row["domain"]
                heading = ", ".join(link(item) for item in domain) if domain else "no arguments"
                method_parts.extend([f"### {heading}", ""])
                codomain = row["codomain"]
                method_parts.append(
                    "Codomain: " + (", ".join(link(item) for item in codomain) if codomain else "unknown")
                )
                method_parts.append(f"Provenance: {row['provenance']}")
                method_parts.append("")
        parts.append(section("methods", "\n".join(method_parts)))

    option_lines = option_section(record, option_usages)
    if option_lines:
        parts.append(section("options", "\n".join(option_lines) + "\n"))

    desc = description(record)
    parts.append(section("description", f"## Description\n\n{desc}\n"))

    examples = record.get("examples") or []
    example_body = "## Examples\n\n"
    if examples:
        example_body += "```macaulay2\n" + "\n".join(examples) + "\n```\n"
    parts.append(section("examples", example_body))

    type_info = record.get("type_info")
    if type_info is not None:
        parts.append(section("hierarchy", hierarchy_section(type_info)))

    return "\n".join(parts) + "\n## Notes\n"


def hierarchy_section(type_info: dict[str, Any]) -> str:
    lines = ["## Hierarchy", ""]
    if type_info.get("parent_type"):
        lines.append(f"Parent type: {link(type_info['parent_type'])}")
    subtypes = type_info.get("subtypes") or []
    if subtypes:
        lines.extend(["", "Subtypes:"])
        lines.extend(f"- {link(item)}" for item in unique_names(subtypes))
    instances = type_info.get("instances") or []
    if instances:
        instances = unique_names(instances)
        lines.extend(["", "Known instances:"])
        lines.extend(f"- {link(item)}" for item in instances[:50])
        if len(instances) > 50:
            lines.append(f"- ... {len(instances) - 50} more")
    return "\n".join(lines) + "\n"


def option_section(record: dict[str, Any], option_usages: dict[str, list[str]]) -> list[str]:
    roles = option_role(record)
    option_info = record.get("option_info") or {}
    options = option_info.get("options") or []
    used_by = option_usages.get(record.get("name") or "", [])
    if not roles and not options and not used_by:
        return []
    lines = ["## Option Usage", ""]
    if roles:
        lines.append("Roles: " + ", ".join(roles))
        lines.append("")
    if used_by:
        lines.extend(["Used by:", ""])
        lines.extend(f"- {link(name)}" for name in used_by)
        lines.append("")
    if options:
        lines.extend(["Accepted options:", ""])
        for option in options:
            name = option.get("name")
            default = option.get("default")
            if default:
                lines.append(f"- {link(name)} default `{default}`")
            else:
                lines.append(f"- {link(name)}")
    return lines


def write_note(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def write_indexes(vault: Path, records: list[dict[str, Any]]) -> None:
    grouped: dict[str, list[dict[str, Any]]] = {name: [] for name in CATEGORY_TITLES}
    for record in records:
        grouped[category(record)].append(record)

    for category_name, category_records in grouped.items():
        title = CATEGORY_TITLES[category_name]
        lines = [
            "---",
            'tags: ["m2", "m2/index"]',
            "generated: true",
            "---",
            "",
            f"# {title}",
            "",
            f"## {title}",
            "",
        ]
        for record in sorted(category_records, key=lambda item: item.get("name") or ""):
            name = record.get("name") or ""
            brief = record.get("description_short") or ""
            suffix = f" - {brief}" if brief else ""
            lines.append(f"- {link(name)}{suffix}")
        lines.extend(["", "## Notes", ""])
        write_note(vault / "indexes" / f"{title}.md", "\n".join(lines))

    missing_fields = {
        "missing-brief": [],
        "missing-description": [],
        "missing-examples": [],
        "missing-codomain": [],
        "missing-source": [],
    }
    for record in records:
        tags = build_tags(record, category(record))
        for tag in missing_fields:
            if tag in tags:
                missing_fields[tag].append(record)

    lines = [
        "---",
        'tags: ["m2", "m2/index", "m2/missing"]',
        "generated: true",
        "---",
        "",
        "# Missing Docs",
        "",
    ]
    for tag, missing_records in missing_fields.items():
        lines.extend(["", f"## {tag}", ""])
        for record in sorted(missing_records, key=lambda item: item.get("name") or ""):
            lines.append(f"- {link(record.get('name') or '')}")
    lines.extend(["", "## Notes", ""])
    write_note(vault / "indexes" / "Missing Docs.md", "\n".join(lines))


def clean_generated_dirs(vault: Path) -> None:
    for dirname in [*CATEGORY_TITLES, "indexes"]:
        shutil.rmtree(vault / dirname, ignore_errors=True)


def load_records(path: Path) -> list[dict[str, Any]]:
    records = []
    for line in path.read_text().splitlines():
        if line.strip():
            records.append(json.loads(line))
    return records


def canonical_name(name: str, names: set[str]) -> str:
    if name.startswith("Core$"):
        suffix = name.removeprefix("Core$")
        alias = CANONICAL_ALIASES.get(suffix)
        if alias and alias in names:
            return alias
        if suffix in names:
            name = suffix
    alias = CANONICAL_ALIASES.get(name)
    if alias and alias in names:
        return alias
    return name


def deduplicate_records(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    names = {record.get("name") or "" for record in records}
    by_canonical: dict[str, dict[str, Any]] = {}
    for record in records:
        name = record.get("name") or ""
        canonical = canonical_name(name, names)
        current = by_canonical.get(canonical)
        if current is None or current.get("name") != canonical:
            by_canonical[canonical] = record
    return list(by_canonical.values())


def should_write_record(record: dict[str, Any]) -> bool:
    name = record.get("name") or ""
    canonical = canonical_name(name, ALL_NAMES) if ALL_NAMES else name
    if canonical.startswith("_"):
        return False
    if name == "oo" or re.fullmatch(r"o\d+", name):
        return False
    category_name = category(record)
    slug = slug_for_name(name, category_name)
    return re.search(r"_x[0-9a-f]+_", slug) is None


def collect_note_links(records: list[dict[str, Any]], canonical_records: list[dict[str, Any]]) -> dict[str, str]:
    names = {record.get("name") or "" for record in records}
    canonical_by_name = {record.get("name") or "": record for record in canonical_records if should_write_record(record)}
    links = {}
    for record in records:
        name = record.get("name") or ""
        canonical = canonical_name(name, names)
        canonical_record = canonical_by_name.get(canonical, record)
        if not should_write_record(canonical_record):
            continue
        category_name = category(canonical_record)
        slug = slug_for_name(canonical, category_name)
        links[name] = f"{category_name}/{slug}"
    return links


def collect_option_usages(records: list[dict[str, Any]]) -> dict[str, list[str]]:
    names = {record.get("name") or "" for record in records}
    usages: dict[str, set[str]] = {}
    for record in records:
        name = canonical_name(record.get("name") or "", names)
        for option in (record.get("option_info") or {}).get("options") or []:
            option_name = option.get("name")
            if option_name:
                usages.setdefault(canonical_name(option_name, names), set()).add(name)
    return {key: sorted(values) for key, values in usages.items()}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=Path("src/data/builtins.details.jsonl"))
    parser.add_argument("--vault", type=Path, default=Path.home() / "obsidian" / "m2")
    parser.add_argument("--limit", type=int, default=None)
    args = parser.parse_args()

    records = load_records(args.input)
    global ALL_NAMES
    ALL_NAMES = {record.get("name") or "" for record in records}
    canonical_records = [record for record in deduplicate_records(records) if should_write_record(record)]
    global NOTE_LINKS
    NOTE_LINKS = collect_note_links(records, canonical_records)
    option_usages = collect_option_usages(records)
    clean_generated_dirs(args.vault)
    written = 0
    for record in canonical_records[: args.limit]:
        name = record.get("name") or ""
        category_name = category(record)
        slug = slug_for_name(name, category_name)
        path = args.vault / category_name / f"{slug}.md"
        write_note(path, markdown_for_record(record, category_name, slug, option_usages))
        written += 1
    if args.limit is None:
        write_indexes(args.vault, canonical_records)

    print(f"wrote {written} notes to {args.vault}")


if __name__ == "__main__":
    main()
