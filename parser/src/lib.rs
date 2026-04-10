use std::borrow::Cow;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use ignore::WalkBuilder;
use memchr::memchr_iter;
use memmap2::Mmap;
use rayon::prelude::*;
use reqwest::blocking::Client;
use rustc_hash::{FxHashMap, FxHashSet};
use rustpython_parser::lexer::lex;
use rustpython_parser::{text_size::TextRange, Mode, Tok};
use serde::{Deserialize, Serialize};
use tar::Archive;

#[derive(Debug, Clone, Serialize)]
pub struct MethodInfo {
    pub name: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FieldInfo {
    pub name: String,
    pub annotation: String,
    pub required: bool,
    pub line: usize,
    pub col: usize,
    pub type_ref: Option<String>,
    pub type_line: Option<usize>,
    pub type_col: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HierarchyNode {
    pub name: String,
    pub module: String,
    pub display_base: String,
    pub path: Option<PathBuf>,
    pub line: Option<usize>,
    pub col: Option<usize>,
    pub external: bool,
    pub recursive: bool,
    pub raw_bases: Vec<String>,
    pub methods: Vec<MethodInfo>,
    pub fields: Vec<FieldInfo>,
    pub ancestors: Vec<HierarchyNode>,
}

impl HierarchyNode {
    fn new(
        name: String,
        module: String,
        display_base: String,
        path: Option<PathBuf>,
        line: Option<usize>,
        col: Option<usize>,
        external: bool,
    ) -> Self {
        Self {
            name,
            module,
            display_base,
            path,
            line,
            col,
            external,
            recursive: false,
            raw_bases: Vec::new(),
            methods: Vec::new(),
            fields: Vec::new(),
            ancestors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Timings {
    pub discover_ms: f64,
    pub parse_ms: f64,
    pub index_ms: f64,
    pub query_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub file: PathBuf,
    pub class_name: String,
    pub hierarchy: HierarchyNode,
    pub timings: Timings,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderNode {
    pub kind: String,
    pub name: String,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub target_path: PathBuf,
    pub target_line: usize,
    pub target_col: usize,
    pub external: bool,
    pub recursive: bool,
    pub detail: String,
    pub required: bool,
    pub type_ref: Option<String>,
    pub type_name: Option<String>,
    pub truncated: bool,
    pub children: Vec<RenderNode>,
}

impl RenderNode {
    fn new(
        kind: &str,
        name: String,
        path: PathBuf,
        line: usize,
        col: usize,
        target_path: PathBuf,
        target_line: usize,
        target_col: usize,
        external: bool,
    ) -> Self {
        Self {
            kind: kind.to_string(),
            name,
            path,
            line,
            col,
            target_path,
            target_line,
            target_col,
            external,
            recursive: false,
            detail: String::new(),
            required: false,
            type_ref: None,
            type_name: None,
            truncated: false,
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderTreeResult {
    pub tree: RenderNode,
    pub timings: Timings,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClassMembersResult {
    pub file: PathBuf,
    pub class_name: String,
    pub methods: Vec<MethodInfo>,
    pub fields: Vec<FieldInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HierarchyItem {
    pub name: String,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub kind: u32,
    pub detail: String,
    pub external: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RangePoint {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRange {
    pub start: RangePoint,
    pub end: RangePoint,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: u32,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub external: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedCallableReference {
    pub name: String,
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub kind: u32,
    pub external: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallHierarchyEdge {
    pub item: CallHierarchyItem,
    pub from_ranges: Vec<SourceRange>,
}

#[derive(Debug, Clone)]
pub struct IndexBuild {
    pub index: WorkspaceIndex,
    pub timings: Timings,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeshedSnapshot {
    pub sha: String,
    pub stdlib_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct WorkspaceIndex {
    root: PathBuf,
    modules_by_name: FxHashMap<String, Arc<ModuleInfo>>,
    modules_by_path: FxHashMap<PathBuf, String>,
    lazy_workspace_modules: Arc<RwLock<FxHashMap<String, Arc<ModuleInfo>>>>,
    lazy_workspace_paths: Arc<RwLock<FxHashMap<PathBuf, String>>>,
    import_roots: Arc<RwLock<Vec<PathBuf>>>,
    external_modules: Arc<RwLock<FxHashMap<String, Arc<ModuleInfo>>>>,
    direct_subtypes: FxHashMap<String, Vec<(String, String)>>,
    outgoing_calls: FxHashMap<String, Vec<CallHierarchyEdge>>,
    incoming_calls: FxHashMap<String, Vec<CallHierarchyEdge>>,
}

#[derive(Debug, Clone)]
struct ModuleInfo {
    path: PathBuf,
    module_name: String,
    imports: FxHashMap<String, ImportBinding>,
    star_imports: Vec<String>,
    classes: Vec<ClassInfo>,
    class_index: FxHashMap<String, usize>,
    functions: Vec<FunctionInfo>,
}

#[derive(Debug, Clone)]
struct ClassInfo {
    name: String,
    line: usize,
    col: usize,
    bases: Vec<String>,
    methods: Vec<MethodInfo>,
    fields: Vec<FieldInfo>,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    line: usize,
    col: usize,
    kind: u32,
    class_name: Option<String>,
    calls: Vec<CallSite>,
}

#[derive(Debug, Clone)]
struct CallSite {
    raw_target: String,
    range: SourceRange,
}

#[derive(Debug, Clone)]
enum ImportBinding {
    Module { module: String },
    ImportedName { module: String, name: String },
}

#[derive(Debug, Clone)]
struct ScopeFrame {
    body_indent: usize,
    kind: ScopeKind,
}

#[derive(Debug, Clone, Copy)]
enum ScopeKind {
    Class(usize),
    Function(usize),
}

#[derive(Debug, Clone)]
struct TokenSpan {
    kind: Tok,
    range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKindTag {
    Async,
    As,
    Class,
    Colon,
    Comma,
    Dedent,
    Def,
    Dot,
    EndOfFile,
    Equal,
    From,
    Import,
    Indent,
    Lbrace,
    Lpar,
    Lsqb,
    Name,
    Newline,
    Other,
    Rbrace,
    Rpar,
    Rsqb,
}

#[derive(Debug, Clone)]
enum ResolvedBase {
    Workspace {
        module: String,
        class_name: String,
        display: String,
    },
    External {
        module: String,
        class_name: String,
        display: String,
    },
    Unresolved {
        display: String,
    },
}

enum Source {
    Heap(Vec<u8>),
    Mmap(Mmap),
}

fn token_tag(token: &Tok) -> TokenKindTag {
    match token {
        Tok::Async => TokenKindTag::Async,
        Tok::As => TokenKindTag::As,
        Tok::Class => TokenKindTag::Class,
        Tok::Colon => TokenKindTag::Colon,
        Tok::Comma => TokenKindTag::Comma,
        Tok::Dedent => TokenKindTag::Dedent,
        Tok::Def => TokenKindTag::Def,
        Tok::Dot => TokenKindTag::Dot,
        Tok::EndOfFile => TokenKindTag::EndOfFile,
        Tok::Equal => TokenKindTag::Equal,
        Tok::From => TokenKindTag::From,
        Tok::Import => TokenKindTag::Import,
        Tok::Indent => TokenKindTag::Indent,
        Tok::Lbrace => TokenKindTag::Lbrace,
        Tok::Lpar => TokenKindTag::Lpar,
        Tok::Lsqb => TokenKindTag::Lsqb,
        Tok::Name { .. } => TokenKindTag::Name,
        Tok::Newline => TokenKindTag::Newline,
        Tok::Rbrace => TokenKindTag::Rbrace,
        Tok::Rpar => TokenKindTag::Rpar,
        Tok::Rsqb => TokenKindTag::Rsqb,
        _ => TokenKindTag::Other,
    }
}

impl WorkspaceIndex {
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn import_roots(&self) -> Vec<PathBuf> {
        self.import_roots.read().clone()
    }

    fn module_for_file(&self, file: &Path) -> Option<&ModuleInfo> {
        self.modules_by_path
            .get(file)
            .and_then(|module_name| self.modules_by_name.get(module_name))
            .map(Arc::as_ref)
    }

    fn module_name_for_file(&self, file: &Path) -> Option<String> {
        if let Some(module) = self.module_for_file(file) {
            return Some(module.module_name.clone());
        }

        {
            let cache = self.lazy_workspace_paths.read();
            if let Some(module_name) = cache.get(file) {
                return Some(module_name.clone());
            }
        }

        self.import_roots()
            .iter()
            .find_map(|root| module_name_from_path(root, file).ok())
            .or_else(|| module_name_from_path(&self.root, file).ok())
    }

    fn resolve_module_name(&self, file: &Path) -> Result<String> {
        let canonical_file = file.canonicalize().ok();
        self.module_name_for_file(file)
            .or_else(|| {
                canonical_file
                    .as_ref()
                    .and_then(|canonical| self.module_name_for_file(canonical))
            })
            .with_context(|| format!("file is not part of the index: {}", file.display()))
    }
}

#[derive(Debug, Deserialize)]
struct GitHubCommitResponse {
    sha: String,
}

fn cache_base_dir() -> Option<PathBuf> {
    if let Some(xdg_cache_home) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg_cache_home));
    }

    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
}

fn vendored_typeshed_root(tool: &str) -> Option<PathBuf> {
    cache_base_dir().map(|cache_dir| cache_dir.join(tool).join("vendored").join("typeshed"))
}

fn discover_cached_typeshed_roots(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let stdlib_root = entry.path().join("stdlib");
        if !stdlib_root.join("builtins.pyi").is_file() {
            continue;
        }
        let modified = stdlib_root
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, stdlib_root));
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().map(|(_, path)| path).collect()
}

fn discover_telepy_typeshed_roots() -> Vec<PathBuf> {
    vendored_typeshed_root("telepy")
        .map(|root| discover_cached_typeshed_roots(&root))
        .unwrap_or_default()
}

fn discover_ty_typeshed_roots() -> Vec<PathBuf> {
    vendored_typeshed_root("ty")
        .map(|root| discover_cached_typeshed_roots(&root))
        .unwrap_or_default()
}

fn archive_entry_target(entry_path: &Path) -> Option<PathBuf> {
    let mut components = entry_path.components();
    components.next()?;
    let second = components.next()?;
    if second.as_os_str() != "stdlib" {
        return None;
    }

    let mut relative = PathBuf::from("stdlib");
    for component in components {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn fetch_typeshed_head_sha(client: &Client) -> Result<String> {
    let response = client
        .get("https://api.github.com/repos/python/typeshed/commits/main")
        .send()
        .context("failed to request python/typeshed HEAD commit")?
        .error_for_status()
        .context("python/typeshed HEAD request failed")?;
    let payload = response
        .json::<GitHubCommitResponse>()
        .context("failed to decode python/typeshed HEAD response")?;
    Ok(payload.sha)
}

fn download_typeshed_snapshot(client: &Client, sha: &str, snapshot_root: &Path) -> Result<()> {
    let archive_url = format!("https://codeload.github.com/python/typeshed/tar.gz/{sha}");
    let response = client
        .get(&archive_url)
        .send()
        .with_context(|| format!("failed to download typeshed archive for {sha}"))?
        .error_for_status()
        .with_context(|| format!("typeshed archive request failed for {sha}"))?;

    let temp_root = snapshot_root.with_file_name(format!(
        ".tmp-{}-{}",
        snapshot_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("typeshed"),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if temp_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    fs::create_dir_all(&temp_root)
        .with_context(|| format!("failed to create {}", temp_root.display()))?;

    let extract_result = (|| -> Result<()> {
        let decoder = GzDecoder::new(response);
        let mut archive = Archive::new(decoder);
        for entry_result in archive
            .entries()
            .context("failed to read typeshed archive")?
        {
            let mut entry = entry_result.context("failed to read typeshed archive entry")?;
            let entry_path = entry
                .path()
                .context("failed to read typeshed archive entry path")?
                .into_owned();
            let Some(relative) = archive_entry_target(&entry_path) else {
                continue;
            };
            let destination = temp_root.join(relative);
            if entry.header().entry_type().is_dir() {
                fs::create_dir_all(&destination)
                    .with_context(|| format!("failed to create {}", destination.display()))?;
                continue;
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            entry
                .unpack(&destination)
                .with_context(|| format!("failed to unpack {}", destination.display()))?;
        }
        Ok(())
    })();

    if extract_result.is_err() {
        let _ = fs::remove_dir_all(&temp_root);
    }
    extract_result?;

    let commit_file = temp_root.join("source_commit.txt");
    fs::write(&commit_file, format!("{sha}\n"))
        .with_context(|| format!("failed to write {}", commit_file.display()))?;

    if snapshot_root.exists() {
        let _ = fs::remove_dir_all(&temp_root);
        return Ok(());
    }

    fs::rename(&temp_root, snapshot_root)
        .or_else(|rename_err| {
            if snapshot_root.exists() {
                let _ = fs::remove_dir_all(&temp_root);
                Ok(())
            } else {
                Err(rename_err)
            }
        })
        .with_context(|| {
            format!(
                "failed to move downloaded typeshed snapshot into {}",
                snapshot_root.display()
            )
        })?;

    Ok(())
}

pub fn ensure_telepy_typeshed_snapshot() -> Result<Option<TypeshedSnapshot>> {
    if let Some(stdlib_root) = discover_telepy_typeshed_roots().into_iter().next() {
        let sha = stdlib_root
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            .map(String::from)
            .context("could not extract SHA from typeshed path")?;
        return Ok(Some(TypeshedSnapshot { sha, stdlib_root }));
    }

    sync_telepy_typeshed_snapshot()
}

pub fn sync_telepy_typeshed_snapshot() -> Result<Option<TypeshedSnapshot>> {
    let Some(cache_root) = vendored_typeshed_root("telepy") else {
        return Ok(None);
    };
    fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to create {}", cache_root.display()))?;

    let client = Client::builder()
        .user_agent("telepy-hierarchy-parser")
        .build()
        .context("failed to build HTTP client for typeshed sync")?;
    let sha = fetch_typeshed_head_sha(&client)?;
    let snapshot_root = cache_root.join(&sha);
    let stdlib_root = snapshot_root.join("stdlib");
    if !stdlib_root.join("builtins.pyi").is_file() {
        download_typeshed_snapshot(&client, &sha, &snapshot_root)?;
    }
    let commit_file = snapshot_root.join("source_commit.txt");
    if !commit_file.is_file() {
        fs::write(&commit_file, format!("{sha}\n"))
            .with_context(|| format!("failed to write {}", commit_file.display()))?;
    }

    if !stdlib_root.join("builtins.pyi").is_file() {
        bail!(
            "typeshed snapshot download did not produce {}",
            stdlib_root.join("builtins.pyi").display()
        );
    }

    Ok(Some(TypeshedSnapshot { sha, stdlib_root }))
}

pub fn build_workspace_index(root: &Path) -> Result<IndexBuild> {
    let total_start = Instant::now();
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;

    let discover_start = Instant::now();
    let files = discover_python_files(&root)?;
    let discover_ms = elapsed_ms(discover_start);

    let parse_start = Instant::now();
    let modules = files
        .par_iter()
        .map(|path| parse_module(&root, path))
        .collect::<Result<Vec<_>>>()?;
    let parse_ms = elapsed_ms(parse_start);

    let index_start = Instant::now();
    let mut modules_by_name = FxHashMap::default();
    let mut modules_by_path = FxHashMap::default();
    for module in modules {
        modules_by_path.insert(module.path.clone(), module.module_name.clone());
        modules_by_name.insert(module.module_name.clone(), Arc::new(module));
    }
    let index_ms = elapsed_ms(index_start);
    let total_ms = elapsed_ms(total_start);
    let import_roots = discover_import_roots(&root);
    let mut index = WorkspaceIndex {
        root,
        modules_by_name,
        modules_by_path,
        lazy_workspace_modules: Arc::new(RwLock::new(FxHashMap::default())),
        lazy_workspace_paths: Arc::new(RwLock::new(FxHashMap::default())),
        import_roots: Arc::new(RwLock::new(import_roots)),
        external_modules: Arc::new(RwLock::new(FxHashMap::default())),
        direct_subtypes: FxHashMap::default(),
        outgoing_calls: FxHashMap::default(),
        incoming_calls: FxHashMap::default(),
    };
    index.direct_subtypes = build_direct_subtype_index(&index);
    let (outgoing_calls, incoming_calls) = build_call_indices(&index);
    index.outgoing_calls = outgoing_calls;
    index.incoming_calls = incoming_calls;

    Ok(IndexBuild {
        index,
        timings: Timings {
            discover_ms,
            parse_ms,
            index_ms,
            query_ms: 0.0,
            total_ms,
        },
    })
}

pub fn build_lazy_hierarchy_index(root: &Path) -> Result<IndexBuild> {
    let total_start = Instant::now();
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;

    let discover_start = Instant::now();
    let import_roots = discover_import_roots(&root);
    let discover_ms = elapsed_ms(discover_start);
    let total_ms = elapsed_ms(total_start);

    Ok(IndexBuild {
        index: WorkspaceIndex {
            root,
            modules_by_name: FxHashMap::default(),
            modules_by_path: FxHashMap::default(),
            lazy_workspace_modules: Arc::new(RwLock::new(FxHashMap::default())),
            lazy_workspace_paths: Arc::new(RwLock::new(FxHashMap::default())),
            import_roots: Arc::new(RwLock::new(import_roots)),
            external_modules: Arc::new(RwLock::new(FxHashMap::default())),
            direct_subtypes: FxHashMap::default(),
            outgoing_calls: FxHashMap::default(),
            incoming_calls: FxHashMap::default(),
        },
        timings: Timings {
            discover_ms,
            parse_ms: 0.0,
            index_ms: 0.0,
            query_ms: 0.0,
            total_ms,
        },
    })
}

fn discover_import_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for env_name in [".venv", "venv"] {
        let env_root = root.join(env_name);
        if !env_root.is_dir() {
            continue;
        }

        let lib_dir = env_root.join("lib");
        if lib_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&lib_dir) {
                for entry in entries.flatten() {
                    let site_packages = entry.path().join("site-packages");
                    if site_packages.is_dir() {
                        roots.push(site_packages);
                    }
                }
            }
        }

        let windows_site_packages = env_root.join("Lib").join("site-packages");
        if windows_site_packages.is_dir() {
            roots.push(windows_site_packages);
        }
    }

    roots.extend(discover_telepy_typeshed_roots());
    roots.extend(discover_ty_typeshed_roots());
    let builtin_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stubs");
    if builtin_root.is_dir() {
        roots.push(builtin_root);
    }

    roots.sort();
    roots.dedup();
    roots
}

pub fn evict_module_file(index: &WorkspaceIndex, file: &Path) {
    let module_name = index.lazy_workspace_paths.read().get(file).cloned();
    if let Some(module_name) = module_name {
        index.lazy_workspace_modules.write().remove(&module_name);
        index.lazy_workspace_paths.write().remove(file);
    }
}

pub fn refresh_import_roots(index: &WorkspaceIndex) {
    let discovered = discover_import_roots(index.root());
    let mut roots = index.import_roots.write();
    let mut seen = roots.iter().cloned().collect::<FxHashSet<_>>();
    for root in discovered {
        if seen.insert(root.clone()) {
            roots.push(root);
        }
    }
}

pub fn query_type_hierarchy(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
) -> Result<QueryResult> {
    let total_start = Instant::now();
    let file = file.to_path_buf();
    let module_name = index.resolve_module_name(&file)?;
    let (module, _) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;
    let class_name = if class_name.is_empty() {
        module
            .classes
            .first()
            .map(|class| class.name.clone())
            .context("no classes found in module")?
    } else {
        class_name.to_string()
    };

    if !module.class_index.contains_key(&class_name) {
        bail!(
            "class '{}' not found in module {}",
            class_name,
            module.module_name
        );
    }

    let mut visited = FxHashSet::default();
    let hierarchy = build_class_node(index, &module_name, &class_name, &class_name, &mut visited)?;
    let total_ms = elapsed_ms(total_start);

    Ok(QueryResult {
        file,
        class_name,
        hierarchy,
        timings: Timings {
            discover_ms: 0.0,
            parse_ms: 0.0,
            index_ms: 0.0,
            query_ms: total_ms,
            total_ms,
        },
    })
}

fn hierarchy_item_from_class(
    module: &ModuleInfo,
    class: &ClassInfo,
    external: bool,
    detail: String,
) -> HierarchyItem {
    HierarchyItem {
        name: class.name.clone(),
        path: module.path.clone(),
        line: class.line,
        col: class.col,
        kind: 5,
        detail,
        external,
    }
}

fn callable_key(module_name: &str, function: &FunctionInfo) -> String {
    let owner = function.class_name.as_deref().unwrap_or("");
    format!(
        "{module_name}::{owner}::{}@{}:{}",
        function.name, function.line, function.col
    )
}

fn class_key(module_name: &str, class_name: &str) -> String {
    format!("{module_name}::{class_name}")
}

fn call_item_from_function(
    module: &ModuleInfo,
    function: &FunctionInfo,
    external: bool,
) -> CallHierarchyItem {
    CallHierarchyItem {
        name: function.name.clone(),
        kind: function.kind,
        path: module.path.clone(),
        line: function.line,
        col: function.col,
        external,
    }
}

fn locate_class_target(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
) -> Result<(String, ClassInfo, bool)> {
    let file = file.to_path_buf();
    let module_name = index.resolve_module_name(&file)?;
    let (module, external) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;
    let class = if class_name.is_empty() {
        module
            .classes
            .first()
            .cloned()
            .context("no classes found in module")?
    } else {
        let class_index = *module.class_index.get(class_name).with_context(|| {
            format!("class '{}.{}' is not in the index", module_name, class_name)
        })?;
        module.classes[class_index].clone()
    };
    Ok((module_name, class, external))
}

pub fn query_subtypes(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
) -> Result<Vec<HierarchyItem>> {
    let (target_module, target_class, _) = locate_class_target(index, file, class_name)?;
    let mut items = Vec::new();

    let target_key = class_key(&target_module, &target_class.name);
    for (child_module_name, child_class_name) in
        index.direct_subtypes.get(&target_key).into_iter().flatten()
    {
        let Some(child_module) = index.modules_by_name.get(child_module_name) else {
            continue;
        };
        let Some(class_index) = child_module.class_index.get(child_class_name) else {
            continue;
        };
        if let Some(class) = child_module.classes.get(*class_index) {
            items.push(hierarchy_item_from_class(
                child_module,
                class,
                false,
                String::new(),
            ));
        }
    }

    items.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
    Ok(items)
}

pub fn query_class_members(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
) -> Result<ClassMembersResult> {
    let (module_name, class, _) = locate_class_target(index, file, class_name)?;
    let (module, _) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;
    Ok(ClassMembersResult {
        file: module.path.clone(),
        class_name: class.name.clone(),
        methods: class.methods.clone(),
        fields: class.fields.clone(),
    })
}

pub fn query_resolved_class_fields(
    index: &WorkspaceIndex,
    file: &Path,
    raw_type: &str,
) -> Result<ClassMembersResult> {
    let file = file.to_path_buf();
    let module_name = index.resolve_module_name(&file)?;
    let (module, _) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;
    let resolved = resolve_base(index, &module, raw_type);

    let (resolved_module_name, resolved_class_name) = match resolved {
        ResolvedBase::Workspace {
            module, class_name, ..
        }
        | ResolvedBase::External {
            module, class_name, ..
        } => (module, class_name),
        ResolvedBase::Unresolved { display } => {
            bail!(
                "could not resolve field type '{display}' from {}",
                file.display()
            )
        }
    };

    let (resolved_module, _) = get_module(index, &resolved_module_name)
        .with_context(|| format!("module '{}' is not part of the index", resolved_module_name))?;
    let class_index = *resolved_module
        .class_index
        .get(&resolved_class_name)
        .with_context(|| {
            format!(
                "class '{}.{}' is not in the index",
                resolved_module_name, resolved_class_name
            )
        })?;
    let class = resolved_module.classes[class_index].clone();

    Ok(ClassMembersResult {
        file: resolved_module.path.clone(),
        class_name: class.name.clone(),
        methods: class.methods.clone(),
        fields: class.fields.clone(),
    })
}

fn build_call_indices(
    index: &WorkspaceIndex,
) -> (
    FxHashMap<String, Vec<CallHierarchyEdge>>,
    FxHashMap<String, Vec<CallHierarchyEdge>>,
) {
    let mut outgoing = FxHashMap::<String, FxHashMap<String, CallHierarchyEdge>>::default();
    let mut incoming = FxHashMap::<String, FxHashMap<String, CallHierarchyEdge>>::default();

    for module in index.modules_by_name.values() {
        for function in &module.functions {
            let caller_key = callable_key(&module.module_name, function);
            let caller_item = call_item_from_function(module, function, false);
            for call in &function.calls {
                let Some(target) = resolve_callable(index, module, function, &call.raw_target)
                else {
                    continue;
                };

                outgoing
                    .entry(caller_key.clone())
                    .or_default()
                    .entry(target.key.clone())
                    .and_modify(|edge| edge.from_ranges.push(call.range.clone()))
                    .or_insert_with(|| CallHierarchyEdge {
                        item: target.item.clone(),
                        from_ranges: vec![call.range.clone()],
                    });

                incoming
                    .entry(target.key.clone())
                    .or_default()
                    .entry(caller_key.clone())
                    .and_modify(|edge| edge.from_ranges.push(call.range.clone()))
                    .or_insert_with(|| CallHierarchyEdge {
                        item: caller_item.clone(),
                        from_ranges: vec![call.range.clone()],
                    });
            }
        }
    }

    let outgoing = outgoing
        .into_iter()
        .map(|(key, entries)| (key, entries.into_values().collect()))
        .collect();
    let incoming = incoming
        .into_iter()
        .map(|(key, entries)| (key, entries.into_values().collect()))
        .collect();
    (outgoing, incoming)
}

fn build_direct_subtype_index(index: &WorkspaceIndex) -> FxHashMap<String, Vec<(String, String)>> {
    let mut direct_subtypes = FxHashMap::<String, Vec<(String, String)>>::default();

    for (child_module_name, child_module) in &index.modules_by_name {
        for class in &child_module.classes {
            let mut seen_bases = FxHashSet::default();
            for raw_base in &class.bases {
                let resolved = match resolve_base(index, child_module, raw_base) {
                    ResolvedBase::Workspace {
                        module, class_name, ..
                    }
                    | ResolvedBase::External {
                        module, class_name, ..
                    } => Some((module, class_name)),
                    ResolvedBase::Unresolved { .. } => None,
                };
                let Some((base_module_name, base_class_name)) = resolved else {
                    continue;
                };
                let base_key = class_key(&base_module_name, &base_class_name);
                if !seen_bases.insert(base_key.clone()) {
                    continue;
                }
                direct_subtypes
                    .entry(base_key)
                    .or_default()
                    .push((child_module_name.clone(), class.name.clone()));
            }
        }
    }

    for children in direct_subtypes.values_mut() {
        children.sort_by(|(module_a, class_a), (module_b, class_b)| {
            module_a.cmp(module_b).then(class_a.cmp(class_b))
        });
    }

    direct_subtypes
}

#[derive(Debug, Clone)]
struct ResolvedCallable {
    key: String,
    item: CallHierarchyItem,
}

fn resolve_callable(
    index: &WorkspaceIndex,
    module: &ModuleInfo,
    function: &FunctionInfo,
    raw_target: &str,
) -> Option<ResolvedCallable> {
    let mut splits = raw_target.split('.');
    let head = splits.next()?;
    let tail: Vec<&str> = splits.collect();

    if tail.is_empty() {
        if let Some(binding) = module.imports.get(head) {
            return resolve_callable_import_binding(index, binding, &[]);
        }
        if let Some(callable) = resolve_module_function(index, &module.module_name, head) {
            return Some(callable);
        }
        return resolve_named_callable(index, &module.module_name, head);
    }

    if (head == "self" || head == "cls") && tail.len() == 1 {
        if let Some(class_name) = function.class_name.as_deref() {
            return resolve_method_in_class_hierarchy(
                index,
                &module.module_name,
                class_name,
                tail[0],
            );
        }
    }
    if let Some(binding) = module.imports.get(head) {
        return resolve_callable_import_binding(index, binding, &tail);
    }
    if tail.len() == 1 && module.class_index.contains_key(head) {
        return resolve_method_in_class_hierarchy(index, &module.module_name, head, tail[0]);
    }
    resolve_callable_module_path(index, head, &tail)
}

fn resolve_named_callable(
    index: &WorkspaceIndex,
    module_name: &str,
    symbol_name: &str,
) -> Option<ResolvedCallable> {
    let mut visited = FxHashSet::default();
    resolve_named_callable_inner(index, module_name, symbol_name, &mut visited)
}

fn resolve_named_callable_inner(
    index: &WorkspaceIndex,
    module_name: &str,
    symbol_name: &str,
    visited: &mut FxHashSet<(String, String)>,
) -> Option<ResolvedCallable> {
    let key = (module_name.to_string(), symbol_name.to_string());
    if !visited.insert(key.clone()) {
        return None;
    }

    if let Some(callable) = resolve_module_function(index, module_name, symbol_name) {
        visited.remove(&key);
        return Some(callable);
    }

    let (module, _) = get_module(index, module_name)?;
    if let Some(binding) = module.imports.get(symbol_name) {
        visited.remove(&key);
        return resolve_callable_import_binding(index, binding, &[]);
    }

    for star_module in &module.star_imports {
        if let Some(callable) =
            resolve_named_callable_inner(index, star_module, symbol_name, visited)
        {
            visited.remove(&key);
            return Some(callable);
        }
    }

    visited.remove(&key);
    None
}

fn resolve_callable_import_binding(
    index: &WorkspaceIndex,
    binding: &ImportBinding,
    tail: &[&str],
) -> Option<ResolvedCallable> {
    match binding {
        ImportBinding::Module { module } => resolve_callable_module_path(index, module, tail),
        ImportBinding::ImportedName { module, name } => {
            if tail.is_empty() {
                return resolve_named_callable(index, module, name).or_else(|| {
                    resolve_callable_module_path(index, &format!("{module}.{name}"), &[])
                });
            }
            resolve_callable_module_path(index, &format!("{module}.{name}"), tail)
        }
    }
}

fn resolve_callable_module_path(
    index: &WorkspaceIndex,
    module_path: &str,
    tail: &[&str],
) -> Option<ResolvedCallable> {
    if tail.is_empty() {
        let (module_name, function_name) = module_path.rsplit_once('.')?;
        return resolve_module_function(index, module_name, function_name);
    }
    if tail.len() == 1 {
        return resolve_module_function(index, module_path, tail[0]);
    }
    if tail.len() == 2 {
        return resolve_method_in_class_hierarchy(index, module_path, tail[0], tail[1]);
    }
    None
}

fn resolve_module_function(
    index: &WorkspaceIndex,
    module_name: &str,
    function_name: &str,
) -> Option<ResolvedCallable> {
    let (module, external) = get_module(index, module_name)?;
    let function = module
        .functions
        .iter()
        .find(|function| function.class_name.is_none() && function.name == function_name)?;
    Some(ResolvedCallable {
        key: callable_key(module_name, function),
        item: call_item_from_function(&module, function, external),
    })
}

fn resolve_method_in_class_hierarchy(
    index: &WorkspaceIndex,
    module_name: &str,
    class_name: &str,
    method_name: &str,
) -> Option<ResolvedCallable> {
    let mut visited = FxHashSet::default();
    resolve_method_in_class_hierarchy_inner(
        index,
        module_name,
        class_name,
        method_name,
        &mut visited,
    )
}

fn resolve_method_in_class_hierarchy_inner(
    index: &WorkspaceIndex,
    module_name: &str,
    class_name: &str,
    method_name: &str,
    visited: &mut FxHashSet<(String, String, String)>,
) -> Option<ResolvedCallable> {
    let key = (
        module_name.to_string(),
        class_name.to_string(),
        method_name.to_string(),
    );
    if !visited.insert(key.clone()) {
        return None;
    }

    let (module, external) = get_module(index, module_name)?;
    if let Some(function) = module.functions.iter().find(|function| {
        function.class_name.as_deref() == Some(class_name) && function.name == method_name
    }) {
        visited.remove(&key);
        return Some(ResolvedCallable {
            key: callable_key(module_name, function),
            item: call_item_from_function(&module, function, external),
        });
    }

    let class = module
        .class_index
        .get(class_name)
        .and_then(|index| module.classes.get(*index))?;
    for base in &class.bases {
        match resolve_base(index, &module, base) {
            ResolvedBase::Workspace {
                module, class_name, ..
            }
            | ResolvedBase::External {
                module, class_name, ..
            } => {
                if let Some(callable) = resolve_method_in_class_hierarchy_inner(
                    index,
                    &module,
                    &class_name,
                    method_name,
                    visited,
                ) {
                    visited.remove(&key);
                    return Some(callable);
                }
            }
            ResolvedBase::Unresolved { .. } => {}
        }
    }

    visited.remove(&key);
    None
}

fn locate_function_target(
    index: &WorkspaceIndex,
    file: &Path,
    symbol_name: &str,
    line: Option<usize>,
    col: Option<usize>,
) -> Result<(String, FunctionInfo)> {
    let file = file.to_path_buf();
    let module_name = index.resolve_module_name(&file)?;
    let (module, _) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;

    if let (Some(line), Some(col)) = (line, col) {
        if let Some(function) = module
            .functions
            .iter()
            .find(|function| function.line == line && function.col == col)
            .cloned()
        {
            return Ok((module_name, function));
        }
    }

    let function = module
        .functions
        .iter()
        .find(|function| function.name == symbol_name)
        .cloned()
        .with_context(|| {
            format!(
                "function '{}' not found in module {}",
                symbol_name, module_name
            )
        })?;
    Ok((module_name, function))
}

pub fn query_outgoing_calls(
    index: &WorkspaceIndex,
    file: &Path,
    symbol_name: &str,
    line: Option<usize>,
    col: Option<usize>,
) -> Result<Vec<CallHierarchyEdge>> {
    let (module_name, function) = locate_function_target(index, file, symbol_name, line, col)?;
    let mut items = index
        .outgoing_calls
        .get(&callable_key(&module_name, &function))
        .cloned()
        .unwrap_or_default();
    items.sort_by(|a, b| {
        a.item
            .path
            .cmp(&b.item.path)
            .then(a.item.line.cmp(&b.item.line))
            .then(a.item.col.cmp(&b.item.col))
    });
    Ok(items)
}

pub fn query_incoming_calls(
    index: &WorkspaceIndex,
    file: &Path,
    symbol_name: &str,
    line: Option<usize>,
    col: Option<usize>,
) -> Result<Vec<CallHierarchyEdge>> {
    let (module_name, function) = locate_function_target(index, file, symbol_name, line, col)?;
    let mut items = index
        .incoming_calls
        .get(&callable_key(&module_name, &function))
        .cloned()
        .unwrap_or_default();
    items.sort_by(|a, b| {
        a.item
            .path
            .cmp(&b.item.path)
            .then(a.item.line.cmp(&b.item.line))
            .then(a.item.col.cmp(&b.item.col))
    });
    Ok(items)
}

fn range_contains_position(range: &SourceRange, line: usize, col: usize) -> bool {
    if line < range.start.line || line > range.end.line {
        return false;
    }
    if line == range.start.line && col < range.start.col {
        return false;
    }
    if line == range.end.line && col > range.end.col {
        return false;
    }
    true
}

pub fn query_resolved_callable_reference(
    index: &WorkspaceIndex,
    file: &Path,
    line: usize,
    col: usize,
) -> Result<ResolvedCallableReference> {
    let file = file.to_path_buf();
    let module_name = index.resolve_module_name(&file)?;
    let (module, _) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;

    let mut best: Option<(usize, usize, ResolvedCallableReference)> = None;
    for function in &module.functions {
        for call in &function.calls {
            if !range_contains_position(&call.range, line, col) {
                continue;
            }
            let Some(resolved) = resolve_callable(index, &module, function, &call.raw_target)
            else {
                continue;
            };
            let width = call.range.end.col.saturating_sub(call.range.start.col);
            let distance = line.abs_diff(call.range.start.line);
            let candidate = ResolvedCallableReference {
                name: resolved.item.name.clone(),
                path: resolved.item.path.clone(),
                line: resolved.item.line,
                col: resolved.item.col,
                kind: resolved.item.kind,
                external: resolved.item.external,
            };
            match &best {
                Some((best_distance, best_width, _))
                    if (*best_distance, *best_width) <= (distance, width) => {}
                _ => {
                    best = Some((distance, width, candidate));
                }
            }
        }
    }

    best.map(|(_, _, reference)| reference)
        .context("no callable reference found at the current cursor position")
}

fn render_sort_nodes(nodes: &mut [RenderNode]) {
    nodes.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
            .then(a.name.cmp(&b.name))
    });
}

fn render_type_name(type_ref: &str) -> String {
    type_ref.rsplit('.').next().unwrap_or(type_ref).to_string()
}

fn render_method_node(module: &ModuleInfo, method: &MethodInfo, external: bool) -> RenderNode {
    RenderNode::new(
        "method",
        method.name.clone(),
        module.path.clone(),
        method.line,
        method.col,
        module.path.clone(),
        method.line,
        method.col,
        external,
    )
}

fn resolve_field_target(
    index: &WorkspaceIndex,
    module: &ModuleInfo,
    field: &FieldInfo,
) -> Option<(String, String)> {
    let type_ref = field.type_ref.as_deref()?;
    match resolve_base(index, module, type_ref) {
        ResolvedBase::Workspace {
            module, class_name, ..
        }
        | ResolvedBase::External {
            module, class_name, ..
        } => Some((module, class_name)),
        ResolvedBase::Unresolved { .. } => None,
    }
}

fn render_field_node(index: &WorkspaceIndex, module: &ModuleInfo, field: &FieldInfo) -> RenderNode {
    let mut node = RenderNode::new(
        "field",
        field.name.clone(),
        module.path.clone(),
        field.line,
        field.col,
        module.path.clone(),
        field.line,
        field.col,
        false,
    );
    node.detail = field.annotation.clone();
    node.required = field.required;
    node.type_ref = field.type_ref.clone();
    node.type_name = field.type_ref.as_deref().map(render_type_name);

    if let Some((resolved_module, resolved_class)) = resolve_field_target(index, module, field) {
        if let Some((target_module, _target_external)) = get_module(index, &resolved_module) {
            if let Some(class_index) = target_module.class_index.get(&resolved_class) {
                if let Some(target_class) = target_module.classes.get(*class_index) {
                    if !target_class.fields.is_empty() || !target_class.methods.is_empty() {
                        node.kind = "field_object".to_string();
                    }
                }
            }
        }
    }

    node
}

fn render_class_member_children(
    index: &WorkspaceIndex,
    module: &ModuleInfo,
    class: &ClassInfo,
    external: bool,
) -> Vec<RenderNode> {
    let mut children = Vec::with_capacity(class.fields.len() + class.methods.len());
    for field in &class.fields {
        children.push(render_field_node(index, module, field));
    }
    for method in &class.methods {
        children.push(render_method_node(module, method, external));
    }
    children
}

fn build_super_render_node(
    index: &WorkspaceIndex,
    module_name: &str,
    class_name: &str,
    visited: &mut FxHashSet<String>,
) -> Result<RenderNode> {
    let key = format!("{module_name}::{class_name}");
    let (module, external) = get_module(index, module_name)
        .with_context(|| format!("module '{}' is not in the index", module_name))?;
    let class_index = *module
        .class_index
        .get(class_name)
        .with_context(|| format!("class '{}.{}' is not in the index", module_name, class_name))?;
    let class = &module.classes[class_index];

    let mut node = RenderNode::new(
        "class",
        class.name.clone(),
        module.path.clone(),
        class.line,
        class.col,
        module.path.clone(),
        class.line,
        class.col,
        external,
    );

    if !visited.insert(key.clone()) {
        node.recursive = true;
        return Ok(node);
    }

    node.children = render_class_member_children(index, &module, class, external);
    let mut class_children = Vec::new();
    for base in &class.bases {
        if base.contains('=') {
            continue;
        }
        match resolve_base(index, &module, base) {
            ResolvedBase::Workspace {
                module, class_name, ..
            }
            | ResolvedBase::External {
                module, class_name, ..
            } => {
                if let Ok(child) = build_super_render_node(index, &module, &class_name, visited) {
                    class_children.push(child);
                }
            }
            ResolvedBase::Unresolved { .. } => {}
        }
    }

    if class_children.is_empty() && class.name != "object" {
        if let Ok(object_node) = build_super_render_node(index, "builtins", "object", visited) {
            class_children.push(object_node);
        }
    }

    render_sort_nodes(&mut class_children);
    node.children.extend(class_children);
    visited.remove(&key);
    Ok(node)
}

fn find_direct_subtypes(
    index: &WorkspaceIndex,
    target_module: &str,
    target_class: &str,
) -> Vec<(String, ClassInfo)> {
    let mut matches = Vec::new();

    let target_key = class_key(target_module, target_class);
    for (module_name, class_name) in index.direct_subtypes.get(&target_key).into_iter().flatten() {
        let Some(module) = index.modules_by_name.get(module_name) else {
            continue;
        };
        let Some(class_index) = module.class_index.get(class_name) else {
            continue;
        };
        if let Some(class) = module.classes.get(*class_index) {
            matches.push((module_name.clone(), class.clone()));
        }
    }

    matches.sort_by(|(module_a, class_a), (module_b, class_b)| {
        module_a
            .cmp(module_b)
            .then(class_a.line.cmp(&class_b.line))
            .then(class_a.col.cmp(&class_b.col))
            .then(class_a.name.cmp(&class_b.name))
    });
    matches
}

fn build_sub_render_node_limited(
    index: &WorkspaceIndex,
    module_name: &str,
    class_name: &str,
    visited: &mut FxHashSet<String>,
    max_depth: Option<usize>,
    member_depth: Option<usize>,
) -> Result<RenderNode> {
    let key = format!("{module_name}::{class_name}");
    let (module, external) = get_module(index, module_name)
        .with_context(|| format!("module '{}' is not in the index", module_name))?;
    let class_index = *module
        .class_index
        .get(class_name)
        .with_context(|| format!("class '{}.{}' is not in the index", module_name, class_name))?;
    let class = &module.classes[class_index];

    let mut node = RenderNode::new(
        "class",
        class.name.clone(),
        module.path.clone(),
        class.line,
        class.col,
        module.path.clone(),
        class.line,
        class.col,
        external,
    );

    if !visited.insert(key.clone()) {
        node.recursive = true;
        return Ok(node);
    }

    let include_members = member_depth.unwrap_or(usize::MAX) > 0;
    if include_members {
        node.children = render_class_member_children(index, &module, class, external);
    } else {
        node.truncated = true;
    }

    let mut class_children = Vec::new();
    let direct_subtypes = find_direct_subtypes(index, module_name, class_name);
    let can_descend = max_depth.unwrap_or(usize::MAX) > 1;

    if can_descend {
        let next_depth = max_depth.map(|depth| depth.saturating_sub(1));
        let next_member_depth = member_depth.map(|depth| depth.saturating_sub(1));
        for (child_module_name, child_class) in direct_subtypes {
            if let Ok(child) = build_sub_render_node_limited(
                index,
                &child_module_name,
                &child_class.name,
                visited,
                next_depth,
                next_member_depth,
            ) {
                class_children.push(child);
            }
        }
    } else if !direct_subtypes.is_empty() {
        node.truncated = true;
    }

    if can_descend {
        render_sort_nodes(&mut class_children);
        node.children.extend(class_children);
    } else {
        render_sort_nodes(&mut node.children);
    }

    visited.remove(&key);
    Ok(node)
}

pub fn query_subtypes_tree_limited(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
    max_depth: Option<usize>,
    member_depth: Option<usize>,
) -> Result<RenderTreeResult> {
    let total_start = Instant::now();
    let (module_name, class, _) = locate_class_target(index, file, class_name)?;
    let mut visited = FxHashSet::default();
    let tree = build_sub_render_node_limited(
        index,
        &module_name,
        &class.name,
        &mut visited,
        max_depth,
        member_depth,
    )?;
    let total_ms = elapsed_ms(total_start);
    Ok(RenderTreeResult {
        tree,
        timings: Timings {
            discover_ms: 0.0,
            parse_ms: 0.0,
            index_ms: 0.0,
            query_ms: total_ms,
            total_ms,
        },
    })
}

pub fn query_subtypes_tree(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
) -> Result<RenderTreeResult> {
    query_subtypes_tree_limited(index, file, class_name, None, None)
}

fn locate_call_target(
    index: &WorkspaceIndex,
    item: &CallHierarchyItem,
) -> Option<(String, Arc<ModuleInfo>, FunctionInfo, bool)> {
    let (module_name, function) = locate_function_target(
        index,
        &item.path,
        &item.name,
        Some(item.line),
        Some(item.col),
    )
    .ok()?;
    let (module, external) = get_module(index, &module_name)?;
    Some((module_name, module, function, external))
}

fn build_call_leaf(
    item: &CallHierarchyItem,
    display_path: PathBuf,
    display_line: usize,
    display_col: usize,
) -> RenderNode {
    RenderNode::new(
        "call",
        item.name.clone(),
        display_path,
        display_line,
        display_col,
        item.path.clone(),
        item.line,
        item.col,
        item.external,
    )
}

fn build_call_render_node(
    index: &WorkspaceIndex,
    module_name: &str,
    module: &ModuleInfo,
    function: &FunctionInfo,
    display_path: PathBuf,
    display_line: usize,
    display_col: usize,
    external: bool,
    incoming: bool,
    visited: &mut FxHashSet<String>,
) -> RenderNode {
    let key = callable_key(module_name, function);
    let mut node = RenderNode::new(
        "call",
        function.name.clone(),
        display_path,
        display_line,
        display_col,
        module.path.clone(),
        function.line,
        function.col,
        external,
    );

    if !visited.insert(key.clone()) {
        node.recursive = true;
        return node;
    }

    let mut children = Vec::new();
    let mut edges = if incoming {
        index.incoming_calls.get(&key).cloned().unwrap_or_default()
    } else {
        index.outgoing_calls.get(&key).cloned().unwrap_or_default()
    };
    edges.sort_by(|a, b| {
        a.item
            .path
            .cmp(&b.item.path)
            .then(a.item.line.cmp(&b.item.line))
            .then(a.item.col.cmp(&b.item.col))
            .then(a.item.name.cmp(&b.item.name))
    });

    for edge in edges {
        let mut ranges = edge.from_ranges.clone();
        ranges.sort_by(|a, b| {
            a.start
                .line
                .cmp(&b.start.line)
                .then(a.start.col.cmp(&b.start.col))
                .then(a.end.line.cmp(&b.end.line))
                .then(a.end.col.cmp(&b.end.col))
        });

        for range in ranges {
            let display_path = if incoming {
                edge.item.path.clone()
            } else {
                module.path.clone()
            };
            let display_line = range.start.line;
            let display_col = range.start.col;

            let child =
                if let Some((child_module_name, child_module, child_function, child_external)) =
                    locate_call_target(index, &edge.item)
                {
                    build_call_render_node(
                        index,
                        &child_module_name,
                        &child_module,
                        &child_function,
                        display_path,
                        display_line,
                        display_col,
                        child_external,
                        incoming,
                        visited,
                    )
                } else {
                    build_call_leaf(&edge.item, display_path, display_line, display_col)
                };
            children.push(child);
        }
    }

    render_sort_nodes(&mut children);
    node.children = children;
    visited.remove(&key);
    node
}

pub fn query_supertypes_tree(
    index: &WorkspaceIndex,
    file: &Path,
    class_name: &str,
) -> Result<RenderTreeResult> {
    let total_start = Instant::now();
    let (module_name, class, _) = locate_class_target(index, file, class_name)?;
    let mut visited = FxHashSet::default();
    let tree = build_super_render_node(index, &module_name, &class.name, &mut visited)?;
    let total_ms = elapsed_ms(total_start);
    Ok(RenderTreeResult {
        tree,
        timings: Timings {
            discover_ms: 0.0,
            parse_ms: 0.0,
            index_ms: 0.0,
            query_ms: total_ms,
            total_ms,
        },
    })
}

pub fn query_outgoing_calls_tree(
    index: &WorkspaceIndex,
    file: &Path,
    symbol_name: &str,
    line: Option<usize>,
    col: Option<usize>,
) -> Result<RenderTreeResult> {
    let total_start = Instant::now();
    let (module_name, function) = locate_function_target(index, file, symbol_name, line, col)?;
    let (module, external) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;
    let mut visited = FxHashSet::default();
    let tree = build_call_render_node(
        index,
        &module_name,
        &module,
        &function,
        module.path.clone(),
        function.line,
        function.col,
        external,
        false,
        &mut visited,
    );
    let total_ms = elapsed_ms(total_start);
    Ok(RenderTreeResult {
        tree,
        timings: Timings {
            discover_ms: 0.0,
            parse_ms: 0.0,
            index_ms: 0.0,
            query_ms: total_ms,
            total_ms,
        },
    })
}

pub fn query_incoming_calls_tree(
    index: &WorkspaceIndex,
    file: &Path,
    symbol_name: &str,
    line: Option<usize>,
    col: Option<usize>,
) -> Result<RenderTreeResult> {
    let total_start = Instant::now();
    let (module_name, function) = locate_function_target(index, file, symbol_name, line, col)?;
    let (module, external) = get_module(index, &module_name)
        .with_context(|| format!("module '{}' is not part of the index", module_name))?;
    let mut visited = FxHashSet::default();
    let tree = build_call_render_node(
        index,
        &module_name,
        &module,
        &function,
        module.path.clone(),
        function.line,
        function.col,
        external,
        true,
        &mut visited,
    );
    let total_ms = elapsed_ms(total_start);
    Ok(RenderTreeResult {
        tree,
        timings: Timings {
            discover_ms: 0.0,
            parse_ms: 0.0,
            index_ms: 0.0,
            query_ms: total_ms,
            total_ms,
        },
    })
}

fn discover_python_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(false);
    builder.git_ignore(true);
    builder.git_exclude(true);
    builder.parents(true);
    builder.filter_entry(|entry| {
        let path = entry.path();
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            return !should_skip_dir(path);
        }
        true
    });

    let mut files = Vec::new();
    for entry in builder.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("py") | Some("pyi") => files.push(path),
            _ => {}
        }
    }
    files.sort();
    Ok(files)
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            matches!(
                name,
                ".git"
                    | ".hg"
                    | ".svn"
                    | ".mypy_cache"
                    | ".pytest_cache"
                    | ".ruff_cache"
                    | ".tox"
                    | ".venv"
                    | "__pycache__"
                    | "node_modules"
                    | "site-packages"
                    | "dist-packages"
            )
        })
        .unwrap_or(false)
}

fn parse_module(root: &Path, path: &Path) -> Result<ModuleInfo> {
    let module_name = module_name_from_path(root, path)?;
    parse_module_with_name(path, module_name)
}

fn parse_module_with_name(path: &Path, module_name: String) -> Result<ModuleInfo> {
    let package_name = package_name_for_module(path, &module_name);
    let source = read_source(path)?;
    let bytes = source_bytes(&source);
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(String::from_utf8_lossy(bytes).into_owned()),
    };

    let mut parser =
        ModuleParser::new(path.to_path_buf(), module_name, package_name, text.as_ref());
    parser.parse();
    Ok(parser.finish())
}

fn locate_module_file(import_root: &Path, module_name: &str) -> Option<PathBuf> {
    let segments = module_name.split('.').collect::<Vec<_>>();
    if segments.is_empty() {
        return None;
    }

    let mut dir = import_root.to_path_buf();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        dir.push(segment);
    }

    let leaf = segments[segments.len() - 1];
    let module_pyi = dir.join(format!("{leaf}.pyi"));
    if module_pyi.is_file() {
        return Some(module_pyi);
    }

    let module_py = dir.join(format!("{leaf}.py"));
    if module_py.is_file() {
        return Some(module_py);
    }

    let package_pyi = dir.join(leaf).join("__init__.pyi");
    if package_pyi.is_file() {
        return Some(package_pyi);
    }

    let package_py = dir.join(leaf).join("__init__.py");
    if package_py.is_file() {
        return Some(package_py);
    }

    None
}

fn load_external_module(index: &WorkspaceIndex, module_name: &str) -> Option<Arc<ModuleInfo>> {
    {
        let cache = index.external_modules.read();
        if let Some(module) = cache.get(module_name) {
            return Some(Arc::clone(module));
        }
    }

    let path = index
        .import_roots()
        .iter()
        .find_map(|root| locate_module_file(root, module_name))?;
    let module = Arc::new(parse_module_with_name(&path, module_name.to_string()).ok()?);

    {
        let mut cache = index.external_modules.write();
        cache.insert(module_name.to_string(), Arc::clone(&module));
    }
    Some(module)
}

fn load_workspace_module(index: &WorkspaceIndex, module_name: &str) -> Option<Arc<ModuleInfo>> {
    {
        let cache = index.lazy_workspace_modules.read();
        if let Some(module) = cache.get(module_name) {
            return Some(Arc::clone(module));
        }
    }

    let path = locate_module_file(&index.root, module_name)?;
    let module = Arc::new(parse_module_with_name(&path, module_name.to_string()).ok()?);

    {
        let mut path_cache = index.lazy_workspace_paths.write();
        path_cache.insert(module.path.clone(), module.module_name.clone());
    }
    {
        let mut cache = index.lazy_workspace_modules.write();
        cache.insert(module_name.to_string(), Arc::clone(&module));
    }

    Some(module)
}

fn get_module(index: &WorkspaceIndex, module_name: &str) -> Option<(Arc<ModuleInfo>, bool)> {
    if let Some(module) = index.modules_by_name.get(module_name) {
        return Some((Arc::clone(module), false));
    }
    if let Some(module) = load_workspace_module(index, module_name) {
        return Some((module, false));
    }
    load_external_module(index, module_name).map(|module| (module, true))
}

fn module_name_from_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", path.display(), root.display()))?;
    let mut segments = Vec::new();
    for component in relative.components() {
        let part = component.as_os_str().to_string_lossy();
        segments.push(part.to_string());
    }
    if segments.is_empty() {
        bail!("empty module path for {}", path.display());
    }

    if let Some(last) = segments.last_mut() {
        if last == "__init__.py" || last == "__init__.pyi" {
            segments.pop();
        } else if let Some(stripped) = last.strip_suffix(".py") {
            *last = stripped.to_string();
        } else if let Some(stripped) = last.strip_suffix(".pyi") {
            *last = stripped.to_string();
        }
    }

    Ok(segments
        .into_iter()
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("."))
}

fn package_name_for_module(path: &Path, module_name: &str) -> String {
    if matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("__init__.py") | Some("__init__.pyi")
    ) {
        module_name.to_string()
    } else {
        module_name
            .rsplit_once('.')
            .map(|(package, _)| package.to_string())
            .unwrap_or_default()
    }
}

struct ModuleParser<'a> {
    path: PathBuf,
    module_name: String,
    package_name: String,
    source: &'a str,
    bytes: &'a [u8],
    line_index: LineIndex<'a>,
    current_indent: usize,
    pending_scope: Option<ScopeFrame>,
    scopes: Vec<ScopeFrame>,
    statement_tokens: Vec<TokenSpan>,
    imports: FxHashMap<String, ImportBinding>,
    star_imports: Vec<String>,
    classes: Vec<ClassInfo>,
    functions: Vec<FunctionInfo>,
}

impl<'a> ModuleParser<'a> {
    fn new(path: PathBuf, module_name: String, package_name: String, source: &'a str) -> Self {
        let bytes = source.as_bytes();
        Self {
            path,
            module_name,
            package_name,
            source,
            bytes,
            line_index: LineIndex::new(bytes),
            current_indent: 0,
            pending_scope: None,
            scopes: Vec::new(),
            statement_tokens: Vec::new(),
            imports: FxHashMap::default(),
            star_imports: Vec::new(),
            classes: Vec::new(),
            functions: Vec::new(),
        }
    }

    fn parse(&mut self) {
        for token in lex(self.source, Mode::Module) {
            let Ok((kind, range)) = token else {
                continue;
            };
            let tag = token_tag(&kind);
            if tag == TokenKindTag::EndOfFile {
                self.finish_statement();
                break;
            }

            match tag {
                TokenKindTag::Indent => {
                    self.current_indent += 1;
                    self.activate_pending_scope();
                }
                TokenKindTag::Dedent => {
                    self.finish_statement();
                    self.current_indent = self.current_indent.saturating_sub(1);
                    self.trim_scopes();
                }
                TokenKindTag::Newline => {
                    self.finish_statement();
                }
                _ => {
                    if self.statement_tokens.is_empty() {
                        self.clear_stale_pending_scope();
                    }
                    self.statement_tokens.push(TokenSpan { kind, range });
                }
            }
        }
    }

    fn finish(mut self) -> ModuleInfo {
        // Fields typed as an enum class in the same module are never required.
        let enum_names: FxHashSet<String> = self
            .classes
            .iter()
            .filter(|c| is_enum_class(&c.bases))
            .map(|c| c.name.clone())
            .collect();
        if !enum_names.is_empty() {
            for class in &mut self.classes {
                if !is_enum_class(&class.bases) {
                    for field in &mut class.fields {
                        if field.type_ref.as_deref().is_some_and(|t| enum_names.contains(t)) {
                            field.required = false;
                        }
                    }
                }
            }
        }

        let class_index = self
            .classes
            .iter()
            .enumerate()
            .map(|(index, class)| (class.name.clone(), index))
            .collect();

        ModuleInfo {
            path: self.path,
            module_name: self.module_name,
            imports: self.imports,
            star_imports: self.star_imports,
            classes: self.classes,
            class_index,
            functions: self.functions,
        }
    }

    fn finish_statement(&mut self) {
        if self.statement_tokens.is_empty() {
            return;
        }

        self.process_statement();
        self.statement_tokens.clear();
    }

    fn process_statement(&mut self) {
        let Some(first) = self.statement_tokens.first().cloned() else {
            return;
        };

        if self.current_class_scope().is_none() && self.current_function_scope().is_none() {
            match token_tag(&first.kind) {
                TokenKindTag::Import => {
                    self.parse_import_statement();
                    return;
                }
                TokenKindTag::From => {
                    self.parse_from_import_statement();
                    return;
                }
                _ => {}
            }
            if self.parse_alias_statement() {
                return;
            }
            if self.parse_reference_assignment_statement() {
                return;
            }
        }

        if self.current_function_scope().is_none() && token_tag(&first.kind) == TokenKindTag::Class
        {
            self.parse_class_statement();
            return;
        }

        if self.is_function_statement() {
            if let Some(class_index) = self.class_body_target() {
                self.parse_function_statement(Some(class_index));
            } else {
                self.parse_function_statement(None);
            }
            return;
        }

        if let Some(class_index) = self.class_body_target() {
            if self.parse_field_statement(class_index) {
                return;
            }
            self.parse_assignment_field_statement(class_index);
            return;
        }

        if let Some(function_index) = self.current_function_target() {
            self.parse_call_statement(function_index);
        }
    }

    fn parse_import_statement(&mut self) {
        let mut index = 1;
        while index < self.statement_tokens.len() {
            let Some((module, next_index)) = self.parse_dotted_name(index) else {
                break;
            };
            let mut alias = module
                .rsplit('.')
                .next()
                .map(ToString::to_string)
                .unwrap_or_else(|| module.clone());
            index = next_index;
            if self.token_kind(index) == Some(TokenKindTag::As) {
                if let Some(name) = self.token_name(index + 1) {
                    alias = name.to_string();
                    index += 2;
                }
            }
            self.imports.insert(alias, ImportBinding::Module { module });
            if self.token_kind(index) == Some(TokenKindTag::Comma) {
                index += 1;
                continue;
            }
            break;
        }
    }

    fn parse_from_import_statement(&mut self) {
        let mut index = 1;
        let mut level = 0usize;
        while self.token_kind(index) == Some(TokenKindTag::Dot) {
            level += 1;
            index += 1;
        }

        let (module_part, next_index) = self
            .parse_dotted_name(index)
            .unwrap_or_else(|| (String::new(), index));
        index = next_index;
        if self.token_kind(index) != Some(TokenKindTag::Import) {
            return;
        }
        index += 1;
        if self.token_kind(index) == Some(TokenKindTag::Lpar) {
            index += 1;
        }

        let resolved_module = self.resolve_from_module(level, &module_part);

        while index < self.statement_tokens.len() {
            if self.token_kind(index) == Some(TokenKindTag::Rpar) {
                break;
            }
            if self.slice(self.statement_tokens[index].range) == "*" {
                self.star_imports.push(resolved_module.clone());
                break;
            }
            let Some(imported_name) = self.token_name(index).map(ToString::to_string) else {
                index += 1;
                continue;
            };
            let mut alias = imported_name.clone();
            index += 1;
            if self.token_kind(index) == Some(TokenKindTag::As) {
                if let Some(name) = self.token_name(index + 1) {
                    alias = name.to_string();
                    index += 2;
                }
            }

            if module_part.is_empty() {
                self.imports.insert(
                    alias,
                    ImportBinding::Module {
                        module: format!("{}.{}", resolved_module, imported_name),
                    },
                );
            } else {
                self.imports.insert(
                    alias,
                    ImportBinding::ImportedName {
                        module: resolved_module.clone(),
                        name: imported_name,
                    },
                );
            }

            if self.token_kind(index) == Some(TokenKindTag::Comma) {
                index += 1;
                continue;
            }
            break;
        }
    }

    fn parse_class_statement(&mut self) {
        let Some((name, name_index)) = self.name_after(0, TokenKindTag::Class) else {
            return;
        };
        let bases = self.extract_class_bases(name_index + 1);
        let (line, col) = self.line_index.line_col_at(text_start(name.range));
        let class_index = self.classes.len();
        self.classes.push(ClassInfo {
            name: self.slice(name.range),
            line,
            col,
            bases,
            methods: Vec::new(),
            fields: Vec::new(),
        });
        self.pending_scope = Some(ScopeFrame {
            body_indent: self.current_indent + 1,
            kind: ScopeKind::Class(class_index),
        });
    }

    fn parse_function_statement(&mut self, class_index: Option<usize>) {
        let def_index = if self.token_kind(0) == Some(TokenKindTag::Async) {
            1
        } else {
            0
        };
        let Some((name, _)) = self.name_after(def_index, TokenKindTag::Def) else {
            return;
        };
        let method_name = self.slice(name.range);
        let (line, col) = self.line_index.line_col_at(text_start(name.range));
        let kind = if class_index.is_some() { 12 } else { 6 };
        if let Some(class_index) = class_index {
            self.classes[class_index].methods.push(MethodInfo {
                name: method_name.clone(),
                line,
                col,
            });
        }
        let function_index = self.functions.len();
        self.functions.push(FunctionInfo {
            name: method_name,
            line,
            col,
            kind,
            class_name: class_index.map(|index| self.classes[index].name.clone()),
            calls: Vec::new(),
        });
        self.pending_scope = Some(ScopeFrame {
            body_indent: self.current_indent + 1,
            kind: ScopeKind::Function(function_index),
        });
    }

    fn parse_field_statement(&mut self, class_index: usize) -> bool {
        if self.statement_tokens.len() < 3 {
            return false;
        }
        if token_tag(&self.statement_tokens[0].kind) != TokenKindTag::Name
            || token_tag(&self.statement_tokens[1].kind) != TokenKindTag::Colon
        {
            return false;
        }

        let end_index = self
            .top_level_equal_index()
            .unwrap_or_else(|| self.statement_tokens.len());
        if end_index <= 2 {
            return false;
        }

        let name = self.slice(self.statement_tokens[0].range);
        let annotation = self.slice_span(2, end_index - 1);
        let annotation = normalize_inline_whitespace(&annotation);
        let has_default = end_index < self.statement_tokens.len();
        let required = !has_default && annotation_is_required(&annotation);
        let type_ref = normalize_type_head(&annotation);
        let (line, col) = self
            .line_index
            .line_col_at(text_start(self.statement_tokens[0].range));
        let (type_line, type_col) = self
            .line_index
            .line_col_at(text_start(self.statement_tokens[2].range));
        let type_ref = (!type_ref.is_empty()).then_some(type_ref);
        self.classes[class_index].fields.push(FieldInfo {
            name,
            annotation,
            required,
            line,
            col,
            type_ref: type_ref.clone(),
            type_line: Some(type_line),
            type_col: Some(type_col),
        });
        if let Some(type_ref) = type_ref {
            self.push_reference_node(
                self.classes[class_index]
                    .fields
                    .last()
                    .map(|field| field.name.clone())
                    .unwrap_or_default(),
                line,
                col,
                8,
                Some(self.classes[class_index].name.clone()),
                vec![CallSite {
                    raw_target: type_ref.clone(),
                    range: self.reference_range_from_text(type_line, type_col, &type_ref),
                }],
            );
        }
        true
    }

    fn parse_assignment_field_statement(&mut self, class_index: usize) -> bool {
        if self.statement_tokens.len() < 3 {
            return false;
        }
        if self.token_kind(0) != Some(TokenKindTag::Name)
            || self.token_kind(1) != Some(TokenKindTag::Equal)
        {
            return false;
        }

        let name = self.slice(self.statement_tokens[0].range);
        let raw_annotation =
            normalize_inline_whitespace(&self.slice_span(2, self.statement_tokens.len() - 1));
        if raw_annotation.is_empty() {
            return false;
        }
        let in_enum = is_enum_class(&self.classes[class_index].bases);
        let (annotation, type_ref) = if in_enum {
            let t = if raw_annotation.starts_with('"') || raw_annotation.starts_with('\'') {
                "str"
            } else if raw_annotation.bytes().next().map_or(false, |b| b.is_ascii_digit() || b == b'-') {
                "int"
            } else {
                ""
            };
            (t.to_string(), None)
        } else {
            let tr = normalize_type_head(&raw_annotation);
            let tr = (!tr.is_empty()).then_some(tr);
            (raw_annotation, tr)
        };
        let (line, col) = self
            .line_index
            .line_col_at(text_start(self.statement_tokens[0].range));
        let (type_line, type_col) = self
            .line_index
            .line_col_at(text_start(self.statement_tokens[2].range));
        self.classes[class_index].fields.push(FieldInfo {
            name,
            annotation,
            required: !in_enum,
            line,
            col,
            type_ref: type_ref.clone(),
            type_line: Some(type_line),
            type_col: Some(type_col),
        });
        if let Some(type_ref) = type_ref {
            self.push_reference_node(
                self.classes[class_index]
                    .fields
                    .last()
                    .map(|field| field.name.clone())
                    .unwrap_or_default(),
                line,
                col,
                8,
                Some(self.classes[class_index].name.clone()),
                vec![CallSite {
                    raw_target: type_ref.clone(),
                    range: self.reference_range_from_text(type_line, type_col, &type_ref),
                }],
            );
        }
        true
    }

    fn parse_alias_statement(&mut self) -> bool {
        if self.statement_tokens.len() < 3 {
            return false;
        }
        if self.token_kind(0) != Some(TokenKindTag::Name)
            || self.token_kind(1) != Some(TokenKindTag::Equal)
        {
            return false;
        }

        let Some(alias) = self.token_name(0).map(ToString::to_string) else {
            return false;
        };
        let Some(binding) = self.resolve_alias_binding(2) else {
            return false;
        };
        self.imports.insert(alias, binding);
        true
    }

    fn parse_reference_assignment_statement(&mut self) -> bool {
        if self.statement_tokens.len() < 3 {
            return false;
        }
        if self.token_kind(0) != Some(TokenKindTag::Name)
            || self.token_kind(1) != Some(TokenKindTag::Equal)
        {
            return false;
        }

        let Some(name) = self.token_name(0).map(ToString::to_string) else {
            return false;
        };
        let refs = self.extract_reference_sites(2, self.statement_tokens.len());
        if refs.is_empty() {
            return false;
        }
        let (line, col) = self
            .line_index
            .line_col_at(text_start(self.statement_tokens[0].range));
        self.push_reference_node(name, line, col, 13, None, refs);
        true
    }

    fn resolve_alias_binding(&self, start: usize) -> Option<ImportBinding> {
        let (rhs, next_index) = self.parse_dotted_name(start)?;
        if next_index != self.statement_tokens.len() {
            return None;
        }

        let segments = rhs.split('.').collect::<Vec<_>>();
        if segments.is_empty() {
            return None;
        }

        if segments.len() == 1 {
            let head = segments[0];
            if let Some(binding) = self.imports.get(head) {
                return Some(binding.clone());
            }
            if self.classes.iter().any(|class| class.name == head) {
                return Some(ImportBinding::ImportedName {
                    module: self.module_name.clone(),
                    name: head.to_string(),
                });
            }
            return None;
        }

        let head = segments[0];
        let tail = &segments[1..];
        let binding = self.imports.get(head)?;
        match binding {
            ImportBinding::Module { module } => {
                if tail.len() == 1 {
                    Some(ImportBinding::ImportedName {
                        module: module.clone(),
                        name: tail[0].to_string(),
                    })
                } else {
                    let module_name = format!("{}.{}", module, tail[..tail.len() - 1].join("."));
                    Some(ImportBinding::ImportedName {
                        module: module_name,
                        name: tail[tail.len() - 1].to_string(),
                    })
                }
            }
            ImportBinding::ImportedName { module, name } => {
                let mut qualified = vec![name.as_str()];
                qualified.extend_from_slice(tail);
                if qualified.len() == 1 {
                    Some(ImportBinding::ImportedName {
                        module: module.clone(),
                        name: qualified[0].to_string(),
                    })
                } else {
                    let module_name =
                        format!("{}.{}", module, qualified[..qualified.len() - 1].join("."));
                    Some(ImportBinding::ImportedName {
                        module: module_name,
                        name: qualified[qualified.len() - 1].to_string(),
                    })
                }
            }
        }
    }

    fn parse_call_statement(&mut self, function_index: usize) {
        let mut index = 0usize;
        while index < self.statement_tokens.len() {
            if self.token_kind(index) != Some(TokenKindTag::Name) {
                index += 1;
                continue;
            }
            if index > 0 && self.token_kind(index - 1) == Some(TokenKindTag::Dot) {
                index += 1;
                continue;
            }

            let Some((raw_target, next_index, last_name_index)) = self.parse_call_chain(index)
            else {
                index += 1;
                continue;
            };
            if self.token_kind(next_index) != Some(TokenKindTag::Lpar) {
                index += 1;
                continue;
            }

            let start_token = &self.statement_tokens[index];
            let end_token = &self.statement_tokens[last_name_index];
            let (start_line, start_col) =
                self.line_index.line_col_at(text_start(start_token.range));
            let (end_line, end_col) = self.line_index.line_col_at(text_end(end_token.range));
            self.functions[function_index].calls.push(CallSite {
                raw_target,
                range: SourceRange {
                    start: RangePoint {
                        line: start_line,
                        col: start_col,
                    },
                    end: RangePoint {
                        line: end_line,
                        col: end_col,
                    },
                },
            });

            index = next_index + 1;
        }
    }

    fn parse_call_chain(&self, start: usize) -> Option<(String, usize, usize)> {
        let mut index = start;
        let mut segments = Vec::new();
        let first = self.statement_tokens.get(index)?;
        if token_tag(&first.kind) != TokenKindTag::Name {
            return None;
        }
        segments.push(self.slice(first.range));
        let mut last_name_index = index;
        index += 1;

        while self.token_kind(index) == Some(TokenKindTag::Dot)
            && self.token_kind(index + 1) == Some(TokenKindTag::Name)
        {
            segments.push(self.slice(self.statement_tokens[index + 1].range));
            last_name_index = index + 1;
            index += 2;
        }

        Some((segments.join("."), index, last_name_index))
    }

    fn extract_reference_sites(&mut self, start: usize, end: usize) -> Vec<CallSite> {
        let mut refs = Vec::new();
        let mut seen = FxHashSet::default();
        let mut index = start;

        while index < end {
            if self.token_kind(index) != Some(TokenKindTag::Name) {
                index += 1;
                continue;
            }
            if index > start && self.token_kind(index - 1) == Some(TokenKindTag::Dot) {
                index += 1;
                continue;
            }

            let Some((raw_target, next_index, last_name_index)) =
                self.parse_reference_chain(index, end)
            else {
                index += 1;
                continue;
            };

            if next_index < end && self.token_kind(next_index) == Some(TokenKindTag::Lpar) {
                index = next_index + 1;
                continue;
            }
            if next_index < end && self.token_kind(next_index) == Some(TokenKindTag::Equal) {
                index = next_index + 1;
                continue;
            }

            let start_token = &self.statement_tokens[index];
            let end_token = &self.statement_tokens[last_name_index];
            let (start_line, start_col) =
                self.line_index.line_col_at(text_start(start_token.range));
            let (end_line, end_col) = self.line_index.line_col_at(text_end(end_token.range));
            let dedupe_key = (
                raw_target.clone(),
                start_line,
                start_col,
                end_line,
                end_col,
            );
            if seen.insert(dedupe_key) {
                refs.push(CallSite {
                    raw_target,
                    range: SourceRange {
                        start: RangePoint {
                            line: start_line,
                            col: start_col,
                        },
                        end: RangePoint {
                            line: end_line,
                            col: end_col,
                        },
                    },
                });
            }

            index = next_index.max(index + 1);
        }

        refs
    }

    fn parse_reference_chain(&self, start: usize, end: usize) -> Option<(String, usize, usize)> {
        let mut index = start;
        let mut segments = Vec::new();
        let first = self.statement_tokens.get(index)?;
        if token_tag(&first.kind) != TokenKindTag::Name {
            return None;
        }
        segments.push(self.slice(first.range));
        let mut last_name_index = index;
        index += 1;

        while index + 1 < end
            && self.token_kind(index) == Some(TokenKindTag::Dot)
            && self.token_kind(index + 1) == Some(TokenKindTag::Name)
        {
            segments.push(self.slice(self.statement_tokens[index + 1].range));
            last_name_index = index + 1;
            index += 2;
        }

        Some((segments.join("."), index, last_name_index))
    }

    fn extract_class_bases(&self, mut index: usize) -> Vec<String> {
        while self.token_kind(index) == Some(TokenKindTag::Lsqb) {
            index = self.skip_balanced(index, TokenKindTag::Lsqb, TokenKindTag::Rsqb);
        }
        if self.token_kind(index) != Some(TokenKindTag::Lpar) {
            return Vec::new();
        }

        let mut parts = Vec::new();
        let mut depth_paren = 0usize;
        let mut depth_square = 0usize;
        let mut depth_brace = 0usize;
        let mut segment_start: Option<usize> = None;
        let mut position = index;

        while position < self.statement_tokens.len() {
            let token = &self.statement_tokens[position];
            match &token.kind {
                Tok::Lpar => {
                    depth_paren += 1;
                    if depth_paren == 1 {
                        position += 1;
                        segment_start = Some(position);
                        continue;
                    }
                }
                Tok::Rpar => {
                    if depth_paren == 1 {
                        if let Some(start) = segment_start {
                            if start < position {
                                let text = normalize_inline_whitespace(
                                    &self.slice_span(start, position - 1),
                                );
                                if !text.is_empty() {
                                    parts.push(text);
                                }
                            }
                        }
                        break;
                    }
                    depth_paren = depth_paren.saturating_sub(1);
                }
                Tok::Lsqb => depth_square += 1,
                Tok::Rsqb => depth_square = depth_square.saturating_sub(1),
                Tok::Lbrace => depth_brace += 1,
                Tok::Rbrace => depth_brace = depth_brace.saturating_sub(1),
                Tok::Comma if depth_paren == 1 && depth_square == 0 && depth_brace == 0 => {
                    if let Some(start) = segment_start {
                        if start < position {
                            let text =
                                normalize_inline_whitespace(&self.slice_span(start, position - 1));
                            if !text.is_empty() {
                                parts.push(text);
                            }
                        }
                    }
                    segment_start = Some(position + 1);
                }
                _ => {}
            }
            position += 1;
        }

        parts
    }

    fn resolve_from_module(&self, level: usize, module_part: &str) -> String {
        if level == 0 {
            return module_part.to_string();
        }

        let mut segments = if self.package_name.is_empty() {
            Vec::new()
        } else {
            self.package_name
                .split('.')
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        for _ in 1..level {
            segments.pop();
        }
        if !module_part.is_empty() {
            segments.extend(module_part.split('.').map(ToString::to_string));
        }
        segments.join(".")
    }

    fn top_level_equal_index(&self) -> Option<usize> {
        let mut square = 0usize;
        let mut paren = 0usize;
        let mut brace = 0usize;
        for (index, token) in self.statement_tokens.iter().enumerate().skip(2) {
            match &token.kind {
                Tok::Lsqb => square += 1,
                Tok::Rsqb => square = square.saturating_sub(1),
                Tok::Lpar => paren += 1,
                Tok::Rpar => paren = paren.saturating_sub(1),
                Tok::Lbrace => brace += 1,
                Tok::Rbrace => brace = brace.saturating_sub(1),
                Tok::Equal if square == 0 && paren == 0 && brace == 0 => return Some(index),
                _ => {}
            }
        }
        None
    }

    fn is_function_statement(&self) -> bool {
        matches!(
            (self.token_kind(0), self.token_kind(1), self.token_kind(2)),
            (Some(TokenKindTag::Def), Some(TokenKindTag::Name), _)
                | (
                    Some(TokenKindTag::Async),
                    Some(TokenKindTag::Def),
                    Some(TokenKindTag::Name)
                )
        )
    }

    fn class_body_target(&self) -> Option<usize> {
        if self.current_function_scope().is_some() {
            return None;
        }
        let scope = self.current_class_scope()?;
        if scope.body_indent != self.current_indent {
            return None;
        }
        match scope.kind {
            ScopeKind::Class(index) => Some(index),
            ScopeKind::Function(_) => None,
        }
    }

    fn current_function_target(&self) -> Option<usize> {
        match self.current_function_scope()?.kind {
            ScopeKind::Function(index) => Some(index),
            ScopeKind::Class(_) => None,
        }
    }

    fn activate_pending_scope(&mut self) {
        if let Some(scope) = self.pending_scope.take() {
            self.scopes.push(scope);
        }
    }

    fn clear_stale_pending_scope(&mut self) {
        if self
            .pending_scope
            .as_ref()
            .is_some_and(|pending| self.current_indent < pending.body_indent)
        {
            self.pending_scope = None;
        }
    }

    fn trim_scopes(&mut self) {
        while self
            .scopes
            .last()
            .is_some_and(|scope| scope.body_indent > self.current_indent)
        {
            self.scopes.pop();
        }
    }

    fn current_class_scope(&self) -> Option<&ScopeFrame> {
        self.scopes
            .iter()
            .rev()
            .find(|scope| matches!(scope.kind, ScopeKind::Class(_)))
    }

    fn current_function_scope(&self) -> Option<&ScopeFrame> {
        self.scopes
            .iter()
            .rev()
            .find(|scope| matches!(scope.kind, ScopeKind::Function(_)))
    }

    fn name_after(&self, index: usize, expected: TokenKindTag) -> Option<(TokenSpan, usize)> {
        if self.token_kind(index) != Some(expected) {
            return None;
        }
        let token = self.statement_tokens.get(index + 1).cloned()?;
        if token_tag(&token.kind) != TokenKindTag::Name {
            return None;
        }
        Some((token, index + 1))
    }

    fn parse_dotted_name(&self, start: usize) -> Option<(String, usize)> {
        let mut index = start;
        let mut segments = Vec::new();
        let first = self.statement_tokens.get(index)?;
        if token_tag(&first.kind) != TokenKindTag::Name {
            return None;
        }
        segments.push(self.slice(first.range));
        index += 1;
        while self.token_kind(index) == Some(TokenKindTag::Dot)
            && self.token_kind(index + 1) == Some(TokenKindTag::Name)
        {
            segments.push(self.slice(self.statement_tokens[index + 1].range));
            index += 2;
        }
        Some((segments.join("."), index))
    }

    fn skip_balanced(&self, start: usize, open: TokenKindTag, close: TokenKindTag) -> usize {
        let mut depth = 0usize;
        let mut index = start;
        while index < self.statement_tokens.len() {
            match token_tag(&self.statement_tokens[index].kind) {
                kind if kind == open => depth += 1,
                kind if kind == close => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return index + 1;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        index
    }

    fn token_kind(&self, index: usize) -> Option<TokenKindTag> {
        self.statement_tokens
            .get(index)
            .map(|token| token_tag(&token.kind))
    }

    fn token_name(&self, index: usize) -> Option<&str> {
        let token = self.statement_tokens.get(index)?;
        if let Tok::Name { name } = &token.kind {
            return Some(name.as_str());
        }
        None
    }

    fn slice(&self, range: TextRange) -> String {
        self.bytes
            .get(text_start(range)..text_end(range))
            .map(|slice| String::from_utf8_lossy(slice).to_string())
            .unwrap_or_default()
    }

    fn slice_span(&self, start: usize, end: usize) -> String {
        let Some(first) = self.statement_tokens.get(start) else {
            return String::new();
        };
        let Some(last) = self.statement_tokens.get(end) else {
            return String::new();
        };
        self.bytes
            .get(text_start(first.range)..text_end(last.range))
            .map(|slice| String::from_utf8_lossy(slice).to_string())
            .unwrap_or_default()
    }

    fn push_reference_node(
        &mut self,
        name: String,
        line: usize,
        col: usize,
        kind: u32,
        class_name: Option<String>,
        calls: Vec<CallSite>,
    ) {
        if calls.is_empty() {
            return;
        }
        self.functions.push(FunctionInfo {
            name,
            line,
            col,
            kind,
            class_name,
            calls,
        });
    }

    fn reference_range_from_text(&self, line: usize, col: usize, text: &str) -> SourceRange {
        let width = text.chars().count().saturating_sub(1);
        SourceRange {
            start: RangePoint { line, col },
            end: RangePoint {
                line,
                col: col + width,
            },
        }
    }
}

fn build_class_node(
    index: &WorkspaceIndex,
    module_name: &str,
    class_name: &str,
    display_base: &str,
    visited: &mut FxHashSet<String>,
) -> Result<HierarchyNode> {
    let key = format!("{module_name}::{class_name}");
    if !visited.insert(key.clone()) {
        let mut node = HierarchyNode::new(
            class_name.to_string(),
            module_name.to_string(),
            display_base.to_string(),
            None,
            None,
            None,
            false,
        );
        node.recursive = true;
        return Ok(node);
    }

    let (module, external) = get_module(index, module_name)
        .with_context(|| format!("module '{}' is not in the index", module_name))?;
    let class_index = *module
        .class_index
        .get(class_name)
        .with_context(|| format!("class '{}.{}' is not in the index", module_name, class_name))?;
    let class = &module.classes[class_index];

    let mut ancestors = Vec::new();
    for base in &class.bases {
        if base.contains('=') {
            continue;
        }
        match resolve_base(index, &module, base) {
            ResolvedBase::Workspace {
                module,
                class_name,
                display,
            } => {
                ancestors.push(build_class_node(
                    index,
                    &module,
                    &class_name,
                    &display,
                    visited,
                )?);
            }
            ResolvedBase::External {
                module,
                class_name,
                display,
            } => {
                if let Ok(node) = build_class_node(index, &module, &class_name, &display, visited) {
                    ancestors.push(node);
                } else {
                    ancestors.push(HierarchyNode::new(
                        class_name.clone(),
                        module.clone(),
                        display,
                        None,
                        None,
                        None,
                        true,
                    ));
                }
            }
            ResolvedBase::Unresolved { display } => ancestors.push(HierarchyNode::new(
                display.clone(),
                String::new(),
                display,
                None,
                None,
                None,
                true,
            )),
        }
    }
    if ancestors.is_empty() && class.name != "object" {
        if let Ok(node) = build_class_node(index, "builtins", "object", "object", visited) {
            ancestors.push(node);
        }
    }

    visited.remove(&key);
    Ok(HierarchyNode {
        raw_bases: class.bases.clone(),
        methods: class.methods.clone(),
        fields: class.fields.clone(),
        ancestors,
        ..HierarchyNode::new(
            class.name.clone(),
            module.module_name.clone(),
            display_base.to_string(),
            Some(module.path.clone()),
            Some(class.line),
            Some(class.col),
            external,
        )
    })
}

fn resolve_base(index: &WorkspaceIndex, module: &ModuleInfo, raw_base: &str) -> ResolvedBase {
    let display = raw_base.to_string();
    if raw_base.contains('=') {
        return ResolvedBase::Unresolved { display };
    }
    let normalized = normalize_type_head(raw_base);
    if normalized.is_empty() {
        return ResolvedBase::Unresolved { display };
    }

    let segments = normalized.split('.').collect::<Vec<_>>();
    if segments.is_empty() {
        return ResolvedBase::Unresolved { display };
    }

    if segments.len() == 1 {
        let name = segments[0];
        if module.class_index.contains_key(name) {
            return ResolvedBase::Workspace {
                module: module.module_name.clone(),
                class_name: name.to_string(),
                display,
            };
        }
        if let Some(binding) = module.imports.get(name) {
            return resolve_import_binding(index, binding, &[], &display);
        }
        if let Some(resolved) = resolve_named_symbol(index, &module.module_name, name, &display) {
            return resolved;
        }
        if let Some(resolved) = resolve_named_symbol(index, "builtins", name, &display) {
            return resolved;
        }
        if name == "object" {
            return ResolvedBase::External {
                module: "builtins".to_string(),
                class_name: "object".to_string(),
                display,
            };
        }
        return ResolvedBase::Unresolved { display };
    }

    let head = segments[0];
    let tail = &segments[1..];
    if let Some(binding) = module.imports.get(head) {
        return resolve_import_binding(index, binding, tail, &display);
    }

    let external_module = segments[..segments.len() - 1].join(".");
    ResolvedBase::External {
        module: external_module,
        class_name: segments[segments.len() - 1].to_string(),
        display,
    }
}

fn resolve_import_binding(
    index: &WorkspaceIndex,
    binding: &ImportBinding,
    tail: &[&str],
    display: &str,
) -> ResolvedBase {
    match binding {
        ImportBinding::Module { module } => resolve_module_path(index, module, tail, display),
        ImportBinding::ImportedName { module, name } => {
            if tail.is_empty() {
                if let Some(resolved) = resolve_named_symbol(index, module, name, display) {
                    return resolved;
                }
                let module_candidate = format!("{module}.{name}");
                return resolve_module_path(index, &module_candidate, &[], display);
            }

            let module_candidate = format!("{module}.{name}");
            resolve_module_path(index, &module_candidate, tail, display)
        }
    }
}

fn resolve_named_symbol(
    index: &WorkspaceIndex,
    module_name: &str,
    symbol_name: &str,
    display: &str,
) -> Option<ResolvedBase> {
    let mut visited = FxHashSet::default();
    resolve_named_symbol_inner(index, module_name, symbol_name, display, &mut visited)
}

fn resolve_named_symbol_inner(
    index: &WorkspaceIndex,
    module_name: &str,
    symbol_name: &str,
    display: &str,
    visited: &mut FxHashSet<(String, String)>,
) -> Option<ResolvedBase> {
    let key = (module_name.to_string(), symbol_name.to_string());
    if !visited.insert(key.clone()) {
        return None;
    }

    let (module, external) = get_module(index, module_name)?;
    if module.class_index.contains_key(symbol_name) {
        visited.remove(&key);
        let resolved = if external {
            ResolvedBase::External {
                module: module_name.to_string(),
                class_name: symbol_name.to_string(),
                display: display.to_string(),
            }
        } else {
            ResolvedBase::Workspace {
                module: module_name.to_string(),
                class_name: symbol_name.to_string(),
                display: display.to_string(),
            }
        };
        return Some(resolved);
    }

    if let Some(binding) = module.imports.get(symbol_name) {
        visited.remove(&key);
        return Some(resolve_import_binding(index, binding, &[], display));
    }

    for star_module in &module.star_imports {
        if let Some(resolved) =
            resolve_named_symbol_inner(index, star_module, symbol_name, display, visited)
        {
            visited.remove(&key);
            return Some(resolved);
        }
    }

    visited.remove(&key);
    None
}

fn resolve_module_path(
    index: &WorkspaceIndex,
    module_path: &str,
    tail: &[&str],
    display: &str,
) -> ResolvedBase {
    let (module_name, class_name) = if tail.is_empty() {
        match module_path.rsplit_once('.') {
            Some((module, class_name)) => (module.to_string(), class_name.to_string()),
            None => {
                return ResolvedBase::Unresolved {
                    display: display.to_string(),
                }
            }
        }
    } else if tail.len() == 1 {
        (module_path.to_string(), tail[0].to_string())
    } else {
        (
            format!("{module_path}.{}", tail[..tail.len() - 1].join(".")),
            tail[tail.len() - 1].to_string(),
        )
    };

    if let Some((module, external)) = get_module(index, &module_name) {
        if module.class_index.contains_key(&class_name) {
            return if external {
                ResolvedBase::External {
                    module: module_name,
                    class_name,
                    display: display.to_string(),
                }
            } else {
                ResolvedBase::Workspace {
                    module: module_name,
                    class_name,
                    display: display.to_string(),
                }
            };
        }
        if let Some(resolved) = resolve_named_symbol(index, &module_name, &class_name, display) {
            return resolved;
        }
    }

    ResolvedBase::External {
        module: module_name,
        class_name,
        display: display.to_string(),
    }
}

fn is_enum_class(bases: &[String]) -> bool {
    bases.iter().any(|base| {
        let head = base.split('.').next_back().unwrap_or(base);
        matches!(head, "Enum" | "IntEnum" | "StrEnum" | "Flag" | "IntFlag")
    })
}

fn annotation_is_required(annotation: &str) -> bool {
    let flat: Vec<u8> = annotation
        .bytes()
        .filter(|&b| b != b' ')
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let s = flat.as_slice();
    !contains_bytes(s, b"|none") && !contains_bytes(s, b"optional[") && !contains_bytes(s, b"=none")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn normalize_type_head(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    let mut angle = 0usize;
    let mut square = 0usize;
    let mut paren = 0usize;
    for ch in trimmed.chars() {
        match ch {
            '[' => {
                square += 1;
                if square == 1 {
                    break;
                }
            }
            '(' => {
                paren += 1;
                if paren == 1 {
                    break;
                }
            }
            '|' | ',' if angle == 0 && square == 0 && paren == 0 => break,
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            ' ' | '\t' | '\r' | '\n' => {}
            _ => result.push(ch),
        }
    }
    result
}

fn normalize_inline_whitespace(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut first = true;
    for word in input.split_whitespace() {
        if !first {
            result.push(' ');
        }
        result.push_str(word);
        first = false;
    }
    result
}

fn read_source(path: &Path) -> Result<Source> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.len() == 0 {
        return Ok(Source::Heap(Vec::new()));
    }

    // SAFETY: The file is opened read-only and we do not mutate the mapping.
    // The underlying file is not modified while the mapping is alive because
    // this is a short-lived CLI tool that only reads workspace sources.
    let mmap = unsafe { Mmap::map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))?;
    Ok(Source::Mmap(mmap))
}

fn source_bytes(source: &Source) -> &[u8] {
    match source {
        Source::Heap(bytes) => bytes.as_slice(),
        Source::Mmap(bytes) => bytes.as_ref(),
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn text_start(range: TextRange) -> usize {
    usize::from(range.start())
}

fn text_end(range: TextRange) -> usize {
    usize::from(range.end())
}

struct LineIndex<'a> {
    bytes: &'a [u8],
    line_starts: Option<Vec<usize>>,
    cursor: usize,
}

impl<'a> LineIndex<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            line_starts: None,
            cursor: 0,
        }
    }

    fn line_col_at(&mut self, offset: usize) -> (usize, usize) {
        if self.line_starts.is_none() {
            self.line_starts = Some(build_line_starts(self.bytes));
        }
        let line_starts = self.line_starts.as_ref().expect("line starts");
        line_col_at_fast(offset, line_starts, &mut self.cursor)
    }
}

fn build_line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::with_capacity(bytes.len() / 20 + 2);
    starts.push(0);
    for index in memchr_iter(b'\n', bytes) {
        if index + 1 < bytes.len() {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_col_at_fast(offset: usize, line_starts: &[usize], cursor: &mut usize) -> (usize, usize) {
    if line_starts.is_empty() {
        return (1, offset + 1);
    }
    if *cursor < line_starts.len() && offset >= line_starts[*cursor] {
        while *cursor + 1 < line_starts.len() && line_starts[*cursor + 1] <= offset {
            *cursor += 1;
        }
        let line_start = line_starts[*cursor];
        return (*cursor + 1, offset - line_start + 1);
    }

    match line_starts.binary_search(&offset) {
        Ok(index) => {
            *cursor = index;
            (index + 1, 1)
        }
        Err(index) => {
            let line_index = index.saturating_sub(1);
            *cursor = line_index;
            let line_start = line_starts[line_index];
            (line_index + 1, offset - line_start + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hierarchy-parser-test-{}-{}",
            std::process::id(),
            nanos
        ))
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, contents).expect("write file");
    }

    fn build_test_index(files: &[(&str, &str)]) -> WorkspaceIndex {
        let root = PathBuf::from("/tmp/project");
        let modules = files
            .iter()
            .map(|(relative, source)| {
                let path = root.join(relative);
                let module_name = module_name_from_path(&root, &path).unwrap();
                let package_name = package_name_for_module(&path, &module_name);
                let mut parser = ModuleParser::new(path.clone(), module_name, package_name, source);
                parser.parse();
                parser.finish()
            })
            .collect::<Vec<_>>();

        let mut modules_by_name = FxHashMap::default();
        let mut modules_by_path = FxHashMap::default();
        for module in modules {
            modules_by_path.insert(module.path.clone(), module.module_name.clone());
            modules_by_name.insert(module.module_name.clone(), Arc::new(module));
        }

        let mut index = WorkspaceIndex {
            root,
            modules_by_name,
            modules_by_path,
            lazy_workspace_modules: Arc::new(RwLock::new(FxHashMap::default())),
            lazy_workspace_paths: Arc::new(RwLock::new(FxHashMap::default())),
            import_roots: Arc::new(RwLock::new(vec![
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stubs")
            ])),
            external_modules: Arc::new(RwLock::new(FxHashMap::default())),
            direct_subtypes: FxHashMap::default(),
            outgoing_calls: FxHashMap::default(),
            incoming_calls: FxHashMap::default(),
        };
        index.direct_subtypes = build_direct_subtype_index(&index);
        let (outgoing_calls, incoming_calls) = build_call_indices(&index);
        index.outgoing_calls = outgoing_calls;
        index.incoming_calls = incoming_calls;
        index
    }

    #[test]
    fn parses_class_members_and_resolves_relative_imports() {
        let index = build_test_index(&[
            (
                "src/pkg/common.py",
                "class BaseModel:\n    collection_id: int\n    def save(self):\n        return 1\n",
            ),
            (
                "src/pkg/model.py",
                "from . import common\n\nclass Prize(common.BaseModel):\n    slug: str\n    image: str | None\n\n    def default_slug(self):\n        return self.slug\n",
            ),
        ]);

        let file = PathBuf::from("/tmp/project/src/pkg/model.py");
        let result = query_type_hierarchy(&index, &file, "Prize").unwrap();

        assert_eq!(result.hierarchy.name, "Prize");
        assert_eq!(result.hierarchy.fields.len(), 2);
        assert!(result.hierarchy.fields[0].required);
        assert!(!result.hierarchy.fields[1].required);
        assert_eq!(result.hierarchy.ancestors.len(), 1);
        assert_eq!(result.hierarchy.ancestors[0].name, "BaseModel");
    }

    #[test]
    fn lazy_hierarchy_index_resolves_ancestor_chain() {
        let root = unique_temp_root();
        write_file(
            &root.join("src/pkg/common.py"),
            "class BaseModel:\n    collection_id: int\n",
        );
        write_file(
            &root.join("src/pkg/model.py"),
            "from . import common\n\nclass Prize(common.BaseModel):\n    slug: str\n",
        );

        let build = build_lazy_hierarchy_index(&root).expect("build lazy hierarchy index");
        let result = query_type_hierarchy(&build.index, &root.join("src/pkg/model.py"), "Prize")
            .expect("query hierarchy");

        assert_eq!(result.hierarchy.name, "Prize");
        assert_eq!(result.hierarchy.ancestors.len(), 1);
        assert_eq!(result.hierarchy.ancestors[0].name, "BaseModel");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_from_import_module_aliases() {
        let index = build_test_index(&[
            ("src/contrib/pydantic/base_types.py", "class Enum:\n    pass\n"),
            (
                "src/pkg/model.py",
                "from src.contrib.pydantic import base_types\n\nclass Choice(base_types.Enum):\n    pass\n",
            ),
        ]);

        let file = PathBuf::from("/tmp/project/src/pkg/model.py");
        let result = query_type_hierarchy(&index, &file, "Choice").unwrap();
        assert_eq!(result.hierarchy.ancestors.len(), 1);
        assert_eq!(result.hierarchy.ancestors[0].name, "Enum");
        assert_eq!(
            result.hierarchy.ancestors[0].module,
            "src.contrib.pydantic.base_types"
        );
    }

    #[test]
    fn resolves_external_package_reexports_and_builtins() {
        let root = unique_temp_root();
        write_file(
            &root.join("src/pkg/common.py"),
            "import pydantic\n\nclass BaseModel(pydantic.BaseModel):\n    collection_id: int\n",
        );
        write_file(
            &root.join("src/pkg/model.py"),
            "from . import common\n\nclass Prize(common.BaseModel):\n    slug: str\n",
        );
        write_file(
            &root.join(".venv/lib/python3.14/site-packages/pydantic/__init__.py"),
            "from .main import *\n",
        );
        write_file(
            &root.join(".venv/lib/python3.14/site-packages/pydantic/main.py"),
            "class ModelMetaclass:\n    pass\n\nclass BaseModel(metaclass=ModelMetaclass):\n    pass\n",
        );

        let build = build_workspace_index(&root).expect("build workspace index");
        let result = query_type_hierarchy(&build.index, &root.join("src/pkg/model.py"), "Prize")
            .expect("query");

        assert_eq!(result.hierarchy.ancestors.len(), 1);
        assert_eq!(result.hierarchy.ancestors[0].name, "BaseModel");
        assert_eq!(result.hierarchy.ancestors[0].ancestors.len(), 1);
        assert_eq!(
            result.hierarchy.ancestors[0].ancestors[0].module,
            "pydantic.main"
        );
        assert_eq!(
            result.hierarchy.ancestors[0].ancestors[0].ancestors.len(),
            1
        );
        assert_eq!(
            result.hierarchy.ancestors[0].ancestors[0].ancestors[0].module,
            "builtins"
        );

        let external_result = query_type_hierarchy(
            &build.index,
            &root.join(".venv/lib/python3.14/site-packages/pydantic/main.py"),
            "BaseModel",
        )
        .expect("query external module");
        assert_eq!(external_result.hierarchy.module, "pydantic.main");
        assert_eq!(external_result.hierarchy.ancestors[0].module, "builtins");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_bare_builtin_bases_in_external_modules() {
        let root = unique_temp_root();
        write_file(
            &root.join(".venv/lib/python3.14/site-packages/builtins.pyi"),
            "class object:\n    pass\n\nclass str(object):\n    pass\n",
        );
        write_file(
            &root.join(".venv/lib/python3.14/site-packages/enum.pyi"),
            "class ReprEnum:\n    pass\n\nclass StrEnum(str, ReprEnum):\n    pass\n",
        );

        let build = build_workspace_index(&root).expect("build workspace index");
        let result = query_type_hierarchy(
            &build.index,
            &root.join(".venv/lib/python3.14/site-packages/enum.pyi"),
            "StrEnum",
        )
        .expect("query external module");

        assert_eq!(result.hierarchy.module, "enum");
        assert_eq!(result.hierarchy.ancestors.len(), 2);
        assert_eq!(result.hierarchy.ancestors[0].name, "str");
        assert_eq!(result.hierarchy.ancestors[0].module, "builtins");
        assert_eq!(result.hierarchy.ancestors[1].name, "ReprEnum");
        assert_eq!(result.hierarchy.ancestors[1].module, "enum");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_workspace_subtypes() {
        let index = build_test_index(&[
            ("src/pkg/common.py", "class BaseModel:\n    pass\n"),
            (
                "src/pkg/model_a.py",
                "from .common import BaseModel\n\nclass Prize(BaseModel):\n    pass\n",
            ),
            (
                "src/pkg/model_b.py",
                "from .common import BaseModel\n\nclass Reward(BaseModel):\n    pass\n",
            ),
        ]);

        let items = query_subtypes(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/common.py"),
            "BaseModel",
        )
        .unwrap();
        let names = items.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert_eq!(names, vec!["Prize", "Reward"]);
    }

    #[test]
    fn resolves_field_type_members() {
        let index = build_test_index(&[
            (
                "src/pkg/common.py",
                "class Collection:\n    title: str\n    slug: str | None\n\n    def normalize(self) -> str:\n        return self.title\n",
            ),
            (
                "src/pkg/model.py",
                "from .common import Collection\n\nclass Prize:\n    collection: Collection\n",
            ),
        ]);

        let members = query_class_members(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            "Prize",
        )
        .expect("class members");
        assert_eq!(members.fields.len(), 1);
        assert_eq!(members.fields[0].type_ref.as_deref(), Some("Collection"));
        assert_eq!(members.fields[0].type_line, Some(4));
        assert_eq!(members.fields[0].type_col, Some(17));

        let resolved = query_resolved_class_fields(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            "Collection",
        )
        .expect("resolved field members");
        assert_eq!(resolved.class_name, "Collection");
        assert_eq!(resolved.methods.len(), 1);
        assert_eq!(resolved.methods[0].name, "normalize");
        assert_eq!(resolved.fields.len(), 2);
        assert_eq!(resolved.fields[0].name, "title");
        assert_eq!(resolved.fields[1].name, "slug");
    }

    #[test]
    fn marks_method_only_field_targets_as_field_objects() {
        let index = build_test_index(&[
            (
                "src/pkg/common.py",
                "class Collection:\n    def normalize(self) -> str:\n        return \"\"\n",
            ),
            (
                "src/pkg/model.py",
                "from .common import Collection\n\nclass Prize:\n    collection: Collection\n",
            ),
        ]);

        let tree = query_subtypes_tree_limited(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            "Prize",
            Some(1),
            Some(1),
        )
        .expect("subtype tree");
        assert_eq!(tree.tree.children.len(), 1);
        assert_eq!(tree.tree.children[0].name, "collection");
        assert_eq!(tree.tree.children[0].kind, "field_object");
    }

    #[test]
    fn resolves_assigned_class_aliases_and_enum_members() {
        let index = build_test_index(&[
            (
                "src/pkg/source.py",
                "import enum\n\nclass Choice(str, enum.Enum):\n    button = \"button\"\n    picker = \"picker\"\n",
            ),
            (
                "src/pkg/model.py",
                "from . import source\n\nChoiceAlias = source.Choice\n\nclass Prize:\n    choice: ChoiceAlias\n",
            ),
        ]);

        let resolved = query_resolved_class_fields(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            "ChoiceAlias",
        )
        .expect("resolved alias enum members");
        assert_eq!(resolved.class_name, "Choice");
        assert_eq!(resolved.fields.len(), 2);
        assert_eq!(resolved.fields[0].name, "button");
        assert_eq!(resolved.fields[1].name, "picker");
    }

    #[test]
    fn resolves_incoming_and_outgoing_calls() {
        let index = build_test_index(&[
            (
                "src/pkg/model.py",
                "def helper():\n    return 1\n\nclass Prize:\n    def save(self):\n        helper()\n        self.validate()\n\n    def validate(self):\n        return helper()\n",
            ),
        ]);

        let outgoing_helper = query_outgoing_calls(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            "save",
            Some(5),
            Some(9),
        )
        .unwrap();
        let outgoing_names = outgoing_helper
            .iter()
            .map(|edge| edge.item.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(outgoing_names, vec!["helper", "validate"]);

        let incoming_helper = query_incoming_calls(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            "helper",
            Some(1),
            Some(5),
        )
        .unwrap();
        let incoming_names = incoming_helper
            .iter()
            .map(|edge| edge.item.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(incoming_names, vec!["save", "validate"]);
    }

    #[test]
    fn resolves_callable_reference_under_cursor() {
        let index = build_test_index(&[
            (
                "src/pkg/model.py",
                "def helper():\n    return 1\n\nclass Prize:\n    def save(self):\n        helper()\n        self.validate()\n\n    def validate(self):\n        return helper()\n",
            ),
        ]);

        let helper_ref = query_resolved_callable_reference(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            6,
            10,
        )
        .unwrap();
        assert_eq!(helper_ref.name, "helper");
        assert_eq!(helper_ref.line, 1);

        let validate_ref = query_resolved_callable_reference(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/model.py"),
            7,
            16,
        )
        .unwrap();
        assert_eq!(validate_ref.name, "validate");
        assert_eq!(validate_ref.line, 9);
    }

    #[test]
    fn resolves_incoming_calls_through_validator_aliases_and_field_usage() {
        let index = build_test_index(&[
            (
                "src/pkg/common.py",
                "from typing import Annotated\nimport pydantic\n\ndef process_str(value):\n    return value\n\nProcessedStringField = Annotated[str, pydantic.AfterValidator(process_str)]\n\nclass Prize:\n    name: ProcessedStringField\n",
            ),
        ]);

        let incoming = query_incoming_calls(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/common.py"),
            "process_str",
            Some(4),
            Some(5),
        )
        .unwrap();
        let incoming_names = incoming
            .iter()
            .map(|edge| edge.item.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(incoming_names, vec!["ProcessedStringField"]);

        let tree = query_incoming_calls_tree(
            &index,
            &PathBuf::from("/tmp/project/src/pkg/common.py"),
            "process_str",
            Some(4),
            Some(5),
        )
        .unwrap();
        assert_eq!(tree.tree.children.len(), 1);
        assert_eq!(tree.tree.children[0].name, "ProcessedStringField");
        assert_eq!(tree.tree.children[0].children.len(), 1);
        assert_eq!(tree.tree.children[0].children[0].name, "name");
    }

    #[test]
    fn module_name_from_init_pyi_uses_package_name() {
        let root = PathBuf::from("/tmp/project/.venv/lib/python3.14/site-packages");
        let path = root.join("nh3/__init__.pyi");
        let module_name = module_name_from_path(&root, &path).unwrap();
        assert_eq!(module_name, "nh3");
    }
}
