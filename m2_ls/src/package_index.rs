use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tree_sitter::Parser;

use crate::typesystem::BuiltinData;

#[derive(Debug, Clone)]
pub(crate) struct SourceResolver {
    roots: Vec<PathBuf>,
}

impl SourceResolver {
    pub(crate) fn from_environment() -> Self {
        let mut roots = Vec::new();
        if let Some(paths) = std::env::var_os("M2_LSP_SOURCE_PATH") {
            roots.extend(std::env::split_paths(&paths));
        }
        if let Some(paths) = Self::runtime_m2_path() {
            roots.extend(paths);
        }
        Self::new(roots)
    }

    pub(crate) fn new(roots: Vec<PathBuf>) -> Self {
        let mut deduped = Vec::new();
        for root in roots {
            Self::push_root(&mut deduped, root.clone());
            if root.file_name().is_some_and(|name| name == "packages") {
                if let Some(parent) = root.parent() {
                    Self::push_root(&mut deduped, parent.to_path_buf());
                }
            }
        }
        SourceResolver { roots: deduped }
    }

    fn push_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
        if !roots.iter().any(|existing| existing == &root) {
            roots.push(root);
        }
    }

    fn runtime_m2_path() -> Option<Vec<PathBuf>> {
        let output = Command::new("M2")
            .args(["--silent", "--stop", "-q", "-e", "print stack path; exit 0"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8(output.stdout).ok()?;
        let current_dir = std::env::current_dir().ok();
        let roots = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else if let Some(current_dir) = &current_dir {
                    current_dir.join(path)
                } else {
                    path
                }
            })
            .collect::<Vec<_>>();

        (!roots.is_empty()).then_some(roots)
    }

    pub(crate) fn resolve_source_file(&self, source_file: &str) -> Option<PathBuf> {
        let source = Path::new(source_file);
        if source.is_absolute() && source.exists() {
            return Some(source.to_path_buf());
        }

        self.roots
            .iter()
            .map(|root| root.join(source))
            .find(|candidate| candidate.exists())
    }

    pub(crate) fn resolve_package_file(&self, package_name: &str) -> Option<PathBuf> {
        let source_file = format!("{package_name}.m2");
        self.resolve_source_file(&source_file)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PackageIndexer {
    pub(crate) cache_dir: PathBuf,
    pub(crate) extractor_script: Option<PathBuf>,
}

impl PackageIndexer {
    pub(crate) fn from_environment() -> Self {
        let cache_dir = std::env::var_os("M2_LSP_PACKAGE_INDEX_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_package_index_dir);
        let extractor_script = find_extractor_script();

        PackageIndexer {
            cache_dir,
            extractor_script,
        }
    }

    pub(crate) fn load_or_generate(&self, package_name: &str) -> Option<BuiltinData> {
        if let Some(index) = self.load(package_name) {
            return Some(index);
        }
        self.generate(package_name).ok()?;
        self.load(package_name)
    }

    pub(crate) fn load(&self, package_name: &str) -> Option<BuiltinData> {
        let names = fs::read_to_string(self.names_path(package_name)).ok()?;
        let details = fs::read_to_string(self.details_path(package_name)).ok()?;
        Some(BuiltinData::load_from_split(&names, &details))
    }

    fn generate(&self, package_name: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.cache_dir)?;
        let Some(script) = &self.extractor_script else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "package index extractor script not found",
            ));
        };

        let status = Command::new("M2")
            .arg("--script")
            .arg(script)
            .arg(&self.cache_dir)
            .arg(package_name)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("package index extractor failed"))
        }
    }

    fn names_path(&self, package_name: &str) -> PathBuf {
        self.cache_dir.join(format!("{package_name}.names"))
    }

    fn details_path(&self, package_name: &str) -> PathBuf {
        self.cache_dir.join(format!("{package_name}.details.jsonl"))
    }
}

fn find_extractor_script() -> Option<PathBuf> {
    std::env::var_os("M2_LSP_EXTRACT_PACKAGE_INDEX")
        .map(PathBuf::from)
        .or_else(|| {
            extractor_script_candidates()
                .into_iter()
                .find(|candidate| candidate.exists())
        })
}

pub(crate) fn extractor_script_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_extractor_candidates(&mut candidates, Path::new("."));

    if let Ok(current_dir) = std::env::current_dir() {
        push_extractor_candidates(&mut candidates, &current_dir);
    }

    push_extractor_candidates(&mut candidates, Path::new(env!("CARGO_MANIFEST_DIR")));

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            for ancestor in exe_dir.ancestors() {
                push_extractor_candidates(&mut candidates, ancestor);
            }
        }
    }

    candidates
}

fn push_extractor_candidates(candidates: &mut Vec<PathBuf>, root: &Path) {
    let local = root.join("scripts/extract_package_index.m2");
    if !candidates.iter().any(|candidate| candidate == &local) {
        candidates.push(local);
    }

    let workspace = root.join("m2_ls/scripts/extract_package_index.m2");
    if !candidates.iter().any(|candidate| candidate == &workspace) {
        candidates.push(workspace);
    }
}

fn default_package_index_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("macaulay2-lsp")
        .join("package-index")
}

fn unquoted_string_literal<'a>(text: &'a str, node: tree_sitter::Node<'_>) -> Option<&'a str> {
    if node.kind() != "string_literal" {
        return None;
    }

    let value = &text[node.start_byte()..node.end_byte()];
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
}

pub(crate) fn package_source_string<'a>(
    text: &'a str,
    node: tree_sitter::Node<'_>,
) -> Option<&'a str> {
    let package_name = unquoted_string_literal(text, node)?;
    let parent = node.parent()?;

    if parent.kind() == "sequence" {
        if !is_first_named_child(parent, node) {
            return None;
        }

        return parent
            .parent()
            .and_then(|call| binary_expression_left_symbol(text, call))
            .filter(|name| matches!(*name, "needsPackage" | "loadPackage" | "debug"))
            .map(|_| package_name);
    }

    binary_expression_left_symbol(text, parent)
        .filter(|name| matches!(*name, "needsPackage" | "loadPackage" | "debug"))
        .map(|_| package_name)
}

pub(crate) fn collect_imported_packages(text: &str) -> Vec<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_macaulay2::language())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };

    let root = tree.root_node();
    let mut packages = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = root.walk();
    let mut reached_root = false;
    while !reached_root {
        let node = cursor.node();
        if node.kind() == "string_literal" {
            if let Some(package_name) = package_source_string(text, node) {
                if seen.insert(package_name.to_string()) {
                    packages.push(package_name.to_string());
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                reached_root = true;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }

    packages
}

fn is_first_named_child(parent: tree_sitter::Node, child: tree_sitter::Node) -> bool {
    parent
        .named_child(0)
        .is_some_and(|first| first.id() == child.id())
}

fn binary_expression_left_symbol<'a>(text: &'a str, node: tree_sitter::Node) -> Option<&'a str> {
    if node.kind() != "binary_expression" {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    if left.kind() != "symbol" {
        return None;
    }

    let operator = node.child_by_field_name("operator")?;
    if operator.kind() != "SPACE" {
        return None;
    }

    Some(&text[left.start_byte()..left.end_byte()])
}
