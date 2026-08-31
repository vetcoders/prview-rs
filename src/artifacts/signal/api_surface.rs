//! Deterministic, revision-backed Rust API snapshots.
//!
//! This backend parses complete source files from [`RevisionFileSource`], walks
//! ordinary Rust modules, and computes source-level external reachability. It
//! deliberately does not compare revisions or emit artifacts/verdicts. Anything
//! that needs compiler, macro, feature-matrix, or prelude resolution is retained
//! as a typed unknown instead of being interpreted as an empty API.

use super::revision_source::{
    RevisionContentKind, RevisionEntry, RevisionFileSource, RevisionProvenance, RevisionRead,
};
use quote::{ToTokens, quote};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::fold::Fold;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Attribute, Fields, Item, Meta, Token, UseTree, Visibility};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustApiSnapshot {
    pub provenance: RevisionProvenance,
    pub crates: Vec<RustCrateSnapshot>,
    pub modules: Vec<RustModuleSnapshot>,
    pub module_aliases: Vec<RustModuleAlias>,
    pub items: Vec<RustApiItem>,
    /// Parsed ordinary declarations, including non-public counterparts in an
    /// externally reachable parent module. Delta analysis uses this evidence
    /// only to prove public/non-public transitions; artifact views continue to
    /// use `items` as the externally reachable API surface.
    pub declarations: Vec<RustApiDeclaration>,
    pub reexports: Vec<RustApiReexport>,
    pub unknowns: Vec<RustApiUnknown>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustApiDeclaration {
    pub key: RustApiItemKey,
    pub kind: RustApiItemKind,
    pub contract: String,
    pub cfg_guard: Vec<String>,
    pub source_path: String,
    pub evidence: String,
    pub provenance: RevisionProvenance,
    pub certainty: RustSourceCertainty,
    pub declared_public: bool,
    pub parent_externally_reachable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustModuleAlias {
    pub crate_name: String,
    pub module_path: Vec<String>,
    pub target_module_path: Vec<String>,
    pub cfg_guard: Vec<String>,
    pub source_path: String,
    pub provenance: RevisionProvenance,
    pub certainty: RustSourceCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustCrateSnapshot {
    pub name: String,
    pub manifest_path: String,
    pub root_path: String,
    pub provenance: RevisionProvenance,
    pub certainty: RustSourceCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustModuleSnapshot {
    pub crate_name: String,
    pub module_path: Vec<String>,
    pub source_path: String,
    pub externally_reachable: bool,
    pub cfg_guard: Vec<String>,
    pub provenance: RevisionProvenance,
    pub certainty: RustSourceCertainty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RustSourceCertainty {
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RustNamespace {
    Type,
    Value,
    Macro,
    Module,
    Crate,
    CargoFeature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RustApiItemKind {
    Function,
    Struct,
    Union,
    Enum,
    Trait,
    TypeAlias,
    Constant,
    Static,
    ForeignFunction,
    ForeignStatic,
    StructConstructor,
    InherentAssociatedFunction,
    InherentAssociatedConstant,
    Macro,
    Module,
    Crate,
    CargoFeature,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RustApiItemKey {
    pub crate_name: String,
    pub module_path: Vec<String>,
    pub namespace: RustNamespace,
    pub external_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustApiItem {
    pub key: RustApiItemKey,
    pub kind: RustApiItemKind,
    pub contract: String,
    pub cfg_guard: Vec<String>,
    pub source_path: String,
    pub evidence: String,
    pub provenance: RevisionProvenance,
    pub certainty: RustSourceCertainty,
    pub origin_module_path: Vec<String>,
    pub origin_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustApiReexport {
    pub crate_name: String,
    pub module_path: Vec<String>,
    pub external_name: String,
    pub namespace: RustNamespace,
    pub target_module_path: Vec<String>,
    pub target_name: String,
    pub cfg_guard: Vec<String>,
    pub source_path: String,
    pub provenance: RevisionProvenance,
    pub certainty: RustSourceCertainty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RustApiUnknownKind {
    ManifestRead,
    ManifestNonUtf8,
    ManifestParse,
    MissingLibRoot,
    SourceRead,
    SourceNonUtf8,
    SourceParse,
    MissingModule,
    AmbiguousModule,
    NonRegularModule,
    ModuleCycle,
    UnsupportedModulePath,
    GlobReexport,
    UnresolvedReexport,
    AmbiguousReexport,
    ReexportCycle,
    MacroGeneratedItems,
    IncludeMacro,
    UnsupportedExternResolution,
    UnresolvedInherentOwner,
    CfgPredicate,
    ResolutionLimit,
    PathNonUtf8,
    TraitImplResolution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustApiUnknown {
    pub kind: RustApiUnknownKind,
    pub crate_name: Option<String>,
    pub module_path: Vec<String>,
    pub source_path: String,
    pub cfg_guard: Vec<String>,
    pub evidence: String,
    pub provenance: RevisionProvenance,
}

pub fn snapshot_rust_api(source: &dyn RevisionFileSource) -> RustApiSnapshot {
    SnapshotBuilder::new(source).build()
}

#[cfg(test)]
fn snapshot_rust_api_with_resolution_budget(
    source: &dyn RevisionFileSource,
    budget: usize,
) -> RustApiSnapshot {
    SnapshotBuilder::new(source)
        .with_reexport_iteration_budget(budget)
        .build()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolKey {
    crate_name: String,
    module_path: Vec<String>,
    name: String,
    namespace: RustNamespace,
}

#[derive(Debug, Clone)]
struct RawSymbol {
    key: SymbolKey,
    kind: RustApiItemKind,
    contract: String,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ModuleAliasOrigin {
    target_module_path: Vec<String>,
    cfg_guard: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolAliasOrigin {
    target: SymbolKey,
    kind: RustApiItemKind,
    contract: String,
    cfg_guard: Vec<String>,
}

#[derive(Debug, Clone)]
struct UseEdge {
    crate_name: String,
    module_path: Vec<String>,
    module_reachable: bool,
    cfg_guard: Vec<String>,
    source_path: String,
    leaves: Vec<UseLeaf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UseLeaf {
    segments: Vec<String>,
    alias: String,
    glob: bool,
}

#[derive(Debug, Clone)]
struct PendingAssoc {
    crate_name: String,
    owner_module_path: Vec<String>,
    owner_name: String,
    name: String,
    kind: RustApiItemKind,
    namespace: RustNamespace,
    contract: String,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
}

#[derive(Debug, Clone)]
struct PendingTraitImpl {
    crate_name: String,
    trait_module_path: Vec<String>,
    trait_name: String,
    owner_module_path: Vec<String>,
    owner_name: String,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
}

#[derive(Debug, Clone)]
struct ModuleProof {
    crate_name: String,
    module_path: Vec<String>,
    declared_public: bool,
    cfg_guard: Vec<String>,
}

#[derive(Debug, Clone)]
struct CfgOutcome {
    guards: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct ModuleVisibilityProof {
    externally_reachable: bool,
    declared_public: bool,
}

struct SnapshotBuilder<'a> {
    source: &'a dyn RevisionFileSource,
    provenance: RevisionProvenance,
    inventory: BTreeMap<String, RevisionEntry>,
    crates: Vec<RustCrateSnapshot>,
    modules: Vec<RustModuleSnapshot>,
    module_aliases: Vec<RustModuleAlias>,
    items: Vec<RustApiItem>,
    declarations: Vec<RustApiDeclaration>,
    reexports: Vec<RustApiReexport>,
    unknowns: Vec<RustApiUnknown>,
    symbols: BTreeMap<SymbolKey, Vec<RawSymbol>>,
    uses: Vec<UseEdge>,
    pending_assoc: Vec<PendingAssoc>,
    pending_trait_impls: Vec<PendingTraitImpl>,
    module_proofs: Vec<ModuleProof>,
    all_module_aliases: Vec<RustModuleAlias>,
    completed_sources: BTreeMap<(String, String, Vec<String>, Vec<String>), bool>,
    active_sources: BTreeSet<(String, String)>,
    proc_macro_crates: BTreeSet<String>,
    reexport_iteration_budget: Option<usize>,
}

impl<'a> SnapshotBuilder<'a> {
    fn new(source: &'a dyn RevisionFileSource) -> Self {
        Self {
            source,
            provenance: source.provenance().clone(),
            inventory: source
                .entries()
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect(),
            crates: Vec::new(),
            modules: Vec::new(),
            module_aliases: Vec::new(),
            items: Vec::new(),
            declarations: Vec::new(),
            reexports: Vec::new(),
            unknowns: Vec::new(),
            symbols: BTreeMap::new(),
            uses: Vec::new(),
            pending_assoc: Vec::new(),
            pending_trait_impls: Vec::new(),
            module_proofs: Vec::new(),
            all_module_aliases: Vec::new(),
            completed_sources: BTreeMap::new(),
            active_sources: BTreeSet::new(),
            proc_macro_crates: BTreeSet::new(),
            reexport_iteration_budget: None,
        }
    }

    #[cfg(test)]
    fn with_reexport_iteration_budget(mut self, budget: usize) -> Self {
        self.reexport_iteration_budget = Some(budget);
        self
    }

    fn build(mut self) -> RustApiSnapshot {
        self.record_inventory_path_unknowns();
        self.discover_crates();
        self.resolve_reexports();
        self.resolve_trait_impls();
        self.resolve_inherent_items();
        self.materialize_public_modules();
        self.attach_public_reexport_origins();
        self.crates.sort_by(|left, right| {
            (&left.name, &left.manifest_path, &left.root_path).cmp(&(
                &right.name,
                &right.manifest_path,
                &right.root_path,
            ))
        });
        self.modules.sort_by(|left, right| {
            (
                &left.crate_name,
                &left.module_path,
                &left.source_path,
                &left.cfg_guard,
            )
                .cmp(&(
                    &right.crate_name,
                    &right.module_path,
                    &right.source_path,
                    &right.cfg_guard,
                ))
        });
        self.items.sort_by(|left, right| {
            (
                &left.key,
                left.kind,
                &left.cfg_guard,
                &left.contract,
                &left.source_path,
                &left.origin_module_path,
                &left.origin_name,
            )
                .cmp(&(
                    &right.key,
                    right.kind,
                    &right.cfg_guard,
                    &right.contract,
                    &right.source_path,
                    &right.origin_module_path,
                    &right.origin_name,
                ))
        });
        self.items.dedup();
        self.declarations.sort_by(|left, right| {
            (
                &left.key,
                left.kind,
                &left.cfg_guard,
                &left.contract,
                left.declared_public,
                &left.source_path,
            )
                .cmp(&(
                    &right.key,
                    right.kind,
                    &right.cfg_guard,
                    &right.contract,
                    right.declared_public,
                    &right.source_path,
                ))
        });
        self.declarations.dedup();
        self.module_aliases.sort_by(|left, right| {
            (
                &left.crate_name,
                &left.module_path,
                &left.target_module_path,
                &left.cfg_guard,
            )
                .cmp(&(
                    &right.crate_name,
                    &right.module_path,
                    &right.target_module_path,
                    &right.cfg_guard,
                ))
        });
        self.module_aliases.dedup();
        self.reexports.sort_by(|left, right| {
            (
                &left.crate_name,
                &left.module_path,
                &left.external_name,
                left.namespace,
                &left.target_module_path,
                &left.target_name,
                &left.cfg_guard,
            )
                .cmp(&(
                    &right.crate_name,
                    &right.module_path,
                    &right.external_name,
                    right.namespace,
                    &right.target_module_path,
                    &right.target_name,
                    &right.cfg_guard,
                ))
        });
        self.reexports.dedup();
        self.unknowns.sort_by(|left, right| {
            (
                left.kind,
                &left.crate_name,
                &left.module_path,
                &left.source_path,
                &left.cfg_guard,
                &left.evidence,
            )
                .cmp(&(
                    right.kind,
                    &right.crate_name,
                    &right.module_path,
                    &right.source_path,
                    &right.cfg_guard,
                    &right.evidence,
                ))
        });
        self.unknowns.dedup();
        RustApiSnapshot {
            provenance: self.provenance,
            crates: self.crates,
            modules: self.modules,
            module_aliases: self.module_aliases,
            items: self.items,
            declarations: self.declarations,
            reexports: self.reexports,
            unknowns: self.unknowns,
        }
    }

    fn record_inventory_path_unknowns(&mut self) {
        let paths: Vec<_> = self
            .inventory
            .values()
            .filter(|entry| {
                entry.kind == super::revision_source::RevisionEntryKind::Unsupported
                    && entry.path.contains(crate::git::NON_UTF8_GIT_PATH_PREFIX)
            })
            .map(|entry| crate::git::display_git_path(&entry.path).to_owned())
            .collect();
        for path in paths {
            self.unknown(
                RustApiUnknownKind::PathNonUtf8,
                None,
                &[],
                &path,
                "Git tree contains a path component that cannot be represented as UTF-8".to_owned(),
            );
        }
    }

    fn materialize_public_modules(&mut self) {
        let mut modules: Vec<_> = self
            .modules
            .iter()
            .filter(|module| module.externally_reachable && !module.module_path.is_empty())
            .map(|module| {
                let (name, parent) = module
                    .module_path
                    .split_last()
                    .expect("non-root module has a final component");
                (
                    module.crate_name.clone(),
                    parent.to_vec(),
                    name.clone(),
                    module.cfg_guard.clone(),
                    module.source_path.clone(),
                    module.module_path.clone(),
                )
            })
            .collect();
        modules.extend(self.module_aliases.iter().filter_map(|alias| {
            let (name, parent) = alias.module_path.split_last()?;
            Some((
                alias.crate_name.clone(),
                parent.to_vec(),
                name.clone(),
                alias.cfg_guard.clone(),
                alias.source_path.clone(),
                alias.target_module_path.clone(),
            ))
        }));
        for (crate_name, module_path, name, cfg_guard, source_path, origin_module_path) in modules {
            self.items.push(RustApiItem {
                key: RustApiItemKey {
                    crate_name,
                    module_path,
                    namespace: RustNamespace::Module,
                    external_name: name.clone(),
                },
                kind: RustApiItemKind::Module,
                contract: "pub mod __prview_name ;".to_owned(),
                cfg_guard,
                source_path,
                evidence: format!("public module {name}"),
                provenance: self.provenance.clone(),
                certainty: RustSourceCertainty::Confirmed,
                origin_module_path,
                origin_name: name,
            });
        }
    }

    fn attach_public_reexport_origins(&mut self) {
        let public_origins: BTreeSet<_> = self
            .items
            .iter()
            .map(|item| {
                (
                    item.key.crate_name.clone(),
                    item.key.namespace,
                    item.key.module_path.clone(),
                    item.key.external_name.clone(),
                )
            })
            .collect();
        for item in &mut self.items {
            let origin_is_distinct = item.origin_module_path != item.key.module_path
                || item.origin_name != item.key.external_name;
            let origin_is_public = public_origins.contains(&(
                item.key.crate_name.clone(),
                item.key.namespace,
                item.origin_module_path.clone(),
                item.origin_name.clone(),
            ));
            if origin_is_distinct && origin_is_public {
                let origin = if item.origin_module_path.is_empty() {
                    item.origin_name.clone()
                } else {
                    format!(
                        "{}::{}",
                        item.origin_module_path.join("::"),
                        item.origin_name
                    )
                };
                item.contract = format!("{}\nreexport-origin:{origin}", item.contract);
            }
        }
    }

    fn discover_crates(&mut self) {
        let manifests: Vec<_> = self
            .inventory
            .iter()
            .filter(|(path, _entry)| {
                Path::new(path.as_str())
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some("Cargo.toml")
            })
            .map(|(path, _)| path.clone())
            .collect();
        let allowed = api_crate_manifests(self.source, &manifests);
        for manifest_path in manifests {
            if !allowed.contains(&manifest_path) {
                continue;
            }
            if !self
                .inventory
                .get(&manifest_path)
                .is_some_and(is_live_regular_entry)
            {
                let evidence = self
                    .inventory
                    .get(&manifest_path)
                    .map(|entry| {
                        format!(
                            "manifest is unavailable: {:?} {:?}",
                            entry.kind, entry.state
                        )
                    })
                    .unwrap_or_else(|| "manifest inventory entry disappeared".to_owned());
                self.unknown(
                    RustApiUnknownKind::ManifestRead,
                    None,
                    &[],
                    &manifest_path,
                    evidence,
                );
                continue;
            }
            let Some(text) = self.read_utf8(
                None,
                &[],
                &manifest_path,
                &[],
                RustApiUnknownKind::ManifestRead,
                RustApiUnknownKind::ManifestNonUtf8,
            ) else {
                continue;
            };
            let manifest: toml::Value = match toml::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        None,
                        &[],
                        &manifest_path,
                        error.to_string(),
                    );
                    continue;
                }
            };
            let Some(package_value) = manifest.get("package") else {
                if manifest.get("workspace").is_some_and(toml::Value::is_table) {
                    continue;
                }
                self.unknown(
                    RustApiUnknownKind::ManifestParse,
                    None,
                    &[],
                    &manifest_path,
                    "manifest has neither a package table nor a valid workspace table".to_owned(),
                );
                continue;
            };
            let Some(package) = package_value.as_table() else {
                self.unknown(
                    RustApiUnknownKind::ManifestParse,
                    None,
                    &[],
                    &manifest_path,
                    "package must be a table".to_owned(),
                );
                continue;
            };
            let Some(package_name) = required_string(package, "name") else {
                self.unknown(
                    RustApiUnknownKind::ManifestParse,
                    None,
                    &[],
                    &manifest_path,
                    "package.name must be a string".to_owned(),
                );
                continue;
            };
            if let Err(reason) = validate_package_name(package_name) {
                self.unknown(
                    RustApiUnknownKind::ManifestParse,
                    None,
                    &[],
                    &manifest_path,
                    reason,
                );
                continue;
            }
            let lib = match manifest.get("lib") {
                Some(value) => match value.as_table() {
                    Some(table) => Some(table),
                    None => {
                        self.unknown(
                            RustApiUnknownKind::ManifestParse,
                            None,
                            &[],
                            &manifest_path,
                            "lib must be a table".to_owned(),
                        );
                        continue;
                    }
                },
                None => None,
            };
            let autolib = match package.get("autolib") {
                Some(value) => match value.as_bool() {
                    Some(value) => value,
                    None => {
                        self.unknown(
                            RustApiUnknownKind::ManifestParse,
                            None,
                            &[],
                            &manifest_path,
                            "package.autolib must be a boolean".to_owned(),
                        );
                        continue;
                    }
                },
                None => true,
            };
            let manifest_dir = parent_repo_path(&manifest_path);
            if lib.is_none() && !autolib {
                continue;
            }
            let explicit_root = match optional_string(lib, "path") {
                Ok(value) => value,
                Err(reason) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        None,
                        &[],
                        &manifest_path,
                        reason,
                    );
                    continue;
                }
            };
            let explicit_name = match optional_string(lib, "name") {
                Ok(value) => value,
                Err(reason) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        None,
                        &[],
                        &manifest_path,
                        reason,
                    );
                    continue;
                }
            };
            if let Some(name) = explicit_name
                && let Err(reason) = validate_lib_name(name)
            {
                self.unknown(
                    RustApiUnknownKind::ManifestParse,
                    None,
                    &[],
                    &manifest_path,
                    reason,
                );
                continue;
            }
            let proc_macro = match optional_bool(lib, "proc-macro") {
                Ok(value) => value.unwrap_or(false),
                Err(reason) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        None,
                        &[],
                        &manifest_path,
                        reason,
                    );
                    continue;
                }
            };
            let crate_types = match lib_crate_types(lib) {
                Ok(types) => types,
                Err(reason) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        None,
                        &[],
                        &manifest_path,
                        reason,
                    );
                    continue;
                }
            };
            let root_path =
                match safe_join_repo_path(&manifest_dir, explicit_root.unwrap_or("src/lib.rs")) {
                    Ok(path) => path,
                    Err(reason) => {
                        self.unknown(
                            RustApiUnknownKind::ManifestParse,
                            None,
                            &[],
                            &manifest_path,
                            reason,
                        );
                        continue;
                    }
                };
            let crate_name = normalize_identifier(
                explicit_name
                    .map(str::to_owned)
                    .unwrap_or_else(|| package_name.replace('-', "_")),
            );
            let cargo_features = match cargo_feature_contracts(&manifest) {
                Ok(features) => features,
                Err(reason) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        Some(&crate_name),
                        &[],
                        &manifest_path,
                        reason,
                    );
                    Vec::new()
                }
            };
            if !self
                .inventory
                .get(&root_path)
                .is_some_and(is_live_regular_entry)
            {
                let state = self
                    .inventory
                    .get(&root_path)
                    .map(|entry| format!("{:?} {:?}", entry.kind, entry.state))
                    .unwrap_or_else(|| "missing inventory entry".to_owned());
                self.unknown(
                    RustApiUnknownKind::MissingLibRoot,
                    Some(&crate_name),
                    &[],
                    &root_path,
                    format!("library root declared by {manifest_path} is unavailable: {state}"),
                );
                continue;
            }
            if proc_macro {
                self.proc_macro_crates.insert(crate_name.clone());
            }
            let base_dir = parent_repo_path(&root_path);
            if !self.load_module(
                &crate_name,
                Vec::new(),
                &root_path,
                &base_dir,
                ModuleVisibilityProof {
                    externally_reachable: true,
                    declared_public: true,
                },
                Vec::new(),
            ) {
                self.proc_macro_crates.remove(&crate_name);
                continue;
            }
            self.items.push(RustApiItem {
                key: RustApiItemKey {
                    crate_name: crate_name.clone(),
                    module_path: Vec::new(),
                    namespace: RustNamespace::Crate,
                    external_name: crate_name.clone(),
                },
                kind: RustApiItemKind::Crate,
                contract: format!("library proc_macro={proc_macro}; crate_types={crate_types:?}"),
                cfg_guard: Vec::new(),
                source_path: manifest_path.clone(),
                evidence: format!(
                    "library crate {crate_name} from {manifest_path}; root={root_path}"
                ),
                provenance: self.provenance.clone(),
                certainty: RustSourceCertainty::Confirmed,
                origin_module_path: Vec::new(),
                origin_name: crate_name.clone(),
            });
            for (feature_name, feature_contract) in cargo_features {
                self.items.push(RustApiItem {
                    key: RustApiItemKey {
                        crate_name: crate_name.clone(),
                        module_path: Vec::new(),
                        namespace: RustNamespace::CargoFeature,
                        external_name: feature_name.clone(),
                    },
                    kind: RustApiItemKind::CargoFeature,
                    contract: feature_contract.clone(),
                    cfg_guard: Vec::new(),
                    source_path: manifest_path.clone(),
                    evidence: feature_contract,
                    provenance: self.provenance.clone(),
                    certainty: RustSourceCertainty::Confirmed,
                    origin_module_path: Vec::new(),
                    origin_name: feature_name,
                });
            }
            self.crates.push(RustCrateSnapshot {
                name: crate_name,
                manifest_path: manifest_path.clone(),
                root_path,
                provenance: self.provenance.clone(),
                certainty: RustSourceCertainty::Confirmed,
            });
        }
    }

    fn load_module(
        &mut self,
        crate_name: &str,
        module_path: Vec<String>,
        source_path: &str,
        logical_child_base: &str,
        visibility: ModuleVisibilityProof,
        cfg_guard: Vec<String>,
    ) -> bool {
        let variant_key = (
            crate_name.to_owned(),
            source_path.to_owned(),
            module_path.clone(),
            cfg_guard.clone(),
        );
        let active_key = (crate_name.to_owned(), source_path.to_owned());
        if let Some(outcome) = self.completed_sources.get(&variant_key) {
            return *outcome;
        }
        if !self.active_sources.insert(active_key.clone()) {
            self.unknown_guarded(
                RustApiUnknownKind::ModuleCycle,
                Some(crate_name),
                &module_path,
                source_path,
                &cfg_guard,
                "module source was reached more than once".to_owned(),
            );
            return false;
        }
        let Some(text) = self.read_utf8(
            Some(crate_name),
            &module_path,
            source_path,
            &cfg_guard,
            RustApiUnknownKind::SourceRead,
            RustApiUnknownKind::SourceNonUtf8,
        ) else {
            self.active_sources.remove(&active_key);
            self.completed_sources.insert(variant_key, false);
            return false;
        };
        let file = match syn::parse_file(&text) {
            Ok(file) => file,
            Err(error) => {
                self.unknown_guarded(
                    RustApiUnknownKind::SourceParse,
                    Some(crate_name),
                    &module_path,
                    source_path,
                    &cfg_guard,
                    error.to_string(),
                );
                self.active_sources.remove(&active_key);
                self.completed_sources.insert(variant_key, false);
                return false;
            }
        };
        self.modules.push(RustModuleSnapshot {
            crate_name: crate_name.to_owned(),
            module_path: module_path.clone(),
            source_path: source_path.to_owned(),
            externally_reachable: visibility.externally_reachable,
            cfg_guard: cfg_guard.clone(),
            provenance: self.provenance.clone(),
            certainty: RustSourceCertainty::Confirmed,
        });
        self.module_proofs.push(ModuleProof {
            crate_name: crate_name.to_owned(),
            module_path: module_path.clone(),
            declared_public: visibility.declared_public,
            cfg_guard: cfg_guard.clone(),
        });
        let physical_declaring_dir = parent_repo_path(source_path);
        self.walk_items(
            crate_name,
            &module_path,
            source_path,
            &physical_declaring_dir,
            logical_child_base,
            visibility.externally_reachable,
            &cfg_guard,
            &file.items,
        );
        self.active_sources.remove(&active_key);
        self.completed_sources.insert(variant_key, true);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_items(
        &mut self,
        crate_name: &str,
        module_path: &[String],
        source_path: &str,
        physical_declaring_dir: &str,
        logical_child_base: &str,
        module_reachable: bool,
        inherited_cfg: &[String],
        items: &[Item],
    ) {
        for item in items {
            let mut cfg_guard = inherited_cfg.to_vec();
            let cfg = canonical_cfg(item_attrs(item));
            cfg_guard.extend(cfg.guards);
            cfg_guard.sort();
            cfg_guard.dedup();
            if !cfg.errors.is_empty() {
                for evidence in cfg.errors {
                    self.unknown_guarded(
                        RustApiUnknownKind::CfgPredicate,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &cfg_guard,
                        evidence,
                    );
                }
                continue;
            }
            let requires_transform_proof = node_requires_transform_proof(item);
            let transforming: Vec<_> = if requires_transform_proof {
                transforming_attrs(item_attrs(item)).collect()
            } else {
                Vec::new()
            };
            if !transforming.is_empty() {
                for attr in transforming {
                    self.unknown_guarded(
                        RustApiUnknownKind::MacroGeneratedItems,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &cfg_guard,
                        bind_transform_evidence(canonical_tokens(attr.to_token_stream()), item),
                    );
                }
                continue;
            }
            if requires_transform_proof {
                let conditional_transforming = transforming_cfg_attrs(item_attrs(item));
                if !conditional_transforming.is_empty() {
                    for evidence in conditional_transforming {
                        self.unknown_guarded(
                            RustApiUnknownKind::MacroGeneratedItems,
                            Some(crate_name),
                            module_path,
                            source_path,
                            &cfg_guard,
                            bind_transform_evidence(evidence, item),
                        );
                    }
                    continue;
                }
            }
            for macro_call in expression_include_macros(item) {
                let evidence = format!(
                    "item:{}\ninclude:{}\nincluded-digest:{}",
                    canonical_tokens(item.to_token_stream()),
                    canonical_tokens(macro_call.to_token_stream()),
                    self.include_digest(source_path, &macro_call),
                );
                self.unknown_guarded(
                    RustApiUnknownKind::IncludeMacro,
                    Some(crate_name),
                    module_path,
                    source_path,
                    &cfg_guard,
                    evidence,
                );
            }
            match item {
                Item::Mod(module) => {
                    let name = normalize_identifier(module.ident.to_string());
                    let mut child_path = module_path.to_vec();
                    child_path.push(name.clone());
                    let child_reachable = module_reachable && is_public(&module.vis);
                    if let Some((_, inline_items)) = &module.content {
                        let inline_base = match inline_module_child_base(
                            &module.attrs,
                            physical_declaring_dir,
                            logical_child_base,
                            &name,
                        ) {
                            Ok(base) => base,
                            Err(reason) => {
                                self.unknown_guarded(
                                    RustApiUnknownKind::UnsupportedModulePath,
                                    Some(crate_name),
                                    &child_path,
                                    source_path,
                                    &cfg_guard,
                                    reason,
                                );
                                continue;
                            }
                        };
                        self.modules.push(RustModuleSnapshot {
                            crate_name: crate_name.to_owned(),
                            module_path: child_path.clone(),
                            source_path: source_path.to_owned(),
                            externally_reachable: child_reachable,
                            cfg_guard: cfg_guard.clone(),
                            provenance: self.provenance.clone(),
                            certainty: RustSourceCertainty::Confirmed,
                        });
                        self.module_proofs.push(ModuleProof {
                            crate_name: crate_name.to_owned(),
                            module_path: child_path.clone(),
                            declared_public: is_public(&module.vis),
                            cfg_guard: cfg_guard.clone(),
                        });
                        self.walk_items(
                            crate_name,
                            &child_path,
                            source_path,
                            &inline_base,
                            &inline_base,
                            child_reachable,
                            &cfg_guard,
                            inline_items,
                        );
                    } else if let Some(evidence) = conditional_path_selection(&module.attrs) {
                        self.unknown_guarded(
                            RustApiUnknownKind::UnsupportedModulePath,
                            Some(crate_name),
                            &child_path,
                            source_path,
                            &cfg_guard,
                            evidence,
                        );
                    } else if has_path_attribute(&module.attrs) {
                        match module_path_attribute(&module.attrs)
                            .and_then(|path| safe_join_repo_path(physical_declaring_dir, &path))
                        {
                            Ok(path)
                                if self.inventory.get(&path).is_some_and(is_live_regular_entry) =>
                            {
                                let next_base = parent_repo_path(&path);
                                self.load_module(
                                    crate_name,
                                    child_path,
                                    &path,
                                    &next_base,
                                    ModuleVisibilityProof {
                                        externally_reachable: child_reachable,
                                        declared_public: is_public(&module.vis),
                                    },
                                    cfg_guard,
                                );
                            }
                            Ok(path) => self.unknown_guarded(
                                RustApiUnknownKind::SourceRead,
                                Some(crate_name),
                                &child_path,
                                &path,
                                &cfg_guard,
                                "#[path] target is unavailable".to_owned(),
                            ),
                            Err(reason) => self.unknown_guarded(
                                RustApiUnknownKind::UnsupportedModulePath,
                                Some(crate_name),
                                &child_path,
                                source_path,
                                &cfg_guard,
                                reason,
                            ),
                        }
                    } else {
                        self.load_external_module(
                            crate_name,
                            child_path,
                            logical_child_base,
                            &name,
                            ModuleVisibilityProof {
                                externally_reachable: child_reachable,
                                declared_public: is_public(&module.vis),
                            },
                            cfg_guard,
                        );
                    }
                }
                Item::Use(item_use) if is_public(&item_use.vis) => {
                    let mut leaves = Vec::new();
                    flatten_use_tree(&item_use.tree, Vec::new(), &mut leaves);
                    self.uses.push(UseEdge {
                        crate_name: crate_name.to_owned(),
                        module_path: module_path.to_vec(),
                        module_reachable,
                        cfg_guard,
                        source_path: source_path.to_owned(),
                        leaves,
                    });
                }
                Item::ExternCrate(item_extern)
                    if module_reachable && is_public(&item_extern.vis) =>
                {
                    self.unknown_guarded(
                        RustApiUnknownKind::UnsupportedExternResolution,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &cfg_guard,
                        canonical_tokens(item_extern.to_token_stream()),
                    );
                }
                Item::Impl(item_impl) => self.collect_impl(
                    crate_name,
                    module_path,
                    source_path,
                    module_reachable,
                    &cfg_guard,
                    item_impl,
                ),
                Item::ForeignMod(foreign) => {
                    for foreign_item in &foreign.items {
                        let mut foreign_guard = cfg_guard.clone();
                        let foreign_cfg = canonical_cfg(foreign_item_attrs(foreign_item));
                        foreign_guard.extend(foreign_cfg.guards);
                        foreign_guard.sort();
                        foreign_guard.dedup();
                        if !foreign_cfg.errors.is_empty() {
                            for evidence in foreign_cfg.errors {
                                self.unknown_guarded(
                                    RustApiUnknownKind::CfgPredicate,
                                    Some(crate_name),
                                    module_path,
                                    source_path,
                                    &foreign_guard,
                                    evidence,
                                );
                            }
                            continue;
                        }
                        let foreign_transformers: Vec<_> =
                            transforming_attrs(foreign_item_attrs(foreign_item)).collect();
                        let conditional = transforming_cfg_attrs(foreign_item_attrs(foreign_item));
                        if !foreign_transformers.is_empty() || !conditional.is_empty() {
                            for evidence in foreign_transformers
                                .into_iter()
                                .map(|attr| canonical_tokens(attr.to_token_stream()))
                                .chain(conditional)
                            {
                                self.unknown_guarded(
                                    RustApiUnknownKind::MacroGeneratedItems,
                                    Some(crate_name),
                                    module_path,
                                    source_path,
                                    &foreign_guard,
                                    bind_transform_evidence(evidence, foreign_item),
                                );
                            }
                            continue;
                        }
                        match foreign_item {
                            syn::ForeignItem::Fn(function) if is_public(&function.vis) => {
                                let name = normalize_identifier(function.sig.ident.to_string());
                                let contract = normalized_foreign_contract(foreign, foreign_item);
                                self.record_symbol(
                                    crate_name,
                                    module_path,
                                    source_path,
                                    module_reachable,
                                    &foreign_guard,
                                    name,
                                    RustNamespace::Value,
                                    RustApiItemKind::ForeignFunction,
                                    contract,
                                );
                            }
                            syn::ForeignItem::Static(value) if is_public(&value.vis) => {
                                let name = normalize_identifier(value.ident.to_string());
                                let contract = normalized_foreign_contract(foreign, foreign_item);
                                self.record_symbol(
                                    crate_name,
                                    module_path,
                                    source_path,
                                    module_reachable,
                                    &foreign_guard,
                                    name,
                                    RustNamespace::Value,
                                    RustApiItemKind::ForeignStatic,
                                    contract,
                                );
                            }
                            syn::ForeignItem::Type(value) if is_public(&value.vis) => {
                                self.unknown_guarded(
                                    RustApiUnknownKind::UnsupportedExternResolution,
                                    Some(crate_name),
                                    module_path,
                                    source_path,
                                    &foreign_guard,
                                    canonical_tokens(value.to_token_stream()),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                Item::Fn(function) if self.proc_macro_crates.contains(crate_name) => {
                    if let Some(name) = proc_macro_external_name(function) {
                        if module_path.is_empty() && is_public(&function.vis) {
                            self.record_symbol(
                                crate_name,
                                &[],
                                source_path,
                                true,
                                &cfg_guard,
                                name,
                                RustNamespace::Macro,
                                RustApiItemKind::Macro,
                                normalized_contract_without_item_name(item.clone()),
                            );
                        } else {
                            self.unknown_guarded(
                                RustApiUnknownKind::MacroGeneratedItems,
                                Some(crate_name),
                                module_path,
                                source_path,
                                &cfg_guard,
                                "proc-macro export must be a public function at crate root"
                                    .to_owned(),
                            );
                        }
                    } else if is_public(&function.vis) {
                        self.unknown_guarded(
                            RustApiUnknownKind::MacroGeneratedItems,
                            Some(crate_name),
                            module_path,
                            source_path,
                            &cfg_guard,
                            "public function in proc-macro crate lacks a supported proc-macro attribute".to_owned(),
                        );
                    }
                }
                Item::Macro(item_macro) => {
                    let macro_name = item_macro.mac.path.to_token_stream().to_string();
                    if let Some(macro_ident) = item_macro.ident.as_ref().filter(|_| {
                        item_macro
                            .attrs
                            .iter()
                            .any(|attr| attr.path().is_ident("macro_export"))
                    }) {
                        let name = normalize_identifier(macro_ident.to_string());
                        self.record_symbol(
                            crate_name,
                            &[],
                            source_path,
                            true,
                            &cfg_guard,
                            name,
                            RustNamespace::Macro,
                            RustApiItemKind::Macro,
                            normalized_macro_contract(item_macro),
                        );
                        continue;
                    }
                    let kind = if matches!(
                        macro_name.as_str(),
                        "include" | "include_str" | "include_bytes"
                    ) {
                        RustApiUnknownKind::IncludeMacro
                    } else {
                        RustApiUnknownKind::MacroGeneratedItems
                    };
                    if item_macro.ident.is_none() || macro_name == "include" {
                        let mut evidence = canonical_tokens(item_macro.to_token_stream());
                        if kind == RustApiUnknownKind::IncludeMacro {
                            evidence.push_str("\nincluded-digest:");
                            evidence.push_str(&self.include_digest(source_path, &item_macro.mac));
                        }
                        self.unknown_guarded(
                            kind,
                            Some(crate_name),
                            module_path,
                            source_path,
                            &cfg_guard,
                            evidence,
                        );
                    }
                }
                _ => {
                    if transforming_attrs(item_attrs(item)).next().is_none()
                        && transforming_cfg_attrs(item_attrs(item)).is_empty()
                        && let Some((name, namespace, kind, contract, declared_public)) =
                            ordinary_item_contract(item)
                    {
                        let evidence = contract.clone();
                        self.declarations.push(RustApiDeclaration {
                            key: RustApiItemKey {
                                crate_name: crate_name.to_owned(),
                                module_path: module_path.to_vec(),
                                namespace,
                                external_name: name,
                            },
                            kind,
                            contract,
                            cfg_guard: cfg_guard.clone(),
                            source_path: source_path.to_owned(),
                            evidence,
                            provenance: self.provenance.clone(),
                            certainty: RustSourceCertainty::Confirmed,
                            declared_public,
                            parent_externally_reachable: module_reachable,
                        });
                    }
                    if let Some((name, namespace, kind, contract)) = public_item_contract(item) {
                        self.record_symbol(
                            crate_name,
                            module_path,
                            source_path,
                            module_reachable,
                            &cfg_guard,
                            name,
                            namespace,
                            kind,
                            contract,
                        );
                        if let Item::Struct(value) = item
                            && !matches!(value.fields, Fields::Named(_))
                            && value.fields.iter().all(|field| is_public(&field.vis))
                        {
                            self.record_symbol(
                                crate_name,
                                module_path,
                                source_path,
                                module_reachable,
                                &cfg_guard,
                                normalize_identifier(value.ident.to_string()),
                                RustNamespace::Value,
                                RustApiItemKind::StructConstructor,
                                normalized_contract_without_item_name(item.clone()),
                            );
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_symbol(
        &mut self,
        crate_name: &str,
        module_path: &[String],
        source_path: &str,
        module_reachable: bool,
        cfg_guard: &[String],
        name: String,
        namespace: RustNamespace,
        kind: RustApiItemKind,
        contract: String,
    ) {
        let evidence = contract.clone();
        let key = SymbolKey {
            crate_name: crate_name.to_owned(),
            module_path: module_path.to_vec(),
            name: name.clone(),
            namespace,
        };
        let raw = RawSymbol {
            key: key.clone(),
            kind,
            contract: contract.clone(),
            cfg_guard: cfg_guard.to_vec(),
            source_path: source_path.to_owned(),
            evidence,
        };
        self.symbols.entry(key).or_default().push(raw.clone());
        if module_reachable {
            self.items
                .push(self.external_item(&raw, module_path.to_vec(), name));
        }
    }

    fn external_item(
        &self,
        raw: &RawSymbol,
        module_path: Vec<String>,
        external_name: String,
    ) -> RustApiItem {
        RustApiItem {
            key: RustApiItemKey {
                crate_name: raw.key.crate_name.clone(),
                module_path,
                namespace: raw.key.namespace,
                external_name,
            },
            kind: raw.kind,
            contract: raw.contract.clone(),
            cfg_guard: raw.cfg_guard.clone(),
            source_path: raw.source_path.clone(),
            evidence: raw.evidence.clone(),
            provenance: self.provenance.clone(),
            certainty: RustSourceCertainty::Confirmed,
            origin_module_path: raw.key.module_path.clone(),
            origin_name: raw.key.name.clone(),
        }
    }

    fn load_external_module(
        &mut self,
        crate_name: &str,
        module_path: Vec<String>,
        child_base: &str,
        name: &str,
        visibility: ModuleVisibilityProof,
        cfg_guard: Vec<String>,
    ) {
        let flat = safe_join_repo_path(child_base, &format!("{name}.rs"))
            .expect("parser-derived module path is safe");
        let nested = safe_join_repo_path(child_base, &format!("{name}/mod.rs"))
            .expect("parser-derived module path is safe");
        let candidates: Vec<_> = [&flat, &nested]
            .into_iter()
            .filter(|path| self.inventory.get(*path).is_some_and(is_live_regular_entry))
            .cloned()
            .collect();
        match candidates.as_slice() {
            [] => {
                let unavailable = [flat, nested]
                    .into_iter()
                    .find_map(|path| self.inventory.get(&path).map(|entry| (path, entry)));
                if let Some((path, entry)) = unavailable {
                    let kind = if matches!(
                        entry.state,
                        super::revision_source::RevisionEntryState::NonRegular { .. }
                    ) || entry.kind
                        != super::revision_source::RevisionEntryKind::RegularFile
                    {
                        RustApiUnknownKind::NonRegularModule
                    } else {
                        RustApiUnknownKind::SourceRead
                    };
                    self.unknown_guarded(
                        kind,
                        Some(crate_name),
                        &module_path,
                        &path,
                        &cfg_guard,
                        format!("module candidate is unavailable: {:?}", entry.state),
                    );
                } else {
                    self.unknown_guarded(
                        RustApiUnknownKind::MissingModule,
                        Some(crate_name),
                        &module_path,
                        child_base,
                        &cfg_guard,
                        format!("neither {name}.rs nor {name}/mod.rs exists"),
                    );
                }
            }
            [one] => {
                let next_base = if one.ends_with("/mod.rs") {
                    parent_repo_path(one)
                } else {
                    one.strip_suffix(".rs").unwrap_or(one).to_owned()
                };
                self.load_module(
                    crate_name,
                    module_path,
                    one,
                    &next_base,
                    visibility,
                    cfg_guard,
                );
            }
            _ => self.unknown_guarded(
                RustApiUnknownKind::AmbiguousModule,
                Some(crate_name),
                &module_path,
                child_base,
                &cfg_guard,
                candidates.join(" and "),
            ),
        }
    }

    fn collect_impl(
        &mut self,
        crate_name: &str,
        module_path: &[String],
        source_path: &str,
        _module_reachable: bool,
        cfg_guard: &[String],
        item_impl: &syn::ItemImpl,
    ) {
        if let Some((_, trait_path, _)) = &item_impl.trait_ {
            let syn::Type::Path(owner_type) = item_impl.self_ty.as_ref() else {
                return;
            };
            let Some((trait_module_path, trait_name)) = resolve_impl_owner(module_path, trait_path)
            else {
                return;
            };
            let Some((owner_module_path, owner_name)) =
                resolve_impl_owner(module_path, &owner_type.path)
            else {
                return;
            };
            // Trait impls are globally usable when the trait and owner are
            // public, even if the impl lives in a private helper module.
            self.pending_trait_impls.push(PendingTraitImpl {
                crate_name: crate_name.to_owned(),
                trait_module_path,
                trait_name,
                owner_module_path,
                owner_name,
                cfg_guard: cfg_guard.to_vec(),
                source_path: source_path.to_owned(),
                evidence: normalized_trait_impl_contract(item_impl),
            });
            return;
        }
        let syn::Type::Path(type_path) = item_impl.self_ty.as_ref() else {
            self.unknown_guarded(
                RustApiUnknownKind::UnsupportedExternResolution,
                Some(crate_name),
                module_path,
                source_path,
                cfg_guard,
                "unsupported inherent impl self type".to_owned(),
            );
            return;
        };
        let Some((owner_module_path, owner_name)) =
            resolve_impl_owner(module_path, &type_path.path)
        else {
            return;
        };
        for item in &item_impl.items {
            let (name, namespace, kind, attrs, public) = match item {
                syn::ImplItem::Fn(function) => (
                    normalize_identifier(function.sig.ident.to_string()),
                    RustNamespace::Value,
                    RustApiItemKind::InherentAssociatedFunction,
                    &function.attrs,
                    is_public(&function.vis),
                ),
                syn::ImplItem::Const(constant) => (
                    normalize_identifier(constant.ident.to_string()),
                    RustNamespace::Value,
                    RustApiItemKind::InherentAssociatedConstant,
                    &constant.attrs,
                    is_public(&constant.vis),
                ),
                _ => continue,
            };
            if !public {
                continue;
            }
            let mut associated_cfg = cfg_guard.to_vec();
            let item_cfg = canonical_cfg(attrs);
            associated_cfg.extend(item_cfg.guards);
            associated_cfg.sort();
            associated_cfg.dedup();
            if !item_cfg.errors.is_empty() {
                for evidence in item_cfg.errors {
                    self.unknown_guarded(
                        RustApiUnknownKind::CfgPredicate,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &associated_cfg,
                        evidence,
                    );
                }
                continue;
            }
            let transformers: Vec<_> = transforming_attrs(attrs).collect();
            let conditional = transforming_cfg_attrs(attrs);
            if !transformers.is_empty() || !conditional.is_empty() {
                for evidence in transformers
                    .into_iter()
                    .map(|attr| canonical_tokens(attr.to_token_stream()))
                    .chain(conditional)
                {
                    self.unknown_guarded(
                        RustApiUnknownKind::MacroGeneratedItems,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &associated_cfg,
                        bind_transform_evidence(evidence, item),
                    );
                }
                continue;
            }
            let evidence = normalized_associated_contract(item_impl, item, false);
            let contract = normalized_associated_contract(item_impl, item, true);
            self.pending_assoc.push(PendingAssoc {
                crate_name: crate_name.to_owned(),
                owner_module_path: owner_module_path.clone(),
                owner_name: owner_name.clone(),
                name,
                kind,
                namespace,
                contract: contract.clone(),
                cfg_guard: associated_cfg,
                source_path: source_path.to_owned(),
                evidence,
            });
        }
    }

    fn resolve_reexports(&mut self) {
        let mut aliases: BTreeMap<SymbolKey, Vec<RawSymbol>> = BTreeMap::new();
        let mut ambiguous_aliases: BTreeSet<SymbolKey> = BTreeSet::new();
        let mut ambiguous_module_aliases: BTreeSet<(String, Vec<String>)> = BTreeSet::new();
        let mut module_alias_origins: BTreeMap<(String, Vec<String>), BTreeSet<ModuleAliasOrigin>> =
            BTreeMap::new();
        let mut symbol_alias_origins: BTreeMap<SymbolKey, BTreeSet<SymbolAliasOrigin>> =
            BTreeMap::new();
        let source_backed_items = self.items.clone();
        let iteration_budget = self
            .reexport_iteration_budget
            .unwrap_or_else(|| reexport_iteration_budget(&self.uses));
        let mut rebuild_required = true;
        let mut completed = false;
        for _ in 0..iteration_budget {
            if rebuild_required {
                self.items = source_backed_items.clone();
                self.reexports.clear();
                self.module_aliases.clear();
                self.all_module_aliases.clear();
                aliases.clear();
                rebuild_required = false;
            }
            let mut progressed = false;
            for edge_index in 0..self.uses.len() {
                let edge = self.uses[edge_index].clone();
                for leaf in &edge.leaves {
                    if leaf.glob {
                        if edge.module_reachable {
                            self.unknown_guarded(
                                RustApiUnknownKind::GlobReexport,
                                Some(&edge.crate_name),
                                &edge.module_path,
                                &edge.source_path,
                                &edge.cfg_guard,
                                leaf.segments.join("::"),
                            );
                        }
                        continue;
                    }
                    let mut module_candidates = self.resolve_module_leaf(&edge, leaf);
                    if !module_candidates.is_empty() {
                        let module_conflict =
                            module_candidates
                                .iter()
                                .enumerate()
                                .any(|(index, (path, guard))| {
                                    module_candidates.iter().skip(index + 1).any(
                                        |(other_path, other_guard)| {
                                            path != other_path
                                                && !guards_proven_disjoint(guard, other_guard)
                                        },
                                    )
                                });
                        if module_conflict {
                            self.unknown_guarded(
                                RustApiUnknownKind::AmbiguousReexport,
                                Some(&edge.crate_name),
                                &edge.module_path,
                                &edge.source_path,
                                &edge.cfg_guard,
                                format!(
                                    "{} has conflicting relative/root module origins",
                                    leaf.segments.join("::")
                                ),
                            );
                            module_candidates.clear();
                        }
                        for (target_module_path, target_guard) in module_candidates {
                            let target_visibility = self.module_visibility_at(
                                &edge.crate_name,
                                &target_module_path,
                                &target_guard,
                            );
                            if target_visibility != ModulePathVisibility::Public {
                                if edge.module_reachable {
                                    let (kind, evidence) = match target_visibility {
                                        ModulePathVisibility::OverlappingPrivate => (
                                            RustApiUnknownKind::AmbiguousReexport,
                                            format!(
                                                "module {} has an overlapping private proof",
                                                target_module_path.join("::")
                                            ),
                                        ),
                                        ModulePathVisibility::Unproven => (
                                            RustApiUnknownKind::UnresolvedReexport,
                                            format!(
                                                "module {} is not legally public at its declaration",
                                                target_module_path.join("::")
                                            ),
                                        ),
                                        ModulePathVisibility::Public => unreachable!(),
                                    };
                                    self.unknown_guarded(
                                        kind,
                                        Some(&edge.crate_name),
                                        &edge.module_path,
                                        &edge.source_path,
                                        &target_guard,
                                        evidence,
                                    );
                                }
                                continue;
                            }
                            let mut external_module_path = edge.module_path.clone();
                            external_module_path.push(normalize_identifier(&leaf.alias));
                            let mut alias_guard = target_guard;
                            alias_guard.extend(edge.cfg_guard.clone());
                            alias_guard.sort();
                            alias_guard.dedup();
                            let alias = RustModuleAlias {
                                crate_name: edge.crate_name.clone(),
                                module_path: external_module_path.clone(),
                                target_module_path: target_module_path.clone(),
                                cfg_guard: alias_guard.clone(),
                                source_path: edge.source_path.clone(),
                                provenance: self.provenance.clone(),
                                certainty: RustSourceCertainty::Confirmed,
                            };
                            let alias_identity =
                                (alias.crate_name.clone(), alias.module_path.clone());
                            if has_ambiguous_module_prefix(
                                &ambiguous_module_aliases,
                                &alias.crate_name,
                                &alias.module_path,
                            ) && !ambiguous_module_aliases.contains(&alias_identity)
                            {
                                continue;
                            }
                            let origins = module_alias_origins
                                .entry(alias_identity.clone())
                                .or_default();
                            origins.insert(ModuleAliasOrigin {
                                target_module_path: alias.target_module_path.clone(),
                                cfg_guard: alias.cfg_guard.clone(),
                            });
                            let conflict_pairs = module_alias_conflict_pairs(origins);
                            if !conflict_pairs.is_empty() {
                                let newly_ambiguous =
                                    ambiguous_module_aliases.insert(alias_identity.clone());
                                self.all_module_aliases.retain(|existing| {
                                    !(existing.crate_name == alias_identity.0
                                        && existing.module_path.starts_with(&alias_identity.1))
                                });
                                self.module_aliases.retain(|existing| {
                                    !(existing.crate_name == alias_identity.0
                                        && existing.module_path.starts_with(&alias_identity.1))
                                });
                                aliases.retain(|key, _| {
                                    !(key.crate_name == alias_identity.0
                                        && key.module_path.starts_with(&alias_identity.1))
                                });
                                ambiguous_aliases.retain(|key| {
                                    !(key.crate_name == alias_identity.0
                                        && key.module_path.starts_with(&alias_identity.1))
                                });
                                self.items.retain(|item| {
                                    !(item.key.crate_name == alias_identity.0
                                        && item.key.module_path.starts_with(&alias_identity.1))
                                });
                                self.reexports.retain(|reexport| {
                                    !(reexport.crate_name == alias_identity.0
                                        && reexport.module_path.starts_with(&alias_identity.1))
                                });
                                for (left, right) in conflict_pairs {
                                    let ambiguity_guard =
                                        combined_guards(&left.cfg_guard, &right.cfg_guard);
                                    self.unknown_guarded(
                                        RustApiUnknownKind::AmbiguousReexport,
                                        Some(&edge.crate_name),
                                        &edge.module_path,
                                        &edge.source_path,
                                        &ambiguity_guard,
                                        format!(
                                            "module alias {} has conflicting origins",
                                            alias.module_path.join("::")
                                        ),
                                    );
                                }
                                if newly_ambiguous {
                                    rebuild_required = true;
                                }
                                continue;
                            }
                            if !self.all_module_aliases.contains(&alias) {
                                self.all_module_aliases.push(alias.clone());
                                progressed = true;
                            }
                            if edge.module_reachable && !self.module_aliases.contains(&alias) {
                                self.module_aliases.push(alias);
                                progressed = true;
                            }
                            if edge.module_reachable {
                                for (blocked_path, blocked_guard) in self
                                    .overlapping_private_segments_below(
                                        &edge.crate_name,
                                        &target_module_path,
                                        &alias_guard,
                                    )
                                {
                                    let suffix = &blocked_path[target_module_path.len()..];
                                    let mut blocked_external_path = external_module_path.clone();
                                    blocked_external_path.extend_from_slice(suffix);
                                    self.unknown_guarded(
                                        RustApiUnknownKind::AmbiguousReexport,
                                        Some(&edge.crate_name),
                                        &blocked_external_path,
                                        &edge.source_path,
                                        &blocked_guard,
                                        format!(
                                            "private module proof overlaps public guarded lineage for {}",
                                            blocked_path.join("::")
                                        ),
                                    );
                                }
                            }
                            let descendants: Vec<_> = self
                                .symbols
                                .values()
                                .flatten()
                                .filter(|raw| {
                                    raw.key.crate_name == edge.crate_name
                                        && raw.key.module_path.starts_with(&target_module_path)
                                        && self.module_path_is_public_below(
                                            &edge.crate_name,
                                            &target_module_path,
                                            &raw.key.module_path,
                                            &raw.cfg_guard,
                                        )
                                })
                                .cloned()
                                .collect();
                            for mut raw in descendants {
                                let suffix = &raw.key.module_path[target_module_path.len()..];
                                let mut projected_module = external_module_path.clone();
                                projected_module.extend_from_slice(suffix);
                                let mut guards = raw.cfg_guard.clone();
                                guards.extend(alias_guard.clone());
                                guards.sort();
                                guards.dedup();
                                raw.cfg_guard = guards.clone();
                                let alias_key = SymbolKey {
                                    crate_name: edge.crate_name.clone(),
                                    module_path: projected_module.clone(),
                                    name: raw.key.name.clone(),
                                    namespace: raw.key.namespace,
                                };
                                let values = aliases.entry(alias_key).or_default();
                                if !values
                                    .iter()
                                    .any(|value| raw_symbol_semantic_eq(value, &raw))
                                {
                                    values.push(raw.clone());
                                    progressed = true;
                                }
                                if edge.module_reachable {
                                    self.items.push(self.external_item(
                                        &raw,
                                        projected_module.clone(),
                                        raw.key.name.clone(),
                                    ));
                                    self.reexports.push(RustApiReexport {
                                        crate_name: edge.crate_name.clone(),
                                        module_path: projected_module,
                                        external_name: raw.key.name.clone(),
                                        namespace: raw.key.namespace,
                                        target_module_path: raw.key.module_path.clone(),
                                        target_name: raw.key.name.clone(),
                                        cfg_guard: guards,
                                        source_path: edge.source_path.clone(),
                                        provenance: self.provenance.clone(),
                                        certainty: RustSourceCertainty::Confirmed,
                                    });
                                }
                            }
                            let alias_descendants: Vec<_> = aliases
                                .iter()
                                .filter(|(key, _)| {
                                    key.crate_name == edge.crate_name
                                        && key.module_path.starts_with(&target_module_path)
                                })
                                .flat_map(|(key, values)| {
                                    values
                                        .iter()
                                        .cloned()
                                        .map(|raw| (key.clone(), raw))
                                        .collect::<Vec<_>>()
                                })
                                .filter(|(key, raw)| {
                                    self.module_path_is_public_below(
                                        &edge.crate_name,
                                        &target_module_path,
                                        &key.module_path,
                                        &raw.cfg_guard,
                                    )
                                })
                                .collect();
                            for (alias_key_source, mut raw) in alias_descendants {
                                let suffix =
                                    &alias_key_source.module_path[target_module_path.len()..];
                                let mut projected_module = external_module_path.clone();
                                projected_module.extend_from_slice(suffix);
                                let mut guards = raw.cfg_guard.clone();
                                guards.extend(alias_guard.clone());
                                guards.sort();
                                guards.dedup();
                                raw.cfg_guard = guards.clone();
                                let projected_key = SymbolKey {
                                    crate_name: edge.crate_name.clone(),
                                    module_path: projected_module.clone(),
                                    name: alias_key_source.name.clone(),
                                    namespace: alias_key_source.namespace,
                                };
                                let values = aliases.entry(projected_key).or_default();
                                if !values
                                    .iter()
                                    .any(|value| raw_symbol_semantic_eq(value, &raw))
                                {
                                    values.push(raw.clone());
                                    progressed = true;
                                }
                                if edge.module_reachable {
                                    self.items.push(self.external_item(
                                        &raw,
                                        projected_module.clone(),
                                        alias_key_source.name.clone(),
                                    ));
                                    self.reexports.push(RustApiReexport {
                                        crate_name: edge.crate_name.clone(),
                                        module_path: projected_module,
                                        external_name: alias_key_source.name,
                                        namespace: raw.key.namespace,
                                        target_module_path: raw.key.module_path.clone(),
                                        target_name: raw.key.name.clone(),
                                        cfg_guard: guards,
                                        source_path: edge.source_path.clone(),
                                        provenance: self.provenance.clone(),
                                        certainty: RustSourceCertainty::Confirmed,
                                    });
                                }
                            }
                            let nested_module_aliases: Vec<_> = self
                                .all_module_aliases
                                .iter()
                                .filter(|nested| {
                                    nested.crate_name == edge.crate_name
                                        && nested.module_path.starts_with(&target_module_path)
                                        && nested.module_path != target_module_path
                                })
                                .cloned()
                                .collect();
                            for nested in nested_module_aliases {
                                let suffix = &nested.module_path[target_module_path.len()..];
                                let mut projected_alias_path = external_module_path.clone();
                                projected_alias_path.extend_from_slice(suffix);
                                let mut nested_guards = alias_guard.clone();
                                nested_guards.extend(nested.cfg_guard.clone());
                                nested_guards.sort();
                                nested_guards.dedup();
                                let projected_alias = RustModuleAlias {
                                    crate_name: edge.crate_name.clone(),
                                    module_path: projected_alias_path.clone(),
                                    target_module_path: nested.target_module_path.clone(),
                                    cfg_guard: nested_guards.clone(),
                                    source_path: edge.source_path.clone(),
                                    provenance: self.provenance.clone(),
                                    certainty: RustSourceCertainty::Confirmed,
                                };
                                if has_ambiguous_module_prefix(
                                    &ambiguous_module_aliases,
                                    &projected_alias.crate_name,
                                    &projected_alias.module_path,
                                ) {
                                    continue;
                                }
                                if !self.all_module_aliases.contains(&projected_alias) {
                                    self.all_module_aliases.push(projected_alias.clone());
                                    progressed = true;
                                }
                                if edge.module_reachable
                                    && !self.module_aliases.contains(&projected_alias)
                                {
                                    self.module_aliases.push(projected_alias);
                                    progressed = true;
                                }
                                let nested_descendants: Vec<_> = self
                                    .symbols
                                    .values()
                                    .flatten()
                                    .filter(|raw| {
                                        raw.key.crate_name == edge.crate_name
                                            && raw
                                                .key
                                                .module_path
                                                .starts_with(&nested.target_module_path)
                                            && self.module_path_is_public_below(
                                                &edge.crate_name,
                                                &nested.target_module_path,
                                                &raw.key.module_path,
                                                &raw.cfg_guard,
                                            )
                                    })
                                    .cloned()
                                    .collect();
                                for mut raw in nested_descendants {
                                    let nested_suffix =
                                        &raw.key.module_path[nested.target_module_path.len()..];
                                    let mut projected_module = projected_alias_path.clone();
                                    projected_module.extend_from_slice(nested_suffix);
                                    let mut guards = raw.cfg_guard.clone();
                                    guards.extend(nested_guards.clone());
                                    guards.sort();
                                    guards.dedup();
                                    raw.cfg_guard = guards.clone();
                                    if edge.module_reachable {
                                        self.items.push(self.external_item(
                                            &raw,
                                            projected_module.clone(),
                                            raw.key.name.clone(),
                                        ));
                                        self.reexports.push(RustApiReexport {
                                            crate_name: edge.crate_name.clone(),
                                            module_path: projected_module,
                                            external_name: raw.key.name.clone(),
                                            namespace: raw.key.namespace,
                                            target_module_path: raw.key.module_path.clone(),
                                            target_name: raw.key.name.clone(),
                                            cfg_guard: guards,
                                            source_path: edge.source_path.clone(),
                                            provenance: self.provenance.clone(),
                                            certainty: RustSourceCertainty::Confirmed,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    let candidates = self.resolve_use_leaf(&edge, leaf, &aliases);
                    if candidates.is_empty() {
                        continue;
                    }
                    let prepared: Vec<_> = candidates
                        .into_iter()
                        .map(|mut raw| {
                            let mut guards = raw.cfg_guard.clone();
                            guards.extend(edge.cfg_guard.clone());
                            guards.sort();
                            guards.dedup();
                            raw.cfg_guard = guards;
                            raw
                        })
                        .collect();
                    let mut conflicts = BTreeSet::new();
                    for raw in &prepared {
                        if prepared.iter().any(|other| {
                            other.key.namespace == raw.key.namespace
                                && !guards_proven_disjoint(&other.cfg_guard, &raw.cfg_guard)
                                && (other.key != raw.key || other.contract != raw.contract)
                        }) {
                            conflicts.insert((raw.key.namespace, raw.cfg_guard.clone()));
                        }
                    }
                    for (namespace, guards) in &conflicts {
                        self.unknown_guarded(
                            RustApiUnknownKind::AmbiguousReexport,
                            Some(&edge.crate_name),
                            &edge.module_path,
                            &edge.source_path,
                            guards,
                            format!("{} is ambiguous in {namespace:?}", leaf.segments.join("::")),
                        );
                    }
                    for raw in prepared {
                        if conflicts.contains(&(raw.key.namespace, raw.cfg_guard.clone())) {
                            continue;
                        }
                        let guards = raw.cfg_guard.clone();
                        let alias_key = SymbolKey {
                            crate_name: edge.crate_name.clone(),
                            module_path: edge.module_path.clone(),
                            name: normalize_identifier(&leaf.alias),
                            namespace: raw.key.namespace,
                        };
                        let origins = symbol_alias_origins.entry(alias_key.clone()).or_default();
                        origins.insert(SymbolAliasOrigin {
                            target: raw.key.clone(),
                            kind: raw.kind,
                            contract: raw.contract.clone(),
                            cfg_guard: guards.clone(),
                        });
                        let conflict_pairs = symbol_alias_conflict_pairs(origins);
                        if !conflict_pairs.is_empty() {
                            let alias_name = alias_key.name.clone();
                            self.items.retain(|item| {
                                !(item.key.crate_name == edge.crate_name
                                    && item.key.module_path == edge.module_path
                                    && item.key.external_name == alias_name
                                    && item.key.namespace == raw.key.namespace)
                            });
                            self.reexports.retain(|reexport| {
                                !(reexport.crate_name == edge.crate_name
                                    && reexport.module_path == edge.module_path
                                    && reexport.external_name == alias_name
                                    && reexport.namespace == raw.key.namespace)
                            });
                            aliases.remove(&alias_key);
                            let newly_ambiguous = ambiguous_aliases.insert(alias_key.clone());
                            for (left, right) in conflict_pairs {
                                let ambiguity_guard =
                                    combined_guards(&left.cfg_guard, &right.cfg_guard);
                                self.unknown_guarded(
                                    RustApiUnknownKind::AmbiguousReexport,
                                    Some(&edge.crate_name),
                                    &edge.module_path,
                                    &edge.source_path,
                                    &ambiguity_guard,
                                    format!(
                                        "symbol alias {alias_name} has conflicting origins in {:?}",
                                        raw.key.namespace
                                    ),
                                );
                            }
                            if newly_ambiguous {
                                rebuild_required = true;
                            }
                            continue;
                        }
                        if ambiguous_aliases.contains(&alias_key) {
                            continue;
                        }
                        let values = aliases.entry(alias_key).or_default();
                        if values
                            .iter()
                            .any(|value| raw_symbol_semantic_eq(value, &raw))
                        {
                            continue;
                        }
                        values.push(raw.clone());
                        progressed = true;
                        if edge.module_reachable {
                            self.items.push(self.external_item(
                                &raw,
                                edge.module_path.clone(),
                                normalize_identifier(&leaf.alias),
                            ));
                            self.reexports.push(RustApiReexport {
                                crate_name: edge.crate_name.clone(),
                                module_path: edge.module_path.clone(),
                                external_name: normalize_identifier(&leaf.alias),
                                namespace: raw.key.namespace,
                                target_module_path: raw.key.module_path.clone(),
                                target_name: raw.key.name.clone(),
                                cfg_guard: guards,
                                source_path: edge.source_path.clone(),
                                provenance: self.provenance.clone(),
                                certainty: RustSourceCertainty::Confirmed,
                            });
                        }
                    }
                }
            }
            if !rebuild_required && !progressed {
                completed = true;
                break;
            }
        }
        if !completed {
            self.items = source_backed_items;
            self.reexports.clear();
            self.module_aliases.clear();
            self.all_module_aliases.clear();
            let crate_name = self.uses.first().map(|edge| edge.crate_name.clone());
            self.unknown_guarded(
                RustApiUnknownKind::ResolutionLimit,
                crate_name.as_deref(),
                &[],
                "<reexport-resolver>",
                &[],
                format!(
                    "reexport closure did not settle within graph-derived budget {iteration_budget}"
                ),
            );
            return;
        }
        for edge in self.uses.clone() {
            for leaf in &edge.leaves {
                if !leaf.glob
                    && self.resolve_use_leaf(&edge, leaf, &aliases).is_empty()
                    && self.resolve_module_leaf(&edge, leaf).is_empty()
                {
                    let kind = if self.looks_like_reexport_cycle(&edge, leaf) {
                        RustApiUnknownKind::ReexportCycle
                    } else if self.looks_like_external_resolution(&edge, leaf) {
                        RustApiUnknownKind::UnsupportedExternResolution
                    } else {
                        RustApiUnknownKind::UnresolvedReexport
                    };
                    self.unknown_guarded(
                        kind,
                        Some(&edge.crate_name),
                        &edge.module_path,
                        &edge.source_path,
                        &edge.cfg_guard,
                        leaf.segments.join("::"),
                    );
                }
            }
        }
    }

    fn looks_like_reexport_cycle(&self, edge: &UseEdge, leaf: &UseLeaf) -> bool {
        let Some(target_name) = leaf.segments.last() else {
            return false;
        };
        self.uses.iter().any(|candidate| {
            candidate.crate_name == edge.crate_name
                && !(candidate.module_path == edge.module_path
                    && candidate.source_path == edge.source_path
                    && candidate.leaves == edge.leaves)
                && candidate
                    .leaves
                    .iter()
                    .any(|other| other.alias == *target_name)
        })
    }

    fn looks_like_external_resolution(&self, edge: &UseEdge, leaf: &UseLeaf) -> bool {
        let Some(first) = leaf.segments.first() else {
            return false;
        };
        if matches!(first.as_str(), "crate" | "self" | "super") {
            return false;
        }
        !self.modules.iter().any(|module| {
            module.crate_name == edge.crate_name
                && (module.module_path.first() == Some(first)
                    || (module.module_path.starts_with(&edge.module_path)
                        && module.module_path.get(edge.module_path.len()) == Some(first)))
        })
    }

    fn resolve_use_leaf(
        &self,
        edge: &UseEdge,
        leaf: &UseLeaf,
        aliases: &BTreeMap<SymbolKey, Vec<RawSymbol>>,
    ) -> Vec<RawSymbol> {
        let paths = use_candidate_paths(&edge.module_path, &leaf.segments);
        let mut found = Vec::new();
        for path in paths {
            let Some((name, module_path)) = path.split_last() else {
                continue;
            };
            for namespace in [
                RustNamespace::Type,
                RustNamespace::Value,
                RustNamespace::Macro,
            ] {
                let key = SymbolKey {
                    crate_name: edge.crate_name.clone(),
                    module_path: module_path.to_vec(),
                    name: normalize_identifier(name),
                    namespace,
                };
                if let Some(values) = self.symbols.get(&key) {
                    found.extend(values.clone());
                }
                if let Some(values) = aliases.get(&key) {
                    found.extend(values.clone());
                }
            }
        }
        found.sort_by(|left, right| {
            (&left.key, &left.cfg_guard, &left.contract).cmp(&(
                &right.key,
                &right.cfg_guard,
                &right.contract,
            ))
        });
        found.dedup_by(|left, right| {
            left.key == right.key
                && left.cfg_guard == right.cfg_guard
                && left.contract == right.contract
        });
        found
    }

    fn resolve_module_leaf(
        &self,
        edge: &UseEdge,
        leaf: &UseLeaf,
    ) -> Vec<(Vec<String>, Vec<String>)> {
        let mut found = Vec::new();
        for path in use_candidate_paths(&edge.module_path, &leaf.segments) {
            for module in &self.modules {
                if module.crate_name == edge.crate_name && module.module_path == path {
                    found.push((path.clone(), module.cfg_guard.clone()));
                }
            }
            for alias in &self.all_module_aliases {
                if alias.crate_name == edge.crate_name && alias.module_path == path {
                    found.push((alias.target_module_path.clone(), alias.cfg_guard.clone()));
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }

    fn module_visibility_at(
        &self,
        crate_name: &str,
        module_path: &[String],
        guard: &[String],
    ) -> ModulePathVisibility {
        let positive_matching = |proof: &&ModuleProof| {
            proof.crate_name == crate_name
                && proof.module_path == module_path
                && guard_lineage_contains(guard, &proof.cfg_guard)
        };
        let has_public = self
            .module_proofs
            .iter()
            .filter(positive_matching)
            .any(|proof| proof.declared_public);
        if !has_public {
            return ModulePathVisibility::Unproven;
        }
        let has_overlapping_private = self.module_proofs.iter().any(|proof| {
            proof.crate_name == crate_name
                && proof.module_path == module_path
                && !proof.declared_public
                && !guards_proven_disjoint(guard, &proof.cfg_guard)
        });
        if has_overlapping_private {
            ModulePathVisibility::OverlappingPrivate
        } else {
            ModulePathVisibility::Public
        }
    }

    fn module_path_is_public_below(
        &self,
        crate_name: &str,
        root: &[String],
        descendant: &[String],
        guard: &[String],
    ) -> bool {
        if !descendant.starts_with(root) {
            return false;
        }
        (root.len() + 1..=descendant.len()).all(|length| {
            self.module_visibility_at(crate_name, &descendant[..length], guard)
                == ModulePathVisibility::Public
        })
    }

    fn overlapping_private_segments_below(
        &self,
        crate_name: &str,
        root: &[String],
        alias_guard: &[String],
    ) -> Vec<(Vec<String>, Vec<String>)> {
        let mut blocked = Vec::new();
        for public in self.module_proofs.iter().filter(|proof| {
            proof.crate_name == crate_name
                && proof.declared_public
                && proof.module_path.starts_with(root)
                && proof.module_path.len() > root.len()
        }) {
            let mut effective_guard = alias_guard.to_vec();
            effective_guard.extend(public.cfg_guard.clone());
            effective_guard.sort();
            effective_guard.dedup();
            let overlapping_private: Vec<_> = self
                .module_proofs
                .iter()
                .filter(|private| {
                    private.crate_name == crate_name
                        && private.module_path == public.module_path
                        && !private.declared_public
                        && !guards_proven_disjoint(&effective_guard, &private.cfg_guard)
                })
                .collect();
            if !overlapping_private.is_empty() {
                let mut ambiguity_guard = effective_guard;
                for private in overlapping_private {
                    ambiguity_guard.extend(private.cfg_guard.clone());
                }
                ambiguity_guard.sort();
                ambiguity_guard.dedup();
                blocked.push((public.module_path.clone(), ambiguity_guard));
            }
        }
        blocked.sort();
        blocked.dedup();
        blocked
    }

    fn resolve_trait_impls(&mut self) {
        for pending in self.pending_trait_impls.clone() {
            let trait_public = self.items.iter().any(|item| {
                item.kind == RustApiItemKind::Trait
                    && item.key.crate_name == pending.crate_name
                    && item.origin_module_path == pending.trait_module_path
                    && item.origin_name == pending.trait_name
                    && !guards_proven_disjoint(&item.cfg_guard, &pending.cfg_guard)
            });
            let trait_declared_locally = self.declarations.iter().any(|declaration| {
                declaration.kind == RustApiItemKind::Trait
                    && declaration.key.crate_name == pending.crate_name
                    && declaration.key.module_path == pending.trait_module_path
                    && declaration.key.external_name == pending.trait_name
                    && !guards_proven_disjoint(&declaration.cfg_guard, &pending.cfg_guard)
            });
            let owner_public = self.items.iter().any(|item| {
                item.key.namespace == RustNamespace::Type
                    && item.key.crate_name == pending.crate_name
                    && item.origin_module_path == pending.owner_module_path
                    && item.origin_name == pending.owner_name
                    && !guards_proven_disjoint(&item.cfg_guard, &pending.cfg_guard)
            });
            if owner_public
                && (trait_public || (!trait_declared_locally && trait_is_external_public(&pending)))
            {
                self.unknown_guarded(
                    RustApiUnknownKind::TraitImplResolution,
                    Some(&pending.crate_name),
                    &pending.owner_module_path,
                    &pending.source_path,
                    &pending.cfg_guard,
                    pending.evidence,
                );
            }
        }
    }

    fn resolve_inherent_items(&mut self) {
        for assoc in self.pending_assoc.clone() {
            let owner_key = SymbolKey {
                crate_name: assoc.crate_name.clone(),
                module_path: assoc.owner_module_path.clone(),
                name: assoc.owner_name.clone(),
                namespace: RustNamespace::Type,
            };
            if !self.symbols.contains_key(&owner_key) {
                self.unknown_guarded(
                    RustApiUnknownKind::UnresolvedInherentOwner,
                    Some(&assoc.crate_name),
                    &assoc.owner_module_path,
                    &assoc.source_path,
                    &assoc.cfg_guard,
                    format!("cannot prove inherent owner {}", assoc.owner_name),
                );
                continue;
            }
            let external_types: Vec<_> = self
                .items
                .iter()
                .filter(|item| {
                    item.key.namespace == RustNamespace::Type
                        && item.key.crate_name == assoc.crate_name
                        && item.origin_module_path == assoc.owner_module_path
                        && item.origin_name == assoc.owner_name
                })
                .cloned()
                .collect();
            for external_type in external_types {
                let mut guards = external_type.cfg_guard.clone();
                guards.extend(assoc.cfg_guard.clone());
                guards.sort();
                guards.dedup();
                self.items.push(RustApiItem {
                    key: RustApiItemKey {
                        crate_name: assoc.crate_name.clone(),
                        module_path: external_type.key.module_path,
                        namespace: assoc.namespace,
                        external_name: format!(
                            "{}::{}",
                            external_type.key.external_name, assoc.name
                        ),
                    },
                    kind: assoc.kind,
                    contract: assoc.contract.clone(),
                    cfg_guard: guards,
                    source_path: assoc.source_path.clone(),
                    evidence: assoc.evidence.clone(),
                    provenance: self.provenance.clone(),
                    certainty: RustSourceCertainty::Confirmed,
                    origin_module_path: assoc.owner_module_path.clone(),
                    origin_name: format!("{}::{}", assoc.owner_name, assoc.name),
                });
            }
        }
    }

    fn read_utf8(
        &mut self,
        crate_name: Option<&str>,
        module_path: &[String],
        path: &str,
        cfg_guard: &[String],
        read_kind: RustApiUnknownKind,
        non_utf8_kind: RustApiUnknownKind,
    ) -> Option<String> {
        match self.source.read(path) {
            Ok(RevisionRead::Bytes(bytes))
                if bytes.content_kind == RevisionContentKind::Utf8Text =>
            {
                match String::from_utf8(bytes.bytes) {
                    Ok(text) => Some(text),
                    Err(error) => {
                        self.unknown_guarded(
                            non_utf8_kind,
                            crate_name,
                            module_path,
                            path,
                            cfg_guard,
                            error.to_string(),
                        );
                        None
                    }
                }
            }
            Ok(RevisionRead::Bytes(_)) => {
                self.unknown_guarded(
                    non_utf8_kind,
                    crate_name,
                    module_path,
                    path,
                    cfg_guard,
                    "source bytes are binary or non-UTF8".to_owned(),
                );
                None
            }
            Ok(other) => {
                let kind = match other {
                    _ if read_kind == RustApiUnknownKind::ManifestRead => read_kind,
                    RevisionRead::NonRegular { .. } => RustApiUnknownKind::NonRegularModule,
                    _ => read_kind,
                };
                self.unknown_guarded(
                    kind,
                    crate_name,
                    module_path,
                    path,
                    cfg_guard,
                    format!("{other:?}"),
                );
                None
            }
            Err(error) => {
                self.unknown_guarded(
                    read_kind,
                    crate_name,
                    module_path,
                    path,
                    cfg_guard,
                    error.to_string(),
                );
                None
            }
        }
    }

    fn include_digest(&self, source_path: &str, macro_call: &syn::Macro) -> String {
        let Some(relative) = include_literal_path(macro_call) else {
            return "unresolved".to_owned();
        };
        let parent = parent_repo_path(source_path);
        let Ok(path) = safe_join_repo_path(&parent, &relative) else {
            return "unresolved".to_owned();
        };
        match self.source.read(&path) {
            Ok(RevisionRead::Bytes(bytes)) => {
                use sha2::{Digest, Sha256};
                hex::encode(Sha256::digest(&bytes.bytes))
            }
            _ => "unresolved".to_owned(),
        }
    }

    fn unknown(
        &mut self,
        kind: RustApiUnknownKind,
        crate_name: Option<&str>,
        module_path: &[String],
        source_path: &str,
        evidence: String,
    ) {
        self.unknown_guarded(kind, crate_name, module_path, source_path, &[], evidence);
    }

    fn unknown_guarded(
        &mut self,
        kind: RustApiUnknownKind,
        crate_name: Option<&str>,
        module_path: &[String],
        source_path: &str,
        cfg_guard: &[String],
        evidence: String,
    ) {
        self.unknowns.push(RustApiUnknown {
            kind,
            crate_name: crate_name.map(str::to_owned),
            module_path: module_path.to_vec(),
            source_path: source_path.to_owned(),
            cfg_guard: cfg_guard.to_vec(),
            evidence,
            provenance: self.provenance.clone(),
        });
    }
}

fn public_item_contract(item: &Item) -> Option<(String, RustNamespace, RustApiItemKind, String)> {
    let (name, namespace, kind, contract, public) = ordinary_item_contract(item)?;
    public.then_some((name, namespace, kind, contract))
}

fn ordinary_item_contract(
    item: &Item,
) -> Option<(String, RustNamespace, RustApiItemKind, String, bool)> {
    let (name, namespace, kind, public) = match item {
        Item::Fn(value) => (
            value.sig.ident.to_string(),
            RustNamespace::Value,
            RustApiItemKind::Function,
            is_public(&value.vis),
        ),
        Item::Struct(value) => (
            value.ident.to_string(),
            RustNamespace::Type,
            RustApiItemKind::Struct,
            is_public(&value.vis),
        ),
        Item::Union(value) => (
            value.ident.to_string(),
            RustNamespace::Type,
            RustApiItemKind::Union,
            is_public(&value.vis),
        ),
        Item::Enum(value) => (
            value.ident.to_string(),
            RustNamespace::Type,
            RustApiItemKind::Enum,
            is_public(&value.vis),
        ),
        Item::Trait(value) => (
            value.ident.to_string(),
            RustNamespace::Type,
            RustApiItemKind::Trait,
            is_public(&value.vis),
        ),
        Item::Type(value) => (
            value.ident.to_string(),
            RustNamespace::Type,
            RustApiItemKind::TypeAlias,
            is_public(&value.vis),
        ),
        Item::Const(value) => (
            value.ident.to_string(),
            RustNamespace::Value,
            RustApiItemKind::Constant,
            is_public(&value.vis),
        ),
        Item::Static(value) => (
            value.ident.to_string(),
            RustNamespace::Value,
            RustApiItemKind::Static,
            is_public(&value.vis),
        ),
        _ => return None,
    };
    Some((
        normalize_identifier(name),
        namespace,
        kind,
        normalized_contract_without_item_name(item.clone()),
        public,
    ))
}

fn normalized_contract(mut item: Item) -> String {
    normalize_item_attrs(&mut item);
    match &mut item {
        Item::Fn(function) => {
            *function.block = syn::parse_quote!({});
            alpha_normalize_signature(&mut function.sig);
            trim_signature_punctuation(&mut function.sig);
        }
        Item::Struct(value) => {
            let layout_sensitive = has_layout_sensitive_repr(&value.attrs);
            let mut normalizer = SignatureAlphaNormalizer::default();
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            for field in &mut value.fields {
                field.ty = normalizer.fold_type(field.ty.clone());
            }
            normalizer.pop_scope();
            filter_private_fields(&mut value.fields, layout_sensitive);
            trim_fields_punctuation(&mut value.fields);
            trim_generics_punctuation(&mut value.generics);
        }
        Item::Union(value) => {
            let layout_sensitive = has_layout_sensitive_repr(&value.attrs);
            let mut fields = Fields::Named(value.fields.clone());
            let mut normalizer = SignatureAlphaNormalizer::default();
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            for field in &mut fields {
                field.ty = normalizer.fold_type(field.ty.clone());
            }
            normalizer.pop_scope();
            filter_private_fields(&mut fields, layout_sensitive);
            let Fields::Named(fields) = fields else {
                unreachable!("union fields are named")
            };
            value.fields = fields;
            trim_trailing_punct(&mut value.fields.named);
            trim_generics_punctuation(&mut value.generics);
        }
        Item::Enum(value) => {
            let mut normalizer = SignatureAlphaNormalizer::default();
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            for variant in &mut value.variants {
                for field in &mut variant.fields {
                    field.ty = normalizer.fold_type(field.ty.clone());
                }
            }
            normalizer.pop_scope();
            trim_trailing_punct(&mut value.variants);
            trim_generics_punctuation(&mut value.generics);
            for variant in &mut value.variants {
                trim_fields_punctuation(&mut variant.fields);
            }
        }
        Item::Trait(value) => {
            let mut normalizer = SignatureAlphaNormalizer::default();
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            value.supertraits = value
                .supertraits
                .clone()
                .into_iter()
                .map(|bound| normalizer.fold_type_param_bound(bound))
                .collect();
            trim_generics_punctuation(&mut value.generics);
            for trait_item in &mut value.items {
                match trait_item {
                    syn::TraitItem::Const(value) => {
                        normalizer.push_generics(&value.generics);
                        value.generics = normalizer.fold_generics(value.generics.clone());
                        value.ty = normalizer.fold_type(value.ty.clone());
                        if let Some((eq, default)) = value.default.take() {
                            value.default = Some((eq, normalizer.fold_expr(default)));
                        }
                        trim_generics_punctuation(&mut value.generics);
                        normalizer.pop_scope();
                    }
                    syn::TraitItem::Fn(function) => {
                        function.default = None;
                        function.semi_token = Some(Default::default());
                        normalizer.normalize_signature(&mut function.sig);
                        trim_signature_punctuation(&mut function.sig);
                    }
                    syn::TraitItem::Type(value) => {
                        normalizer.push_generics(&value.generics);
                        value.generics = normalizer.fold_generics(value.generics.clone());
                        value.bounds = value
                            .bounds
                            .clone()
                            .into_iter()
                            .map(|bound| normalizer.fold_type_param_bound(bound))
                            .collect();
                        if let Some((eq, default)) = value.default.take() {
                            value.default = Some((eq, normalizer.fold_type(default)));
                        }
                        trim_generics_punctuation(&mut value.generics);
                        normalizer.pop_scope();
                    }
                    _ => {}
                }
            }
            normalizer.pop_scope();
        }
        Item::Type(value) => {
            let mut normalizer = SignatureAlphaNormalizer::default();
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            *value.ty = normalizer.fold_type((*value.ty).clone());
            normalizer.pop_scope();
            trim_generics_punctuation(&mut value.generics);
        }
        _ => {}
    }
    let folded = CanonicalFold.fold_item(item);
    canonical_tokens(folded.to_token_stream())
}

fn normalized_contract_without_item_name(mut item: Item) -> String {
    let replacement = |ident: &mut syn::Ident| {
        *ident = syn::Ident::new("__prview_name", ident.span());
    };
    match &mut item {
        Item::Fn(value) => replacement(&mut value.sig.ident),
        Item::Struct(value) => replacement(&mut value.ident),
        Item::Union(value) => replacement(&mut value.ident),
        Item::Enum(value) => replacement(&mut value.ident),
        Item::Trait(value) => replacement(&mut value.ident),
        Item::Type(value) => replacement(&mut value.ident),
        Item::Const(value) => replacement(&mut value.ident),
        Item::Static(value) => replacement(&mut value.ident),
        _ => {}
    }
    normalized_contract(item)
}

fn normalized_macro_contract(item: &syn::ItemMacro) -> String {
    let mut item = item.clone();
    normalize_attrs(&mut item.attrs, true);
    if let Some(ident) = &mut item.ident {
        *ident = syn::Ident::new("__prview_name", ident.span());
    }
    canonical_tokens(item.to_token_stream())
}

fn filter_private_fields(fields: &mut Fields, layout_sensitive: bool) {
    match fields {
        Fields::Named(named) => {
            if !layout_sensitive {
                // Rust's default representation gives downstream callers no
                // field-order ABI. Keep private field TYPES in the contract
                // (they can change Send/Sync and other auto traits), but sort
                // them by their semantic input so a pure private-field reorder
                // is not reported as a breaking API change. Public named-field
                // order is likewise not observable in literals or patterns.
                let mut fields = named.named.iter().cloned().collect::<Vec<_>>();
                fields.sort_by_key(|field| {
                    if is_public(&field.vis) {
                        format!(
                            "0:{}",
                            field
                                .ident
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default()
                        )
                    } else {
                        let mut semantic = field.clone();
                        semantic.ident = Some(syn::Ident::new(
                            "__prview_private_field",
                            field
                                .ident
                                .as_ref()
                                .expect("named field has an identifier")
                                .span(),
                        ));
                        format!("1:{}", canonical_tokens(semantic.to_token_stream()))
                    }
                });
                named.named = fields.into_iter().collect();
            }
            for (index, field) in named.named.iter_mut().enumerate() {
                if !is_public(&field.vis) {
                    field.ident = Some(syn::Ident::new(
                        &format!("__prview_private_field_{index}"),
                        field
                            .ident
                            .as_ref()
                            .expect("named field has an identifier")
                            .span(),
                    ));
                }
            }
        }
        Fields::Unnamed(unnamed) => {
            unnamed.unnamed = unnamed
                .unnamed
                .clone()
                .into_iter()
                .enumerate()
                .map(|(index, mut field)| {
                    let marker: Attribute = if is_public(&field.vis) {
                        syn::parse_quote!(#[prview_tuple_index = #index])
                    } else {
                        syn::parse_quote!(#[prview_tuple_private_index = #index])
                    };
                    field.attrs.push(marker);
                    field
                })
                .collect();
        }
        Fields::Unit => {}
    }
}

fn has_layout_sensitive_repr(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attribute| {
        if !attribute.path().is_ident("repr") {
            return false;
        }
        let syn::Meta::List(list) = &attribute.meta else {
            return false;
        };
        list.tokens
            .to_string()
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|token| matches!(token, "C" | "packed" | "transparent"))
    })
}

fn normalize_item_attrs(item: &mut Item) {
    let attrs = match item {
        Item::Fn(value) => &mut value.attrs,
        Item::Struct(value) => &mut value.attrs,
        Item::Union(value) => &mut value.attrs,
        Item::Enum(value) => &mut value.attrs,
        Item::Trait(value) => &mut value.attrs,
        Item::Type(value) => &mut value.attrs,
        Item::Const(value) => &mut value.attrs,
        Item::Static(value) => &mut value.attrs,
        _ => return,
    };
    normalize_attrs(attrs, true);
    match item {
        Item::Struct(value) => normalize_fields(&mut value.fields),
        Item::Union(value) => {
            for field in &mut value.fields.named {
                normalize_attrs(&mut field.attrs, false);
            }
        }
        Item::Enum(value) => {
            for variant in &mut value.variants {
                normalize_attrs(&mut variant.attrs, false);
                normalize_fields(&mut variant.fields);
            }
        }
        Item::Trait(value) => {
            for trait_item in &mut value.items {
                match trait_item {
                    syn::TraitItem::Const(value) => normalize_attrs(&mut value.attrs, false),
                    syn::TraitItem::Fn(value) => normalize_attrs(&mut value.attrs, false),
                    syn::TraitItem::Type(value) => normalize_attrs(&mut value.attrs, false),
                    syn::TraitItem::Macro(value) => normalize_attrs(&mut value.attrs, false),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn normalize_fields(fields: &mut Fields) {
    for field in fields.iter_mut() {
        normalize_attrs(&mut field.attrs, false);
    }
}

fn normalize_attrs(attrs: &mut Vec<Attribute>, drop_cfg: bool) {
    attrs.retain_mut(|attr| {
        let name = attr
            .path()
            .segments
            .first()
            .map(|segment| segment.ident.to_string());
        if matches!(
            name.as_deref(),
            Some("doc" | "rustfmt" | "allow" | "warn" | "deny" | "forbid" | "expect")
        ) || (drop_cfg && matches!(name.as_deref(), Some("cfg" | "cfg_attr")))
        {
            return false;
        }
        canonicalize_meta_in_place(&mut attr.meta);
        true
    });
}

fn canonicalize_meta_in_place(meta: &mut Meta) {
    let Meta::List(list) = meta else {
        return;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return;
    };
    let mut parts: Vec<_> = parts.into_iter().collect();
    for part in &mut parts {
        canonicalize_meta_in_place(part);
    }
    if list.path.is_ident("all") || list.path.is_ident("any") {
        parts.sort_by_key(canonical_meta);
        parts.dedup_by(|left, right| canonical_meta(left) == canonical_meta(right));
    }
    list.tokens = quote!(#(#parts),*);
}

fn trim_signature_punctuation(signature: &mut syn::Signature) {
    trim_trailing_punct(&mut signature.inputs);
    trim_generics_punctuation(&mut signature.generics);
}

fn alpha_normalize_signature(signature: &mut syn::Signature) {
    SignatureAlphaNormalizer::default().normalize_signature(signature);
}

/// Canonicalize names that bind only inside a public signature while retaining
/// every use relationship. Generic order remains observable; spelling does not.
#[derive(Default)]
struct SignatureAlphaNormalizer {
    ident_scopes: Vec<BTreeMap<String, syn::Ident>>,
    lifetime_scopes: Vec<BTreeMap<String, syn::Lifetime>>,
    next_type: usize,
    next_const: usize,
    next_lifetime: usize,
}

impl SignatureAlphaNormalizer {
    fn normalize_signature(&mut self, signature: &mut syn::Signature) {
        self.push_generics(&signature.generics);
        signature.generics = self.fold_generics(signature.generics.clone());
        for argument in &mut signature.inputs {
            match argument {
                syn::FnArg::Receiver(receiver) => {
                    *receiver = self.fold_receiver(receiver.clone());
                }
                syn::FnArg::Typed(argument) => {
                    *argument.ty = self.fold_type((*argument.ty).clone());
                    *argument.pat = syn::parse_quote!(_);
                }
            }
        }
        signature.output = self.fold_return_type(signature.output.clone());
        self.pop_scope();
    }

    fn push_generics(&mut self, generics: &syn::Generics) {
        let params = generics.params.iter().cloned().collect::<Vec<_>>();
        self.push_params(&params);
    }

    fn push_params(&mut self, params: &[syn::GenericParam]) {
        let mut idents = BTreeMap::new();
        let mut lifetimes = BTreeMap::new();
        for parameter in params {
            match parameter {
                syn::GenericParam::Lifetime(parameter) => {
                    let replacement = syn::Lifetime::new(
                        &format!("'__prview_l{}", self.next_lifetime),
                        parameter.lifetime.ident.span(),
                    );
                    self.next_lifetime += 1;
                    lifetimes.insert(parameter.lifetime.to_string(), replacement);
                }
                syn::GenericParam::Type(parameter) => {
                    let replacement = syn::Ident::new(
                        &format!("__PrviewT{}", self.next_type),
                        parameter.ident.span(),
                    );
                    self.next_type += 1;
                    idents.insert(parameter.ident.to_string(), replacement);
                }
                syn::GenericParam::Const(parameter) => {
                    let replacement = syn::Ident::new(
                        &format!("__PRVIEW_C{}", self.next_const),
                        parameter.ident.span(),
                    );
                    self.next_const += 1;
                    idents.insert(parameter.ident.to_string(), replacement);
                }
            }
        }
        self.ident_scopes.push(idents);
        self.lifetime_scopes.push(lifetimes);
    }

    fn pop_scope(&mut self) {
        self.ident_scopes.pop();
        self.lifetime_scopes.pop();
    }

    fn fold_path_idents(&mut self, mut path: syn::Path, fold_head: bool) -> syn::Path {
        if fold_head
            && path.leading_colon.is_none()
            && let Some(segment) = path.segments.first_mut()
        {
            segment.ident = self.fold_ident(segment.ident.clone());
        }
        for segment in &mut path.segments {
            segment.arguments = self.fold_path_arguments(segment.arguments.clone());
        }
        path
    }
}

impl Fold for SignatureAlphaNormalizer {
    fn fold_ident(&mut self, ident: syn::Ident) -> syn::Ident {
        self.ident_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&ident.to_string()).cloned())
            .unwrap_or(ident)
    }

    fn fold_lifetime(&mut self, lifetime: syn::Lifetime) -> syn::Lifetime {
        self.lifetime_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&lifetime.to_string()).cloned())
            .unwrap_or(lifetime)
    }

    fn fold_path(&mut self, path: syn::Path) -> syn::Path {
        // Only the head of an unqualified path can name a type or const
        // binder. Later segments are associated items and remain public API.
        self.fold_path_idents(path, true)
    }

    fn fold_type_path(&mut self, type_path: syn::TypePath) -> syn::TypePath {
        let qualified = type_path.qself.is_some();
        syn::TypePath {
            qself: type_path.qself.map(|qself| self.fold_qself(qself)),
            // In `<T as Trait>::Item`, the binder lives in `qself.ty`;
            // `Trait` and `Item` are public names, even if one is spelled T.
            path: self.fold_path_idents(type_path.path, !qualified),
        }
    }

    fn fold_assoc_type(&mut self, associated: syn::AssocType) -> syn::AssocType {
        syn::AssocType {
            ident: associated.ident,
            generics: associated
                .generics
                .map(|generics| self.fold_angle_bracketed_generic_arguments(generics)),
            eq_token: associated.eq_token,
            ty: self.fold_type(associated.ty),
        }
    }

    fn fold_assoc_const(&mut self, associated: syn::AssocConst) -> syn::AssocConst {
        syn::AssocConst {
            ident: associated.ident,
            generics: associated
                .generics
                .map(|generics| self.fold_angle_bracketed_generic_arguments(generics)),
            eq_token: associated.eq_token,
            value: self.fold_expr(associated.value),
        }
    }

    fn fold_constraint(&mut self, constraint: syn::Constraint) -> syn::Constraint {
        syn::Constraint {
            ident: constraint.ident,
            generics: constraint
                .generics
                .map(|generics| self.fold_angle_bracketed_generic_arguments(generics)),
            colon_token: constraint.colon_token,
            bounds: constraint
                .bounds
                .into_iter()
                .map(|bound| self.fold_type_param_bound(bound))
                .collect(),
        }
    }

    fn fold_member(&mut self, member: syn::Member) -> syn::Member {
        member
    }

    fn fold_type_bare_fn(&mut self, bare_fn: syn::TypeBareFn) -> syn::TypeBareFn {
        let has_binders = bare_fn.lifetimes.is_some();
        if let Some(lifetimes) = &bare_fn.lifetimes {
            let params = lifetimes.lifetimes.iter().cloned().collect::<Vec<_>>();
            self.push_params(&params);
        }
        let folded = syn::fold::fold_type_bare_fn(self, bare_fn);
        if has_binders {
            self.pop_scope();
        }
        folded
    }

    fn fold_trait_bound(&mut self, bound: syn::TraitBound) -> syn::TraitBound {
        let has_binders = bound.lifetimes.is_some();
        if let Some(lifetimes) = &bound.lifetimes {
            let params = lifetimes.lifetimes.iter().cloned().collect::<Vec<_>>();
            self.push_params(&params);
        }
        let folded = syn::fold::fold_trait_bound(self, bound);
        if has_binders {
            self.pop_scope();
        }
        folded
    }

    fn fold_bare_fn_arg(&mut self, argument: syn::BareFnArg) -> syn::BareFnArg {
        let mut folded = syn::fold::fold_bare_fn_arg(self, argument);
        folded.name = None;
        folded
    }
}

fn trim_generics_punctuation(generics: &mut syn::Generics) {
    trim_trailing_punct(&mut generics.params);
    if let Some(where_clause) = &mut generics.where_clause {
        trim_trailing_punct(&mut where_clause.predicates);
    }
}

fn trim_fields_punctuation(fields: &mut Fields) {
    match fields {
        Fields::Named(named) => trim_trailing_punct(&mut named.named),
        Fields::Unnamed(unnamed) => trim_trailing_punct(&mut unnamed.unnamed),
        Fields::Unit => {}
    }
}

fn trim_trailing_punct<T, P>(values: &mut Punctuated<T, P>) {
    if values.trailing_punct() {
        values.pop_punct();
    }
}

struct CanonicalFold;

impl Fold for CanonicalFold {
    fn fold_ident(&mut self, ident: syn::Ident) -> syn::Ident {
        let text = ident.to_string();
        let normalized = normalize_identifier(text.trim_start_matches("r#"));
        if text.starts_with("r#") && is_rust_keyword(&normalized) {
            syn::Ident::new_raw(&normalized, ident.span())
        } else {
            syn::Ident::new(&normalized, ident.span())
        }
    }

    fn fold_lit_str(&mut self, literal: syn::LitStr) -> syn::LitStr {
        syn::LitStr::new(&literal.value(), literal.span())
    }
}

fn normalized_associated_contract(
    item_impl: &syn::ItemImpl,
    item: &syn::ImplItem,
    hide_owner: bool,
) -> String {
    let mut impl_attrs = item_impl.attrs.clone();
    normalize_attrs(&mut impl_attrs, true);
    let defaultness = &item_impl.defaultness;
    let unsafety = &item_impl.unsafety;
    let mut normalizer = SignatureAlphaNormalizer::default();
    normalizer.push_generics(&item_impl.generics);
    let mut generics = normalizer.fold_generics(item_impl.generics.clone());
    trim_generics_punctuation(&mut generics);
    let mut self_ty = normalizer.fold_type((*item_impl.self_ty).clone());
    if hide_owner
        && let syn::Type::Path(type_path) = &mut self_ty
        && let Some(last) = type_path.path.segments.last().cloned()
    {
        let mut replacement = last;
        replacement.ident = syn::Ident::new("__prview_owner", replacement.ident.span());
        type_path.qself = None;
        type_path.path.leading_colon = None;
        type_path.path.segments.clear();
        type_path.path.segments.push(replacement);
    }
    let item_tokens = match item.clone() {
        syn::ImplItem::Fn(mut function) => {
            normalize_attrs(&mut function.attrs, false);
            function.block = syn::parse_quote!({});
            normalizer.normalize_signature(&mut function.sig);
            trim_signature_punctuation(&mut function.sig);
            function.to_token_stream()
        }
        syn::ImplItem::Const(mut value) => {
            normalize_attrs(&mut value.attrs, false);
            value.to_token_stream()
        }
        _ => return String::new(),
    };
    normalizer.pop_scope();
    let tokens =
        quote!(#(#impl_attrs)* #defaultness #unsafety impl #generics #self_ty { #item_tokens });
    canonical_tokens(tokens)
}

fn normalized_trait_impl_contract(item_impl: &syn::ItemImpl) -> String {
    let mut item_impl = item_impl.clone();
    normalize_attrs(&mut item_impl.attrs, true);
    let mut normalizer = SignatureAlphaNormalizer::default();
    normalizer.push_generics(&item_impl.generics);
    item_impl.generics = normalizer.fold_generics(item_impl.generics);
    trim_generics_punctuation(&mut item_impl.generics);
    item_impl.self_ty = Box::new(normalizer.fold_type(*item_impl.self_ty));
    if let Some((not, trait_path, for_token)) = item_impl.trait_.take() {
        item_impl.trait_ = Some((not, normalizer.fold_path(trait_path), for_token));
    }
    for item in &mut item_impl.items {
        match item {
            syn::ImplItem::Fn(function) => {
                normalize_attrs(&mut function.attrs, false);
                function.block = syn::parse_quote!({});
                normalizer.normalize_signature(&mut function.sig);
                trim_signature_punctuation(&mut function.sig);
            }
            syn::ImplItem::Const(value) => {
                normalize_attrs(&mut value.attrs, false);
                value.ty = normalizer.fold_type(value.ty.clone());
            }
            syn::ImplItem::Type(alias) => {
                normalize_attrs(&mut alias.attrs, false);
                alias.generics = normalizer.fold_generics(alias.generics.clone());
                trim_generics_punctuation(&mut alias.generics);
                alias.ty = normalizer.fold_type(alias.ty.clone());
            }
            syn::ImplItem::Macro(value) => normalize_attrs(&mut value.attrs, false),
            _ => {}
        }
    }
    normalizer.pop_scope();
    canonical_tokens(CanonicalFold.fold_item_impl(item_impl).to_token_stream())
}

fn normalized_foreign_contract(foreign: &syn::ItemForeignMod, item: &syn::ForeignItem) -> String {
    let mut attrs = foreign.attrs.clone();
    normalize_attrs(&mut attrs, true);
    let unsafety = &foreign.unsafety;
    let abi = &foreign.abi;
    let mut item = item.clone();
    match &mut item {
        syn::ForeignItem::Fn(value) => {
            normalize_attrs(&mut value.attrs, false);
            value.sig.ident = syn::Ident::new("__prview_name", value.sig.ident.span());
            alpha_normalize_signature(&mut value.sig);
            trim_signature_punctuation(&mut value.sig);
        }
        syn::ForeignItem::Static(value) => {
            normalize_attrs(&mut value.attrs, false);
            value.ident = syn::Ident::new("__prview_name", value.ident.span());
        }
        syn::ForeignItem::Type(value) => normalize_attrs(&mut value.attrs, false),
        syn::ForeignItem::Macro(value) => normalize_attrs(&mut value.attrs, false),
        _ => {}
    }
    canonical_tokens(quote!(#(#attrs)* #unsafety #abi { #item }))
}

fn canonical_cfg(attrs: &[Attribute]) -> CfgOutcome {
    let mut guards = Vec::new();
    let mut errors = Vec::new();
    for attr in attrs {
        let Some(name) = attr
            .path()
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        else {
            continue;
        };
        if name != "cfg" && name != "cfg_attr" {
            continue;
        }
        let Meta::List(list) = &attr.meta else {
            errors.push(canonical_tokens(attr.to_token_stream()));
            continue;
        };
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        match parser.parse2(list.tokens.clone()) {
            Ok(parts) if name == "cfg" && parts.len() == 1 => {
                match canonical_meta_checked(parts.first().expect("one predicate")) {
                    Ok(predicate) => guards.push(predicate),
                    Err(_) => errors.push(canonical_tokens(attr.to_token_stream())),
                }
            }
            Ok(parts) if name == "cfg_attr" && parts.len() >= 2 => {
                let mut iter = parts.iter();
                let predicate = match canonical_meta_checked(iter.next().expect("predicate")) {
                    Ok(predicate) => predicate,
                    Err(_) => {
                        errors.push(canonical_tokens(attr.to_token_stream()));
                        continue;
                    }
                };
                let mut semantic = Vec::new();
                let mut invalid = false;
                for nested in iter {
                    let Some(nested_name) = nested
                        .path()
                        .segments
                        .first()
                        .map(|segment| normalize_identifier(segment.ident.to_string()))
                    else {
                        invalid = true;
                        break;
                    };
                    if is_non_contract_attr(&nested_name) || nested_name == "path" {
                        continue;
                    }
                    match canonical_meta_checked(nested) {
                        Ok(value) => semantic.push(value),
                        Err(_) => {
                            invalid = true;
                            break;
                        }
                    }
                }
                if invalid {
                    errors.push(canonical_tokens(attr.to_token_stream()));
                } else if !semantic.is_empty() {
                    semantic.sort();
                    semantic.dedup();
                    guards.push(format!("cfg_attr({predicate};{})", semantic.join(",")));
                }
            }
            Ok(_) | Err(_) => errors.push(canonical_tokens(attr.to_token_stream())),
        }
    }
    guards.sort();
    guards.dedup();
    CfgOutcome { guards, errors }
}

fn canonical_meta_checked(meta: &Meta) -> Result<String, ()> {
    match meta {
        Meta::Path(path) => Ok(canonical_tokens(path.to_token_stream())),
        Meta::NameValue(value) => {
            if matches!(value.value, syn::Expr::Lit(_)) {
                Ok(canonical_tokens(value.to_token_stream()))
            } else {
                Err(())
            }
        }
        Meta::List(list) => {
            let name = canonical_tokens(list.path.to_token_stream());
            let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
            let parts = parser.parse2(list.tokens.clone()).map_err(|_| ())?;
            if parts.is_empty() && !matches!(name.as_str(), "all" | "any") {
                return Err(());
            }
            let mut operands: Vec<_> = parts
                .iter()
                .map(canonical_meta_checked)
                .collect::<Result<_, _>>()?;
            if name == "all" || name == "any" {
                operands.sort();
                operands.dedup();
            }
            Ok(format!("{name}({})", operands.join(",")))
        }
    }
}

fn canonical_meta(meta: &Meta) -> String {
    match meta {
        Meta::Path(path) => canonical_tokens(path.to_token_stream()),
        Meta::NameValue(value) => canonical_tokens(value.to_token_stream()),
        Meta::List(list) => {
            let name = canonical_tokens(list.path.to_token_stream());
            let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
            let Ok(parts) = parser.parse2(list.tokens.clone()) else {
                return canonical_tokens(list.to_token_stream());
            };
            let mut operands: Vec<_> = parts.iter().map(canonical_meta).collect();
            if name == "all" || name == "any" {
                operands.sort();
                operands.dedup();
            }
            format!("{name}({})", operands.join(","))
        }
    }
}

fn flatten_use_tree(tree: &UseTree, prefix: Vec<String>, output: &mut Vec<UseLeaf>) {
    match tree {
        UseTree::Path(path) => {
            let mut next = prefix;
            next.push(normalize_identifier(path.ident.to_string()));
            flatten_use_tree(&path.tree, next, output);
        }
        UseTree::Name(name) => {
            let mut segments = prefix;
            let ident = normalize_identifier(name.ident.to_string());
            if ident != "self" {
                segments.push(ident.clone());
            }
            let alias = if ident == "self" {
                segments.last().cloned().unwrap_or(ident)
            } else {
                ident
            };
            output.push(UseLeaf {
                segments,
                alias,
                glob: false,
            });
        }
        UseTree::Rename(rename) => {
            let mut segments = prefix;
            let ident = normalize_identifier(rename.ident.to_string());
            if ident != "self" {
                segments.push(ident);
            }
            output.push(UseLeaf {
                segments,
                alias: normalize_identifier(rename.rename.to_string()),
                glob: false,
            });
        }
        UseTree::Glob(_) => output.push(UseLeaf {
            segments: prefix,
            alias: "*".to_owned(),
            glob: true,
        }),
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_use_tree(item, prefix.clone(), output);
            }
        }
    }
}

fn use_candidate_paths(current: &[String], segments: &[String]) -> Vec<Vec<String>> {
    if segments.is_empty() {
        return Vec::new();
    }
    match segments[0].as_str() {
        "crate" => vec![segments[1..].to_vec()],
        "self" => {
            let mut path = current.to_vec();
            path.extend_from_slice(&segments[1..]);
            vec![path]
        }
        "super" => {
            let mut path = current.to_vec();
            let mut index = 0;
            while segments.get(index).is_some_and(|part| part == "super") {
                path.pop();
                index += 1;
            }
            path.extend_from_slice(&segments[index..]);
            vec![path]
        }
        _ => {
            let mut relative = current.to_vec();
            relative.extend_from_slice(segments);
            if current.is_empty() {
                vec![relative]
            } else {
                vec![relative, segments.to_vec()]
            }
        }
    }
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(value) => &value.attrs,
        Item::Enum(value) => &value.attrs,
        Item::ExternCrate(value) => &value.attrs,
        Item::Fn(value) => &value.attrs,
        Item::ForeignMod(value) => &value.attrs,
        Item::Impl(value) => &value.attrs,
        Item::Macro(value) => &value.attrs,
        Item::Mod(value) => &value.attrs,
        Item::Static(value) => &value.attrs,
        Item::Struct(value) => &value.attrs,
        Item::Trait(value) => &value.attrs,
        Item::TraitAlias(value) => &value.attrs,
        Item::Type(value) => &value.attrs,
        Item::Union(value) => &value.attrs,
        Item::Use(value) => &value.attrs,
        _ => &[],
    }
}

fn foreign_item_attrs(item: &syn::ForeignItem) -> &[Attribute] {
    match item {
        syn::ForeignItem::Fn(value) => &value.attrs,
        syn::ForeignItem::Static(value) => &value.attrs,
        syn::ForeignItem::Type(value) => &value.attrs,
        syn::ForeignItem::Macro(value) => &value.attrs,
        _ => &[],
    }
}

fn has_path_attribute(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("path"))
}

fn conditional_path_selection(attrs: &[Attribute]) -> Option<String> {
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("cfg_attr") {
            return None;
        }
        let Meta::List(list) = &attr.meta else {
            return None;
        };
        let Ok(parts) = parser.parse2(list.tokens.clone()) else {
            return None;
        };
        parts
            .iter()
            .skip(1)
            .any(|meta| meta.path().is_ident("path"))
            .then(|| canonical_tokens(attr.to_token_stream()))
    })
}

fn inline_module_child_base(
    attrs: &[Attribute],
    physical_declaring_dir: &str,
    logical_child_base: &str,
    name: &str,
) -> Result<String, String> {
    if let Some(evidence) = conditional_path_selection(attrs) {
        return Err(format!(
            "conditional inline module path cannot be selected: {evidence}"
        ));
    }
    if has_path_attribute(attrs) {
        let path = module_path_attribute(attrs)?;
        safe_join_repo_path(physical_declaring_dir, &path)
    } else {
        safe_join_repo_path(logical_child_base, name)
    }
}

fn module_path_attribute(attrs: &[Attribute]) -> Result<String, String> {
    let matches: Vec<_> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("path"))
        .collect();
    if matches.len() != 1 {
        return Err("#[path] must occur exactly once".to_owned());
    }
    match &matches[0].meta {
        Meta::NameValue(value) => match &value.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(path),
                ..
            }) => Ok(path.value()),
            _ => Err("#[path] value must be one string literal".to_owned()),
        },
        _ => Err("#[path] must use name-value string syntax".to_owned()),
    }
}

fn is_live_regular_entry(entry: &RevisionEntry) -> bool {
    entry.kind == super::revision_source::RevisionEntryKind::RegularFile
        && matches!(
            entry.state,
            super::revision_source::RevisionEntryState::Present
                | super::revision_source::RevisionEntryState::Added
                | super::revision_source::RevisionEntryState::RenamedFrom { .. }
        )
}

fn required_string<'a>(table: &'a toml::Table, key: &str) -> Option<&'a str> {
    table.get(key).and_then(toml::Value::as_str)
}

fn peek_manifest_toml(source: &dyn RevisionFileSource, path: &str) -> Option<toml::Value> {
    let RevisionRead::Bytes(bytes) = source.read(path).ok()? else {
        return None;
    };
    if bytes.content_kind != RevisionContentKind::Utf8Text {
        return None;
    }
    let text = String::from_utf8(bytes.bytes).ok()?;
    toml::from_str(&text).ok()
}

fn toml_string_array(table: &toml::Table, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn parent_manifest_dir(manifest_path: &str) -> String {
    Path::new(manifest_path)
        .parent()
        .and_then(|parent| parent.to_str())
        .unwrap_or("")
        .replace('\\', "/")
}

fn cargo_glob_matches(pattern: &str, path: &str) -> bool {
    fn parts(value: &str) -> Vec<&str> {
        if value.is_empty() || value == "." {
            Vec::new()
        } else {
            value
                .split('/')
                .filter(|part| !part.is_empty() && *part != ".")
                .collect()
        }
    }
    fn component_match(pattern: &str, value: &str) -> bool {
        let mut p = pattern.chars().peekable();
        let mut v = value.chars().peekable();
        while let Some(pc) = p.next() {
            match pc {
                '*' => {
                    if p.peek().is_none() {
                        return true;
                    }
                    let rest: String = p.collect();
                    let remaining: String = v.collect();
                    for i in 0..=remaining.len() {
                        if remaining.is_char_boundary(i) && component_match(&rest, &remaining[i..])
                        {
                            return true;
                        }
                    }
                    return false;
                }
                '?' => {
                    if v.next().is_none() {
                        return false;
                    }
                }
                other => {
                    if v.next() != Some(other) {
                        return false;
                    }
                }
            }
        }
        v.next().is_none()
    }
    fn match_parts(pattern: &[&str], path: &[&str]) -> bool {
        match (pattern.split_first(), path.split_first()) {
            (None, None) => true,
            (Some((&"**", rest)), _) => {
                rest.is_empty()
                    || match_parts(rest, path)
                    || (!path.is_empty() && match_parts(pattern, &path[1..]))
            }
            (Some((pat, rest)), Some((seg, prest))) => {
                component_match(pat, seg) && match_parts(rest, prest)
            }
            _ => false,
        }
    }
    match_parts(&parts(pattern), &parts(path))
}

/// Product crates are workspace members (minus `exclude`), or the root package
/// of a non-workspace repo. Nested fixture/tool packages are not API surface.
fn api_crate_manifests(source: &dyn RevisionFileSource, manifests: &[String]) -> BTreeSet<String> {
    let parsed: Vec<(&str, toml::Value)> = manifests
        .iter()
        .filter_map(|path| peek_manifest_toml(source, path).map(|value| (path.as_str(), value)))
        .collect();
    let mut workspaces = Vec::new();
    let mut packages = Vec::new();
    for (path, manifest) in &parsed {
        let dir = parent_manifest_dir(path);
        if let Some(workspace) = manifest.get("workspace").and_then(toml::Value::as_table) {
            let members = toml_string_array(workspace, "members");
            let exclude = toml_string_array(workspace, "exclude");
            workspaces.push((
                dir.clone(),
                members,
                exclude,
                manifest.get("package").is_some(),
            ));
        }
        if manifest.get("package").is_some() {
            packages.push(*path);
        }
    }
    if manifests.iter().any(|path| path == "Cargo.toml")
        && !parsed.iter().any(|(path, _)| *path == "Cargo.toml")
    {
        // A malformed or non-UTF-8 root manifest is still the repository's
        // authority. Keep it selected so the snapshot emits its typed unknown
        // instead of silently falling through to a nested fixture package.
        return BTreeSet::from(["Cargo.toml".to_owned()]);
    }
    if let Some((_, root)) = parsed.iter().find(|(path, _)| *path == "Cargo.toml") {
        let Some(workspace) = root.get("workspace").and_then(toml::Value::as_table) else {
            return if root.get("package").is_some() {
                BTreeSet::from(["Cargo.toml".to_owned()])
            } else {
                BTreeSet::new()
            };
        };

        let members = toml_string_array(workspace, "members");
        let exclude = toml_string_array(workspace, "exclude");
        let mut allowed = BTreeSet::new();
        if root.get("package").is_some() {
            // A package declared by the workspace root is always a workspace
            // member; nested workspaces must not displace it as API authority.
            allowed.insert("Cargo.toml".to_owned());
        }
        for package in &packages {
            if *package == "Cargo.toml" {
                continue;
            }
            let package_dir = parent_manifest_dir(package);
            let included = members
                .iter()
                .any(|pattern| cargo_glob_matches(pattern, &package_dir));
            let excluded = exclude
                .iter()
                .any(|pattern| cargo_glob_matches(pattern, &package_dir));
            if included && !excluded {
                allowed.insert((*package).to_owned());
            }
        }
        return allowed;
    }

    // Compatibility fallback for revision sources rooted below the repository
    // root (for example a caller-provided single-crate source). A real repo with
    // Cargo.toml above never reaches this branch.
    if workspaces.is_empty() {
        return packages.into_iter().map(str::to_owned).collect();
    }
    let mut allowed = BTreeSet::new();
    for (dir, members, exclude, has_package) in &workspaces {
        let member_patterns = if members.is_empty() {
            if *has_package {
                vec![".".to_owned()]
            } else {
                Vec::new()
            }
        } else {
            members.clone()
        };
        for package in &packages {
            let package_dir = parent_manifest_dir(package);
            let relative = if dir.is_empty() {
                package_dir
            } else if package_dir == *dir {
                String::new()
            } else if let Some(rest) = package_dir.strip_prefix(&format!("{dir}/")) {
                rest.to_owned()
            } else {
                continue;
            };
            let included = member_patterns
                .iter()
                .any(|pattern| cargo_glob_matches(pattern, &relative));
            let excluded = exclude
                .iter()
                .any(|pattern| cargo_glob_matches(pattern, &relative));
            if included && !excluded {
                allowed.insert((*package).to_owned());
            }
        }
    }
    allowed
}

fn trait_is_external_public(pending: &PendingTraitImpl) -> bool {
    let first = pending.trait_module_path.first().map(String::as_str);
    match first {
        Some("std" | "core" | "alloc") => true,
        Some("crate" | "self" | "super") => false,
        Some(segment) if segment == pending.crate_name => false,
        Some(_) => true,
        // An unqualified trait may have entered scope through `use`, including
        // an external crate import. Source-only analysis cannot resolve that
        // binding safely, so retain the impl as typed uncertainty instead of
        // maintaining a necessarily incomplete allowlist of trait names.
        None => true,
    }
}

fn cargo_feature_contracts(manifest: &toml::Value) -> Result<Vec<(String, String)>, String> {
    let mut contracts = BTreeMap::new();
    let mut suppressed = BTreeSet::new();
    if let Some(features) = manifest.get("features") {
        let Some(features) = features.as_table() else {
            return Err("features must be a table".to_owned());
        };
        for (name, value) in features {
            let Some(members) = value.as_array() else {
                return Err(format!("features.{name} must be an array of strings"));
            };
            let mut members: Vec<_> = members
                .iter()
                .map(|member| {
                    member
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("features.{name} must contain only strings"))
                })
                .collect::<Result<_, _>>()?;
            for member in &members {
                if let Some(rest) = member.strip_prefix("dep:") {
                    let dep_name = rest.split('/').next().unwrap_or(rest);
                    suppressed.insert(dep_name.to_owned());
                }
            }
            members.sort();
            members.dedup();
            contracts.insert(name.clone(), format!("cargo feature {name} = {members:?}"));
        }
    }
    for name in optional_dependency_feature_names(manifest)? {
        if contracts.contains_key(&name) || suppressed.contains(&name) {
            continue;
        }
        contracts.insert(
            name.clone(),
            format!("cargo feature {name} = {:?}", [format!("dep:{name}")]),
        );
    }
    Ok(contracts.into_iter().collect())
}

fn optional_dependency_feature_names(manifest: &toml::Value) -> Result<Vec<String>, String> {
    let mut names = BTreeSet::new();
    collect_optional_dependency_names(manifest.get("dependencies"), &mut names)?;
    collect_optional_dependency_names(manifest.get("build-dependencies"), &mut names)?;
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for spec in targets.values() {
            let Some(table) = spec.as_table() else {
                continue;
            };
            collect_optional_dependency_names(table.get("dependencies"), &mut names)?;
            collect_optional_dependency_names(table.get("build-dependencies"), &mut names)?;
        }
    }
    Ok(names.into_iter().collect())
}

fn collect_optional_dependency_names(
    deps: Option<&toml::Value>,
    names: &mut BTreeSet<String>,
) -> Result<(), String> {
    let Some(deps) = deps else {
        return Ok(());
    };
    let Some(deps) = deps.as_table() else {
        return Err("dependency table must be a table".to_owned());
    };
    for (name, spec) in deps {
        if let toml::Value::Table(table) = spec
            && table.get("optional").and_then(toml::Value::as_bool) == Some(true)
        {
            names.insert(name.clone());
        }
    }
    Ok(())
}

fn lib_crate_types(lib: Option<&toml::Table>) -> Result<Vec<String>, String> {
    let Some(value) = lib.and_then(|table| table.get("crate-type")) else {
        return Ok(vec!["lib".to_owned()]);
    };
    let mut types = match value {
        toml::Value::String(kind) => vec![kind.clone()],
        toml::Value::Array(kinds) => kinds
            .iter()
            .map(|kind| {
                kind.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "lib.crate-type must contain only strings".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("lib.crate-type must be a string or array of strings".to_owned()),
    };
    types.sort();
    types.dedup();
    if types.is_empty() {
        types.push("lib".to_owned());
    }
    Ok(types)
}

fn include_literal_path(macro_call: &syn::Macro) -> Option<String> {
    syn::parse2::<syn::LitStr>(macro_call.tokens.clone())
        .ok()
        .map(|lit| lit.value())
}

#[derive(Default)]
struct ExpressionIncludeCollector {
    macros: Vec<syn::Macro>,
}

impl Fold for ExpressionIncludeCollector {
    fn fold_expr_macro(&mut self, expression: syn::ExprMacro) -> syn::ExprMacro {
        let name = expression
            .mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string());
        if name
            .as_deref()
            .is_some_and(|name| matches!(name, "include" | "include_str" | "include_bytes"))
        {
            self.macros.push(expression.mac.clone());
        }
        syn::fold::fold_expr_macro(self, expression)
    }
}

fn expression_include_macros(item: &Item) -> Vec<syn::Macro> {
    let expression = match item {
        Item::Const(value) if is_public(&value.vis) => value.expr.as_ref(),
        Item::Static(value) if is_public(&value.vis) => value.expr.as_ref(),
        _ => return Vec::new(),
    };
    let mut collector = ExpressionIncludeCollector::default();
    collector.fold_expr(expression.clone());
    collector.macros
}

fn validate_package_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return Err("package.name must not be empty".to_owned());
    };
    if !(first.is_alphabetic() || first == '_')
        || !characters
            .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!(
            "package.name contains unsupported characters: {name:?}"
        ));
    }
    let crate_name = name.replace('-', "_");
    syn::parse_str::<syn::Ident>(&crate_name).map_err(|_| {
        format!("package.name does not normalize to a Rust crate identifier: {name:?}")
    })?;
    Ok(())
}

fn validate_lib_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("lib.name must not be empty".to_owned());
    }
    syn::parse_str::<syn::Ident>(name)
        .map_err(|_| format!("lib.name must be a Rust crate identifier: {name:?}"))?;
    Ok(())
}

fn optional_string<'a>(
    table: Option<&'a toml::Table>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    let Some(value) = table.and_then(|table| table.get(key)) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(Some)
        .ok_or_else(|| format!("lib.{key} must be a string"))
}

fn optional_bool(table: Option<&toml::Table>, key: &str) -> Result<Option<bool>, String> {
    let Some(value) = table.and_then(|table| table.get(key)) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("lib.{key} must be a boolean"))
}

fn resolve_impl_owner(current: &[String], path: &syn::Path) -> Option<(Vec<String>, String)> {
    let segments: Vec<_> = path
        .segments
        .iter()
        .map(|segment| normalize_identifier(segment.ident.to_string()))
        .collect();
    let (name, prefix) = segments.split_last()?;
    let mut module_path = current.to_vec();
    match prefix.first().map(String::as_str) {
        Some("crate") => module_path = prefix[1..].to_vec(),
        Some("self") => module_path.extend_from_slice(&prefix[1..]),
        Some("super") => {
            let mut index = 0;
            while prefix.get(index).is_some_and(|part| part == "super") {
                module_path.pop();
                index += 1;
            }
            module_path.extend_from_slice(&prefix[index..]);
        }
        Some(_) => module_path.extend_from_slice(prefix),
        None => {}
    }
    Some((module_path, name.clone()))
}

fn raw_symbol_semantic_eq(left: &RawSymbol, right: &RawSymbol) -> bool {
    left.key == right.key
        && left.kind == right.kind
        && left.contract == right.contract
        && left.cfg_guard == right.cfg_guard
}

/// Bounds closure by the finite resolver graph rather than a shared constant.
///
/// Each non-glob use leaf owns at most one module-alias identity and one alias
/// identity in each of Rust's three namespaces. A settling pass either adds a
/// monotonic ambiguity tombstone or a previously absent derived relation. The
/// squared leaf term covers every leaf-to-leaf derived relation; the factor of
/// four covers the module identity plus the three namespace identities. If a
/// future resolver transition violates this measure, exhaustion is handled
/// fail-closed by `resolve_reexports` instead of returning partial positives.
fn reexport_iteration_budget(uses: &[UseEdge]) -> usize {
    let leaf_count = uses
        .iter()
        .flat_map(|edge| &edge.leaves)
        .filter(|leaf| !leaf.glob)
        .count();
    let identity_kinds = 1usize
        + [
            RustNamespace::Type,
            RustNamespace::Value,
            RustNamespace::Macro,
        ]
        .len();
    leaf_count
        .saturating_add(1)
        .saturating_mul(leaf_count.saturating_add(1))
        .saturating_mul(identity_kinds)
}

fn combined_guards(left: &[String], right: &[String]) -> Vec<String> {
    let mut guards = left.to_vec();
    guards.extend_from_slice(right);
    guards.sort();
    guards.dedup();
    guards
}

fn module_alias_conflict_pairs(
    origins: &BTreeSet<ModuleAliasOrigin>,
) -> Vec<(ModuleAliasOrigin, ModuleAliasOrigin)> {
    let origins: Vec<_> = origins.iter().cloned().collect();
    origins
        .iter()
        .enumerate()
        .flat_map(|(index, left)| {
            origins
                .iter()
                .skip(index + 1)
                .filter(move |right| {
                    left.target_module_path != right.target_module_path
                        && !guards_proven_disjoint(&left.cfg_guard, &right.cfg_guard)
                })
                .map(move |right| (left.clone(), right.clone()))
        })
        .collect()
}

fn symbol_alias_conflict_pairs(
    origins: &BTreeSet<SymbolAliasOrigin>,
) -> Vec<(SymbolAliasOrigin, SymbolAliasOrigin)> {
    let origins: Vec<_> = origins.iter().cloned().collect();
    origins
        .iter()
        .enumerate()
        .flat_map(|(index, left)| {
            origins
                .iter()
                .skip(index + 1)
                .filter(move |right| {
                    let same_target = left.target == right.target
                        && left.kind == right.kind
                        && left.contract == right.contract;
                    !same_target && !guards_proven_disjoint(&left.cfg_guard, &right.cfg_guard)
                })
                .map(move |right| (left.clone(), right.clone()))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProvenTargetFamily {
    Unix,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModulePathVisibility {
    Public,
    OverlappingPrivate,
    Unproven,
}

fn guard_lineage_contains(effective_guard: &[String], proof_guard: &[String]) -> bool {
    proof_guard
        .iter()
        .all(|predicate| effective_guard.contains(predicate))
}

fn has_ambiguous_module_prefix(
    ambiguous: &BTreeSet<(String, Vec<String>)>,
    crate_name: &str,
    module_path: &[String],
) -> bool {
    ambiguous.iter().any(|(candidate_crate, prefix)| {
        candidate_crate == crate_name && module_path.starts_with(prefix)
    })
}

pub(super) fn guards_proven_disjoint(left: &[String], right: &[String]) -> bool {
    matches!(
        (proven_target_family(left), proven_target_family(right)),
        (
            Some(ProvenTargetFamily::Unix),
            Some(ProvenTargetFamily::Windows)
        ) | (
            Some(ProvenTargetFamily::Windows),
            Some(ProvenTargetFamily::Unix)
        )
    )
}

fn proven_target_family(guards: &[String]) -> Option<ProvenTargetFamily> {
    guards.iter().find_map(|guard| match guard.as_str() {
        "unix" | "target_family = \"unix\"" => Some(ProvenTargetFamily::Unix),
        "windows" | "target_family = \"windows\"" => Some(ProvenTargetFamily::Windows),
        _ => None,
    })
}

fn item_is_public(item: &Item) -> bool {
    match item {
        Item::Const(value) => is_public(&value.vis),
        Item::Enum(value) => is_public(&value.vis),
        Item::Fn(value) => is_public(&value.vis),
        Item::Mod(value) => is_public(&value.vis),
        Item::Static(value) => is_public(&value.vis),
        Item::Struct(value) => is_public(&value.vis),
        Item::Trait(value) => is_public(&value.vis),
        Item::Type(value) => is_public(&value.vis),
        Item::Union(value) => is_public(&value.vis),
        Item::Use(value) => is_public(&value.vis),
        _ => false,
    }
}

fn node_requires_transform_proof(item: &Item) -> bool {
    item_is_public(item)
        || matches!(
            item,
            Item::Mod(_) | Item::Impl(_) | Item::ForeignMod(_) | Item::Macro(_)
        )
}

fn is_non_contract_attr(name: &str) -> bool {
    matches!(
        name,
        "doc" | "rustfmt" | "allow" | "warn" | "deny" | "forbid" | "expect"
    )
}

fn transforming_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|attr| {
        let name = attr
            .path()
            .segments
            .first()
            .map(|segment| normalize_identifier(segment.ident.to_string()));
        !matches!(
            name.as_deref(),
            Some(
                "cfg"
                    | "cfg_attr"
                    | "doc"
                    | "rustfmt"
                    | "allow"
                    | "warn"
                    | "deny"
                    | "forbid"
                    | "expect"
                    | "must_use"
                    | "deprecated"
                    | "repr"
                    | "non_exhaustive"
                    | "macro_export"
                    | "proc_macro"
                    | "proc_macro_attribute"
                    | "proc_macro_derive"
                    | "no_mangle"
                    | "export_name"
                    | "link_name"
                    | "link_section"
                    | "path"
                    | "cold"
                    | "inline"
                    | "track_caller"
            )
        )
    })
}

fn bind_transform_evidence<T: ToTokens>(evidence: String, input: &T) -> String {
    format!(
        "transform:{evidence}\ninput:{}",
        canonical_tokens(input.to_token_stream())
    )
}

fn transforming_cfg_attrs(attrs: &[Attribute]) -> Vec<String> {
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg_attr"))
        .filter_map(|attr| {
            let Meta::List(list) = &attr.meta else {
                return Some(canonical_tokens(attr.to_token_stream()));
            };
            let Ok(parts) = parser.parse2(list.tokens.clone()) else {
                return Some(canonical_tokens(attr.to_token_stream()));
            };
            parts
                .iter()
                .skip(1)
                .any(|meta| {
                    meta.path()
                        .segments
                        .first()
                        .map(|segment| normalize_identifier(segment.ident.to_string()))
                        .is_none_or(|name| !is_known_contract_attr(&name))
                })
                .then(|| canonical_tokens(attr.to_token_stream()))
        })
        .collect()
}

fn is_known_contract_attr(name: &str) -> bool {
    matches!(
        name,
        "cfg"
            | "cfg_attr"
            | "doc"
            | "rustfmt"
            | "allow"
            | "warn"
            | "deny"
            | "forbid"
            | "expect"
            | "must_use"
            | "deprecated"
            | "repr"
            | "non_exhaustive"
            | "macro_export"
            | "proc_macro"
            | "proc_macro_attribute"
            | "proc_macro_derive"
            | "no_mangle"
            | "export_name"
            | "link_name"
            | "link_section"
            | "path"
            | "cold"
            | "inline"
            | "track_caller"
    )
}

fn proc_macro_external_name(function: &syn::ItemFn) -> Option<String> {
    for attr in &function.attrs {
        if attr.path().is_ident("proc_macro") || attr.path().is_ident("proc_macro_attribute") {
            return Some(normalize_identifier(function.sig.ident.to_string()));
        }
        if attr.path().is_ident("proc_macro_derive") {
            let Meta::List(list) = &attr.meta else {
                return None;
            };
            let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
            let Ok(parts) = parser.parse2(list.tokens.clone()) else {
                return None;
            };
            let Meta::Path(path) = parts.first()? else {
                return None;
            };
            return path
                .get_ident()
                .map(|ident| normalize_identifier(ident.to_string()));
        }
    }
    None
}

fn is_rust_keyword(value: &str) -> bool {
    matches!(
        value,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "try"
            | "gen"
    )
}
fn is_public(visibility: &Visibility) -> bool {
    matches!(visibility, Visibility::Public(_))
}
fn normalize_identifier(value: impl AsRef<str>) -> String {
    value.as_ref().trim_start_matches("r#").nfc().collect()
}
fn canonical_tokens(tokens: impl ToTokens) -> String {
    tokens.to_token_stream().to_string()
}

fn parent_repo_path(path: &str) -> String {
    Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("")
        .replace('\\', "/")
}

fn safe_join_repo_path(base: &str, child: &str) -> Result<String, String> {
    let child_path = Path::new(child);
    let bytes = child.as_bytes();
    let windows_prefix =
        bytes.get(1) == Some(&b':') && bytes.first().is_some_and(u8::is_ascii_alphabetic);
    if child_path.is_absolute() || child.starts_with('\\') || windows_prefix {
        return Err(format!("absolute path is outside the repository: {child}"));
    }
    let path = if base.is_empty() {
        PathBuf::from(child)
    } else {
        Path::new(base).join(child)
    };
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.pop().is_none() {
                    return Err(format!("path escapes repository root: {child}"));
                }
            }
            std::path::Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(format!("path is not UTF-8: {child}"));
                };
                normalized.push(part.to_owned());
            }
            _ => return Err(format!("path uses an unsupported prefix: {child}")),
        }
    }
    Ok(normalized.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::revision_source::{
        GitTree, RevisionBytes, RevisionContentKind, RevisionEntryKind, RevisionEntryState,
        RevisionSourceError,
    };
    use crate::git::{Repository, git_cmd};
    use std::fs;

    #[derive(Clone)]
    struct MemorySource {
        provenance: RevisionProvenance,
        files: BTreeMap<String, Vec<u8>>,
        states: BTreeMap<String, RevisionEntryState>,
    }

    impl MemorySource {
        fn new(files: &[(&str, &[u8])]) -> Self {
            Self {
                provenance: RevisionProvenance::GitTree {
                    commit_oid: "1111111111111111111111111111111111111111".to_owned(),
                },
                files: files
                    .iter()
                    .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec()))
                    .collect(),
                states: BTreeMap::new(),
            }
        }
    }

    impl RevisionFileSource for MemorySource {
        fn provenance(&self) -> &RevisionProvenance {
            &self.provenance
        }
        fn entries(&self) -> Vec<RevisionEntry> {
            let mut paths: BTreeSet<_> = self.files.keys().cloned().collect();
            paths.extend(self.states.keys().cloned());
            paths
                .into_iter()
                .map(|path| RevisionEntry {
                    path: path.clone(),
                    baseline_object_id: Some("fixture-object".to_owned()),
                    mode: 0o100644,
                    kind: RevisionEntryKind::RegularFile,
                    state: self
                        .states
                        .get(&path)
                        .cloned()
                        .unwrap_or(RevisionEntryState::Present),
                    provenance: self.provenance.clone(),
                })
                .collect()
        }
        fn read(&self, path: &str) -> Result<RevisionRead, RevisionSourceError> {
            if let Some(state) = self.states.get(path) {
                let unavailable = match state {
                    RevisionEntryState::Deleted => Some(RevisionRead::Deleted {
                        provenance: self.provenance.clone(),
                    }),
                    RevisionEntryState::Unreadable { reason } => Some(RevisionRead::Unreadable {
                        reason: reason.clone(),
                        provenance: self.provenance.clone(),
                    }),
                    RevisionEntryState::Renamed { to } => Some(RevisionRead::Renamed {
                        to: to.clone(),
                        provenance: self.provenance.clone(),
                    }),
                    RevisionEntryState::NonRegular { kind } => Some(RevisionRead::NonRegular {
                        kind: *kind,
                        provenance: self.provenance.clone(),
                    }),
                    RevisionEntryState::Present
                    | RevisionEntryState::Added
                    | RevisionEntryState::RenamedFrom { .. } => None,
                };
                if let Some(unavailable) = unavailable {
                    return Ok(unavailable);
                }
            }
            Ok(match self.files.get(path) {
                Some(bytes) => RevisionRead::Bytes(RevisionBytes {
                    bytes: bytes.clone(),
                    content_kind: if std::str::from_utf8(bytes).is_ok() {
                        RevisionContentKind::Utf8Text
                    } else {
                        RevisionContentKind::BinaryOrNonUtf8
                    },
                    provenance: self.provenance.clone(),
                }),
                None => RevisionRead::Missing {
                    provenance: self.provenance.clone(),
                },
            })
        }
    }

    fn source(lib: &str) -> MemorySource {
        MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", lib.as_bytes()),
        ])
    }

    fn names(snapshot: &RustApiSnapshot) -> Vec<String> {
        snapshot
            .items
            .iter()
            .map(|item| {
                let prefix = item.key.module_path.join("::");
                if prefix.is_empty() {
                    item.key.external_name.clone()
                } else {
                    format!("{prefix}::{}", item.key.external_name)
                }
            })
            .collect()
    }

    #[test]
    fn rust_api_snapshot_private_module_reachability() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub struct Item; pub fn hidden() {} }\npub use private::Item as PublicItem;\n",
        ));
        let names = names(&snapshot);
        assert!(!names.iter().any(|name| name == "private::hidden"));
        assert!(names.iter().any(|name| name == "PublicItem"));
        assert_eq!(snapshot.reexports.len(), 2);
    }

    #[test]
    fn rust_api_snapshot_nested_alias_reexports_reach_a_fixpoint() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub struct Item; pub use self::Item as Mid; }\n\
             pub use private::{Mid as Public, Item as Direct};\n\
             #[macro_export] macro_rules! Public { () => {} }\n",
        ));
        let names = names(&snapshot);
        assert!(names.iter().any(|name| name == "Public"));
        assert!(names.iter().any(|name| name == "Direct"));
        assert!(snapshot.items.iter().any(|item| {
            item.key.external_name == "Public" && item.key.namespace == RustNamespace::Macro
        }));
        assert!(snapshot.items.iter().any(|item| {
            item.key.external_name == "Public" && item.key.namespace == RustNamespace::Type
        }));
    }

    #[test]
    fn rust_api_snapshot_identity_normalizes_bodies_layout_and_unicode() {
        let left = snapshot_rust_api(&source(
            "pub fn café(x: u8) -> u8 { x }\npub const TEXT: &str = \"left\\\n    right\";",
        ));
        let right = snapshot_rust_api(&source(
            "pub fn cafe\u{301}(x: u8) -> u8\n{ 99 }\npub const TEXT: &str = \"left\\\n right\";",
        ));
        assert_eq!(left.items, right.items);
        let changed = snapshot_rust_api(&source("pub fn café(x: u16) -> u8 { 0 }"));
        assert_ne!(left.items[0].contract, changed.items[0].contract);
    }

    #[test]
    fn rust_api_snapshot_alpha_normalizes_parameter_generic_and_lifetime_binders() {
        let before = snapshot_rust_api(&source(
            "pub fn parse<'input, T: Clone, const N: usize>(input: &'input [T; N]) -> &'input T { &input[0] }",
        ));
        let renamed = snapshot_rust_api(&source(
            "pub fn parse<'source, U: Clone, const M: usize>(source: &'source [U; M]) -> &'source U { &source[0] }",
        ));
        assert_eq!(
            before.items, renamed.items,
            "binder spelling and parameter patterns are not caller-observable"
        );

        let nested_before = snapshot_rust_api(&source(
            "pub struct Parser; \
             pub trait Service { fn call<'a, T>(&'a self, input: T) -> (&'a Self, T); } \
             impl Parser { pub fn parse<'a, T>(&'a self, input: T) -> (&'a Self, T) { (self, input) } } \
             unsafe extern \"C\" { pub fn foreign<'a>(input: &'a u8) -> &'a u8; } \
             pub fn map<T>(callback: for<'a> fn(&'a T) -> &'a T) {}",
        ));
        let nested_renamed = snapshot_rust_api(&source(
            "pub struct Parser; \
             pub trait Service { fn call<'value, U>(&'value self, value: U) -> (&'value Self, U); } \
             impl Parser { pub fn parse<'value, U>(&'value self, value: U) -> (&'value Self, U) { (self, value) } } \
             unsafe extern \"C\" { pub fn foreign<'value>(value: &'value u8) -> &'value u8; } \
             pub fn map<U>(function: for<'value> fn(&'value U) -> &'value U) {}",
        ));
        assert_eq!(
            nested_before.items, nested_renamed.items,
            "trait, inherent, foreign, and higher-ranked binders share the alpha contract"
        );

        let associated_before = snapshot_rust_api(&source(
            "pub trait Marker { type Item; } \
             pub fn project<Item: Marker>() -> <Item as Marker>::Item { todo!() }",
        ));
        let associated_renamed = snapshot_rust_api(&source(
            "pub trait Marker { type Item; } \
             pub fn project<U: Marker>() -> <U as Marker>::Item { todo!() }",
        ));
        assert_eq!(
            associated_before.items, associated_renamed.items,
            "an alpha rename must not rename a same-spelled associated item"
        );

        let trait_members_before = snapshot_rust_api(&source(
            "pub trait Convert<T, const N: usize> { \
             const FALLBACK: Option<[T; N]>; \
             type Out<U: Into<T>>: AsRef<[T; N]>; }",
        ));
        let trait_members_renamed = snapshot_rust_api(&source(
            "pub trait Convert<V, const M: usize> { \
             const FALLBACK: Option<[V; M]>; \
             type Out<W: Into<V>>: AsRef<[V; M]>; }",
        ));
        assert_eq!(
            trait_members_before.items, trait_members_renamed.items,
            "outer and member-local trait binders are spelling-only"
        );
        let trait_members_retargeted = snapshot_rust_api(&source(
            "pub trait Convert<V, const M: usize> { \
             const FALLBACK: Option<[V; M]>; \
             type Out<W: Into<V>>: AsRef<[W; M]>; }",
        ));
        assert_ne!(
            trait_members_before.items, trait_members_retargeted.items,
            "changing which binder reaches an associated member remains observable"
        );
    }

    #[test]
    fn rust_api_snapshot_repr_rust_ignores_private_reorder_but_keeps_private_types() {
        let before = snapshot_rust_api(&source(
            "#[repr(Rust)] pub struct Api { first: u8, second: u16 }",
        ));
        let reordered = snapshot_rust_api(&source(
            "#[repr(Rust)] pub struct Api { second: u16, first: u8 }",
        ));
        assert_eq!(
            before.items, reordered.items,
            "repr(Rust) does not expose private field order as an ABI contract"
        );

        let changed_type = snapshot_rust_api(&source(
            "#[repr(Rust)] pub struct Api { first: u8, second: String }",
        ));
        assert_ne!(
            before.items, changed_type.items,
            "private field types remain observable through auto traits"
        );
    }

    #[test]
    fn rust_api_snapshot_keeps_type_abi_and_lifetime_relations_observable() {
        let typed = snapshot_rust_api(&source("pub fn parse(value: u8) {}"));
        let type_changed = snapshot_rust_api(&source("pub fn parse(value: u16) {}"));
        assert_ne!(typed.items, type_changed.items);

        let c_abi = snapshot_rust_api(&source("pub extern \"C\" fn parse(value: u8) {}"));
        let system_abi = snapshot_rust_api(&source("pub extern \"system\" fn parse(value: u8) {}"));
        assert_ne!(c_abi.items, system_abi.items);

        let returns_first = snapshot_rust_api(&source(
            "pub fn pick<'left, 'right>(left: &'left str, right: &'right str) -> &'left str { left }",
        ));
        let returns_second = snapshot_rust_api(&source(
            "pub fn pick<'a, 'b>(first: &'a str, second: &'b str) -> &'b str { second }",
        ));
        assert_ne!(
            returns_first.items, returns_second.items,
            "alpha normalization must preserve which lifetime reaches the output"
        );
    }

    #[test]
    fn rust_api_snapshot_identity_captures_public_item_bodies_and_inherent_api() {
        let before = snapshot_rust_api(&source(
            "pub struct Public { pub field: u8, private: u8 }\npub enum Choice { A = 1, B(u8) }\npub trait Service: Send { fn call(&self) -> u8 { 1 } }\nimpl Public { pub const VALUE: u8 = 1; pub fn make(x: u8) -> Self { Self { field: x, private: 0 } } fn hidden() {} }",
        ));
        let after = snapshot_rust_api(&source(
            "pub struct Public { pub field: u16, private: String }\npub enum Choice { A = 2, B(u16) }\npub trait Service: Sync { fn call(&self) -> u16 { 2 } }\nimpl Public { pub const VALUE: u8 = 2; pub fn make(x: u16) -> Self { todo!() } fn hidden() {} }",
        ));
        assert_ne!(before.items, after.items);
        assert!(names(&before).iter().any(|name| name == "Public::make"));
        assert!(!names(&before).iter().any(|name| name.ends_with("hidden")));
        let public_before = before
            .items
            .iter()
            .find(|item| item.key.external_name == "Public")
            .unwrap();
        let private_only = snapshot_rust_api(&source(
            "pub struct Public { pub field: u8, private: String }",
        ));
        let public_private_only = private_only
            .items
            .iter()
            .find(|item| item.key.external_name == "Public")
            .unwrap();
        assert_ne!(
            public_before.contract, public_private_only.contract,
            "private field types stay in the parent contract so auto-trait effects are visible"
        );
    }

    #[test]
    fn rust_api_snapshot_cfg_canonicalizes_commutative_operands() {
        let left = snapshot_rust_api(&source(
            "#[cfg(all(unix, any(feature = \"b\", feature = \"a\")))] pub fn gated() {}",
        ));
        let right = snapshot_rust_api(&source(
            "#[cfg(all(any(feature = \"a\", feature = \"b\"), unix))] pub fn gated() {}",
        ));
        assert_eq!(left.items, right.items);
        let different = snapshot_rust_api(&source("#[cfg(windows)] pub fn gated() {}"));
        assert_ne!(left.items[0].cfg_guard, different.items[0].cfg_guard);
    }

    #[test]
    fn rust_api_snapshot_unknowns_are_typed() {
        let fixture = MemorySource::new(&[
            ("Cargo.toml", b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n"),
            ("src/lib.rs", b"mod missing; mod ambiguous; pub mod ordinary; pub use missing::*; pub use self::B as A; pub use self::A as B; include!(\"generated.rs\");"),
            ("src/ambiguous.rs", b"pub fn one() {}"), ("src/ambiguous/mod.rs", b"pub fn two() {}"),
            ("src/ordinary.rs", b"pub fn reached() {}"),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        let kinds: BTreeSet<_> = snapshot
            .unknowns
            .iter()
            .map(|unknown| unknown.kind)
            .collect();
        assert!(kinds.contains(&RustApiUnknownKind::MissingModule));
        assert!(kinds.contains(&RustApiUnknownKind::AmbiguousModule));
        assert!(kinds.contains(&RustApiUnknownKind::GlobReexport));
        assert!(kinds.contains(&RustApiUnknownKind::IncludeMacro));
        assert!(kinds.contains(&RustApiUnknownKind::ReexportCycle));
        assert!(
            names(&snapshot)
                .iter()
                .any(|name| name == "ordinary::reached")
        );
        let non_utf8 = MemorySource::new(&[("Cargo.toml", &[0xff, 0xfe])]);
        assert!(
            snapshot_rust_api(&non_utf8)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::ManifestNonUtf8)
        );

        let invalid_manifest = MemorySource::new(&[("Cargo.toml", b"[package\n")]);
        assert!(
            snapshot_rust_api(&invalid_manifest)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::ManifestParse)
        );

        let missing_root = MemorySource::new(&[(
            "Cargo.toml",
            b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='missing.rs'\n",
        )]);
        assert!(
            snapshot_rust_api(&missing_root)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::MissingLibRoot)
        );

        let missing_default_root = MemorySource::new(&[(
            "Cargo.toml",
            b"[package]\nname='fixture'\nversion='0.0.0'\n",
        )]);
        assert!(
            snapshot_rust_api(&missing_default_root)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::MissingLibRoot)
        );

        let parse_failed = snapshot_rust_api(&source("pub fn broken("));
        assert!(
            parse_failed
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::SourceParse)
        );

        let binary_source = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", &[0xff, 0xfe]),
        ]);
        assert!(
            snapshot_rust_api(&binary_source)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::SourceNonUtf8)
        );

        for state in [
            RevisionEntryState::Deleted,
            RevisionEntryState::Unreadable {
                reason: "permission denied".to_owned(),
            },
            RevisionEntryState::Renamed {
                to: "src/elsewhere.rs".to_owned(),
            },
        ] {
            let mut unavailable = source("mod unavailable;");
            unavailable
                .states
                .insert("src/unavailable.rs".to_owned(), state);
            assert!(
                snapshot_rust_api(&unavailable)
                    .unknowns
                    .iter()
                    .any(|unknown| unknown.kind == RustApiUnknownKind::SourceRead)
            );
        }

        let mut non_regular = source("mod unavailable;");
        non_regular.states.insert(
            "src/unavailable.rs".to_owned(),
            RevisionEntryState::NonRegular {
                kind: RevisionEntryKind::Symlink,
            },
        );
        assert!(
            snapshot_rust_api(&non_regular)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::NonRegularModule)
        );
    }

    fn fixture_snapshot(cell: &str, side: &str) -> RustApiSnapshot {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/api_surface")
            .join(cell)
            .join(side);
        let manifest = fs::read(root.join("Cargo.toml")).unwrap();
        let lib = fs::read(root.join("src/lib.rs")).unwrap();
        snapshot_rust_api(&MemorySource::new(&[
            ("Cargo.toml", &manifest),
            ("src/lib.rs", &lib),
        ]))
    }

    #[test]
    fn rust_api_snapshot_w0_invariants() {
        for cell in [
            "abi_unchanged",
            "cfg_operand_reordered",
            "combining_mark_changed",
            "combining_mark_unchanged",
            "declaration_pairing_noop",
            "literal_layout_only",
            "long_unchanged",
            "module_scope_private_unreachable",
            "move_same_path",
            "reexport_preserved",
            "backslash_reindent",
        ] {
            assert_eq!(
                fixture_snapshot(cell, "base").items,
                fixture_snapshot(cell, "head").items,
                "{cell} must be a snapshot no-op"
            );
        }
        for cell in [
            "abi_changed",
            "cfg_guard_changed",
            "combining_mark_distinct",
            "item_body_changed",
            "item_body_removed",
            "long_below_32_line_cut",
            "module_scope_changed",
            "move_relocated",
            "reexport_changed",
            "reexport_removed",
        ] {
            assert_ne!(
                fixture_snapshot(cell, "base").items,
                fixture_snapshot(cell, "head").items,
                "{cell} must change snapshot structure"
            );
        }
    }

    fn find_item<'a>(snapshot: &'a RustApiSnapshot, name: &str) -> &'a RustApiItem {
        snapshot
            .items
            .iter()
            .find(|item| item.key.external_name == name)
            .unwrap_or_else(|| panic!("missing API item {name:?}: {:?}", names(snapshot)))
    }

    #[test]
    fn rust_api_snapshot_reexport_chain_preserves_every_guard() {
        let snapshot = snapshot_rust_api(&source(
            "mod donor { #[cfg(unix)] pub struct Item; }\n\
             mod middle { #[cfg_attr(feature = \"middle\", cfg(windows))] pub use crate::donor::Item as Mid; }\n\
             #[cfg(feature = \"public\")] pub use middle::Mid as Public;",
        ));
        let guards = &find_item(&snapshot, "Public").cfg_guard;
        assert!(guards.iter().any(|guard| guard.contains("unix")));
        assert!(guards.iter().any(|guard| guard.contains("middle")));
        assert!(guards.iter().any(|guard| guard.contains("public")));
    }

    #[test]
    fn rust_api_snapshot_reexported_private_type_keeps_inherent_api() {
        let snapshot = snapshot_rust_api(&source(
            "mod donor { pub struct Hidden; impl Hidden { pub fn make() -> Self { Self } pub const VALUE: u8 = 1; } }\n\
             pub use donor::Hidden as Public;",
        ));
        let public = names(&snapshot);
        assert!(public.iter().any(|name| name == "Public::make"));
        assert!(public.iter().any(|name| name == "Public::VALUE"));
    }

    #[test]
    fn rust_api_snapshot_cross_module_impl_resolves_public_owner() {
        let snapshot = snapshot_rust_api(&source(
            "mod model { pub struct Hidden; }\n\
             mod implementation { impl crate::model::Hidden { pub fn build() -> Self { Self } } }\n\
             pub use model::Hidden as Public;",
        ));
        assert!(names(&snapshot).iter().any(|name| name == "Public::build"));
    }

    #[test]
    fn rust_api_snapshot_impl_context_is_semantic() {
        let left = snapshot_rust_api(&source(
            "pub struct Public<T>(pub T); impl<T: Copy> Public<T> where T: Send { pub fn value(&self) {} }",
        ));
        let right = snapshot_rust_api(&source(
            "pub struct Public<T>(pub T); impl<T: Clone> Public<T> where T: Sync { pub fn value(&self) {} }",
        ));
        assert_ne!(
            find_item(&left, "Public::value").contract,
            find_item(&right, "Public::value").contract
        );
    }

    #[test]
    fn rust_api_snapshot_relevant_method_attr_is_semantic() {
        let left = snapshot_rust_api(&source(
            "pub struct Public; impl Public { #[must_use] pub fn value() {} }",
        ));
        let right = snapshot_rust_api(&source(
            "pub struct Public; impl Public { pub fn value() {} }",
        ));
        assert_ne!(
            find_item(&left, "Public::value").contract,
            find_item(&right, "Public::value").contract
        );
    }

    #[test]
    fn rust_api_snapshot_nested_docs_are_not_semantic() {
        let left = snapshot_rust_api(&source(
            "pub struct Public { /// field docs\n pub field: u8 } pub enum Choice { /// variant docs\n A } pub trait Service { /// method docs\n fn call(&self); }",
        ));
        let right = snapshot_rust_api(&source(
            "pub struct Public { pub field: u8 } pub enum Choice { A } pub trait Service { fn call(&self); }",
        ));
        assert_eq!(left.items, right.items);
    }

    #[test]
    fn rust_api_snapshot_nested_cfg_reordering_is_not_semantic() {
        let left = snapshot_rust_api(&source(
            "pub struct Public { #[cfg(all(unix, feature = \"x\"))] pub field: u8 }",
        ));
        let right = snapshot_rust_api(&source(
            "pub struct Public { #[cfg(all(feature = \"x\", unix))] pub field: u8 }",
        ));
        assert_eq!(left.items, right.items);
    }

    #[test]
    fn rust_api_snapshot_tuple_public_index_is_structural() {
        let left = snapshot_rust_api(&source("pub struct Public(u8, pub u16);"));
        let right = snapshot_rust_api(&source("pub struct Public(u8, String, pub u16);"));
        assert_ne!(
            find_item(&left, "Public").contract,
            find_item(&right, "Public").contract
        );
    }

    #[test]
    fn rust_api_snapshot_private_macro_export_is_crate_root_api() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { #[macro_export] macro_rules! public_macro { () => {} } }",
        ));
        let item = find_item(&snapshot, "public_macro");
        assert_eq!(item.key.namespace, RustNamespace::Macro);
        assert!(item.key.module_path.is_empty());
    }

    #[test]
    fn rust_api_snapshot_cfg_split_module_variants_are_not_cycles() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            (
                "src/lib.rs",
                b"#[cfg(unix)] pub mod platform; #[cfg(windows)] pub mod platform;",
            ),
            ("src/platform.rs", b"pub fn value() {}"),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        assert_eq!(
            snapshot
                .modules
                .iter()
                .filter(|module| module.module_path == ["platform"])
                .count(),
            2
        );
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::ModuleCycle)
        );
    }

    #[test]
    fn rust_api_snapshot_foreign_parent_contract_and_static_are_semantic() {
        let c = snapshot_rust_api(&source(
            "unsafe extern \"C\" { pub fn call(); pub static VALUE: u8; }",
        ));
        let system = snapshot_rust_api(&source(
            "unsafe extern \"system\" { pub fn call(); pub static VALUE: u8; }",
        ));
        assert_ne!(
            find_item(&c, "call").contract,
            find_item(&system, "call").contract
        );
        assert!(find_item(&c, "VALUE").contract.contains("extern"));
    }

    #[test]
    fn rust_api_snapshot_raw_identifier_is_semantic_noop() {
        let raw = snapshot_rust_api(&source("pub fn r#answer() {}"));
        let plain = snapshot_rust_api(&source("pub fn answer() {}"));
        assert_eq!(raw.items, plain.items);
    }

    #[test]
    fn rust_api_snapshot_lint_only_attr_is_not_semantic() {
        let left = snapshot_rust_api(&source("#[allow(dead_code)] pub fn answer() {}"));
        let right = snapshot_rust_api(&source("pub fn answer() {}"));
        assert_eq!(left.items, right.items);
    }

    #[test]
    fn rust_api_snapshot_cargo_discovery_is_exact_and_validated() {
        let virtual_workspace = MemorySource::new(&[("Cargo.toml", b"[workspace]\nmembers=[]\n")]);
        assert!(snapshot_rust_api(&virtual_workspace).crates.is_empty());

        let not_cargo = MemorySource::new(&[(
            "NotCargo.toml",
            b"[package]\nname='wrong'\nversion='0.0.0'\n",
        )]);
        let snapshot = snapshot_rust_api(&not_cargo);
        assert!(snapshot.crates.is_empty());
        assert!(snapshot.unknowns.is_empty());

        let explicit = MemorySource::new(&[
            ("nested/Cargo.toml", b"[package]\nname='package-name'\nversion='0.0.0'\n[lib]\nname='public_name'\npath='source/root.rs'\n"),
            ("nested/source/root.rs", b"pub fn answer() {}"),
        ]);
        let snapshot = snapshot_rust_api(&explicit);
        assert_eq!(snapshot.crates[0].name, "public_name");
        assert_eq!(snapshot.crates[0].root_path, "nested/source/root.rs");

        let rooted = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='product'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", b"pub fn product() {}"),
            (
                "tests/fixtures/sample/Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("tests/fixtures/sample/src/lib.rs", b"pub fn fixture() {}"),
            (
                "tools/fixtures/bounded-runtime/Cargo.toml",
                b"[package]\nname='bounded'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "tools/fixtures/bounded-runtime/src/lib.rs",
                b"pub fn bounded() {}",
            ),
        ]);
        let snapshot = snapshot_rust_api(&rooted);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["product"],
            "fixture and tool packages are not product API crates"
        );
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| item.key.external_name == "product")
        );
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.key.external_name == "fixture"
                    || item.key.external_name == "bounded")
        );

        let rooted_with_nested_workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='product'\nversion='0.0.0'\n",
            ),
            ("src/lib.rs", b"pub fn product() {}"),
            (
                "tests/fixtures/nested/Cargo.toml",
                b"[workspace]\nmembers=['member']\n",
            ),
            (
                "tests/fixtures/nested/member/Cargo.toml",
                b"[package]\nname='fixture-member'\nversion='0.0.0'\n",
            ),
            (
                "tests/fixtures/nested/member/src/lib.rs",
                b"pub fn fixture_member() {}",
            ),
        ]);
        let snapshot = snapshot_rust_api(&rooted_with_nested_workspace);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["product"],
            "a nested fixture workspace must not replace root package authority"
        );

        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/*']\nexclude=['crates/skip']\n",
            ),
            (
                "crates/api/Cargo.toml",
                b"[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("crates/api/src/lib.rs", b"pub fn api() {}"),
            (
                "crates/skip/Cargo.toml",
                b"[package]\nname='skip'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("crates/skip/src/lib.rs", b"pub fn skip() {}"),
            (
                "tools/scratch/Cargo.toml",
                b"[package]\nname='scratch'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("tools/scratch/src/lib.rs", b"pub fn scratch() {}"),
        ]);
        let snapshot = snapshot_rust_api(&workspace);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["api"]
        );

        let unicode = MemorySource::new(&[
            (
                "Cargo.toml",
                "[package]\nname='café'\nversion='0.0.0'\n".as_bytes(),
            ),
            ("src/lib.rs", b"pub fn answer() {}"),
        ]);
        assert_eq!(snapshot_rust_api(&unicode).crates[0].name, "café");

        let autolib_false = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nautolib=false\n",
            ),
            ("src/lib.rs", b"pub fn ignored() {}"),
        ]);
        assert!(snapshot_rust_api(&autolib_false).crates.is_empty());

        for manifest in [
            "[package]\nversion='0.0.0'\n",
            "[package]\nname=1\nversion='0.0.0'\n",
            "[package]\nname='x'\nversion='0.0.0'\nautolib='yes'\n",
            "lib='bad'\n[package]\nname='x'\nversion='0.0.0'\n",
            "[package]\nname='x'\nversion='0.0.0'\n[lib]\nname=1\n",
            "[package]\nname='x'\nversion='0.0.0'\n[lib]\npath=1\n",
            "[package]\nname='x'\nversion='0.0.0'\n[lib]\nproc-macro='yes'\n",
        ] {
            let source = MemorySource::new(&[
                ("Cargo.toml", manifest.as_bytes()),
                ("src/lib.rs", b"pub fn answer() {}"),
            ]);
            let snapshot = snapshot_rust_api(&source);
            assert!(snapshot.crates.is_empty(), "{manifest}");
            assert!(
                snapshot
                    .unknowns
                    .iter()
                    .any(|unknown| unknown.kind == RustApiUnknownKind::ManifestParse),
                "{manifest}"
            );
        }
    }

    #[test]
    fn rust_api_snapshot_paths_are_fallible_and_source_backed() {
        for path in ["../outside.rs", "/absolute.rs"] {
            let manifest =
                format!("[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='{path}'\n");
            let fixture = MemorySource::new(&[("Cargo.toml", manifest.as_bytes())]);
            let snapshot = snapshot_rust_api(&fixture);
            assert!(snapshot.crates.is_empty());
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::ManifestParse && unknown.evidence.contains(path)
            }));
        }

        let safe = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            ("src/lib.rs", b"#[path = \"alternate.rs\"] pub mod api;"),
            ("src/alternate.rs", b"pub fn answer() {}"),
        ]);
        assert!(
            names(&snapshot_rust_api(&safe))
                .iter()
                .any(|name| name == "api::answer")
        );

        for path in ["../../outside.rs", "/absolute.rs"] {
            let lib = format!("#[path = \"{path}\"] pub mod api;");
            let snapshot = snapshot_rust_api(&source(&lib));
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::UnsupportedModulePath
                    && unknown.cfg_guard.is_empty()
                    && unknown.evidence.contains(path)
            }));
            assert!(
                !snapshot
                    .modules
                    .iter()
                    .any(|module| module.module_path == ["api"])
            );
        }
    }

    #[test]
    fn rust_api_snapshot_positive_records_require_live_parsed_sources() {
        let parse_failed = snapshot_rust_api(&source("pub fn broken("));
        assert!(parse_failed.crates.is_empty());
        assert!(parse_failed.modules.is_empty());
        assert!(parse_failed.items.is_empty());

        let mut deleted_root = source("pub fn answer() {}");
        deleted_root
            .states
            .insert("src/lib.rs".to_owned(), RevisionEntryState::Deleted);
        let snapshot = snapshot_rust_api(&deleted_root);
        assert!(snapshot.crates.is_empty());
        assert!(snapshot.modules.is_empty());

        let mut unavailable_manifest = source("pub fn answer() {}");
        unavailable_manifest.states.insert(
            "Cargo.toml".to_owned(),
            RevisionEntryState::NonRegular {
                kind: RevisionEntryKind::Symlink,
            },
        );
        let snapshot = snapshot_rust_api(&unavailable_manifest);
        assert!(snapshot.crates.is_empty());
        assert!(
            snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::ManifestRead)
        );
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::NonRegularModule)
        );
    }

    #[test]
    fn rust_api_snapshot_live_added_and_renamed_roots_and_modules_are_source_backed() {
        for state in [
            RevisionEntryState::Added,
            RevisionEntryState::RenamedFrom {
                from: "src/old.rs".to_owned(),
            },
        ] {
            let mut fixture = MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='fixture'\nversion='0.0.0'\n",
                ),
                ("src/lib.rs", b"pub mod api;"),
                ("src/api.rs", b"pub fn exact_bytes() {}"),
            ]);
            fixture
                .states
                .insert("src/lib.rs".to_owned(), state.clone());
            fixture.states.insert("src/api.rs".to_owned(), state);
            let snapshot = snapshot_rust_api(&fixture);
            assert!(
                names(&snapshot)
                    .iter()
                    .any(|name| name == "api::exact_bytes")
            );
            assert!(
                snapshot
                    .modules
                    .iter()
                    .all(|module| module.provenance == fixture.provenance)
            );
            assert_eq!(snapshot.provenance, fixture.provenance);
        }
    }

    #[test]
    fn rust_api_snapshot_live_module_wins_over_stale_alternative() {
        for stale in [
            RevisionEntryState::Deleted,
            RevisionEntryState::Renamed {
                to: "src/old.rs".to_owned(),
            },
        ] {
            let mut fixture = MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='fixture'\nversion='0.0.0'\n",
                ),
                ("src/lib.rs", b"pub mod api;"),
                ("src/api/mod.rs", b"pub fn answer() {}"),
            ]);
            fixture.states.insert("src/api.rs".to_owned(), stale);
            let snapshot = snapshot_rust_api(&fixture);
            assert!(names(&snapshot).iter().any(|name| name == "api::answer"));
            assert!(
                !snapshot
                    .unknowns
                    .iter()
                    .any(|unknown| unknown.kind == RustApiUnknownKind::AmbiguousModule)
            );
        }
    }

    fn semantic_items(
        snapshot: &RustApiSnapshot,
    ) -> Vec<(RustApiItemKey, RustApiItemKind, String, Vec<String>)> {
        snapshot
            .items
            .iter()
            .map(|item| {
                (
                    item.key.clone(),
                    item.kind,
                    item.contract.clone(),
                    item.cfg_guard.clone(),
                )
            })
            .collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SemanticProjection {
        items: Vec<(RustApiItemKey, RustApiItemKind, String, Vec<String>)>,
        module_aliases: Vec<(String, Vec<String>, Vec<String>)>,
        reexports: Vec<SemanticReexport>,
        unknowns: Vec<SemanticUnknown>,
    }

    type SemanticReexport = (String, Vec<String>, String, RustNamespace, Vec<String>);
    type SemanticUnknown = (
        RustApiUnknownKind,
        Option<String>,
        Vec<String>,
        String,
        Vec<String>,
    );

    fn semantic_projection(snapshot: &RustApiSnapshot) -> SemanticProjection {
        SemanticProjection {
            items: semantic_items(snapshot),
            module_aliases: snapshot
                .module_aliases
                .iter()
                .map(|alias| {
                    (
                        alias.crate_name.clone(),
                        alias.module_path.clone(),
                        alias.cfg_guard.clone(),
                    )
                })
                .collect(),
            reexports: snapshot
                .reexports
                .iter()
                .map(|reexport| {
                    (
                        reexport.crate_name.clone(),
                        reexport.module_path.clone(),
                        reexport.external_name.clone(),
                        reexport.namespace,
                        reexport.cfg_guard.clone(),
                    )
                })
                .collect(),
            unknowns: snapshot
                .unknowns
                .iter()
                .map(|unknown| {
                    (
                        unknown.kind,
                        unknown.crate_name.clone(),
                        unknown.module_path.clone(),
                        unknown.evidence.clone(),
                        unknown.cfg_guard.clone(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn rust_api_snapshot_module_alias_projects_descendants() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { pub mod nested { pub struct Item; } } } \
             pub use private::donor as public;",
        ));
        assert!(
            snapshot
                .module_aliases
                .iter()
                .any(|alias| alias.module_path == ["public"])
        );
        assert!(
            names(&snapshot)
                .iter()
                .any(|name| name == "public::nested::Item")
        );
    }

    #[test]
    fn rust_api_snapshot_private_donor_rename_keeps_public_contract() {
        let old = snapshot_rust_api(&source(
            "mod donor { pub struct Old; } pub use donor::Old as Public;",
        ));
        let new = snapshot_rust_api(&source(
            "mod donor { pub struct New; } pub use donor::New as Public;",
        ));
        assert_eq!(semantic_items(&old), semantic_items(&new));
        assert_ne!(
            find_item(&old, "Public").origin_name,
            find_item(&new, "Public").origin_name
        );
    }

    #[test]
    fn rust_api_snapshot_public_reexport_retarget_changes_contract() {
        let old = snapshot_rust_api(&source(
            "pub mod a { pub struct A; } pub mod b { pub struct B; } pub use a::A as Public;",
        ));
        let new = snapshot_rust_api(&source(
            "pub mod a { pub struct A; } pub mod b { pub struct B; } pub use b::B as Public;",
        ));
        assert_ne!(
            find_item(&old, "Public").contract,
            find_item(&new, "Public").contract,
            "retargeting a public reexport between public types must change the compared contract"
        );
        assert!(
            find_item(&old, "Public")
                .contract
                .contains("reexport-origin:a::A")
        );
        assert!(
            find_item(&new, "Public")
                .contract
                .contains("reexport-origin:b::B")
        );
    }

    #[test]
    fn rust_api_snapshot_private_module_public_trait_impl_is_unknown() {
        let snapshot = snapshot_rust_api(&source(
            "pub trait Marker {} pub struct Value; \
             mod helper { impl crate::Marker for crate::Value {} }",
        ));
        assert!(
            snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::TraitImplResolution
                    && unknown.evidence.contains("impl")
                    && unknown.evidence.contains("Marker")
            }),
            "a public trait impl declared in a private module is still globally usable"
        );
    }

    #[test]
    fn rust_api_snapshot_declared_name_placeholder_preserves_type_references() {
        let before = snapshot_rust_api(&source("pub struct Same; pub fn Same() -> Same { Same }"));
        let after = snapshot_rust_api(&source("pub struct Same; pub fn Same() -> u8 { 0 }"));
        let before = before
            .items
            .iter()
            .find(|item| {
                item.key.external_name == "Same"
                    && item.key.namespace == RustNamespace::Value
                    && item.kind == RustApiItemKind::Function
            })
            .unwrap();
        let after = after
            .items
            .iter()
            .find(|item| {
                item.key.external_name == "Same"
                    && item.key.namespace == RustNamespace::Value
                    && item.kind == RustApiItemKind::Function
            })
            .unwrap();
        assert_ne!(before.contract, after.contract);
        assert!(before.contract.contains("Same"));
    }

    #[test]
    fn rust_api_snapshot_namespaces_include_constructors_and_macros() {
        let snapshot = snapshot_rust_api(&source(
            "pub struct Same(pub u8); #[macro_export] macro_rules! Same { () => {} }",
        ));
        let namespaces: BTreeSet<_> = snapshot
            .items
            .iter()
            .filter(|item| item.key.external_name == "Same")
            .map(|item| item.key.namespace)
            .collect();
        assert_eq!(
            namespaces,
            BTreeSet::from([
                RustNamespace::Type,
                RustNamespace::Value,
                RustNamespace::Macro
            ])
        );
        let named = snapshot_rust_api(&source("pub struct Named { pub value: u8 }"));
        assert!(
            !named
                .items
                .iter()
                .any(|item| item.key.external_name == "Named"
                    && item.key.namespace == RustNamespace::Value)
        );
    }

    #[test]
    fn rust_api_snapshot_proc_and_attribute_macros_are_honest() {
        let proc_source = MemorySource::new(&[
            ("Cargo.toml", b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\nproc-macro=true\n"),
            ("src/lib.rs", b"#[proc_macro] pub fn bang(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input } #[proc_macro_attribute] pub fn attr(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input } #[proc_macro_derive(Derived)] pub fn derive(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }"),
        ]);
        let snapshot = snapshot_rust_api(&proc_source);
        for name in ["bang", "attr", "Derived"] {
            assert!(
                snapshot
                    .items
                    .iter()
                    .any(|item| item.key.external_name == name
                        && item.key.namespace == RustNamespace::Macro)
            );
        }
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.kind == RustApiItemKind::Function)
        );

        let generated = snapshot_rust_api(&source("#[custom_transform] pub struct Public;"));
        assert!(generated.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::MacroGeneratedItems
                && unknown.evidence.contains("custom_transform")
        }));
        assert!(
            !generated
                .items
                .iter()
                .any(|item| item.key.external_name == "Public")
        );
    }

    #[test]
    fn rust_api_snapshot_reexport_resolution_is_namespace_and_guard_aware() {
        let guarded = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; }\n\
             #[cfg(unix)] pub use a::Item as Public;\n\
             #[cfg(windows)] pub use b::Item as Public;",
        ));
        assert_eq!(
            guarded
                .items
                .iter()
                .filter(|item| item.key.external_name == "Public"
                    && item.key.namespace == RustNamespace::Type)
                .count(),
            2
        );
        assert!(
            !guarded
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::AmbiguousReexport)
        );

        let ambiguous = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; }\n\
             pub use a::Item as Public; pub use b::Item as Public;",
        ));
        assert!(
            ambiguous
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::AmbiguousReexport)
        );
        assert!(
            !ambiguous
                .items
                .iter()
                .any(|item| item.key.external_name == "Public")
        );
    }

    #[test]
    fn rust_api_snapshot_conditional_unknowns_keep_guards() {
        let snapshot = snapshot_rust_api(&source(
            "#[cfg(feature = \"generated\")] pub use missing::Item;",
        ));
        let unknown = snapshot
            .unknowns
            .iter()
            .find(|unknown| {
                matches!(
                    unknown.kind,
                    RustApiUnknownKind::UnresolvedReexport
                        | RustApiUnknownKind::UnsupportedExternResolution
                )
            })
            .unwrap();
        assert!(
            unknown
                .cfg_guard
                .iter()
                .any(|guard| guard.contains("generated"))
        );
    }

    #[test]
    fn rust_api_snapshot_r2_failed_shared_root_is_not_reused_as_success() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/broken.rs'\n",
            ),
            (
                "nested/Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='../src/broken.rs'\n",
            ),
            ("src/broken.rs", b"pub fn broken("),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        assert!(snapshot.crates.is_empty(), "{:#?}", snapshot.crates);
        assert!(snapshot.modules.is_empty(), "{:#?}", snapshot.modules);
        assert_eq!(
            snapshot
                .unknowns
                .iter()
                .filter(|unknown| unknown.kind == RustApiUnknownKind::SourceParse)
                .count(),
            1
        );
    }

    #[test]
    fn rust_api_snapshot_r2_empty_cargo_names_are_manifest_unknowns() {
        for manifest in [
            "[package]\nname=''\nversion='0.0.0'\n",
            "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\nname=''\n",
        ] {
            let fixture = MemorySource::new(&[
                ("Cargo.toml", manifest.as_bytes()),
                ("src/lib.rs", b"pub fn answer() {}"),
            ]);
            let snapshot = snapshot_rust_api(&fixture);
            assert!(
                snapshot.crates.is_empty(),
                "{manifest}: {:#?}",
                snapshot.crates
            );
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::ManifestParse
                    && unknown.evidence.contains("name")
            }));
        }
    }

    #[test]
    fn rust_api_snapshot_r2_direct_path_uses_physical_declaring_directory() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            ("src/lib.rs", b"pub mod a;"),
            ("src/a.rs", b"#[path = \"alternate.rs\"] pub mod api;"),
            ("src/alternate.rs", b"pub fn answer() {}"),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        assert!(names(&snapshot).iter().any(|name| name == "a::api::answer"));
        assert!(!snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::SourceRead
                && unknown.source_path.contains("a/alternate.rs")
        }));
    }

    #[test]
    fn rust_api_snapshot_r2_inline_path_changes_child_declaring_base() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            (
                "src/lib.rs",
                b"#[path = \"thread_files\"] pub mod thread { #[path = \"tls.rs\"] pub mod local_data; }",
            ),
            ("src/thread_files/tls.rs", b"pub fn answer() {}"),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        assert!(
            names(&snapshot)
                .iter()
                .any(|name| name == "thread::local_data::answer")
        );
    }

    #[test]
    fn rust_api_snapshot_r2_cfg_attr_path_suppresses_arbitrary_default() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            (
                "src/lib.rs",
                b"#[cfg_attr(feature = \"alternate\", path = \"alternate.rs\")] pub mod api;",
            ),
            ("src/api.rs", b"pub struct ArbitraryDefault;"),
            ("src/alternate.rs", b"pub struct Conditional;"),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        assert!(
            !names(&snapshot)
                .iter()
                .any(|name| name.contains("ArbitraryDefault"))
        );
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::UnsupportedModulePath
                && unknown.evidence.contains("cfg_attr")
        }));
    }

    #[test]
    fn rust_api_snapshot_r2_malformed_cfg_inherits_outer_guard_and_suppresses_item() {
        let snapshot = snapshot_rust_api(&source(
            "#[cfg(feature = \"outer\")] #[cfg(any(unix, broken = ))] pub fn value() {}",
        ));
        assert!(!names(&snapshot).iter().any(|name| name == "value"));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::CfgPredicate
                && unknown
                    .cfg_guard
                    .iter()
                    .any(|guard| guard.contains("outer"))
        }));
    }

    #[test]
    fn rust_api_snapshot_r2_private_module_reexport_is_not_confirmed() {
        let snapshot = snapshot_rust_api(&source(
            "mod donor { pub struct Item; } pub use donor as public;",
        ));
        assert!(
            snapshot.module_aliases.is_empty(),
            "{:#?}",
            snapshot.module_aliases
        );
        assert!(
            !names(&snapshot)
                .iter()
                .any(|name| name.starts_with("public"))
        );
        assert!(snapshot.unknowns.iter().any(|unknown| {
            matches!(
                unknown.kind,
                RustApiUnknownKind::UnresolvedReexport | RustApiUnknownKind::AmbiguousReexport
            )
        }));
    }

    #[test]
    fn rust_api_snapshot_r2_public_alias_does_not_leak_private_descendant() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { mod hidden { pub struct Secret; } pub struct Visible; } } \
             pub use private::donor as public;",
        ));
        let public = names(&snapshot);
        assert!(public.iter().any(|name| name == "public::Visible"));
        assert!(
            !public.iter().any(|name| name.contains("Secret")),
            "{public:#?}"
        );
    }

    #[test]
    fn rust_api_snapshot_r2_public_alias_includes_item_reexport_descendant() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { mod inner { pub struct Item; } pub use inner::Item as Exported; } } \
             pub use private::donor as public;",
        ));
        let public = names(&snapshot);
        assert!(
            public.iter().any(|name| name == "public::Exported"),
            "{public:#?}"
        );
        assert!(!public.iter().any(|name| name == "public::inner::Item"));
    }

    #[test]
    fn rust_api_snapshot_r2_wholly_private_alias_is_semantic_noop() {
        let left = snapshot_rust_api(&source(
            "mod private { pub mod a { pub struct Item; } pub use a as alias; }",
        ));
        let right = snapshot_rust_api(&source(
            "mod private { pub mod b { pub struct Item; } pub use b as alias; }",
        ));
        assert_eq!(left.module_aliases, right.module_aliases);
        assert_eq!(semantic_items(&left), semantic_items(&right));
    }

    #[test]
    fn rust_api_snapshot_r2_relative_root_module_candidates_are_ambiguous() {
        let snapshot = snapshot_rust_api(&source(
            "pub mod choice { pub struct Root; } \
             pub mod scope { pub mod choice { pub struct Local; } pub use choice as public; }",
        ));
        assert!(
            !names(&snapshot)
                .iter()
                .any(|name| name.starts_with("scope::public::"))
        );
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::AmbiguousReexport
                && unknown.module_path == ["scope"]
        }));
    }

    #[test]
    fn rust_api_snapshot_r2_overlapping_cfg_origins_are_ambiguous() {
        let snapshot = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; } \
             #[cfg(any(unix, windows))] pub use a::Item as Public; \
             #[cfg(unix)] pub use b::Item as Public;",
        ));
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.key.external_name == "Public")
        );
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::AmbiguousReexport
                && unknown.evidence.contains("conflicting")
        }));
    }

    #[test]
    fn rust_api_snapshot_r2_transforming_attrs_on_impl_and_method_suppress_api() {
        for source_text in [
            "pub struct Public; #[custom_impl] impl Public { pub fn value() {} }",
            "pub struct Public; impl Public { #[custom_method] pub fn value() {} }",
        ] {
            let snapshot = snapshot_rust_api(&source(source_text));
            assert!(!names(&snapshot).iter().any(|name| name == "Public::value"));
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::MacroGeneratedItems
                    && unknown.evidence.contains("custom_")
            }));
        }
    }

    #[test]
    fn rust_api_snapshot_r2_transforming_attrs_cover_module_foreign_and_macro() {
        let snapshot = snapshot_rust_api(&source(
            "#[custom_module] mod private { pub struct Item; } pub use private::Item as Public; \
             #[custom_foreign] extern \"C\" { pub fn call(); } \
             #[custom_macro] #[macro_export] macro_rules! exported { () => {} }",
        ));
        for name in ["Public", "call", "exported"] {
            assert!(
                !names(&snapshot).iter().any(|candidate| candidate == name),
                "{name}"
            );
        }
        assert_eq!(
            snapshot
                .unknowns
                .iter()
                .filter(|unknown| unknown.kind == RustApiUnknownKind::MacroGeneratedItems)
                .count(),
            3
        );
    }

    #[test]
    fn rust_api_snapshot_r2_docs_lints_and_macro_attrs_are_semantic_noops() {
        let left = snapshot_rust_api(&source(
            "#[cfg_attr(feature = \"docs\", doc = \"answer\")] #[cfg_attr(feature = \"lint\", allow(dead_code))] pub fn answer() {} \
             #[doc = \"macro\"] #[allow(unused_macros)] #[macro_export] macro_rules! exported { () => {} }",
        ));
        let right = snapshot_rust_api(&source(
            "pub fn answer() {} #[macro_export] macro_rules! exported { () => {} }",
        ));
        assert_eq!(semantic_items(&left), semantic_items(&right));
    }

    #[test]
    fn rust_api_snapshot_r2_private_and_nested_proc_macros_are_not_root_api() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\nproc-macro=true\n",
            ),
            (
                "src/lib.rs",
                b"#[proc_macro] fn private_bang(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input } \
                  mod nested { #[proc_macro_attribute] pub fn nested_attr(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input } }",
            ),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        assert!(!snapshot.items.iter().any(|item| {
            matches!(
                item.key.external_name.as_str(),
                "private_bang" | "nested_attr"
            )
        }));
        assert_eq!(
            snapshot
                .unknowns
                .iter()
                .filter(|unknown| unknown.kind == RustApiUnknownKind::MacroGeneratedItems)
                .count(),
            2
        );
    }

    #[test]
    fn rust_api_snapshot_r2_w0_projection_carries_reexports_unknowns_and_manifest_set() {
        let manifest_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api_surface/manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
        let manifest_cells: BTreeSet<String> = manifest["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cell| cell["id"].as_str().unwrap().to_owned())
            .collect();
        let expected: BTreeSet<String> = [
            "abi_unchanged",
            "backslash_reindent",
            "cfg_operand_reordered",
            "cfg_unchanged",
            "combining_mark_changed",
            "combining_mark_unchanged",
            "declaration_pairing_noop",
            "legacy_non_api_control",
            "literal_layout_only",
            "long_unchanged",
            "module_scope_private_unreachable",
            "module_scope_unchanged",
            "move_same_path",
            "reexport_preserved",
            "abi_changed",
            "backslash_literal_changed",
            "cfg_guard_changed",
            "combining_mark_distinct",
            "declaration_pairing_changed",
            "function_added",
            "function_removed",
            "item_body_changed",
            "item_body_removed",
            "item_opener_removed",
            "literal_changed",
            "long_below_32_line_cut",
            "long_within_cap_changed",
            "module_scope_changed",
            "module_scope_unseen_opener",
            "move_relocated",
            "reexport_changed",
            "reexport_removed",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(manifest_cells, expected);

        let preserved = fixture_snapshot("reexport_preserved", "base");
        assert!(!semantic_projection(&preserved).reexports.is_empty());
        let changed = fixture_snapshot("reexport_changed", "head");
        assert!(
            !semantic_projection(&changed).reexports.is_empty()
                || !semantic_projection(&changed).unknowns.is_empty(),
            "changed reexport cell needs an external contract or typed unknown"
        );
    }

    #[test]
    fn rust_api_snapshot_r2_sibling_proof_boundary_self_attack() {
        let mut failed = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/binary.rs'\n",
            ),
            (
                "nested/Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='../src/binary.rs'\n",
            ),
            ("src/binary.rs", &[0xff, 0xfe]),
        ]);
        failed
            .states
            .insert("src/binary.rs".to_owned(), RevisionEntryState::Present);
        let failed = snapshot_rust_api(&failed);
        assert!(failed.crates.is_empty());
        assert!(failed.modules.is_empty());

        let restricted = snapshot_rust_api(&source(
            "pub(crate) mod donor { pub struct Item; } pub use donor as public;",
        ));
        assert!(restricted.module_aliases.is_empty());
        assert!(
            !names(&restricted)
                .iter()
                .any(|name| name.starts_with("public"))
        );

        let nested = snapshot_rust_api(&source(
            "mod private { pub mod donor { pub mod child { pub struct Visible; mod hidden { pub struct Secret; } } pub use child as exported_child; } } \
             pub use private::donor as public;",
        ));
        let nested_names = names(&nested);
        assert!(
            nested_names
                .iter()
                .any(|name| name == "public::exported_child::Visible"),
            "{nested_names:#?}"
        );
        assert!(!nested_names.iter().any(|name| name.contains("Secret")));

        let overlapping_features = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; } \
             #[cfg(feature = \"a\")] pub use a::Item as Public; \
             #[cfg(feature = \"b\")] pub use b::Item as Public;",
        ));
        assert!(!overlapping_features.items.iter().any(|item| {
            item.key.external_name == "Public" && item.key.namespace == RustNamespace::Type
        }));
        assert!(
            overlapping_features
                .unknowns
                .iter()
                .any(|unknown| { unknown.kind == RustApiUnknownKind::AmbiguousReexport })
        );

        let overlapping_modules = snapshot_rust_api(&source(
            "pub mod a { pub struct First; } pub mod b { pub struct Second; } \
             #[cfg(feature = \"a\")] pub use a as public; \
             #[cfg(feature = \"b\")] pub use b as public;",
        ));
        assert!(
            !overlapping_modules
                .module_aliases
                .iter()
                .any(|alias| alias.module_path == ["public"])
        );
        assert!(
            !names(&overlapping_modules)
                .iter()
                .any(|name| name.starts_with("public::"))
        );
        assert!(overlapping_modules.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::AmbiguousReexport
                && unknown.evidence.contains("module alias")
        }));
    }

    #[test]
    fn rust_api_snapshot_nested_self_module_alias_projects_descendants() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { pub mod nested { pub struct Item; } } } \
             pub use private::donor::{self as public};",
        ));
        assert!(
            names(&snapshot)
                .iter()
                .any(|name| name == "public::nested::Item")
        );
    }

    #[test]
    fn rust_api_snapshot_r3_guarded_visibility_does_not_cross_variants() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { \
                 #[cfg(unix)] pub mod child { pub struct Visible; } \
                 #[cfg(windows)] mod child { pub struct Leak; } \
             } } \
             pub use private::donor as public;",
        ));
        let public = names(&snapshot);
        assert!(
            public.iter().any(|name| name == "public::child::Visible"),
            "{public:#?}"
        );
        assert!(
            !public.iter().any(|name| name == "public::child::Leak"),
            "{public:#?}"
        );
    }

    #[test]
    fn rust_api_snapshot_r3_one_use_leaf_resolves_each_namespace() {
        let snapshot = snapshot_rust_api(&source(
            "mod donor { \
                 pub mod Same { pub struct X; } \
                 #[allow(non_snake_case)] pub fn Same() {} \
             } \
             pub use donor::Same as Public;",
        ));
        assert!(snapshot.module_aliases.iter().any(|alias| {
            alias.module_path == ["Public"] && alias.target_module_path == ["donor", "Same"]
        }));
        assert!(snapshot.items.iter().any(|item| {
            item.key.module_path.is_empty()
                && item.key.external_name == "Public"
                && item.key.namespace == RustNamespace::Value
        }));
        assert!(names(&snapshot).iter().any(|name| name == "Public::X"));
    }

    #[test]
    fn rust_api_snapshot_r3_parent_ambiguity_invalidates_derived_subtree() {
        let snapshot = snapshot_rust_api(&source(
            "pub mod a { \
                 pub mod child { pub struct A; } \
                 pub use child as nested; \
             } \
             pub mod b { \
                 pub mod child { pub struct B; } \
                 pub use child as nested; \
             } \
             #[cfg(feature = \"a\")] pub use a as public; \
             #[cfg(feature = \"b\")] pub use b as public;",
        ));
        assert!(
            !snapshot
                .module_aliases
                .iter()
                .any(|alias| alias.module_path.starts_with(&["public".to_owned()])),
            "{:#?}",
            snapshot.module_aliases
        );
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.key.module_path.starts_with(&["public".to_owned()])),
            "{:#?}",
            snapshot.items
        );
        assert!(
            !snapshot
                .reexports
                .iter()
                .any(|item| item.module_path.starts_with(&["public".to_owned()])),
            "{:#?}",
            snapshot.reexports
        );
        assert_eq!(
            snapshot
                .unknowns
                .iter()
                .filter(|unknown| {
                    unknown.kind == RustApiUnknownKind::AmbiguousReexport
                        && unknown.evidence.contains("module alias public")
                })
                .count(),
            1
        );
    }

    #[test]
    fn rust_api_snapshot_r3_reachable_extern_crate_is_typed_unknown() {
        let snapshot = snapshot_rust_api(&source("pub extern crate core;"));
        assert_eq!(snapshot.items.len(), 1, "{:#?}", snapshot.items);
        assert_eq!(snapshot.items[0].kind, RustApiItemKind::Crate);
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::UnsupportedExternResolution
                && unknown.evidence.contains("core")
        }));
    }

    #[test]
    fn rust_api_snapshot_r3_unknown_projection_keeps_public_identity() {
        let left = snapshot_rust_api(&source("pub use external_a::Thing as Public;"));
        let right = snapshot_rust_api(&source("pub use external_b::Thing as Public;"));
        assert_ne!(semantic_projection(&left), semantic_projection(&right));

        let old = snapshot_rust_api(&source(
            "mod donor { pub struct Old; } pub use donor::Old as Public;",
        ));
        let new = snapshot_rust_api(&source(
            "mod renamed { pub struct New; } pub use renamed::New as Public;",
        ));
        assert_eq!(semantic_projection(&old), semantic_projection(&new));
    }

    #[test]
    fn rust_api_snapshot_r3_namespace_ambiguity_does_not_erase_valid_siblings() {
        let snapshot = snapshot_rust_api(&source(
            "pub mod Same { pub struct Root; } \
             #[macro_export] macro_rules! Same { () => {} } \
             pub mod scope { \
                 pub mod Same { pub struct Local; } \
                 #[allow(non_snake_case)] pub fn Same() {} \
                 pub use Same as Public; \
             }",
        ));
        assert!(
            !snapshot
                .module_aliases
                .iter()
                .any(|alias| { alias.module_path == ["scope", "Public"] })
        );
        for namespace in [RustNamespace::Value, RustNamespace::Macro] {
            assert!(
                snapshot.items.iter().any(|item| {
                    item.key.module_path == ["scope"]
                        && item.key.external_name == "Public"
                        && item.key.namespace == namespace
                }),
                "{namespace:?}: {:#?}",
                snapshot.items
            );
        }
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::AmbiguousReexport
                && unknown.module_path == ["scope"]
                && unknown.evidence.contains("module origins")
        }));
    }

    #[test]
    fn rust_api_snapshot_r3_overlapping_private_variant_cannot_lend_visibility() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { \
                 #[cfg(feature = \"public\")] pub mod child { pub struct Visible; } \
                 #[cfg(feature = \"private\")] mod child { pub struct Leak; } \
             } } \
             pub use private::donor as public;",
        ));
        assert!(
            !names(&snapshot)
                .iter()
                .any(|name| name == "public::child::Leak")
        );
    }

    #[test]
    fn rust_api_snapshot_r3_three_level_alias_subtree_stays_invalidated() {
        let snapshot = snapshot_rust_api(&source(
            "pub mod a { \
                 pub mod child { pub mod grand { pub struct A; } } \
                 pub use child as level_one; \
                 pub use level_one as level_two; \
             } \
             pub mod b { pub mod child { pub struct B; } } \
             #[cfg(feature = \"a\")] pub use a as public; \
             #[cfg(feature = \"b\")] pub use b as public;",
        ));
        assert!(
            !snapshot
                .module_aliases
                .iter()
                .any(|alias| { alias.module_path.starts_with(&["public".to_owned()]) }),
            "{:#?}",
            snapshot.module_aliases
        );
        assert!(
            !names(&snapshot)
                .iter()
                .any(|name| name.starts_with("public::"))
        );
    }

    #[test]
    fn rust_api_snapshot_r3_repeated_source_reads_keep_distinct_evidence() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            (
                "src/lib.rs",
                b"#[path = \"missing.rs\"] pub mod first; #[path = \"missing.rs\"] pub mod second;",
            ),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        let paths: BTreeSet<Vec<String>> = snapshot
            .unknowns
            .iter()
            .filter(|unknown| unknown.kind == RustApiUnknownKind::SourceRead)
            .map(|unknown| unknown.module_path.clone())
            .collect();
        assert_eq!(
            paths,
            BTreeSet::from([
                ["first".to_owned()].to_vec(),
                ["second".to_owned()].to_vec()
            ])
        );
    }

    #[test]
    fn rust_api_snapshot_r3_cargo_name_leading_character_contract() {
        let valid = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='_fixture'\nversion='0.0.0'\n",
            ),
            ("src/lib.rs", b"pub fn answer() {}"),
        ]);
        assert_eq!(snapshot_rust_api(&valid).crates[0].name, "_fixture");

        let invalid = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='1fixture'\nversion='0.0.0'\n",
            ),
            ("src/lib.rs", b"pub fn answer() {}"),
        ]);
        let invalid = snapshot_rust_api(&invalid);
        assert!(invalid.crates.is_empty());
        assert!(invalid.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::ManifestParse && unknown.evidence.contains("name")
        }));
    }

    #[test]
    fn rust_api_snapshot_r4_overlapping_private_feature_vetoes_broad_positive() {
        let snapshot = snapshot_rust_api(&source(
            "mod private { pub mod donor { \
                 #[cfg(feature = \"a\")] pub mod child { pub struct Visible; } \
                 #[cfg(feature = \"b\")] mod child { pub struct Hidden; } \
             } } \
             pub use private::donor as public;",
        ));
        let public = names(&snapshot);
        assert!(
            !public.iter().any(|name| {
                name == "public::child::Visible" || name == "public::child::Hidden"
            }),
            "{public:#?}"
        );
        assert!(
            snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::AmbiguousReexport
                    && unknown.module_path == ["public", "child"]
                    && unknown
                        .cfg_guard
                        .iter()
                        .any(|guard| guard.contains("feature = \"a\""))
                    && unknown
                        .cfg_guard
                        .iter()
                        .any(|guard| guard.contains("feature = \"b\""))
                    && unknown.evidence.contains("private")
            }),
            "{:#?}",
            snapshot.unknowns
        );
    }

    #[test]
    fn rust_api_snapshot_r4_module_root_causally_invalidates_escaped_item() {
        let snapshot = snapshot_rust_api(&source(
            "pub mod a { pub struct Item; } \
             pub mod b { pub struct Item; } \
             #[cfg(feature = \"a\")] pub use a as public; \
             pub use public::Item as Other; \
             #[cfg(feature = \"b\")] pub use b as public;",
        ));
        assert!(
            !names(&snapshot)
                .iter()
                .any(|name| name.starts_with("public::") || name == "Other"),
            "{:#?}",
            snapshot.items
        );
    }

    #[test]
    fn rust_api_snapshot_r4_module_root_invalidation_is_order_independent() {
        let forward = snapshot_rust_api(&source(
            "pub mod a { pub struct Item; } \
             pub mod b { pub struct Item; } \
             #[cfg(feature = \"a\")] pub use a as public; \
             pub use public::Item as Other; \
             #[cfg(feature = \"b\")] pub use b as public;",
        ));
        let permuted = snapshot_rust_api(&source(
            "pub mod a { pub struct Item; } \
             pub mod b { pub struct Item; } \
             #[cfg(feature = \"b\")] pub use b as public; \
             #[cfg(feature = \"a\")] pub use a as public; \
             pub use public::Item as Other;",
        ));
        assert_eq!(
            semantic_projection(&forward),
            semantic_projection(&permuted)
        );
    }

    #[test]
    fn rust_api_snapshot_r4_multilevel_escape_tracks_ambiguous_proof_root() {
        let snapshot = snapshot_rust_api(&source(
            "pub mod a { pub mod nested { pub struct Item; } } \
             pub mod b { pub mod nested { pub struct Item; } } \
             #[cfg(feature = \"a\")] pub use a as public; \
             pub use public::nested::Item as Escaped; \
             pub use Escaped as Twice; \
             #[cfg(feature = \"b\")] pub use b as public;",
        ));
        assert!(
            !names(&snapshot).iter().any(|name| {
                name.starts_with("public::") || name == "Escaped" || name == "Twice"
            }),
            "{:#?}",
            snapshot.items
        );
    }

    #[test]
    fn rust_api_snapshot_r4_symbolic_root_invalidates_dependents_only() {
        let snapshot = snapshot_rust_api(&source(
            "mod a { pub struct Item; } \
             mod b { pub struct Item; } \
             pub struct Unrelated; \
             #[cfg(feature = \"a\")] pub use a::Item as Root; \
             pub use Root as Other; \
             #[cfg(feature = \"b\")] pub use b::Item as Root;",
        ));
        let public = names(&snapshot);
        assert!(!public.iter().any(|name| name == "Root" || name == "Other"));
        assert!(public.iter().any(|name| name == "Unrelated"));
    }

    #[test]
    fn rust_api_snapshot_r4_symbolic_permutation_preserves_full_projection() {
        let forward = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; } pub struct Unrelated; \
             #[cfg(feature = \"a\")] pub use a::Item as Root; \
             pub use Root as Other; \
             #[cfg(feature = \"b\")] pub use b::Item as Root;",
        ));
        let permuted = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; } pub struct Unrelated; \
             #[cfg(feature = \"b\")] pub use b::Item as Root; \
             #[cfg(feature = \"a\")] pub use a::Item as Root; \
             pub use Root as Other;",
        ));
        assert_eq!(
            semantic_projection(&forward),
            semantic_projection(&permuted)
        );
    }

    fn module_conflict_family(count: usize) -> RustApiSnapshot {
        let mut source_text = String::new();
        for index in 0..count {
            source_text.push_str(&format!(
                "pub mod A{index} {{ pub struct Item; }} \
                 pub mod B{index} {{ pub struct Item; }} \
                 #[cfg(feature = \"a{index}\")] pub use A{index} as P{index}; \
                 pub use P{index}::Item as O{index}; \
                 #[cfg(feature = \"b{index}\")] pub use B{index} as P{index}; "
            ));
        }
        snapshot_rust_api(&source(&source_text))
    }

    fn assert_complete_module_conflict_family(count: usize) {
        let snapshot = module_conflict_family(count);
        let leaked: Vec<_> = snapshot
            .items
            .iter()
            .filter(|item| item.key.external_name.starts_with('O'))
            .map(|item| (item.key.external_name.clone(), item.key.namespace))
            .collect();
        assert!(leaked.is_empty(), "count={count}, leaked={leaked:?}");
        let ambiguities = snapshot
            .unknowns
            .iter()
            .filter(|unknown| {
                unknown.kind == RustApiUnknownKind::AmbiguousReexport
                    && unknown.evidence.starts_with("module alias P")
            })
            .count();
        assert_eq!(ambiguities, count, "{:#?}", snapshot.unknowns);
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::ResolutionLimit)
        );
    }

    #[test]
    fn rust_api_snapshot_r5_module_conflict_boundary_127() {
        assert_complete_module_conflict_family(127);
    }

    #[test]
    fn rust_api_snapshot_r5_module_conflict_boundary_128() {
        assert_complete_module_conflict_family(128);
    }

    #[test]
    fn rust_api_snapshot_r5_module_conflict_boundary_129() {
        assert_complete_module_conflict_family(129);
    }

    fn reverse_legal_alias_chain(length: usize) -> RustApiSnapshot {
        let mut source_text = "pub struct Seed; ".to_owned();
        for index in (0..length).rev() {
            let target = if index == 0 {
                "Seed".to_owned()
            } else {
                format!("A{}", index - 1)
            };
            source_text.push_str(&format!("pub use {target} as A{index}; "));
        }
        snapshot_rust_api(&source(&source_text))
    }

    fn assert_reverse_legal_alias_chain(length: usize) {
        let snapshot = reverse_legal_alias_chain(length);
        let highest = format!("A{}", length - 1);
        assert!(
            snapshot.items.iter().any(|item| {
                item.key.module_path.is_empty() && item.key.external_name == highest
            }),
            "length={length}, names={:#?}",
            names(&snapshot)
        );
        assert!(!snapshot.unknowns.iter().any(|unknown| {
            matches!(
                unknown.kind,
                RustApiUnknownKind::UnresolvedReexport
                    | RustApiUnknownKind::ReexportCycle
                    | RustApiUnknownKind::ResolutionLimit
            ) && unknown.evidence.contains(&highest)
        }));
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::ResolutionLimit)
        );
    }

    #[test]
    fn rust_api_snapshot_r5_reverse_legal_alias_chain_127() {
        assert_reverse_legal_alias_chain(127);
    }

    #[test]
    fn rust_api_snapshot_r5_reverse_legal_alias_chain_128() {
        assert_reverse_legal_alias_chain(128);
    }

    #[test]
    fn rust_api_snapshot_r5_reverse_legal_alias_chain_129() {
        assert_reverse_legal_alias_chain(129);
    }

    #[test]
    fn rust_api_snapshot_r5_resolution_budget_exhaustion_is_fail_closed() {
        let fixture = source("pub struct Seed; pub use A0 as A1; pub use Seed as A0;");
        let snapshot = snapshot_rust_api_with_resolution_budget(&fixture, 1);
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| { matches!(item.key.external_name.as_str(), "A0" | "A1") })
        );
        assert!(snapshot.reexports.is_empty());
        assert!(snapshot.module_aliases.is_empty());
        let resolution_limits: Vec<_> = snapshot
            .unknowns
            .iter()
            .filter(|unknown| unknown.kind == RustApiUnknownKind::ResolutionLimit)
            .collect();
        assert_eq!(resolution_limits.len(), 1, "{:#?}", snapshot.unknowns);
        assert!(
            resolution_limits[0]
                .evidence
                .contains("graph-derived budget 1")
        );
    }

    fn feature_guard(name: &str) -> String {
        format!("feature = \"{name}\"")
    }

    fn pair_guards(names: &[&str]) -> BTreeSet<Vec<String>> {
        let mut pairs = BTreeSet::new();
        for (index, left) in names.iter().enumerate() {
            for right in names.iter().skip(index + 1) {
                pairs.insert(vec![feature_guard(left), feature_guard(right)]);
            }
        }
        pairs
    }

    fn triple_module_projection(order: &[&str]) -> SemanticProjection {
        let mut source_text = "pub mod a { pub struct Item; } \
                               pub mod b { pub struct Item; } \
                               pub mod c { pub struct Item; } "
            .to_owned();
        for name in order {
            source_text.push_str(&format!(
                "#[cfg(feature = \"{name}\")] pub use {name} as public; "
            ));
        }
        let snapshot = snapshot_rust_api(&source(&source_text));
        assert!(
            !snapshot
                .module_aliases
                .iter()
                .any(|alias| alias.module_path == ["public"])
        );
        let actual: BTreeSet<Vec<String>> = snapshot
            .unknowns
            .iter()
            .filter(|unknown| {
                unknown.kind == RustApiUnknownKind::AmbiguousReexport
                    && unknown.evidence == "module alias public has conflicting origins"
            })
            .map(|unknown| unknown.cfg_guard.clone())
            .collect();
        assert_eq!(actual, pair_guards(&["a", "b", "c"]));
        semantic_projection(&snapshot)
    }

    #[test]
    fn rust_api_snapshot_r5_triple_module_conflict_is_permutation_complete() {
        let permutations = [
            ["a", "b", "c"],
            ["a", "c", "b"],
            ["b", "a", "c"],
            ["b", "c", "a"],
            ["c", "a", "b"],
            ["c", "b", "a"],
        ];
        let expected = triple_module_projection(&permutations[0]);
        for permutation in permutations.iter().skip(1) {
            assert_eq!(expected, triple_module_projection(permutation));
        }
    }

    fn symbolic_projection(order: &[&str]) -> SemanticProjection {
        let mut source_text = "mod a { pub type Ty = u8; pub fn value() {} } \
                               mod b { pub type Ty = u16; pub fn value() {} } \
                               mod c { pub type Ty = u32; pub fn value() {} } "
            .to_owned();
        for name in order {
            source_text.push_str(&format!(
                "#[cfg(feature = \"{name}\")] pub use {name}::Ty as Root; \
                 #[cfg(feature = \"{name}\")] pub use {name}::value as Root; "
            ));
        }
        let snapshot = snapshot_rust_api(&source(&source_text));
        for namespace in [RustNamespace::Type, RustNamespace::Value] {
            assert!(!snapshot.items.iter().any(|item| {
                item.key.external_name == "Root" && item.key.namespace == namespace
            }));
            let marker = format!("in {namespace:?}");
            let actual: BTreeSet<Vec<String>> = snapshot
                .unknowns
                .iter()
                .filter(|unknown| {
                    unknown.kind == RustApiUnknownKind::AmbiguousReexport
                        && unknown.evidence.contains("symbol alias Root")
                        && unknown.evidence.contains(&marker)
                })
                .map(|unknown| unknown.cfg_guard.clone())
                .collect();
            assert_eq!(actual, pair_guards(&["a", "b", "c"]));
        }
        semantic_projection(&snapshot)
    }

    #[test]
    fn rust_api_snapshot_r5_symbolic_conflicts_are_namespace_permutation_complete() {
        let permutations = [
            ["a", "b", "c"],
            ["a", "c", "b"],
            ["b", "a", "c"],
            ["b", "c", "a"],
            ["c", "a", "b"],
            ["c", "b", "a"],
        ];
        let expected = symbolic_projection(&permutations[0]);
        for permutation in permutations.iter().skip(1) {
            assert_eq!(expected, symbolic_projection(permutation));
        }
    }

    #[test]
    fn rust_api_snapshot_r5_four_origin_mixed_namespace_order_self_attack() {
        fn project(order: &[&str]) -> SemanticProjection {
            let mut source_text = "mod a { pub type Ty = u8; pub fn value() {} } \
                                   mod b { pub type Ty = u16; pub fn value() {} } \
                                   mod c { pub type Ty = u32; pub fn value() {} } \
                                   mod d { pub type Ty = u64; pub fn value() {} } \
                                   #[macro_export] macro_rules! Stable { () => {} } \
                                   pub use Stable as Root; "
                .to_owned();
            for name in order {
                source_text.push_str(&format!(
                    "#[cfg(feature = \"{name}\")] pub use {name}::Ty as Root; \
                     #[cfg(feature = \"{name}\")] pub use {name}::value as Root; "
                ));
            }
            let snapshot = snapshot_rust_api(&source(&source_text));
            assert!(snapshot.items.iter().any(|item| {
                item.key.external_name == "Root" && item.key.namespace == RustNamespace::Macro
            }));
            for namespace in [RustNamespace::Type, RustNamespace::Value] {
                assert!(!snapshot.items.iter().any(|item| {
                    item.key.external_name == "Root" && item.key.namespace == namespace
                }));
            }
            semantic_projection(&snapshot)
        }

        assert_eq!(
            project(&["a", "b", "c", "d"]),
            project(&["d", "b", "a", "c"])
        );
    }

    #[test]
    fn rust_api_snapshot_w0_complete_semantic_projection() {
        let unchanged = [
            "abi_unchanged",
            "backslash_reindent",
            "cfg_operand_reordered",
            "cfg_unchanged",
            "combining_mark_changed",
            "combining_mark_unchanged",
            "declaration_pairing_noop",
            "legacy_non_api_control",
            "literal_layout_only",
            "long_unchanged",
            "module_scope_private_unreachable",
            "module_scope_unchanged",
            "move_same_path",
            "reexport_preserved",
        ];
        let changed = [
            "abi_changed",
            "backslash_literal_changed",
            "cfg_guard_changed",
            "combining_mark_distinct",
            "declaration_pairing_changed",
            "function_added",
            "function_removed",
            "item_body_changed",
            "item_body_removed",
            "item_opener_removed",
            "literal_changed",
            "long_below_32_line_cut",
            "long_within_cap_changed",
            "module_scope_changed",
            "module_scope_unseen_opener",
            "move_relocated",
            "reexport_changed",
            "reexport_removed",
        ];
        assert_eq!(unchanged.len() + changed.len(), 32);
        for cell in unchanged {
            let base = fixture_snapshot(cell, "base");
            let head = fixture_snapshot(cell, "head");
            assert_eq!(
                semantic_projection(&base),
                semantic_projection(&head),
                "{cell}"
            );
        }
        for cell in changed {
            let base = fixture_snapshot(cell, "base");
            let head = fixture_snapshot(cell, "head");
            assert_ne!(
                semantic_projection(&base),
                semantic_projection(&head),
                "{cell}"
            );
        }
    }

    fn run_git(repo: &Path, args: &[&str]) -> Vec<u8> {
        let output = git_cmd().args(args).current_dir(repo).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn commit(repo: &Path, message: &str) -> String {
        run_git(repo, &["add", "-A"]);
        run_git(
            repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-m",
                message,
            ],
        );
        String::from_utf8(run_git(repo, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    }

    #[test]
    fn rust_api_snapshot_two_exact_oids_remain_independent() {
        let temp = tempfile::tempdir().unwrap();
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
        )
        .unwrap();
        fs::write(temp.path().join("src/lib.rs"), "pub fn value(x: u8) {}\n").unwrap();
        let first = commit(temp.path(), "first");
        fs::write(temp.path().join("src/lib.rs"), "pub fn value(x: u16) {}\n").unwrap();
        let second = commit(temp.path(), "second");
        let repo = Repository::open(temp.path()).unwrap();
        let first_snapshot = snapshot_rust_api(&GitTree::new(&repo, &first).unwrap());
        let second_snapshot = snapshot_rust_api(&GitTree::new(&repo, &second).unwrap());
        assert_ne!(first_snapshot.provenance, second_snapshot.provenance);
        assert_ne!(first_snapshot.items, second_snapshot.items);
        assert_eq!(
            first_snapshot.crates[0].provenance,
            first_snapshot.provenance
        );
        assert_eq!(
            second_snapshot.crates[0].provenance,
            second_snapshot.provenance
        );
    }
}
