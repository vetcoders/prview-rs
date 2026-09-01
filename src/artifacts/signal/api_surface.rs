//! Deterministic, revision-backed Rust API snapshots.
//!
//! This backend parses complete source files from [`RevisionFileSource`], walks
//! ordinary Rust modules, and computes source-level external reachability. It
//! deliberately does not compare revisions or emit artifacts/verdicts. Anything
//! that needs compiler, macro, feature-matrix, or prelude resolution is retained
//! as a typed unknown instead of being interpreted as an empty API.

use super::revision_source::{
    RevisionContentKind, RevisionEntry, RevisionEntryKind, RevisionEntryState, RevisionFileSource,
    RevisionProvenance, RevisionRead,
};
use quote::{ToTokens, quote};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::fold::Fold;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Attribute, Fields, Item, Meta, Token, UseTree, Visibility};
use unicode_normalization::UnicodeNormalization;

pub(crate) const NON_NEUTRALIZABLE_SYMLINK_ROOT: &str = "non-neutralizable-symlink-library-root:";

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
    WorkspaceDiscovery,
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
    PrivateTypeDependency,
    OpaqueReturnAutoTraits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustApiUnknown {
    pub kind: RustApiUnknownKind,
    pub crate_name: Option<String>,
    pub module_path: Vec<String>,
    pub source_path: String,
    pub cfg_guard: Vec<String>,
    pub evidence: String,
    pub resolution_exhausted: bool,
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

#[derive(Debug, Clone)]
struct SelfCrateAlias {
    crate_name: String,
    alias_path: Vec<String>,
    cfg_guard: Vec<String>,
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
struct PendingAssocTransform {
    crate_name: String,
    owner_module_path: Vec<String>,
    owner_name: String,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
}

#[derive(Debug, Clone)]
struct PendingTraitTransform {
    crate_name: String,
    owner_module_path: Vec<String>,
    owner_name: String,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExternalOwnerProjection {
    external_module_path: Vec<String>,
    external_name: String,
    cfg_guard: Vec<String>,
    alias_uncertain: bool,
    resolution_evidence: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingIncludeProof {
    origin: SymbolKey,
    cfg_guard: Vec<String>,
    source_path: String,
    item_evidence: String,
    macro_call: syn::Macro,
}

#[derive(Debug, Clone)]
struct PendingOpaqueReturnProof {
    origin: SymbolKey,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
}

#[derive(Debug, Clone)]
struct OpaqueReturnEvidence {
    cfg_guard: Vec<String>,
    evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PublicIncludeOrigin {
    module_path: Vec<String>,
    external_name: String,
    cfg_guard: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingTraitImpl {
    crate_name: String,
    declaring_module_path: Vec<String>,
    trait_module_path: Vec<String>,
    trait_name: String,
    owner_module_path: Vec<String>,
    owner_name: String,
    owner_path_resolved: bool,
    cfg_guard: Vec<String>,
    source_path: String,
    evidence: String,
    semantic_evidence: String,
}

type PrivateTypeKey = (String, Vec<String>, String);
type PrivateModuleAliasKey = (String, Vec<String>);
type GuardedPrivateModuleTarget = (Vec<String>, Vec<String>);
type GuardedPrivateTypeTarget = (PrivateTypeKey, Vec<String>);

#[derive(Debug, Clone)]
struct GuardedImplEvidence {
    raw_contract: String,
    semantic_evidence: String,
    cfg_guard: Vec<String>,
    declaring_module_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GuardedPrivateTypeKey {
    key: PrivateTypeKey,
    cfg_guard: Vec<String>,
}

#[derive(Debug)]
struct PrivateAliasResolution {
    states: BTreeSet<GuardedPrivateTypeKey>,
    terminals: BTreeSet<GuardedPrivateTypeKey>,
    exhausted: bool,
    exhaustion_digest: Option<String>,
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

#[derive(Debug, Clone, Default)]
struct DeriveNameAmbiguity {
    all_unqualified: bool,
    macro_use: bool,
    names: BTreeSet<String>,
}

impl DeriveNameAmbiguity {
    fn may_shadow(&self, name: &str) -> bool {
        self.all_unqualified || self.names.contains(name)
    }
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
    private_uses: Vec<UseEdge>,
    self_crate_aliases: Vec<SelfCrateAlias>,
    pending_assoc: Vec<PendingAssoc>,
    pending_assoc_transforms: Vec<PendingAssocTransform>,
    pending_trait_transforms: Vec<PendingTraitTransform>,
    pending_include_proofs: Vec<PendingIncludeProof>,
    pending_opaque_return_proofs: Vec<PendingOpaqueReturnProof>,
    pending_trait_impls: Vec<PendingTraitImpl>,
    module_proofs: Vec<ModuleProof>,
    all_module_aliases: Vec<RustModuleAlias>,
    completed_sources: BTreeMap<(String, String, Vec<String>, Vec<String>), bool>,
    active_sources: BTreeSet<(String, String)>,
    proc_macro_crates: BTreeSet<String>,
    rust_linkable_crates: BTreeSet<String>,
    native_artifact_crates: BTreeSet<String>,
    crate_editions: BTreeMap<String, String>,
    cfg_authority_digests: BTreeMap<String, String>,
    macro_implementation_digest: Option<String>,
    macro_invocation_implementation_digest: Option<String>,
    opaque_implementation_digest: Option<String>,
    macro_use_crates: BTreeSet<String>,
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
            private_uses: Vec::new(),
            self_crate_aliases: Vec::new(),
            pending_assoc: Vec::new(),
            pending_assoc_transforms: Vec::new(),
            pending_trait_transforms: Vec::new(),
            pending_include_proofs: Vec::new(),
            pending_opaque_return_proofs: Vec::new(),
            pending_trait_impls: Vec::new(),
            module_proofs: Vec::new(),
            all_module_aliases: Vec::new(),
            completed_sources: BTreeMap::new(),
            active_sources: BTreeSet::new(),
            proc_macro_crates: BTreeSet::new(),
            rust_linkable_crates: BTreeSet::new(),
            native_artifact_crates: BTreeSet::new(),
            crate_editions: BTreeMap::new(),
            cfg_authority_digests: BTreeMap::new(),
            macro_implementation_digest: None,
            macro_invocation_implementation_digest: None,
            opaque_implementation_digest: None,
            macro_use_crates: BTreeSet::new(),
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
        self.resolve_trait_transforms();
        self.resolve_trait_impls();
        self.resolve_inherent_items();
        self.resolve_include_proofs();
        self.resolve_opaque_return_proofs();
        self.materialize_public_modules();
        self.attach_public_reexport_origins();
        self.record_private_type_dependencies();
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

    /// A local type that is not itself externally reachable can still affect a
    /// public signature's layout, auto traits, inference and trait bounds. A
    /// source parser cannot prove the compiler-derived consequence, so retain
    /// a typed unknown whose evidence fingerprints the transitive declarations
    /// and local trait impls instead of claiming a confirmed breaking change.
    fn record_private_type_dependencies(&mut self) {
        use sha2::{Digest, Sha256};

        let mut public_origins: BTreeMap<PrivateTypeKey, Vec<Vec<String>>> = BTreeMap::new();
        for item in self
            .items
            .iter()
            .filter(|item| item.key.namespace == RustNamespace::Type)
        {
            public_origins
                .entry((
                    item.key.crate_name.clone(),
                    item.origin_module_path.clone(),
                    item.origin_name.clone(),
                ))
                .or_default()
                .push(item.cfg_guard.clone());
        }
        let mut declarations: BTreeMap<PrivateTypeKey, Vec<RustApiDeclaration>> = BTreeMap::new();
        for declaration in self.declarations.iter().filter(|declaration| {
            if declaration.key.namespace != RustNamespace::Type {
                return false;
            }
            let key = (
                declaration.key.crate_name.clone(),
                declaration.key.module_path.clone(),
                declaration.key.external_name.clone(),
            );
            // Possible overlap is not proof that the public origin covers this
            // declaration's whole cfg region. In particular, the conservative
            // guard solver cannot prove arbitrary `all(...)`/`not(all(...))`
            // complements; dropping the private declaration in that case loses
            // its layout/auto-trait uncertainty and can manufacture a green API
            // delta. Exclude it only when the declaration guard itself implies
            // an observed public lineage (all predicates of that public guard
            // are present here). Otherwise retain the typed unknown.
            !public_origins.get(&key).is_some_and(|guards| {
                guards.iter().any(|public_guard| {
                    guard_lineage_contains(&declaration.cfg_guard, public_guard)
                })
            })
        }) {
            declarations
                .entry((
                    declaration.key.crate_name.clone(),
                    declaration.key.module_path.clone(),
                    declaration.key.external_name.clone(),
                ))
                .or_default()
                .push(declaration.clone());
        }

        let (private_aliases, private_module_aliases) = private_alias_graph(
            &self.private_uses,
            &self.self_crate_aliases,
            &self.declarations,
        );

        let mut implementation_evidence: BTreeMap<PrivateTypeKey, Vec<GuardedImplEvidence>> =
            BTreeMap::new();
        let mut alias_resolution_unknowns = Vec::new();
        for pending in self
            .pending_trait_impls
            .iter()
            .filter(|pending| pending.owner_path_resolved)
        {
            let initial_key = (
                pending.crate_name.clone(),
                pending.owner_module_path.clone(),
                pending.owner_name.clone(),
            );
            let trait_resolution = resolve_private_type_alias_keys(
                (
                    pending.crate_name.clone(),
                    pending.trait_module_path.clone(),
                    pending.trait_name.clone(),
                ),
                &pending.cfg_guard,
                &private_aliases,
                &private_module_aliases,
            );
            let trait_targets = if trait_resolution.terminals.is_empty() {
                &trait_resolution.states
            } else {
                &trait_resolution.terminals
            };
            let owner_resolution = resolve_private_type_alias_keys(
                initial_key,
                &pending.cfg_guard,
                &private_aliases,
                &private_module_aliases,
            );
            if owner_resolution.exhausted {
                alias_resolution_unknowns.push(RustApiUnknown {
                    kind: RustApiUnknownKind::PrivateTypeDependency,
                    crate_name: Some(pending.crate_name.clone()),
                    module_path: pending.owner_module_path.clone(),
                    source_path: pending.source_path.clone(),
                    cfg_guard: pending.cfg_guard.clone(),
                    evidence: format!(
                        "private owner alias resolution exceeded its finite graph bound for {} ({})",
                        pending.owner_name,
                        owner_resolution
                            .exhaustion_digest
                            .as_deref()
                            .unwrap_or("missing-exhaustion-digest")
                    ),
                    resolution_exhausted: true,
                    provenance: self.provenance.clone(),
                });
            }
            for owner in owner_resolution.states {
                let has_overlapping_declaration =
                    declarations.get(&owner.key).is_some_and(|candidates| {
                        candidates.iter().any(|declaration| {
                            !guards_proven_disjoint(&declaration.cfg_guard, &owner.cfg_guard)
                        })
                    });
                if has_overlapping_declaration {
                    let owner_evidence = guarded_private_type_evidence("resolved-owner", &owner);
                    let mut recorded_pair = false;
                    for trait_state in trait_targets.iter().filter(|trait_state| {
                        !guards_proven_disjoint(&trait_state.cfg_guard, &owner.cfg_guard)
                    }) {
                        let effective_guard =
                            combined_guards(&trait_state.cfg_guard, &owner.cfg_guard);
                        let exhaustion_evidence = trait_resolution
                            .exhaustion_digest
                            .as_ref()
                            .map(|digest| format!("\ntrait-alias-resolution-exhausted:{digest}"))
                            .unwrap_or_default();
                        let resolved_impl_evidence = format!(
                            "{}\n{}\n{}\nimpl-effective-cfg:{effective_guard:?}{exhaustion_evidence}",
                            pending.semantic_evidence,
                            owner_evidence,
                            guarded_private_type_evidence("resolved-trait", trait_state),
                        );
                        implementation_evidence
                            .entry(owner.key.clone())
                            .or_default()
                            .push(GuardedImplEvidence {
                                raw_contract: pending.evidence.clone(),
                                semantic_evidence: resolved_impl_evidence,
                                cfg_guard: effective_guard,
                                declaring_module_path: pending.declaring_module_path.clone(),
                            });
                        recorded_pair = true;
                    }
                    if recorded_pair {
                        continue;
                    }
                    let Some(digest) = &trait_resolution.exhaustion_digest else {
                        continue;
                    };
                    implementation_evidence.entry(owner.key).or_default().push(
                        GuardedImplEvidence {
                            raw_contract: pending.evidence.clone(),
                            semantic_evidence: format!(
                                "{}\n{}\ntrait-alias-resolution-exhausted:{digest}",
                                pending.semantic_evidence, owner_evidence,
                            ),
                            cfg_guard: owner.cfg_guard,
                            declaring_module_path: pending.declaring_module_path.clone(),
                        },
                    );
                }
            }
        }

        let mut dependency_unknowns = Vec::new();
        for item in &self.items {
            let raw_roots = if let Ok(parsed) = syn::parse_str::<Item>(&item.contract) {
                LocalTypeDependencyCollector::collect_item_types(
                    &item.key.crate_name,
                    &item.origin_module_path,
                    &parsed,
                )
            } else if let Ok(parsed) = syn::parse_str::<syn::ItemImpl>(&item.contract) {
                LocalTypeDependencyCollector::collect_impl_types(
                    &item.key.crate_name,
                    &item.origin_module_path,
                    &parsed,
                )
            } else {
                BTreeSet::new()
            };
            if raw_roots.is_empty() {
                continue;
            }

            let initial_guard = combined_guards(&item.cfg_guard, &[]);
            let mut roots: BTreeSet<GuardedPrivateTypeKey> = raw_roots
                .into_iter()
                .map(|key| GuardedPrivateTypeKey {
                    key,
                    cfg_guard: initial_guard.clone(),
                })
                .collect();

            let mut visited = BTreeSet::new();
            let mut closure = Vec::new();
            let mut alias_resolution_exhausted = false;
            while let Some(state) = roots.pop_first() {
                if !visited.insert(state.clone()) {
                    continue;
                }
                let alias_resolution = resolve_private_type_alias_keys(
                    state.key.clone(),
                    &state.cfg_guard,
                    &private_aliases,
                    &private_module_aliases,
                );
                alias_resolution_exhausted |= alias_resolution.exhausted;
                if let Some(digest) = &alias_resolution.exhaustion_digest {
                    closure.push(format!("alias-resolution-exhausted:{digest}"));
                }
                for terminal in &alias_resolution.terminals {
                    if terminal == &state {
                        continue;
                    }
                    let has_local_declaration =
                        declarations.get(&terminal.key).is_some_and(|candidates| {
                            candidates.iter().any(|declaration| {
                                !guards_proven_disjoint(&declaration.cfg_guard, &terminal.cfg_guard)
                            })
                        });
                    let has_local_impl =
                        implementation_evidence
                            .get(&terminal.key)
                            .is_some_and(|impls| {
                                impls.iter().any(|implementation| {
                                    !guards_proven_disjoint(
                                        &implementation.cfg_guard,
                                        &terminal.cfg_guard,
                                    )
                                })
                            });
                    if !has_local_declaration && !has_local_impl {
                        closure.push(format!(
                            "alias-target:{}::{:?}::{}\neffective-cfg:{:?}",
                            terminal.key.0, terminal.key.1, terminal.key.2, terminal.cfg_guard
                        ));
                    }
                }
                roots.extend(
                    alias_resolution
                        .states
                        .into_iter()
                        .filter(|alias| alias != &state),
                );
                if let Some(impls) = implementation_evidence.get(&state.key) {
                    for implementation in impls.iter().filter(|implementation| {
                        !guards_proven_disjoint(&implementation.cfg_guard, &state.cfg_guard)
                    }) {
                        let effective_guard =
                            combined_guards(&state.cfg_guard, &implementation.cfg_guard);
                        closure.push(format!(
                            "impl-cfg:{effective_guard:?}\n{}",
                            implementation.semantic_evidence
                        ));
                        if let Ok(parsed) =
                            syn::parse_str::<syn::ItemImpl>(&implementation.raw_contract)
                        {
                            roots.extend(
                                LocalTypeDependencyCollector::collect_impl_types(
                                    &state.key.0,
                                    &implementation.declaring_module_path,
                                    &parsed,
                                )
                                .into_iter()
                                .map(|key| GuardedPrivateTypeKey {
                                    key,
                                    cfg_guard: effective_guard.clone(),
                                }),
                            );
                        }
                    }
                }
                for declaration in
                    declarations
                        .get(&state.key)
                        .into_iter()
                        .flatten()
                        .filter(|declaration| {
                            !guards_proven_disjoint(&declaration.cfg_guard, &state.cfg_guard)
                        })
                {
                    let effective_guard = combined_guards(&state.cfg_guard, &declaration.cfg_guard);
                    closure.push(format!(
                        "declaration:{}::{:?}::{}\neffective-cfg:{:?}\n{}",
                        state.key.0,
                        state.key.1,
                        state.key.2,
                        effective_guard,
                        declaration.contract
                    ));
                    if let Ok(parsed) = syn::parse_str::<Item>(&declaration.contract) {
                        roots.extend(
                            LocalTypeDependencyCollector::collect_item_types(
                                &state.key.0,
                                &state.key.1,
                                &parsed,
                            )
                            .into_iter()
                            .map(|key| GuardedPrivateTypeKey {
                                key,
                                cfg_guard: effective_guard.clone(),
                            }),
                        );
                    }
                }
            }
            if closure.is_empty() && !alias_resolution_exhausted {
                continue;
            }
            closure.sort();
            closure.dedup();
            let digest = format!("sha256:{:x}", Sha256::digest(closure.join("\n--\n")));
            dependency_unknowns.push(RustApiUnknown {
                kind: RustApiUnknownKind::PrivateTypeDependency,
                crate_name: Some(item.key.crate_name.clone()),
                module_path: item.key.module_path.clone(),
                source_path: item.source_path.clone(),
                cfg_guard: item.cfg_guard.clone(),
                evidence: if alias_resolution_exhausted {
                    format!(
                        "public {:?} {} has non-public local type semantics whose alias resolution exceeded its finite graph bound ({digest})",
                        item.kind, item.key.external_name
                    )
                } else {
                    format!(
                        "public {:?} {} depends on non-public local type semantics ({digest})",
                        item.kind, item.key.external_name
                    )
                },
                resolution_exhausted: alias_resolution_exhausted,
                provenance: self.provenance.clone(),
            });
        }
        self.unknowns.extend(alias_resolution_unknowns);
        self.unknowns.extend(dependency_unknowns);
    }

    fn record_inventory_path_unknowns(&mut self) {
        let paths: Vec<_> = self
            .inventory
            .values()
            .filter(|entry| {
                entry.kind == super::revision_source::RevisionEntryKind::Unsupported
                    && entry.path.contains(crate::git::NON_UTF8_GIT_PATH_PREFIX)
            })
            .map(|entry| crate::git::display_git_path(&entry.path))
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
        let inventory_dirs = live_inventory_directories(&self.inventory);
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
        let (allowed, workspace_ambiguity) =
            api_crate_manifests(self.source, &manifests, &inventory_dirs);
        let parsed_manifest_authorities = parsed_manifest_authorities(self.source, &manifests);
        let cfg_authority_digest = revision_cfg_authority_digest(&self.inventory);
        let macro_digest =
            proc_macro_implementation_digest(self.source, &self.inventory, &manifests, &allowed);
        let opaque_digest =
            opaque_implementation_digest(self.source, &self.inventory, &manifests, &allowed);
        self.macro_invocation_implementation_digest = Some(
            macro_invocation_implementation_digest(&macro_digest, &opaque_digest)
                .unwrap_or_else(|reason| format!("unresolved:{reason}")),
        );
        self.macro_implementation_digest =
            Some(macro_digest.unwrap_or_else(|reason| format!("unresolved:{reason}")));
        self.opaque_implementation_digest =
            Some(opaque_digest.unwrap_or_else(|reason| format!("unresolved:{reason}")));
        if let Some(evidence) = workspace_ambiguity {
            self.unknown(
                RustApiUnknownKind::WorkspaceDiscovery,
                None,
                &[],
                "<workspace-discovery>",
                evidence,
            );
        }
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
            let edition = match effective_library_edition(
                &manifest_path,
                &manifest,
                lib,
                &parsed_manifest_authorities,
            ) {
                Ok(edition) => edition,
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
                None => default_autolib(&manifest, &edition),
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
            let declared_proc_macro = match optional_bool(lib, "proc-macro") {
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
            let crate_types = match lib_crate_types(lib, declared_proc_macro) {
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
            let proc_macro =
                declared_proc_macro || crate_types.iter().any(|kind| kind.as_str() == "proc-macro");
            let crate_name = normalize_identifier(
                explicit_name
                    .map(str::to_owned)
                    .unwrap_or_else(|| package_name.replace('-', "_")),
            );
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
            let root_entry = self.inventory.get(&root_path);
            if !root_entry.is_some_and(is_live_regular_entry) {
                let implicit_root_is_absent = lib.is_none()
                    && root_entry.is_none_or(|entry| {
                        matches!(
                            entry.state,
                            RevisionEntryState::Deleted | RevisionEntryState::Renamed { .. }
                        )
                    });
                if implicit_root_is_absent {
                    continue;
                }
                let state = root_entry
                    .map(|entry| format!("{:?} {:?}", entry.kind, entry.state))
                    .unwrap_or_else(|| "missing inventory entry".to_owned());
                let evidence = if root_entry.is_some_and(is_live_symlink_entry) {
                    format!(
                        "{NON_NEUTRALIZABLE_SYMLINK_ROOT}{root_path}\nlibrary root declared by {manifest_path} is a tracked symlink whose compiler-visible source and module base cannot be proven from revision bytes: {state}"
                    )
                } else {
                    format!("library root declared by {manifest_path} is unavailable: {state}")
                };
                self.unknown(
                    RustApiUnknownKind::MissingLibRoot,
                    Some(&crate_name),
                    &[],
                    &root_path,
                    evidence,
                );
                continue;
            }
            let rust_linkable = crate_types
                .iter()
                .any(|kind| matches!(kind.as_str(), "lib" | "rlib" | "dylib"));
            let native_artifact = crate_types
                .iter()
                .any(|kind| matches!(kind.as_str(), "cdylib" | "staticlib" | "bin"));
            let package_links = match package.get("links") {
                Some(toml::Value::String(links)) => Some(links.as_str()),
                Some(_) => {
                    self.unknown(
                        RustApiUnknownKind::ManifestParse,
                        Some(&crate_name),
                        &[],
                        &manifest_path,
                        "package.links must be a string".to_owned(),
                    );
                    continue;
                }
                None => None,
            };
            let repo_config_cfg_authority =
                repository_cargo_cfg_authority(self.source, &self.inventory, package_links);
            let build_script_cfg_authority =
                match package_has_active_build_script(package, &manifest_dir, &self.inventory) {
                    Ok(active) => Ok(active),
                    Err(reason) => {
                        self.unknown(
                            RustApiUnknownKind::ManifestParse,
                            Some(&crate_name),
                            &[],
                            &manifest_path,
                            reason.clone(),
                        );
                        Err(reason)
                    }
                };
            if package_links.is_some() && matches!(build_script_cfg_authority, Ok(false)) {
                self.unknown(
                    RustApiUnknownKind::ManifestParse,
                    Some(&crate_name),
                    &[],
                    &manifest_path,
                    "package.links requires a live build script".to_owned(),
                );
                continue;
            }
            let cfg_authority = match build_script_cfg_authority {
                Ok(true) => Some(cfg_authority_digest.clone()),
                Ok(false) => match &repo_config_cfg_authority {
                    Ok(true) => Some(cfg_authority_digest.clone()),
                    Ok(false) => None,
                    Err(reason) => Some(format!("unresolved:cargo-config:{reason}")),
                },
                Err(reason) => Some(format!("unresolved:build-script:{reason}")),
            };
            if proc_macro {
                self.proc_macro_crates.insert(crate_name.clone());
            }
            if rust_linkable {
                self.rust_linkable_crates.insert(crate_name.clone());
            }
            if native_artifact {
                self.native_artifact_crates.insert(crate_name.clone());
            }
            self.crate_editions
                .insert(crate_name.clone(), edition.clone());
            if let Some(cfg_authority) = cfg_authority {
                self.cfg_authority_digests
                    .insert(crate_name.clone(), cfg_authority);
            }
            let base_dir = parent_repo_path(&root_path);
            if !self.load_module(
                &crate_name,
                Vec::new(),
                &root_path,
                &base_dir,
                ModuleVisibilityProof {
                    externally_reachable: rust_linkable,
                    declared_public: true,
                },
                Vec::new(),
            ) {
                self.proc_macro_crates.remove(&crate_name);
                self.rust_linkable_crates.remove(&crate_name);
                self.native_artifact_crates.remove(&crate_name);
                self.crate_editions.remove(&crate_name);
                self.cfg_authority_digests.remove(&crate_name);
                continue;
            }
            if !rust_linkable && !proc_macro {
                self.unknown(
                    RustApiUnknownKind::UnsupportedExternResolution,
                    Some(&crate_name),
                    &[],
                    &manifest_path,
                    format!(
                        "native-only library target is not a Rust dependency surface; crate_types={crate_types:?}"
                    ),
                );
            }
            if rust_linkable || proc_macro {
                self.items.push(RustApiItem {
                    key: RustApiItemKey {
                        crate_name: crate_name.clone(),
                        module_path: Vec::new(),
                        namespace: RustNamespace::Crate,
                        external_name: crate_name.clone(),
                    },
                    kind: RustApiItemKind::Crate,
                    contract: format!(
                        "library proc_macro={proc_macro}; crate_types={crate_types:?}"
                    ),
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
        let rust_linkable = self.rust_linkable_crates.contains(crate_name);
        let native_artifact = self.native_artifact_crates.contains(crate_name);
        let mut derive_name_ambiguity = derive_name_ambiguity_for_items(items);
        if self.macro_use_crates.contains(crate_name) {
            derive_name_ambiguity.all_unqualified = true;
        }
        if derive_name_ambiguity.macro_use {
            self.macro_use_crates.insert(crate_name.to_owned());
            derive_name_ambiguity.all_unqualified = true;
        }
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
            let custom_cfg_evidence = item_custom_cfg_evidence(item);
            if !custom_cfg_evidence.is_empty()
                && custom_cfg_can_affect_external_surface(
                    item,
                    self.proc_macro_crates.contains(crate_name),
                )
            {
                let authority = self
                    .cfg_authority_digests
                    .get(crate_name)
                    .map(String::as_str)
                    .unwrap_or("unresolved:no-revision-backed-authority");
                self.unknown_guarded(
                    RustApiUnknownKind::CfgPredicate,
                    Some(crate_name),
                    module_path,
                    source_path,
                    &cfg_guard,
                    format!(
                        "custom-cfg:{}\ncfg-authority-digest:{authority}",
                        custom_cfg_evidence.join(" && ")
                    ),
                );
            }
            // Derives preserve the annotated struct/enum/union and add impls.
            // Keep the input item's confirmed contract, while retaining typed
            // uncertainty only for non-builtin derive implementations.
            for evidence in custom_derive_evidence(item_attrs(item), &derive_name_ambiguity)
                .into_iter()
                .chain(conditional_custom_derive_evidence(
                    item_attrs(item),
                    &derive_name_ambiguity,
                ))
            {
                self.unknown_guarded(
                    RustApiUnknownKind::MacroGeneratedItems,
                    Some(crate_name),
                    module_path,
                    source_path,
                    &cfg_guard,
                    bind_additive_derive_evidence(
                        evidence,
                        item,
                        self.macro_implementation_digest.as_deref(),
                    ),
                );
            }
            for evidence in unresolved_builtin_default_evidence(item, &derive_name_ambiguity) {
                self.unknown_guarded(
                    RustApiUnknownKind::CfgPredicate,
                    Some(crate_name),
                    module_path,
                    source_path,
                    &cfg_guard,
                    evidence,
                );
            }
            // Binary ABI reachability is independent of Rust item reachability:
            // a mixed Rust-linkable/native target may export a symbol from a
            // private owner that `collect_impl` intentionally does not expose.
            if let Item::Impl(item_impl) = item {
                for member in &item_impl.items {
                    let syn::ImplItem::Fn(function) = member else {
                        continue;
                    };
                    let binary_exports = binary_export_attributes(&function.attrs);
                    let member_cfg = canonical_cfg(&function.attrs);
                    let member_guard = combined_guards(&cfg_guard, &member_cfg.guards);
                    for evidence in &member_cfg.errors {
                        self.unknown_guarded(
                            RustApiUnknownKind::CfgPredicate,
                            Some(crate_name),
                            module_path,
                            source_path,
                            &member_guard,
                            format!(
                                "native-export-associated-function:{}\n{evidence}",
                                normalize_identifier(function.sig.ident.to_string())
                            ),
                        );
                    }
                    let mut transform_boundaries =
                        transforming_attrs(&function.attrs)
                            .map(|attr| {
                                format!(
                                    "transform-boundary:attribute\n{}",
                                    canonical_tokens(attr.to_token_stream())
                                )
                            })
                            .chain(transforming_cfg_attrs(&function.attrs).into_iter().map(
                                |evidence| format!("transform-boundary:attribute\n{evidence}"),
                            ))
                            .collect::<Vec<_>>();
                    transform_boundaries.sort();
                    transform_boundaries.dedup();
                    if transform_boundaries.is_empty() {
                        continue;
                    }
                    let member_name = normalize_identifier(function.sig.ident.to_string());
                    let owner_contract = normalized_associated_contract(item_impl, member, false);
                    if binary_exports.is_empty() {
                        if !native_artifact {
                            continue;
                        }
                        for boundary in &transform_boundaries {
                            let mut evidence = bind_associated_native_transform_evidence(
                                boundary,
                                &member_name,
                                member,
                                &owner_contract,
                                self.macro_implementation_digest.as_deref(),
                            );
                            evidence.push_str("\nmacro-generated-native-export-potential");
                            self.unknown_guarded(
                                RustApiUnknownKind::MacroGeneratedItems,
                                Some(crate_name),
                                module_path,
                                source_path,
                                &member_guard,
                                evidence,
                            );
                        }
                        continue;
                    }
                    let export_guards = binary_exports
                        .into_iter()
                        .map(|(guard, _)| guard)
                        .collect::<BTreeSet<_>>();
                    for export_guard in export_guards {
                        let effective_guard = combined_guards(&member_guard, &export_guard);
                        for boundary in &transform_boundaries {
                            self.unknown_guarded(
                                RustApiUnknownKind::MacroGeneratedItems,
                                Some(crate_name),
                                module_path,
                                source_path,
                                &effective_guard,
                                bind_associated_native_transform_evidence(
                                    boundary,
                                    &member_name,
                                    member,
                                    &owner_contract,
                                    self.macro_implementation_digest.as_deref(),
                                ),
                            );
                        }
                    }
                }
                for member in &item_impl.items {
                    let syn::ImplItem::Macro(macro_item) = member else {
                        continue;
                    };
                    if !native_artifact {
                        continue;
                    }
                    let member_cfg = canonical_cfg(&macro_item.attrs);
                    let member_guard = combined_guards(&cfg_guard, &member_cfg.guards);
                    if !member_cfg.errors.is_empty() {
                        for evidence in member_cfg.errors {
                            self.unknown_guarded(
                                RustApiUnknownKind::CfgPredicate,
                                Some(crate_name),
                                module_path,
                                source_path,
                                &member_guard,
                                format!(
                                    "native-export-associated-macro:{}\n{evidence}",
                                    canonical_tokens(macro_item.mac.path.to_token_stream())
                                ),
                            );
                        }
                        continue;
                    }
                    let macro_name = macro_item
                        .mac
                        .path
                        .segments
                        .last()
                        .map(|segment| segment.ident.to_string())
                        .unwrap_or_default();
                    let kind = if matches!(
                        macro_name.as_str(),
                        "include" | "include_str" | "include_bytes"
                    ) {
                        RustApiUnknownKind::IncludeMacro
                    } else {
                        RustApiUnknownKind::MacroGeneratedItems
                    };
                    let input = canonical_tokens(macro_item.to_token_stream());
                    let evidence = if kind == RustApiUnknownKind::IncludeMacro {
                        format!(
                            "native-export-associated-macro\ninput:{input}\ninclude-kind:{macro_name}\nincluded-digest:{}",
                            self.include_digest(source_path, &macro_item.mac)
                        )
                    } else {
                        format!(
                            "native-export-associated-macro\ntransform-boundary:macro-invocation\ninput:{input}\nmacro-implementation-digest:{}\ndeclarative-implementation-digest:{}",
                            self.macro_implementation_digest
                                .as_deref()
                                .unwrap_or("unresolved:not-captured"),
                            self.macro_invocation_implementation_digest
                                .as_deref()
                                .unwrap_or("unresolved:not-captured"),
                        )
                    };
                    self.unknown_guarded(
                        kind,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &member_guard,
                        evidence,
                    );
                }
            }
            // Attribute macros can emit externally reachable items even when
            // their annotated input is private. Treat every syntactically
            // supported item as an opaque transform boundary; visibility is
            // not proof that expansion cannot alter the public surface.
            let transforming: Vec<_> = transforming_attrs(item_attrs(item)).collect();
            if !transforming.is_empty() {
                for attr in transforming {
                    self.unknown_guarded(
                        RustApiUnknownKind::MacroGeneratedItems,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &cfg_guard,
                        bind_transform_evidence(
                            format!(
                                "transform-boundary:attribute\n{}",
                                canonical_tokens(attr.to_token_stream())
                            ),
                            item,
                            self.macro_implementation_digest.as_deref(),
                        ),
                    );
                }
                continue;
            }
            let conditional_transforming = transforming_cfg_attrs(item_attrs(item));
            if !conditional_transforming.is_empty() {
                for evidence in conditional_transforming {
                    self.unknown_guarded(
                        RustApiUnknownKind::MacroGeneratedItems,
                        Some(crate_name),
                        module_path,
                        source_path,
                        &cfg_guard,
                        bind_transform_evidence(
                            format!("transform-boundary:attribute\n{evidence}"),
                            item,
                            self.macro_implementation_digest.as_deref(),
                        ),
                    );
                }
                continue;
            }
            for (name, public, export_guard, evidence) in
                binary_exports(item, !rust_linkable || !module_reachable)
            {
                let effective_guard = combined_guards(&cfg_guard, &export_guard);
                let visibility = if public { "public" } else { "private" };
                self.unknown_guarded(
                    RustApiUnknownKind::UnsupportedExternResolution,
                    Some(crate_name),
                    module_path,
                    source_path,
                    &effective_guard,
                    format!("public-owner:{name}\n{visibility}-binary-export:{evidence}"),
                );
            }
            if rust_linkable
                && let Item::Trait(item_trait) = item
                && is_public(&item_trait.vis)
            {
                for boundary in trait_transform_boundaries(item_trait) {
                    let mut evidence = bind_transform_evidence(
                        boundary,
                        item_trait,
                        self.macro_implementation_digest.as_deref(),
                    );
                    evidence.push_str("\ndeclarative-implementation-digest:");
                    evidence.push_str(
                        self.opaque_implementation_digest
                            .as_deref()
                            .unwrap_or("unresolved:not-captured"),
                    );
                    self.pending_trait_transforms.push(PendingTraitTransform {
                        crate_name: crate_name.to_owned(),
                        owner_module_path: module_path.to_vec(),
                        owner_name: normalize_identifier(item_trait.ident.to_string()),
                        cfg_guard: cfg_guard.clone(),
                        source_path: source_path.to_owned(),
                        evidence,
                    });
                }
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
                Item::Use(item_use) if rust_linkable => {
                    let mut leaves = Vec::new();
                    flatten_use_tree(&item_use.tree, Vec::new(), &mut leaves);
                    let edge = UseEdge {
                        crate_name: crate_name.to_owned(),
                        module_path: module_path.to_vec(),
                        module_reachable,
                        cfg_guard,
                        source_path: source_path.to_owned(),
                        leaves,
                    };
                    if is_public(&item_use.vis) {
                        self.uses.push(edge.clone());
                        if !module_reachable {
                            self.private_uses.push(edge);
                        }
                    } else {
                        self.private_uses.push(edge);
                    }
                }
                Item::ExternCrate(item_extern) if rust_linkable => {
                    if normalize_identifier(item_extern.ident.to_string()) == "self"
                        && let Some((_, alias)) = &item_extern.rename
                    {
                        let mut alias_path = module_path.to_vec();
                        alias_path.push(normalize_identifier(alias.to_string()));
                        self.self_crate_aliases.push(SelfCrateAlias {
                            crate_name: crate_name.to_owned(),
                            alias_path,
                            cfg_guard: cfg_guard.clone(),
                        });
                    }
                    if module_reachable && is_public(&item_extern.vis) {
                        self.unknown_guarded(
                            RustApiUnknownKind::UnsupportedExternResolution,
                            Some(crate_name),
                            module_path,
                            source_path,
                            &cfg_guard,
                            canonical_tokens(item_extern.to_token_stream()),
                        );
                    }
                }
                Item::Impl(item_impl) if rust_linkable => self.collect_impl(
                    crate_name,
                    module_path,
                    source_path,
                    module_reachable,
                    &cfg_guard,
                    item_impl,
                ),
                Item::ForeignMod(foreign) if rust_linkable => {
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
                        let invocation = match foreign_item {
                            syn::ForeignItem::Macro(value) if !macro_is_include(&value.mac) => {
                                Some(format!(
                                    "transform-boundary:declarative-macro\n{}",
                                    canonical_tokens(value.mac.to_token_stream())
                                ))
                            }
                            _ => None,
                        };
                        if !foreign_transformers.is_empty()
                            || !conditional.is_empty()
                            || invocation.is_some()
                        {
                            for evidence in foreign_transformers
                                .into_iter()
                                .map(|attr| {
                                    format!(
                                        "transform-boundary:attribute\n{}",
                                        canonical_tokens(attr.to_token_stream())
                                    )
                                })
                                .chain(conditional.into_iter().map(|evidence| {
                                    format!("transform-boundary:attribute\n{evidence}")
                                }))
                                .chain(invocation)
                            {
                                let mut evidence = bind_transform_evidence(
                                    evidence,
                                    foreign_item,
                                    self.macro_implementation_digest.as_deref(),
                                );
                                evidence.push_str("\ndeclarative-implementation-digest:");
                                evidence.push_str(
                                    self.opaque_implementation_digest
                                        .as_deref()
                                        .unwrap_or("unresolved:not-captured"),
                                );
                                self.unknown_guarded(
                                    RustApiUnknownKind::MacroGeneratedItems,
                                    Some(crate_name),
                                    module_path,
                                    source_path,
                                    &foreign_guard,
                                    evidence,
                                );
                            }
                            continue;
                        }
                        match foreign_item {
                            syn::ForeignItem::Fn(function) if is_public(&function.vis) => {
                                let name = normalize_identifier(function.sig.ident.to_string());
                                let contract = normalized_foreign_contract(foreign, foreign_item);
                                self.queue_include_proofs(
                                    SymbolKey {
                                        crate_name: crate_name.to_owned(),
                                        module_path: module_path.to_vec(),
                                        name: name.clone(),
                                        namespace: RustNamespace::Value,
                                    },
                                    source_path,
                                    &foreign_guard,
                                    contract.clone(),
                                    foreign_contract_include_macros(foreign_item),
                                );
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
                                self.queue_include_proofs(
                                    SymbolKey {
                                        crate_name: crate_name.to_owned(),
                                        module_path: module_path.to_vec(),
                                        name: name.clone(),
                                        namespace: RustNamespace::Value,
                                    },
                                    source_path,
                                    &foreign_guard,
                                    contract.clone(),
                                    foreign_contract_include_macros(foreign_item),
                                );
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
                            syn::ForeignItem::Macro(value) => {
                                for macro_call in foreign_contract_include_macros(foreign_item) {
                                    self.unknown_guarded(
                                        RustApiUnknownKind::IncludeMacro,
                                        Some(crate_name),
                                        module_path,
                                        source_path,
                                        &foreign_guard,
                                        format!(
                                            "foreign-item:{}\ninclude-kind:{}\ninclude:{}\nincluded-digest:{}",
                                            canonical_tokens(value.to_token_stream()),
                                            macro_call
                                                .path
                                                .segments
                                                .last()
                                                .map(|segment| segment.ident.to_string())
                                                .unwrap_or_else(|| "unknown".to_owned()),
                                            canonical_tokens(macro_call.to_token_stream()),
                                            self.include_digest(source_path, &macro_call),
                                        ),
                                    );
                                }
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
                    let macro_name = item_macro
                        .mac
                        .path
                        .segments
                        .last()
                        .map(|segment| segment.ident.to_string())
                        .unwrap_or_default();
                    let export_guards = macro_export_guards(&item_macro.attrs);
                    if let Some(macro_ident) = item_macro
                        .ident
                        .as_ref()
                        .filter(|_| rust_linkable && !export_guards.is_empty())
                    {
                        let name = normalize_identifier(macro_ident.to_string());
                        for export_guard in export_guards {
                            let effective_guard = combined_guards(&cfg_guard, &export_guard);
                            self.record_symbol(
                                crate_name,
                                &[],
                                source_path,
                                true,
                                &effective_guard,
                                name.clone(),
                                RustNamespace::Macro,
                                RustApiItemKind::Macro,
                                format!(
                                    "{}\ndefinition-edition:{}",
                                    normalized_macro_contract(item_macro),
                                    self.crate_editions
                                        .get(crate_name)
                                        .map(String::as_str)
                                        .unwrap_or("unresolved")
                                ),
                            );
                        }
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
                            evidence.push_str("\ninclude-kind:");
                            evidence.push_str(&macro_name);
                            evidence.push_str("\nincluded-digest:");
                            evidence.push_str(&self.include_digest(source_path, &item_macro.mac));
                        } else {
                            evidence = format!(
                                "transform-boundary:macro-invocation\ninput:{evidence}\nmacro-implementation-digest:{}\ndeclarative-implementation-digest:{}",
                                self.macro_implementation_digest
                                    .as_deref()
                                    .unwrap_or("unresolved:not-captured"),
                                self.macro_invocation_implementation_digest
                                    .as_deref()
                                    .unwrap_or("unresolved:not-captured"),
                            );
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
                _ if rust_linkable => {
                    if transforming_attrs(item_attrs(item)).next().is_none()
                        && transforming_cfg_attrs(item_attrs(item)).is_empty()
                        && let Some((name, namespace, kind, contract, declared_public)) =
                            ordinary_item_contract(item, &derive_name_ambiguity)
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
                    if let Some((name, namespace, kind, contract)) =
                        public_item_contract(item, &derive_name_ambiguity)
                    {
                        let origin = SymbolKey {
                            crate_name: crate_name.to_owned(),
                            module_path: module_path.to_vec(),
                            name: name.clone(),
                            namespace,
                        };
                        self.queue_include_proofs(
                            origin.clone(),
                            source_path,
                            &cfg_guard,
                            contract.clone(),
                            public_contract_include_macros(item),
                        );
                        self.queue_opaque_return_proofs(
                            origin,
                            source_path,
                            &cfg_guard,
                            public_item_opaque_return_evidence(item),
                        );
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
                                normalized_confirmed_contract_without_item_name(
                                    item.clone(),
                                    &derive_name_ambiguity,
                                ),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn queue_include_proofs(
        &mut self,
        origin: SymbolKey,
        source_path: &str,
        cfg_guard: &[String],
        item_evidence: String,
        macro_calls: Vec<syn::Macro>,
    ) {
        self.pending_include_proofs
            .extend(
                macro_calls
                    .into_iter()
                    .map(|macro_call| PendingIncludeProof {
                        origin: origin.clone(),
                        cfg_guard: cfg_guard.to_vec(),
                        source_path: source_path.to_owned(),
                        item_evidence: item_evidence.clone(),
                        macro_call,
                    }),
            );
    }

    fn queue_opaque_return_proofs(
        &mut self,
        origin: SymbolKey,
        source_path: &str,
        cfg_guard: &[String],
        evidence: Vec<OpaqueReturnEvidence>,
    ) {
        let implementation_digest = self
            .opaque_implementation_digest
            .as_deref()
            .unwrap_or("unresolved:not-captured");
        self.pending_opaque_return_proofs
            .extend(
                evidence
                    .into_iter()
                    .map(|evidence| PendingOpaqueReturnProof {
                        origin: origin.clone(),
                        cfg_guard: combined_guards(cfg_guard, &evidence.cfg_guard),
                        source_path: source_path.to_owned(),
                        evidence: format!(
                            "{}\nopaque-implementation-digest:{implementation_digest}",
                            evidence.evidence
                        ),
                    }),
            );
    }

    /// Materialize include proofs only when their declaration reached the
    /// external API, directly or through a public reexport. A `pub` item inside
    /// an unreachable private module is not caller-observable by itself and
    /// must not degrade the whole crate snapshot.
    fn resolve_include_proofs(&mut self) {
        let mut public_origins: BTreeMap<SymbolKey, Vec<PublicIncludeOrigin>> = BTreeMap::new();
        for item in &self.items {
            public_origins
                .entry(SymbolKey {
                    crate_name: item.key.crate_name.clone(),
                    module_path: item.origin_module_path.clone(),
                    name: item.origin_name.clone(),
                    namespace: item.key.namespace,
                })
                .or_default()
                .push(PublicIncludeOrigin {
                    module_path: item.key.module_path.clone(),
                    external_name: item.key.external_name.clone(),
                    cfg_guard: item.cfg_guard.clone(),
                });
        }
        for origins in public_origins.values_mut() {
            origins.sort();
            origins.dedup();
        }

        for proof in self.pending_include_proofs.clone() {
            let Some(resolved_origins) = public_origins.get(&proof.origin) else {
                continue;
            };
            for resolved_origin in resolved_origins {
                if guards_proven_disjoint(&proof.cfg_guard, &resolved_origin.cfg_guard) {
                    continue;
                }
                let external_path = if resolved_origin.module_path.is_empty() {
                    resolved_origin.external_name.clone()
                } else {
                    format!(
                        "{}::{}",
                        resolved_origin.module_path.join("::"),
                        resolved_origin.external_name
                    )
                };
                let include_kind = proof
                    .macro_call
                    .path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                let evidence = format!(
                    "origin:{:?}:{}\nitem:{}\ninclude-kind:{}\ninclude:{}\nincluded-digest:{}",
                    proof.origin.namespace,
                    external_path,
                    proof.item_evidence,
                    include_kind,
                    canonical_tokens(proof.macro_call.to_token_stream()),
                    self.include_digest(&proof.source_path, &proof.macro_call),
                );
                let mut effective_guard = proof.cfg_guard.clone();
                effective_guard.extend(resolved_origin.cfg_guard.iter().cloned());
                effective_guard.sort();
                effective_guard.dedup();
                self.unknown_guarded(
                    RustApiUnknownKind::IncludeMacro,
                    Some(&proof.origin.crate_name),
                    &resolved_origin.module_path,
                    &proof.source_path,
                    &effective_guard,
                    evidence,
                );
            }
        }
    }

    /// Materialize body-dependent opaque-return uncertainty only for items that
    /// actually reached the external API, directly or through a reexport.
    fn resolve_opaque_return_proofs(&mut self) {
        let mut public_origins: BTreeMap<SymbolKey, Vec<PublicIncludeOrigin>> = BTreeMap::new();
        for item in &self.items {
            public_origins
                .entry(SymbolKey {
                    crate_name: item.key.crate_name.clone(),
                    module_path: item.origin_module_path.clone(),
                    name: item.origin_name.clone(),
                    namespace: item.key.namespace,
                })
                .or_default()
                .push(PublicIncludeOrigin {
                    module_path: item.key.module_path.clone(),
                    external_name: item.key.external_name.clone(),
                    cfg_guard: item.cfg_guard.clone(),
                });
        }
        for origins in public_origins.values_mut() {
            origins.sort();
            origins.dedup();
        }

        for proof in self.pending_opaque_return_proofs.clone() {
            let Some(resolved_origins) = public_origins.get(&proof.origin) else {
                continue;
            };
            for resolved_origin in resolved_origins {
                if guards_proven_disjoint(&proof.cfg_guard, &resolved_origin.cfg_guard) {
                    continue;
                }
                let external_path = if resolved_origin.module_path.is_empty() {
                    resolved_origin.external_name.clone()
                } else {
                    format!(
                        "{}::{}",
                        resolved_origin.module_path.join("::"),
                        resolved_origin.external_name
                    )
                };
                let mut effective_guard = proof.cfg_guard.clone();
                effective_guard.extend(resolved_origin.cfg_guard.iter().cloned());
                effective_guard.sort();
                effective_guard.dedup();
                self.unknown_guarded(
                    RustApiUnknownKind::OpaqueReturnAutoTraits,
                    Some(&proof.origin.crate_name),
                    &resolved_origin.module_path,
                    &proof.source_path,
                    &effective_guard,
                    format!(
                        "origin:{:?}:{external_path}\n{}",
                        proof.origin.namespace, proof.evidence
                    ),
                );
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
            let Some((trait_module_path, trait_name)) = resolve_impl_owner(module_path, trait_path)
            else {
                return;
            };
            let (owner_module_path, owner_name, owner_path_resolved) =
                match resolve_impl_self_owner(module_path, item_impl.self_ty.as_ref()) {
                    Some((module_path, name)) => (module_path, name, true),
                    None => (
                        Vec::new(),
                        canonical_tokens(item_impl.self_ty.to_token_stream()),
                        false,
                    ),
                };
            let mut evidence = normalized_trait_impl_contract(item_impl);
            let mut semantic_evidence = normalized_trait_impl_semantic_contract(item_impl);
            let mut include_evidence = trait_impl_contract_include_macros(item_impl)
                .into_iter()
                .map(|macro_call| {
                    format!(
                        "\ninclude-kind:{}\ninclude:{}\nincluded-digest:{}",
                        macro_call
                            .path
                            .segments
                            .last()
                            .map(|segment| segment.ident.to_string())
                            .unwrap_or_else(|| "unknown".to_owned()),
                        canonical_tokens(macro_call.to_token_stream()),
                        self.include_digest(source_path, &macro_call),
                    )
                })
                .collect::<Vec<_>>();
            include_evidence.sort();
            include_evidence.dedup();
            for include_evidence in include_evidence {
                evidence.push_str(&include_evidence);
                semantic_evidence.push_str(&include_evidence);
            }
            let mut opaque_evidence = trait_impl_opaque_return_evidence(item_impl);
            opaque_evidence.sort();
            opaque_evidence.dedup();
            for opaque_evidence in opaque_evidence {
                let opaque_evidence = format!(
                    "{opaque_evidence}\nopaque-implementation-digest:{}",
                    self.opaque_implementation_digest
                        .as_deref()
                        .unwrap_or("unresolved:not-captured")
                );
                evidence.push('\n');
                evidence.push_str(&opaque_evidence);
                semantic_evidence.push('\n');
                semantic_evidence.push_str(&opaque_evidence);
            }
            for boundary in impl_transform_boundaries(item_impl) {
                let mut boundary = bind_transform_evidence(
                    boundary,
                    item_impl,
                    self.macro_implementation_digest.as_deref(),
                );
                boundary.push_str("\ndeclarative-implementation-digest:");
                boundary.push_str(
                    self.opaque_implementation_digest
                        .as_deref()
                        .unwrap_or("unresolved:not-captured"),
                );
                evidence.push('\n');
                evidence.push_str(&boundary);
                semantic_evidence.push('\n');
                semantic_evidence.push_str(&boundary);
            }
            // Trait impls are globally usable when the trait and owner are
            // public, even if the impl lives in a private helper module.
            self.pending_trait_impls.push(PendingTraitImpl {
                crate_name: crate_name.to_owned(),
                declaring_module_path: module_path.to_vec(),
                trait_module_path,
                trait_name,
                owner_module_path,
                owner_name,
                owner_path_resolved,
                cfg_guard: cfg_guard.to_vec(),
                source_path: source_path.to_owned(),
                evidence,
                semantic_evidence,
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
        let inherent_impl_contract = normalized_trait_impl_contract(item_impl);
        for item in &item_impl.items {
            let syn::ImplItem::Macro(macro_item) = item else {
                continue;
            };
            let macro_calls = associated_contract_include_macros(item);
            let mut associated_cfg = cfg_guard.to_vec();
            let item_cfg = canonical_cfg(&macro_item.attrs);
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
            if macro_calls.is_empty() {
                let evidence = format!(
                    "transform-boundary:declarative-macro\n{}",
                    canonical_tokens(macro_item.to_token_stream())
                );
                let mut bound_evidence = bind_transform_evidence(
                    evidence,
                    macro_item,
                    self.macro_implementation_digest.as_deref(),
                );
                bound_evidence.push_str("\ndeclarative-implementation-digest:");
                bound_evidence.push_str(
                    self.opaque_implementation_digest
                        .as_deref()
                        .unwrap_or("unresolved:not-captured"),
                );
                bound_evidence.push_str("\ninherent-impl-contract:");
                bound_evidence.push_str(&inherent_impl_contract);
                self.pending_assoc_transforms.push(PendingAssocTransform {
                    crate_name: crate_name.to_owned(),
                    owner_module_path: owner_module_path.clone(),
                    owner_name: owner_name.clone(),
                    cfg_guard: associated_cfg,
                    source_path: source_path.to_owned(),
                    evidence: bound_evidence,
                });
                continue;
            }
            self.queue_include_proofs(
                SymbolKey {
                    crate_name: crate_name.to_owned(),
                    module_path: owner_module_path.clone(),
                    name: owner_name.clone(),
                    namespace: RustNamespace::Type,
                },
                source_path,
                &associated_cfg,
                format!(
                    "impl-owner:__prview_owner\n{}",
                    canonical_tokens(macro_item.to_token_stream())
                ),
                macro_calls,
            );
        }
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
                    .map(|attr| {
                        format!(
                            "transform-boundary:attribute\n{}",
                            canonical_tokens(attr.to_token_stream())
                        )
                    })
                    .chain(
                        conditional
                            .into_iter()
                            .map(|evidence| format!("transform-boundary:attribute\n{evidence}")),
                    )
                {
                    let mut evidence = bind_transform_evidence(
                        evidence,
                        item,
                        self.macro_implementation_digest.as_deref(),
                    );
                    evidence.push_str("\ninherent-impl-contract:");
                    evidence.push_str(&inherent_impl_contract);
                    self.pending_assoc_transforms.push(PendingAssocTransform {
                        crate_name: crate_name.to_owned(),
                        owner_module_path: owner_module_path.clone(),
                        owner_name: owner_name.clone(),
                        cfg_guard: associated_cfg.clone(),
                        source_path: source_path.to_owned(),
                        evidence,
                    });
                }
                continue;
            }
            if !public {
                continue;
            }
            let mut evidence = normalized_associated_contract(item_impl, item, false);
            let contract = normalized_associated_contract(item_impl, item, true);
            let origin_name = format!("{}::{name}", owner_name);
            let include_calls = associated_contract_include_macros(item);
            for macro_call in &include_calls {
                evidence.push_str(&format!(
                    "\ninclude:{}\nincluded-digest:{}",
                    canonical_tokens(macro_call.to_token_stream()),
                    self.include_digest(source_path, macro_call)
                ));
            }
            let opaque_evidence = associated_opaque_return_evidence(item);
            for opaque in &opaque_evidence {
                evidence.push_str(&format!(
                    "\n{opaque}\nopaque-implementation-digest:{}",
                    self.opaque_implementation_digest
                        .as_deref()
                        .unwrap_or("unresolved:not-captured")
                ));
            }
            self.queue_include_proofs(
                SymbolKey {
                    crate_name: crate_name.to_owned(),
                    module_path: owner_module_path.clone(),
                    name: origin_name,
                    namespace,
                },
                source_path,
                &associated_cfg,
                contract.clone(),
                include_calls,
            );
            self.queue_opaque_return_proofs(
                SymbolKey {
                    crate_name: crate_name.to_owned(),
                    module_path: owner_module_path.clone(),
                    name: format!("{}::{name}", owner_name),
                    namespace,
                },
                source_path,
                &associated_cfg,
                opaque_evidence
                    .into_iter()
                    .map(|evidence| OpaqueReturnEvidence {
                        cfg_guard: Vec::new(),
                        evidence,
                    })
                    .collect(),
            );
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

    fn resolve_trait_transforms(&mut self) {
        for transform in self.pending_trait_transforms.clone() {
            let external_origins = self
                .items
                .iter()
                .filter(|item| {
                    item.kind == RustApiItemKind::Trait
                        && item.key.crate_name == transform.crate_name
                        && item.origin_module_path == transform.owner_module_path
                        && item.origin_name == transform.owner_name
                        && !guards_proven_disjoint(&item.cfg_guard, &transform.cfg_guard)
                })
                .map(|item| {
                    (
                        item.key.module_path.clone(),
                        item.key.external_name.clone(),
                        item.cfg_guard.clone(),
                    )
                })
                .collect::<Vec<_>>();
            for (external_module_path, external_name, external_guard) in external_origins {
                let guards = combined_guards(&external_guard, &transform.cfg_guard);
                self.unknown_guarded(
                    RustApiUnknownKind::MacroGeneratedItems,
                    Some(&transform.crate_name),
                    &external_module_path,
                    &transform.source_path,
                    &guards,
                    format!(
                        "public-owner:{external_name}\ntrait-owner:{}\n{}",
                        transform.owner_name, transform.evidence
                    ),
                );
            }
        }
    }

    fn resolve_trait_impls(&mut self) {
        let (private_aliases, private_module_aliases) = private_alias_graph(
            &self.private_uses,
            &self.self_crate_aliases,
            &self.declarations,
        );
        for pending in self.pending_trait_impls.clone() {
            let initial_trait_key = (
                pending.crate_name.clone(),
                pending.trait_module_path.clone(),
                pending.trait_name.clone(),
            );
            let trait_resolution = resolve_private_type_alias_keys(
                initial_trait_key.clone(),
                &pending.cfg_guard,
                &private_aliases,
                &private_module_aliases,
            );
            let mut trait_candidates = Vec::new();
            for state in &trait_resolution.states {
                for item in self.items.iter().filter(|item| {
                    item.kind == RustApiItemKind::Trait
                        && item.key.crate_name == pending.crate_name
                        && state.key
                            == (
                                pending.crate_name.clone(),
                                item.origin_module_path.clone(),
                                item.origin_name.clone(),
                            )
                        && !guards_proven_disjoint(&item.cfg_guard, &state.cfg_guard)
                }) {
                    trait_candidates.push(GuardedPrivateTypeKey {
                        key: state.key.clone(),
                        cfg_guard: combined_guards(&state.cfg_guard, &item.cfg_guard),
                    });
                }

                let is_initial_alias =
                    trait_resolution.states.len() > 1 && state.key == initial_trait_key;
                let has_overlapping_local_declaration =
                    self.declarations.iter().any(|declaration| {
                        declaration.kind == RustApiItemKind::Trait
                            && declaration.key.crate_name == state.key.0
                            && declaration.key.module_path == state.key.1
                            && declaration.key.external_name == state.key.2
                            && !guards_proven_disjoint(&declaration.cfg_guard, &state.cfg_guard)
                    });
                if !is_initial_alias
                    && !has_overlapping_local_declaration
                    && impl_path_is_external_public(&pending.crate_name, &state.key.1)
                {
                    trait_candidates.push(state.clone());
                }
            }
            let owner_resolution = pending.owner_path_resolved.then(|| {
                resolve_private_type_alias_keys(
                    (
                        pending.crate_name.clone(),
                        pending.owner_module_path.clone(),
                        pending.owner_name.clone(),
                    ),
                    &pending.cfg_guard,
                    &private_aliases,
                    &private_module_aliases,
                )
            });
            let initial_owner_key = (
                pending.crate_name.clone(),
                pending.owner_module_path.clone(),
                pending.owner_name.clone(),
            );
            let mut owner_candidates = Vec::new();
            if let Some(resolution) = &owner_resolution {
                for state in &resolution.states {
                    for item in self.items.iter().filter(|item| {
                        item.key.namespace == RustNamespace::Type
                            && item.key.crate_name == pending.crate_name
                            && state.key
                                == (
                                    pending.crate_name.clone(),
                                    item.origin_module_path.clone(),
                                    item.origin_name.clone(),
                                )
                            && !guards_proven_disjoint(&item.cfg_guard, &state.cfg_guard)
                    }) {
                        owner_candidates.push(GuardedPrivateTypeKey {
                            key: state.key.clone(),
                            cfg_guard: combined_guards(&state.cfg_guard, &item.cfg_guard),
                        });
                    }

                    let is_initial_alias =
                        resolution.states.len() > 1 && state.key == initial_owner_key;
                    let has_overlapping_local_declaration =
                        self.declarations.iter().any(|declaration| {
                            declaration.key.namespace == RustNamespace::Type
                                && declaration.key.crate_name == state.key.0
                                && declaration.key.module_path == state.key.1
                                && declaration.key.external_name == state.key.2
                                && !guards_proven_disjoint(&declaration.cfg_guard, &state.cfg_guard)
                        });
                    if !is_initial_alias
                        && !has_overlapping_local_declaration
                        && impl_path_is_external_public(&pending.crate_name, &state.key.1)
                    {
                        owner_candidates.push(state.clone());
                    }
                }
            } else {
                // Non-path owners such as `&Public` cannot be resolved to one
                // nominal key, but remain observable whenever the trait is.
                owner_candidates.push(GuardedPrivateTypeKey {
                    key: initial_owner_key,
                    cfg_guard: combined_guards(&pending.cfg_guard, &[]),
                });
            }
            trait_candidates.sort();
            trait_candidates.dedup();
            owner_candidates.sort();
            owner_candidates.dedup();
            let mut observable_pairs = trait_candidates
                .iter()
                .flat_map(|trait_state| {
                    owner_candidates
                        .iter()
                        .filter(|owner_state| {
                            !guards_proven_disjoint(&trait_state.cfg_guard, &owner_state.cfg_guard)
                        })
                        .map(|owner_state| {
                            format!(
                                "{}\n{}\nimpl-cfg:{:?}",
                                guarded_private_type_evidence("resolved-trait", trait_state),
                                guarded_private_type_evidence("resolved-owner", owner_state),
                                combined_guards(&trait_state.cfg_guard, &owner_state.cfg_guard,)
                            )
                        })
                })
                .collect::<Vec<_>>();
            observable_pairs.sort();
            observable_pairs.dedup();
            let has_observable_region = !observable_pairs.is_empty();
            let trait_resolution_exhausted = trait_resolution.exhausted;
            let owner_resolution_exhausted = owner_resolution
                .as_ref()
                .is_some_and(|resolution| resolution.exhausted);
            if trait_resolution_exhausted || owner_resolution_exhausted || has_observable_region {
                self.unknown_guarded_with_resolution_state(
                    RustApiUnknownKind::TraitImplResolution,
                    Some(&pending.crate_name),
                    &pending.declaring_module_path,
                    &pending.source_path,
                    &pending.cfg_guard,
                    trait_resolution_exhausted || owner_resolution_exhausted,
                    if trait_resolution_exhausted || owner_resolution_exhausted {
                        format!(
                            "private trait/owner alias resolution exceeded its finite graph bound: {}\ntrait-resolution:{}\nowner-resolution:{}",
                            pending.semantic_evidence,
                            trait_resolution
                                .exhaustion_digest
                                .as_deref()
                                .unwrap_or("complete"),
                            owner_resolution
                                .as_ref()
                                .and_then(|resolution| resolution.exhaustion_digest.as_deref())
                                .unwrap_or("complete")
                        )
                    } else {
                        format!(
                            "{}\nresolved-observable-impls:\n{}",
                            pending.semantic_evidence,
                            observable_pairs.join("\n--\n")
                        )
                    },
                );
            }
        }
    }

    fn resolve_inherent_items(&mut self) {
        let (private_aliases, private_module_aliases) = private_alias_graph(
            &self.private_uses,
            &self.self_crate_aliases,
            &self.declarations,
        );
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
                })
                .flat_map(|item| {
                    external_owner_projections(
                        item,
                        &assoc.owner_module_path,
                        &assoc.owner_name,
                        &private_aliases,
                        &private_module_aliases,
                    )
                    .into_iter()
                    .filter(|projection| {
                        !guards_proven_disjoint(&projection.cfg_guard, &assoc.cfg_guard)
                    })
                })
                .collect();
            for external_type in external_types {
                let mut guards = external_type.cfg_guard;
                guards.extend(assoc.cfg_guard.clone());
                guards.sort();
                guards.dedup();
                if external_type.alias_uncertain {
                    self.unknown_guarded(
                        RustApiUnknownKind::UnresolvedInherentOwner,
                        Some(&assoc.crate_name),
                        &external_type.external_module_path,
                        &assoc.source_path,
                        &guards,
                        format!(
                            "public-type-alias:{}\ninherent-owner:{}\nalias-resolution:{}\n{}",
                            external_type.external_name,
                            assoc.owner_name,
                            external_type
                                .resolution_evidence
                                .as_deref()
                                .unwrap_or("complete"),
                            assoc.evidence
                        ),
                    );
                    continue;
                }
                self.items.push(RustApiItem {
                    key: RustApiItemKey {
                        crate_name: assoc.crate_name.clone(),
                        module_path: external_type.external_module_path,
                        namespace: assoc.namespace,
                        external_name: format!("{}::{}", external_type.external_name, assoc.name),
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
        for transform in self.pending_assoc_transforms.clone() {
            let external_types = self
                .items
                .iter()
                .filter(|item| {
                    item.key.namespace == RustNamespace::Type
                        && item.key.crate_name == transform.crate_name
                })
                .flat_map(|item| {
                    external_owner_projections(
                        item,
                        &transform.owner_module_path,
                        &transform.owner_name,
                        &private_aliases,
                        &private_module_aliases,
                    )
                    .into_iter()
                    .filter(|projection| {
                        !guards_proven_disjoint(&projection.cfg_guard, &transform.cfg_guard)
                    })
                })
                .collect::<Vec<_>>();
            for external_type in external_types {
                let mut guards = external_type.cfg_guard;
                guards.extend(transform.cfg_guard.clone());
                guards.sort();
                guards.dedup();
                self.unknown_guarded(
                    RustApiUnknownKind::MacroGeneratedItems,
                    Some(&transform.crate_name),
                    &external_type.external_module_path,
                    &transform.source_path,
                    &guards,
                    format!(
                        "public-owner:{}\ninherent-owner:{}\nalias-resolution:{}\n{}",
                        external_type.external_name,
                        transform.owner_name,
                        external_type
                            .resolution_evidence
                            .as_deref()
                            .unwrap_or("complete"),
                        transform.evidence
                    ),
                );
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
        self.unknown_guarded_with_resolution_state(
            kind,
            crate_name,
            module_path,
            source_path,
            cfg_guard,
            false,
            evidence,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn unknown_guarded_with_resolution_state(
        &mut self,
        kind: RustApiUnknownKind,
        crate_name: Option<&str>,
        module_path: &[String],
        source_path: &str,
        cfg_guard: &[String],
        resolution_exhausted: bool,
        evidence: String,
    ) {
        self.unknowns.push(RustApiUnknown {
            kind,
            crate_name: crate_name.map(str::to_owned),
            module_path: module_path.to_vec(),
            source_path: source_path.to_owned(),
            cfg_guard: cfg_guard.to_vec(),
            evidence,
            resolution_exhausted,
            provenance: self.provenance.clone(),
        });
    }
}

fn public_item_contract(
    item: &Item,
    derive_ambiguity: &DeriveNameAmbiguity,
) -> Option<(String, RustNamespace, RustApiItemKind, String)> {
    let (name, namespace, kind, contract, public) = ordinary_item_contract(item, derive_ambiguity)?;
    public.then_some((name, namespace, kind, contract))
}

fn ordinary_item_contract(
    item: &Item,
    derive_ambiguity: &DeriveNameAmbiguity,
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
        normalized_confirmed_contract_without_item_name(item.clone(), derive_ambiguity),
        public,
    ))
}

fn normalized_contract(mut item: Item) -> String {
    normalize_item_attrs(&mut item);
    let free_identifiers = FreeIdentifierCollector::item(&item);
    match &mut item {
        Item::Fn(function) => {
            *function.block = syn::parse_quote!({});
            alpha_normalize_signature(&mut function.sig);
            trim_signature_punctuation(&mut function.sig);
        }
        Item::Struct(value) => {
            let field_order_sensitive = has_struct_field_order_repr(&value.attrs);
            let mut normalizer = SignatureAlphaNormalizer::with_occupied(free_identifiers.clone());
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            for field in &mut value.fields {
                field.ty = normalizer.fold_type(field.ty.clone());
            }
            normalizer.pop_scope();
            filter_private_fields(&mut value.fields, field_order_sensitive);
            trim_fields_punctuation(&mut value.fields);
            trim_generics_punctuation(&mut value.generics);
        }
        Item::Union(value) => {
            let mut fields = Fields::Named(value.fields.clone());
            let mut normalizer = SignatureAlphaNormalizer::with_occupied(free_identifiers.clone());
            normalizer.push_generics(&value.generics);
            value.generics = normalizer.fold_generics(value.generics.clone());
            for field in &mut fields {
                field.ty = normalizer.fold_type(field.ty.clone());
            }
            normalizer.pop_scope();
            // Every union member starts at offset zero, including under
            // repr(C): layout depends on the member set, not declaration
            // order. Canonicalize that set while retaining names and types.
            filter_private_fields(&mut fields, false);
            let Fields::Named(fields) = fields else {
                unreachable!("union fields are named")
            };
            value.fields = fields;
            trim_trailing_punct(&mut value.fields.named);
            trim_generics_punctuation(&mut value.generics);
        }
        Item::Enum(value) => {
            let layout_sensitive = has_enum_field_order_repr(&value.attrs);
            let mut normalizer = SignatureAlphaNormalizer::with_occupied(free_identifiers.clone());
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
                if !layout_sensitive && let Fields::Named(fields) = &mut variant.fields {
                    let mut canonical = fields.named.iter().cloned().collect::<Vec<_>>();
                    canonical.sort_by_key(|field| {
                        field
                            .ident
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    });
                    fields.named = canonical.into_iter().collect();
                }
                trim_fields_punctuation(&mut variant.fields);
            }
        }
        Item::Trait(value) => {
            let mut normalizer = SignatureAlphaNormalizer::with_occupied(free_identifiers.clone());
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
                        if function.default.is_some() {
                            function
                                .attrs
                                .push(syn::parse_quote!(#[prview_trait_default]));
                        }
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
            let mut normalizer = SignatureAlphaNormalizer::with_occupied(free_identifiers);
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

fn normalized_confirmed_contract_without_item_name(
    mut item: Item,
    derive_ambiguity: &DeriveNameAmbiguity,
) -> String {
    sanitize_confirmed_contract_attrs(&mut item, derive_ambiguity);
    normalized_contract_without_item_name(item)
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
            for field in &mut named.named {
                if !is_public(&field.vis) {
                    // Restricted and inherited visibility are both private to
                    // external callers. Normalize before sorting and token
                    // serialization so spelling-only scope changes do not
                    // become public API deltas.
                    field.vis = Visibility::Inherited;
                }
            }
            if !layout_sensitive {
                // Without repr(C), downstream callers have no named-field
                // order ABI. `transparent` selects one carrier field and
                // standalone `packed`/`align` modify layout without specifying
                // order. Keep private field TYPES in the contract (they can
                // change Send/Sync and other auto traits), but sort them by
                // semantic input so a pure reorder is not a breaking change.
                // Public named-field order is likewise not observable in
                // literals or patterns.
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
                        field.vis = Visibility::Inherited;
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

fn has_enum_field_order_repr(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attribute| meta_contains_enum_field_order_repr(&attribute.meta))
}

fn meta_contains_layout_sensitive_repr(meta: &Meta) -> bool {
    if meta.path().is_ident("repr") {
        let Meta::List(list) = meta else {
            return false;
        };
        return list
            .tokens
            .to_string()
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|token| {
                matches!(
                    token,
                    "C" | "packed"
                        | "transparent"
                        | "align"
                        | "simd"
                        | "u8"
                        | "u16"
                        | "u32"
                        | "u64"
                        | "u128"
                        | "usize"
                        | "i8"
                        | "i16"
                        | "i32"
                        | "i64"
                        | "i128"
                        | "isize"
                )
            });
    }
    if !meta.path().is_ident("cfg_attr") {
        return false;
    }
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    parts
        .iter()
        .skip(1)
        .any(meta_contains_layout_sensitive_repr)
}

fn has_struct_field_order_repr(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attribute| meta_contains_struct_field_order_repr(&attribute.meta))
}

fn meta_contains_struct_field_order_repr(meta: &Meta) -> bool {
    if meta.path().is_ident("repr") {
        let Meta::List(list) = meta else {
            return false;
        };
        // Only repr(C) defines struct field order. `transparent` defines the
        // layout/ABI of its single carrier field, while standalone `packed`
        // and `align` modify layout without changing Rust's unspecified order.
        return list
            .tokens
            .to_string()
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|token| token == "C");
    }
    if !meta.path().is_ident("cfg_attr") {
        return false;
    }
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    parts
        .iter()
        .skip(1)
        .any(meta_contains_struct_field_order_repr)
}

fn meta_contains_enum_field_order_repr(meta: &Meta) -> bool {
    if meta.path().is_ident("repr") {
        let Meta::List(list) = meta else {
            return false;
        };
        return list
            .tokens
            .to_string()
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|token| {
                matches!(
                    token,
                    "C" | "u8"
                        | "u16"
                        | "u32"
                        | "u64"
                        | "u128"
                        | "usize"
                        | "i8"
                        | "i16"
                        | "i32"
                        | "i64"
                        | "i128"
                        | "isize"
                )
            });
    }
    if !meta.path().is_ident("cfg_attr") {
        return false;
    }
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    parts
        .iter()
        .skip(1)
        .any(meta_contains_enum_field_order_repr)
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
        ) || (drop_cfg
            && matches!(name.as_deref(), Some("cfg" | "cfg_attr"))
            && !meta_contains_layout_sensitive_repr(&attr.meta)
            && !meta_contains_derive(&attr.meta))
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
    SignatureAlphaNormalizer::with_occupied(FreeIdentifierCollector::signature(signature))
        .normalize_signature(signature);
}

#[derive(Default)]
struct FreeIdentifierCollector {
    ident_scopes: Vec<BTreeSet<String>>,
    lifetime_scopes: Vec<BTreeSet<String>>,
    value_scopes: Vec<BTreeSet<String>>,
    identifiers: BTreeSet<String>,
}

impl FreeIdentifierCollector {
    fn signature(signature: &syn::Signature) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.walk_signature(signature);
        collector.identifiers
    }

    fn signature_and_block(signature: &syn::Signature, block: &syn::Block) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.push_generics(&signature.generics);
        collector.fold_generics(signature.generics.clone());
        collector.push_value_scope();
        for argument in &signature.inputs {
            match argument {
                syn::FnArg::Receiver(receiver) => {
                    collector.fold_receiver(receiver.clone());
                    collector
                        .value_scopes
                        .last_mut()
                        .expect("function value scope")
                        .insert("self".to_owned());
                }
                syn::FnArg::Typed(argument) => {
                    collector.fold_type((*argument.ty).clone());
                    collector.bind_definite_pattern(argument.pat.as_ref());
                }
            }
        }
        collector.fold_return_type(signature.output.clone());
        collector.fold_root_block(block.clone());
        collector.pop_value_scope();
        collector.pop_scope();
        collector.identifiers
    }

    fn item(item: &Item) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.walk_item(item);
        collector.identifiers
    }

    fn item_impl(item: &syn::ItemImpl) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.walk_impl(item);
        collector.identifiers
    }

    fn impl_header_and_item(item_impl: &syn::ItemImpl, item: &syn::ImplItem) -> BTreeSet<String> {
        let mut collector = Self::default();
        collector.push_generics(&item_impl.generics);
        collector.fold_generics(item_impl.generics.clone());
        collector.fold_type((*item_impl.self_ty).clone());
        if let Some((_, path, _)) = &item_impl.trait_ {
            collector.fold_path(path.clone());
        }
        collector.walk_impl_item(item);
        collector.pop_scope();
        collector.identifiers
    }

    fn push_generics(&mut self, generics: &syn::Generics) {
        let mut idents = BTreeSet::new();
        let mut lifetimes = BTreeSet::new();
        for parameter in &generics.params {
            match parameter {
                syn::GenericParam::Lifetime(parameter) => {
                    lifetimes.insert(parameter.lifetime.to_string());
                }
                syn::GenericParam::Type(parameter) => {
                    idents.insert(normalize_identifier(parameter.ident.to_string()));
                }
                syn::GenericParam::Const(parameter) => {
                    idents.insert(normalize_identifier(parameter.ident.to_string()));
                }
            }
        }
        self.ident_scopes.push(idents);
        self.lifetime_scopes.push(lifetimes);
    }

    fn push_params(&mut self, params: &[syn::GenericParam]) {
        let generics = syn::Generics {
            params: params.iter().cloned().collect(),
            ..syn::Generics::default()
        };
        self.push_generics(&generics);
    }

    fn pop_scope(&mut self) {
        self.ident_scopes.pop();
        self.lifetime_scopes.pop();
    }

    fn push_value_scope(&mut self) {
        self.value_scopes.push(BTreeSet::new());
    }

    fn pop_value_scope(&mut self) {
        self.value_scopes.pop();
    }

    fn value_is_bound(&self, identifier: &str) -> bool {
        self.value_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(identifier))
    }

    fn bind_definite_pattern(&mut self, pattern: &syn::Pat) {
        let mut identifiers = Vec::new();
        collect_pattern_identifiers(pattern, &mut identifiers);
        let scope = self
            .value_scopes
            .last_mut()
            .expect("value binding requires an active lexical scope");
        scope.extend(
            identifiers
                .into_iter()
                .map(|ident| normalize_identifier(ident.to_string())),
        );
    }

    fn reserve_pattern(&mut self, pattern: &syn::Pat) {
        let mut identifiers = Vec::new();
        collect_pattern_identifiers(pattern, &mut identifiers);
        self.identifiers.extend(
            identifiers
                .into_iter()
                .map(|ident| normalize_identifier(ident.to_string())),
        );
    }

    fn fold_root_block(&mut self, mut block: syn::Block) -> syn::Block {
        block.stmts = block
            .stmts
            .into_iter()
            .map(|statement| self.fold_stmt(statement))
            .collect();
        block
    }

    fn ident_is_bound(&self, identifier: &str) -> bool {
        self.ident_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(identifier))
    }

    fn lifetime_is_bound(&self, lifetime: &str) -> bool {
        self.lifetime_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(lifetime))
    }

    fn walk_signature(&mut self, signature: &syn::Signature) {
        self.push_generics(&signature.generics);
        self.fold_generics(signature.generics.clone());
        for argument in &signature.inputs {
            match argument {
                syn::FnArg::Receiver(receiver) => {
                    self.fold_receiver(receiver.clone());
                }
                syn::FnArg::Typed(argument) => {
                    self.fold_type((*argument.ty).clone());
                }
            }
        }
        self.fold_return_type(signature.output.clone());
        self.pop_scope();
    }

    fn walk_item(&mut self, item: &Item) {
        match item {
            Item::Fn(function) => self.walk_signature(&function.sig),
            Item::Struct(value) => {
                self.push_generics(&value.generics);
                self.fold_generics(value.generics.clone());
                for field in &value.fields {
                    self.fold_type(field.ty.clone());
                }
                self.pop_scope();
            }
            Item::Union(value) => {
                self.push_generics(&value.generics);
                self.fold_generics(value.generics.clone());
                for field in &value.fields.named {
                    self.fold_type(field.ty.clone());
                }
                self.pop_scope();
            }
            Item::Enum(value) => {
                self.push_generics(&value.generics);
                self.fold_generics(value.generics.clone());
                for variant in &value.variants {
                    for field in &variant.fields {
                        self.fold_type(field.ty.clone());
                    }
                    if let Some((_, expression)) = &variant.discriminant {
                        self.fold_expr(expression.clone());
                    }
                }
                self.pop_scope();
            }
            Item::Trait(value) => {
                self.push_generics(&value.generics);
                self.fold_generics(value.generics.clone());
                for bound in &value.supertraits {
                    self.fold_type_param_bound(bound.clone());
                }
                for item in &value.items {
                    match item {
                        syn::TraitItem::Fn(function) => self.walk_signature(&function.sig),
                        syn::TraitItem::Const(value) => {
                            self.fold_type(value.ty.clone());
                            if let Some((_, default)) = &value.default {
                                self.fold_expr(default.clone());
                            }
                        }
                        syn::TraitItem::Type(value) => {
                            self.push_generics(&value.generics);
                            self.fold_generics(value.generics.clone());
                            for bound in &value.bounds {
                                self.fold_type_param_bound(bound.clone());
                            }
                            if let Some((_, default)) = &value.default {
                                self.fold_type(default.clone());
                            }
                            self.pop_scope();
                        }
                        _ => {}
                    }
                }
                self.pop_scope();
            }
            Item::Type(value) => {
                self.push_generics(&value.generics);
                self.fold_generics(value.generics.clone());
                self.fold_type((*value.ty).clone());
                self.pop_scope();
            }
            Item::Const(value) => {
                self.fold_type((*value.ty).clone());
                self.fold_expr((*value.expr).clone());
            }
            Item::Static(value) => {
                self.fold_type((*value.ty).clone());
                self.fold_expr((*value.expr).clone());
            }
            _ => {}
        }
    }

    fn walk_impl(&mut self, item: &syn::ItemImpl) {
        self.push_generics(&item.generics);
        self.fold_generics(item.generics.clone());
        self.fold_type((*item.self_ty).clone());
        if let Some((_, path, _)) = &item.trait_ {
            self.fold_path(path.clone());
        }
        for item in &item.items {
            self.walk_impl_item(item);
        }
        self.pop_scope();
    }

    fn walk_impl_item(&mut self, item: &syn::ImplItem) {
        match item {
            syn::ImplItem::Fn(function) => self.walk_signature(&function.sig),
            syn::ImplItem::Const(value) => {
                self.fold_type(value.ty.clone());
                self.fold_expr(value.expr.clone());
            }
            syn::ImplItem::Type(value) => {
                self.push_generics(&value.generics);
                self.fold_generics(value.generics.clone());
                self.fold_type(value.ty.clone());
                self.pop_scope();
            }
            _ => {}
        }
    }

    fn fold_path_idents(&mut self, path: syn::Path, fold_head: bool) -> syn::Path {
        for (index, segment) in path.segments.iter().enumerate() {
            let identifier = normalize_identifier(segment.ident.to_string());
            if index == 0 && fold_head && path.leading_colon.is_none() {
                if !self.ident_is_bound(&identifier) {
                    self.identifiers.insert(identifier);
                }
            } else {
                self.identifiers.insert(identifier);
            }
            self.fold_path_arguments(segment.arguments.clone());
        }
        path
    }
}

impl Fold for FreeIdentifierCollector {
    fn fold_ident(&mut self, ident: syn::Ident) -> syn::Ident {
        let identifier = normalize_identifier(ident.to_string());
        if !self.ident_is_bound(&identifier) {
            self.identifiers.insert(identifier);
        }
        ident
    }

    fn fold_lifetime(&mut self, lifetime: syn::Lifetime) -> syn::Lifetime {
        if !self.lifetime_is_bound(&lifetime.to_string()) {
            self.identifiers.insert(lifetime.ident.to_string());
        }
        lifetime
    }

    fn fold_path(&mut self, path: syn::Path) -> syn::Path {
        self.fold_path_idents(path, true)
    }

    fn fold_expr_path(&mut self, expression: syn::ExprPath) -> syn::ExprPath {
        let qualified = expression.qself.is_some();
        let path = expression.path;
        for (index, segment) in path.segments.iter().enumerate() {
            let identifier = normalize_identifier(segment.ident.to_string());
            let head_is_bound = index == 0
                && path.leading_colon.is_none()
                && (self.value_is_bound(&identifier) || self.ident_is_bound(&identifier));
            if !head_is_bound {
                self.identifiers.insert(identifier);
            }
            self.fold_path_arguments(segment.arguments.clone());
        }
        syn::ExprPath {
            attrs: expression.attrs,
            qself: expression.qself.map(|qself| self.fold_qself(qself)),
            path: if qualified {
                self.fold_path_idents(path, false)
            } else {
                path
            },
        }
    }

    fn fold_block(&mut self, block: syn::Block) -> syn::Block {
        self.push_value_scope();
        let folded = self.fold_root_block(block);
        self.pop_value_scope();
        folded
    }

    fn fold_stmt(&mut self, statement: syn::Stmt) -> syn::Stmt {
        let syn::Stmt::Local(mut local) = statement else {
            return syn::fold::fold_stmt(self, statement);
        };
        if let Some(init) = &mut local.init {
            *init.expr = self.fold_expr((*init.expr).clone());
            if let Some((else_token, divergence)) = init.diverge.take() {
                self.reserve_pattern(&local.pat);
                init.diverge = Some((else_token, Box::new(self.fold_expr(*divergence))));
            } else {
                self.bind_definite_pattern(&local.pat);
            }
        } else {
            self.bind_definite_pattern(&local.pat);
        }
        syn::Stmt::Local(local)
    }

    fn fold_expr_closure(&mut self, mut closure: syn::ExprClosure) -> syn::ExprClosure {
        self.push_value_scope();
        for pattern in &closure.inputs {
            self.bind_definite_pattern(pattern);
        }
        closure.body = Box::new(self.fold_expr(*closure.body));
        self.pop_value_scope();
        closure
    }

    fn fold_arm(&mut self, mut arm: syn::Arm) -> syn::Arm {
        self.push_value_scope();
        self.reserve_pattern(&arm.pat);
        if let Some((if_token, guard)) = arm.guard.take() {
            arm.guard = Some((if_token, Box::new(self.fold_expr(*guard))));
        }
        arm.body = Box::new(self.fold_expr(*arm.body));
        self.pop_value_scope();
        arm
    }

    fn fold_expr_for_loop(&mut self, mut expression: syn::ExprForLoop) -> syn::ExprForLoop {
        expression.expr = Box::new(self.fold_expr(*expression.expr));
        self.push_value_scope();
        self.bind_definite_pattern(&expression.pat);
        expression.body = self.fold_root_block(expression.body);
        self.pop_value_scope();
        expression
    }

    fn fold_macro(&mut self, mac: syn::Macro) -> syn::Macro {
        for segment in &mac.path.segments {
            self.identifiers
                .insert(normalize_identifier(segment.ident.to_string()));
        }
        collect_token_identifiers(mac.tokens.clone(), &mut self.identifiers);
        mac
    }

    fn fold_type_path(&mut self, type_path: syn::TypePath) -> syn::TypePath {
        let qualified = type_path.qself.is_some();
        syn::TypePath {
            qself: type_path.qself.map(|qself| self.fold_qself(qself)),
            path: self.fold_path_idents(type_path.path, !qualified),
        }
    }

    fn fold_type_bare_fn(&mut self, bare_fn: syn::TypeBareFn) -> syn::TypeBareFn {
        let has_binders = bare_fn.lifetimes.is_some();
        if let Some(lifetimes) = &bare_fn.lifetimes {
            self.push_params(&lifetimes.lifetimes.iter().cloned().collect::<Vec<_>>());
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
            self.push_params(&lifetimes.lifetimes.iter().cloned().collect::<Vec<_>>());
        }
        let folded = syn::fold::fold_trait_bound(self, bound);
        if has_binders {
            self.pop_scope();
        }
        folded
    }

    fn fold_bare_fn_arg(&mut self, mut argument: syn::BareFnArg) -> syn::BareFnArg {
        argument.name = None;
        syn::fold::fold_bare_fn_arg(self, argument)
    }
}

/// Canonicalize names that bind only inside a public signature while retaining
/// every use relationship. Generic order remains observable; spelling does not.
#[derive(Default)]
struct SignatureAlphaNormalizer {
    ident_scopes: Vec<BTreeMap<String, syn::Ident>>,
    lifetime_scopes: Vec<BTreeMap<String, syn::Lifetime>>,
    occupied_identifiers: BTreeSet<String>,
}

impl SignatureAlphaNormalizer {
    fn with_occupied(occupied_identifiers: BTreeSet<String>) -> Self {
        Self {
            occupied_identifiers,
            ..Self::default()
        }
    }

    fn fresh_type_identifier(
        &mut self,
        depth: usize,
        index: usize,
        span: proc_macro2::Span,
    ) -> syn::Ident {
        for salt in 0usize.. {
            let candidate = format!("__PrviewT{depth}_{index}_{salt}");
            if self.occupied_identifiers.insert(candidate.clone()) {
                return syn::Ident::new(&candidate, span);
            }
        }
        unreachable!()
    }

    fn fresh_const_identifier(
        &mut self,
        depth: usize,
        index: usize,
        span: proc_macro2::Span,
    ) -> syn::Ident {
        for salt in 0usize.. {
            let candidate = format!("__PRVIEW_C{depth}_{index}_{salt}");
            if self.occupied_identifiers.insert(candidate.clone()) {
                return syn::Ident::new(&candidate, span);
            }
        }
        unreachable!()
    }

    fn fresh_lifetime(
        &mut self,
        depth: usize,
        index: usize,
        span: proc_macro2::Span,
    ) -> syn::Lifetime {
        for salt in 0usize.. {
            let candidate = format!("__prview_l{depth}_{index}_{salt}");
            if self.occupied_identifiers.insert(candidate.clone()) {
                return syn::Lifetime::new(&format!("'{candidate}"), span);
            }
        }
        unreachable!()
    }

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

    fn normalize_signature_and_block(
        &mut self,
        signature: &mut syn::Signature,
        block: &mut syn::Block,
    ) {
        self.occupied_identifiers
            .extend(FreeIdentifierCollector::signature_and_block(
                signature, block,
            ));
        self.push_generics(&signature.generics);
        signature.generics = self.fold_generics(signature.generics.clone());
        for argument in &mut signature.inputs {
            match argument {
                syn::FnArg::Receiver(receiver) => {
                    *receiver = self.fold_receiver(receiver.clone());
                }
                syn::FnArg::Typed(argument) => {
                    *argument.ty = self.fold_type((*argument.ty).clone());
                }
            }
        }
        signature.output = self.fold_return_type(signature.output.clone());
        *block = self.fold_block(block.clone());
        self.pop_scope();

        let mut value_normalizer =
            ValueAlphaNormalizer::with_occupied(self.occupied_identifiers.clone());
        value_normalizer.push_scope();
        for argument in &mut signature.inputs {
            if let syn::FnArg::Typed(argument) = argument {
                *argument.pat = value_normalizer.bind_pattern((*argument.pat).clone());
            }
        }
        *block = value_normalizer.fold_root_block(block.clone());
        value_normalizer.pop_scope();
    }

    fn push_generics(&mut self, generics: &syn::Generics) {
        let params = generics.params.iter().cloned().collect::<Vec<_>>();
        self.push_params(&params);
    }

    fn push_params(&mut self, params: &[syn::GenericParam]) {
        let mut idents = BTreeMap::new();
        let mut lifetimes = BTreeMap::new();
        let depth = self.ident_scopes.len();
        for (index, parameter) in params.iter().enumerate() {
            match parameter {
                syn::GenericParam::Lifetime(parameter) => {
                    let replacement =
                        self.fresh_lifetime(depth, index, parameter.lifetime.ident.span());
                    lifetimes.insert(parameter.lifetime.to_string(), replacement);
                }
                syn::GenericParam::Type(parameter) => {
                    let replacement =
                        self.fresh_type_identifier(depth, index, parameter.ident.span());
                    idents.insert(parameter.ident.to_string(), replacement);
                }
                syn::GenericParam::Const(parameter) => {
                    let replacement =
                        self.fresh_const_identifier(depth, index, parameter.ident.span());
                    idents.insert(parameter.ident.to_string(), replacement);
                }
            }
        }
        self.ident_scopes.push(idents);
        self.lifetime_scopes.push(lifetimes);
    }

    fn pop_scope(&mut self) {
        if let Some(scope) = self.ident_scopes.pop() {
            for replacement in scope.into_values() {
                self.occupied_identifiers.remove(&replacement.to_string());
            }
        }
        if let Some(scope) = self.lifetime_scopes.pop() {
            for replacement in scope.into_values() {
                self.occupied_identifiers
                    .remove(&replacement.ident.to_string());
            }
        }
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
    fn fold_macro(&mut self, mac: syn::Macro) -> syn::Macro {
        // Macro paths and token streams live in expansion-specific namespaces.
        // Rewriting them as type/const generic uses can collapse two different
        // macro expansions into one opaque-return proof.
        mac
    }
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

#[derive(Default)]
struct ValueAlphaNormalizer {
    scopes: Vec<BTreeMap<String, syn::Ident>>,
    occupied_identifiers: BTreeSet<String>,
    next_binding: usize,
}

impl ValueAlphaNormalizer {
    fn with_occupied(occupied_identifiers: BTreeSet<String>) -> Self {
        Self {
            occupied_identifiers,
            ..Self::default()
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    fn pop_scope(&mut self) {
        if let Some(scope) = self.scopes.pop() {
            for replacement in scope.into_values() {
                self.occupied_identifiers.remove(&replacement.to_string());
            }
        }
    }

    fn fresh_binding(&mut self, span: proc_macro2::Span) -> syn::Ident {
        loop {
            let candidate = format!("__prview_v{}", self.next_binding);
            self.next_binding += 1;
            if self.occupied_identifiers.insert(candidate.clone()) {
                return syn::Ident::new(&candidate, span);
            }
        }
    }

    fn resolve_binding(&self, ident: &syn::Ident) -> Option<syn::Ident> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&ident.to_string()).cloned())
    }

    fn bind_pattern(&mut self, pattern: syn::Pat) -> syn::Pat {
        let mut identifiers = Vec::new();
        collect_pattern_identifiers(&pattern, &mut identifiers);
        let mut replacements = BTreeMap::new();
        for ident in identifiers {
            replacements
                .entry(ident.to_string())
                .or_insert_with(|| self.fresh_binding(ident.span()));
        }
        self.scopes
            .last_mut()
            .expect("value binding requires an active lexical scope")
            .extend(replacements.clone());
        PatternAlphaFolder { replacements }.fold_pat(pattern)
    }

    fn preserve_refutable_pattern(&mut self, pattern: &syn::Pat) {
        let mut identifiers = Vec::new();
        collect_pattern_identifiers(pattern, &mut identifiers);
        let scope = self
            .scopes
            .last_mut()
            .expect("refutable pattern requires an active lexical scope");
        for ident in identifiers {
            scope.insert(ident.to_string(), ident);
        }
    }

    fn fold_refutable_condition(&mut self, expression: syn::Expr) -> syn::Expr {
        match expression {
            syn::Expr::Let(mut expression) => {
                expression.expr = Box::new(self.fold_expr(*expression.expr));
                self.preserve_refutable_pattern(&expression.pat);
                syn::Expr::Let(expression)
            }
            syn::Expr::Binary(mut expression) if matches!(expression.op, syn::BinOp::And(_)) => {
                expression.left = Box::new(self.fold_refutable_condition(*expression.left));
                expression.right = Box::new(self.fold_refutable_condition(*expression.right));
                syn::Expr::Binary(expression)
            }
            expression => self.fold_expr(expression),
        }
    }

    fn fold_root_block(&mut self, mut block: syn::Block) -> syn::Block {
        block.stmts = block
            .stmts
            .into_iter()
            .map(|statement| self.fold_stmt(statement))
            .collect();
        block
    }
}

fn collect_pattern_identifiers(pattern: &syn::Pat, identifiers: &mut Vec<syn::Ident>) {
    match pattern {
        syn::Pat::Ident(pattern) => {
            identifiers.push(pattern.ident.clone());
            if let Some((_, subpattern)) = &pattern.subpat {
                collect_pattern_identifiers(subpattern, identifiers);
            }
        }
        syn::Pat::Or(pattern) => {
            for case in &pattern.cases {
                collect_pattern_identifiers(case, identifiers);
            }
        }
        syn::Pat::Paren(pattern) => collect_pattern_identifiers(&pattern.pat, identifiers),
        syn::Pat::Reference(pattern) => collect_pattern_identifiers(&pattern.pat, identifiers),
        syn::Pat::Slice(pattern) => {
            for element in &pattern.elems {
                collect_pattern_identifiers(element, identifiers);
            }
        }
        syn::Pat::Struct(pattern) => {
            for field in &pattern.fields {
                collect_pattern_identifiers(&field.pat, identifiers);
            }
        }
        syn::Pat::Tuple(pattern) => {
            for element in &pattern.elems {
                collect_pattern_identifiers(element, identifiers);
            }
        }
        syn::Pat::TupleStruct(pattern) => {
            for element in &pattern.elems {
                collect_pattern_identifiers(element, identifiers);
            }
        }
        syn::Pat::Type(pattern) => collect_pattern_identifiers(&pattern.pat, identifiers),
        _ => {}
    }
}

fn collect_token_identifiers(tokens: proc_macro2::TokenStream, identifiers: &mut BTreeSet<String>) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Ident(ident) => {
                identifiers.insert(normalize_identifier(ident.to_string()));
            }
            proc_macro2::TokenTree::Group(group) => {
                collect_token_identifiers(group.stream(), identifiers);
            }
            _ => {}
        }
    }
}

struct PatternAlphaFolder {
    replacements: BTreeMap<String, syn::Ident>,
}

impl Fold for PatternAlphaFolder {
    fn fold_field_pat(&mut self, mut field: syn::FieldPat) -> syn::FieldPat {
        let was_shorthand = field.colon_token.is_none();
        field.pat = Box::new(self.fold_pat(*field.pat));
        if was_shorthand {
            // `Pair { field }` binds a local named `field`, but the member name
            // remains part of the program's semantics. Once the binding is
            // alpha-renamed, spell the pattern as `Pair { field: canonical }`
            // so two different members cannot collapse to the same contract.
            field.colon_token = Some(Default::default());
        }
        field
    }

    fn fold_pat_ident(&mut self, mut pattern: syn::PatIdent) -> syn::PatIdent {
        if let Some(replacement) = self.replacements.get(&pattern.ident.to_string()) {
            pattern.ident = replacement.clone();
        }
        syn::fold::fold_pat_ident(self, pattern)
    }
}

impl Fold for ValueAlphaNormalizer {
    fn fold_expr_path(&mut self, mut expression: syn::ExprPath) -> syn::ExprPath {
        expression = syn::fold::fold_expr_path(self, expression);
        if expression.qself.is_none()
            && expression.path.leading_colon.is_none()
            && expression.path.segments.len() == 1
            && let Some(segment) = expression.path.segments.first_mut()
            && let Some(replacement) = self.resolve_binding(&segment.ident)
        {
            segment.ident = replacement;
        }
        expression
    }

    fn fold_block(&mut self, block: syn::Block) -> syn::Block {
        self.push_scope();
        let folded = self.fold_root_block(block);
        self.pop_scope();
        folded
    }

    fn fold_stmt(&mut self, statement: syn::Stmt) -> syn::Stmt {
        let syn::Stmt::Local(mut local) = statement else {
            return syn::fold::fold_stmt(self, statement);
        };
        let refutable = local
            .init
            .as_ref()
            .is_some_and(|initializer| initializer.diverge.is_some());
        if let Some(init) = &mut local.init {
            *init.expr = self.fold_expr((*init.expr).clone());
            if let Some((else_token, divergence)) = init.diverge.take() {
                init.diverge = Some((else_token, Box::new(self.fold_expr(*divergence))));
            }
        }
        if refutable {
            self.preserve_refutable_pattern(&local.pat);
        } else {
            local.pat = self.bind_pattern(local.pat);
        }
        syn::Stmt::Local(local)
    }

    fn fold_expr_closure(&mut self, mut closure: syn::ExprClosure) -> syn::ExprClosure {
        self.push_scope();
        closure.inputs = closure
            .inputs
            .into_iter()
            .map(|pattern| self.bind_pattern(pattern))
            .collect();
        closure.body = Box::new(self.fold_expr(*closure.body));
        self.pop_scope();
        closure
    }

    fn fold_arm(&mut self, mut arm: syn::Arm) -> syn::Arm {
        self.push_scope();
        self.preserve_refutable_pattern(&arm.pat);
        if let Some((if_token, guard)) = arm.guard.take() {
            arm.guard = Some((if_token, Box::new(self.fold_expr(*guard))));
        }
        arm.body = Box::new(self.fold_expr(*arm.body));
        self.pop_scope();
        arm
    }

    fn fold_expr_for_loop(&mut self, mut expression: syn::ExprForLoop) -> syn::ExprForLoop {
        expression.expr = Box::new(self.fold_expr(*expression.expr));
        self.push_scope();
        expression.pat = Box::new(self.bind_pattern(*expression.pat));
        expression.body = self.fold_root_block(expression.body);
        self.pop_scope();
        expression
    }

    fn fold_expr_if(&mut self, mut expression: syn::ExprIf) -> syn::ExprIf {
        self.push_scope();
        expression.cond = Box::new(self.fold_refutable_condition(*expression.cond));
        expression.then_branch = self.fold_root_block(expression.then_branch);
        self.pop_scope();
        if let Some((else_token, branch)) = expression.else_branch.take() {
            expression.else_branch = Some((else_token, Box::new(self.fold_expr(*branch))));
        }
        expression
    }

    fn fold_expr_while(&mut self, mut expression: syn::ExprWhile) -> syn::ExprWhile {
        self.push_scope();
        expression.cond = Box::new(self.fold_refutable_condition(*expression.cond));
        expression.body = self.fold_root_block(expression.body);
        self.pop_scope();
        expression
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

fn sort_semantic_set<T, P>(values: &mut Punctuated<T, P>)
where
    T: ToTokens,
    P: Default,
{
    let mut items: Vec<_> = std::mem::replace(values, Punctuated::new())
        .into_iter()
        .collect();
    items.sort_by_key(|item| canonical_tokens(item.to_token_stream()));
    *values = items.into_iter().collect();
}

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

    fn fold_generics(&mut self, generics: syn::Generics) -> syn::Generics {
        let mut generics = syn::fold::fold_generics(self, generics);
        if let Some(where_clause) = &mut generics.where_clause {
            sort_semantic_set(&mut where_clause.predicates);
        }
        generics
    }

    fn fold_type_param(&mut self, parameter: syn::TypeParam) -> syn::TypeParam {
        let mut parameter = syn::fold::fold_type_param(self, parameter);
        sort_semantic_set(&mut parameter.bounds);
        parameter
    }

    fn fold_lifetime_param(&mut self, parameter: syn::LifetimeParam) -> syn::LifetimeParam {
        let mut parameter = syn::fold::fold_lifetime_param(self, parameter);
        sort_semantic_set(&mut parameter.bounds);
        parameter
    }

    fn fold_predicate_type(&mut self, predicate: syn::PredicateType) -> syn::PredicateType {
        let mut predicate = syn::fold::fold_predicate_type(self, predicate);
        sort_semantic_set(&mut predicate.bounds);
        predicate
    }

    fn fold_predicate_lifetime(
        &mut self,
        predicate: syn::PredicateLifetime,
    ) -> syn::PredicateLifetime {
        let mut predicate = syn::fold::fold_predicate_lifetime(self, predicate);
        sort_semantic_set(&mut predicate.bounds);
        predicate
    }

    fn fold_constraint(&mut self, constraint: syn::Constraint) -> syn::Constraint {
        let mut constraint = syn::fold::fold_constraint(self, constraint);
        sort_semantic_set(&mut constraint.bounds);
        constraint
    }

    fn fold_item_trait(&mut self, item: syn::ItemTrait) -> syn::ItemTrait {
        let mut item = syn::fold::fold_item_trait(self, item);
        sort_semantic_set(&mut item.supertraits);
        item
    }

    fn fold_item_trait_alias(&mut self, item: syn::ItemTraitAlias) -> syn::ItemTraitAlias {
        let mut item = syn::fold::fold_item_trait_alias(self, item);
        sort_semantic_set(&mut item.bounds);
        item
    }

    fn fold_trait_item_type(&mut self, item: syn::TraitItemType) -> syn::TraitItemType {
        let mut item = syn::fold::fold_trait_item_type(self, item);
        sort_semantic_set(&mut item.bounds);
        item
    }

    fn fold_type_impl_trait(&mut self, ty: syn::TypeImplTrait) -> syn::TypeImplTrait {
        let mut ty = syn::fold::fold_type_impl_trait(self, ty);
        sort_semantic_set(&mut ty.bounds);
        ty
    }

    fn fold_type_trait_object(&mut self, ty: syn::TypeTraitObject) -> syn::TypeTraitObject {
        let mut ty = syn::fold::fold_type_trait_object(self, ty);
        sort_semantic_set(&mut ty.bounds);
        ty
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
    let occupied = FreeIdentifierCollector::impl_header_and_item(item_impl, item);
    let mut normalizer = SignatureAlphaNormalizer::with_occupied(occupied);
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
            value.ty = normalizer.fold_type(value.ty);
            value.expr = normalizer.fold_expr(value.expr);
            value.to_token_stream()
        }
        _ => return String::new(),
    };
    normalizer.pop_scope();
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let tokens = quote!(
        #(#impl_attrs)* #defaultness #unsafety
        impl #impl_generics #self_ty #where_clause { #item_tokens }
    );
    let normalized: syn::ItemImpl =
        syn::parse2(tokens).expect("normalized associated-item contract remains valid Rust");
    canonical_tokens(CanonicalFold.fold_item_impl(normalized).to_token_stream())
}

fn normalized_trait_impl_item(item_impl: &syn::ItemImpl) -> syn::ItemImpl {
    let mut item_impl = item_impl.clone();
    normalize_attrs(&mut item_impl.attrs, true);
    let occupied = FreeIdentifierCollector::item_impl(&item_impl);
    let mut normalizer = SignatureAlphaNormalizer::with_occupied(occupied);
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
                value.expr = normalizer.fold_expr(value.expr.clone());
            }
            syn::ImplItem::Type(alias) => {
                normalize_attrs(&mut alias.attrs, false);
                normalizer.push_generics(&alias.generics);
                alias.generics = normalizer.fold_generics(alias.generics.clone());
                trim_generics_punctuation(&mut alias.generics);
                alias.ty = normalizer.fold_type(alias.ty.clone());
                normalizer.pop_scope();
            }
            syn::ImplItem::Macro(value) => normalize_attrs(&mut value.attrs, false),
            _ => {}
        }
    }
    // Ordinary trait impl members are an unordered semantic set. Macro and
    // verbatim items stay in source order because their expansion semantics
    // are opaque to this parser and must remain fail-closed.
    if item_impl.items.iter().all(|item| {
        matches!(
            item,
            syn::ImplItem::Fn(_) | syn::ImplItem::Const(_) | syn::ImplItem::Type(_)
        )
    }) {
        item_impl
            .items
            .sort_by_cached_key(|item| canonical_tokens(item.to_token_stream()));
    }
    normalizer.pop_scope();
    CanonicalFold.fold_item_impl(item_impl)
}

fn normalized_trait_impl_contract(item_impl: &syn::ItemImpl) -> String {
    canonical_tokens(normalized_trait_impl_item(item_impl).to_token_stream())
}

/// Normalizes an impl's caller-observable body while removing only the
/// spelling of the top-level trait and owner paths. Their canonical resolved
/// identities (including effective cfg regions) are appended separately by
/// the alias resolver. Path arguments remain here because changing them can
/// change which impl exists.
fn normalized_trait_impl_semantic_contract(item_impl: &syn::ItemImpl) -> String {
    let mut item_impl = normalized_trait_impl_item(item_impl);
    if let Some((not, mut trait_path, for_token)) = item_impl.trait_.take() {
        if let Some(last) = trait_path.segments.last().cloned() {
            let mut replacement = last;
            replacement.ident = syn::Ident::new("__prview_trait", replacement.ident.span());
            trait_path.leading_colon = None;
            trait_path.segments.clear();
            trait_path.segments.push(replacement);
        }
        item_impl.trait_ = Some((not, trait_path, for_token));
    }
    hide_impl_owner_path(item_impl.self_ty.as_mut());
    canonical_tokens(item_impl.to_token_stream())
}

fn hide_impl_owner_path(ty: &mut syn::Type) -> bool {
    match ty {
        syn::Type::Path(type_path) if type_path.qself.is_none() => {
            let Some(last) = type_path.path.segments.last().cloned() else {
                return false;
            };
            let mut replacement = last;
            replacement.ident = syn::Ident::new("__prview_owner", replacement.ident.span());
            type_path.path.leading_colon = None;
            type_path.path.segments.clear();
            type_path.path.segments.push(replacement);
            true
        }
        syn::Type::Reference(reference) => hide_impl_owner_path(&mut reference.elem),
        syn::Type::Ptr(pointer) => hide_impl_owner_path(&mut pointer.elem),
        syn::Type::Slice(slice) => hide_impl_owner_path(&mut slice.elem),
        syn::Type::Array(array) => hide_impl_owner_path(&mut array.elem),
        syn::Type::Paren(paren) => hide_impl_owner_path(&mut paren.elem),
        syn::Type::Group(group) => hide_impl_owner_path(&mut group.elem),
        _ => false,
    }
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

pub(super) fn trait_member_cfg_key(attrs: &[Attribute]) -> String {
    canonical_cfg(attrs).guards.join("&&")
}

fn cfg_has_custom_predicate(attrs: &[Attribute]) -> bool {
    fn is_known_leaf(name: &str) -> bool {
        matches!(
            name,
            "feature"
                | "target_arch"
                | "target_abi"
                | "target_feature"
                | "target_os"
                | "target_env"
                | "target_family"
                | "target_vendor"
                | "target_endian"
                | "target_pointer_width"
                | "target_has_atomic"
                | "sanitize"
                | "panic"
                | "relocation_model"
                | "code_model"
                | "overflow_checks"
                | "unix"
                | "windows"
                | "test"
                | "debug_assertions"
                | "proc_macro"
                | "doc"
                | "doctest"
                | "miri"
                | "target_thread_local"
                | "ub_checks"
        )
    }

    fn predicate_is_custom(meta: &Meta) -> bool {
        let name = meta
            .path()
            .segments
            .last()
            .map(|segment| normalize_identifier(segment.ident.to_string()))
            .unwrap_or_default();
        match meta {
            Meta::Path(_) | Meta::NameValue(_) => !is_known_leaf(&name),
            Meta::List(list) if matches!(name.as_str(), "all" | "any" | "not") => {
                let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
                parser
                    .parse2(list.tokens.clone())
                    .map(|parts| parts.iter().any(predicate_is_custom))
                    .unwrap_or(true)
            }
            Meta::List(_) => true,
        }
    }

    fn cfg_meta_has_custom(meta: &Meta) -> bool {
        let name = meta
            .path()
            .segments
            .last()
            .map(|segment| normalize_identifier(segment.ident.to_string()));
        let Meta::List(list) = meta else {
            return false;
        };
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let Ok(parts) = parser.parse2(list.tokens.clone()) else {
            return false;
        };
        match name.as_deref() {
            Some("cfg") => parts.first().is_some_and(predicate_is_custom),
            Some("cfg_attr") => {
                parts.first().is_some_and(predicate_is_custom)
                    || parts.iter().skip(1).any(cfg_meta_has_custom)
            }
            _ => false,
        }
    }

    attrs.iter().any(|attr| cfg_meta_has_custom(&attr.meta))
}

fn item_custom_cfg_evidence(item: &Item) -> Vec<String> {
    fn append(attrs: &[Attribute], evidence: &mut Vec<String>) {
        evidence.extend(
            attrs
                .iter()
                .filter(|attr| cfg_has_custom_predicate(std::slice::from_ref(*attr)))
                .map(|attr| canonical_tokens(attr.to_token_stream())),
        );
    }

    fn append_fields(fields: &syn::Fields, evidence: &mut Vec<String>) {
        for field in fields {
            append(&field.attrs, evidence);
        }
    }

    let mut evidence = Vec::new();
    append(item_attrs(item), &mut evidence);
    match item {
        Item::Struct(value) => append_fields(&value.fields, &mut evidence),
        Item::Union(value) => {
            for field in &value.fields.named {
                append(&field.attrs, &mut evidence);
            }
        }
        Item::Enum(value) => {
            for variant in &value.variants {
                append(&variant.attrs, &mut evidence);
                append_fields(&variant.fields, &mut evidence);
            }
        }
        Item::Trait(value) => {
            for member in &value.items {
                append(trait_item_attrs(member), &mut evidence);
            }
        }
        Item::Impl(value) => {
            for member in &value.items {
                let attrs: &[Attribute] = match member {
                    syn::ImplItem::Const(value) => &value.attrs,
                    syn::ImplItem::Fn(value) => &value.attrs,
                    syn::ImplItem::Type(value) => &value.attrs,
                    syn::ImplItem::Macro(value) => &value.attrs,
                    _ => &[],
                };
                append(attrs, &mut evidence);
            }
        }
        Item::ForeignMod(value) => {
            for member in &value.items {
                append(foreign_item_attrs(member), &mut evidence);
            }
        }
        _ => {}
    }
    evidence.sort();
    evidence.dedup();
    evidence
}

fn custom_cfg_can_affect_external_surface(item: &Item, proc_macro_crate: bool) -> bool {
    let Item::Fn(function) = item else {
        return true;
    };
    is_public(&function.vis)
        || proc_macro_crate
        || !binary_exports(item, true).is_empty()
        || transforming_attrs(&function.attrs).next().is_some()
        || !transforming_cfg_attrs(&function.attrs).is_empty()
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

fn item_attrs_mut(item: &mut Item) -> Option<&mut Vec<Attribute>> {
    match item {
        Item::Const(value) => Some(&mut value.attrs),
        Item::Enum(value) => Some(&mut value.attrs),
        Item::ExternCrate(value) => Some(&mut value.attrs),
        Item::Fn(value) => Some(&mut value.attrs),
        Item::ForeignMod(value) => Some(&mut value.attrs),
        Item::Impl(value) => Some(&mut value.attrs),
        Item::Macro(value) => Some(&mut value.attrs),
        Item::Mod(value) => Some(&mut value.attrs),
        Item::Static(value) => Some(&mut value.attrs),
        Item::Struct(value) => Some(&mut value.attrs),
        Item::Trait(value) => Some(&mut value.attrs),
        Item::TraitAlias(value) => Some(&mut value.attrs),
        Item::Type(value) => Some(&mut value.attrs),
        Item::Union(value) => Some(&mut value.attrs),
        Item::Use(value) => Some(&mut value.attrs),
        _ => None,
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
    entry.kind == RevisionEntryKind::RegularFile
        && matches!(
            entry.state,
            RevisionEntryState::Present
                | RevisionEntryState::Added
                | RevisionEntryState::RenamedFrom { .. }
        )
}

fn is_live_symlink_entry(entry: &RevisionEntry) -> bool {
    (entry.kind == RevisionEntryKind::Symlink
        && matches!(
            entry.state,
            RevisionEntryState::Present
                | RevisionEntryState::Added
                | RevisionEntryState::RenamedFrom { .. }
        ))
        || matches!(
            entry.state,
            RevisionEntryState::NonRegular {
                kind: RevisionEntryKind::Symlink
            }
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

fn toml_string_array(table: &toml::Table, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(format!("workspace.{key} must be an array of strings"));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("workspace.{key} must contain only strings"))
        })
        .collect()
}

fn parent_manifest_dir(manifest_path: &str) -> String {
    Path::new(manifest_path)
        .parent()
        .and_then(|parent| parent.to_str())
        .unwrap_or("")
        .replace('\\', "/")
}

fn cargo_authority_manifest_paths(
    parsed: &BTreeMap<String, toml::Value>,
    allowed: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    if allowed.is_empty() {
        return Err("no product crate authority".to_owned());
    }
    for manifest in allowed {
        if !parsed.contains_key(manifest) {
            return Err(format!(
                "product manifest {manifest} is unreadable or invalid"
            ));
        }
    }

    let is_workspace =
        |manifest: &toml::Value| manifest.get("workspace").is_some_and(toml::Value::is_table);
    let workspace_authorities = if parsed.get("Cargo.toml").is_some_and(&is_workspace) {
        vec!["Cargo.toml".to_owned()]
    } else {
        let declared = allowed
            .iter()
            .filter_map(|path| {
                let workspace = parsed
                    .get(path)?
                    .get("package")?
                    .as_table()?
                    .get("workspace")?
                    .as_str()?;
                let workspace_dir =
                    safe_join_repo_path(&parent_manifest_dir(path), workspace).ok()?;
                let workspace_manifest = safe_join_repo_path(&workspace_dir, "Cargo.toml").ok()?;
                parsed
                    .get(&workspace_manifest)
                    .is_some_and(&is_workspace)
                    .then_some(workspace_manifest)
            })
            .collect::<BTreeSet<_>>();
        if !declared.is_empty() {
            declared.into_iter().collect()
        } else {
            parsed
                .iter()
                .filter(|(path, manifest)| {
                    if !is_workspace(manifest) {
                        return false;
                    }
                    if allowed.contains(*path) {
                        return true;
                    }
                    let workspace_dir = parent_manifest_dir(path);
                    !allowed.is_empty()
                        && allowed.iter().all(|product_manifest| {
                            let product_dir = parent_manifest_dir(product_manifest);
                            workspace_dir.is_empty()
                                || Path::new(&product_dir).starts_with(Path::new(&workspace_dir))
                        })
                })
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>()
        }
    };
    if workspace_authorities.is_empty() {
        let package_authorities = allowed
            .iter()
            .filter(|path| {
                parsed
                    .get(*path)
                    .and_then(|manifest| manifest.get("package"))
                    .is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        if package_authorities.len() != 1 {
            return Err(format!(
                "expected one product package lock authority, found {}",
                package_authorities.len()
            ));
        }
        Ok(package_authorities)
    } else {
        if workspace_authorities.len() != 1 {
            return Err(format!(
                "expected one product workspace lock authority, found {}",
                workspace_authorities.len()
            ));
        }
        Ok(workspace_authorities)
    }
}

/// Resolve the lockfile that Cargo actually owns for the selected product
/// authority. A lockfile from an unrelated nested fixture must never qualify a
/// revision proof for the product workspace/package.
fn effective_cargo_lock_paths(
    inventory: &BTreeMap<String, RevisionEntry>,
    parsed: &BTreeMap<String, toml::Value>,
    allowed: &BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    let authorities = cargo_authority_manifest_paths(parsed, allowed)?;

    authorities
        .into_iter()
        .map(|manifest| {
            let lock = safe_join_repo_path(&parent_manifest_dir(&manifest), "Cargo.lock")?;
            if !inventory.get(&lock).is_some_and(is_live_regular_entry) {
                return Err(format!(
                    "no revision-backed effective Cargo.lock for {manifest} (expected {lock})"
                ));
            }
            Ok(lock)
        })
        .collect()
}

fn cargo_config_paths_for_contexts(
    inventory: &BTreeMap<String, RevisionEntry>,
    locks: &BTreeSet<String>,
    manifests: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut configs = BTreeSet::new();
    for context in locks.iter().chain(manifests) {
        let mut dir = parent_repo_path(context);
        loop {
            for name in [".cargo/config", ".cargo/config.toml"] {
                let path = safe_join_repo_path(&dir, name)
                    .expect("a normalized repository directory stays inside the repository");
                if inventory.contains_key(&path) {
                    configs.insert(path);
                }
            }
            if dir.is_empty() {
                break;
            }
            dir = parent_repo_path(&dir);
        }
    }
    configs
}

fn reject_unresolved_cargo_source_overrides(
    source: &dyn RevisionFileSource,
    inventory: &BTreeMap<String, RevisionEntry>,
    parsed: &BTreeMap<String, toml::Value>,
    relevant_manifests: &BTreeSet<String>,
    locks: &BTreeSet<String>,
) -> Result<(), String> {
    if relevant_manifests.iter().any(|path| {
        parsed.get(path).is_some_and(|manifest| {
            manifest.get("patch").is_some() || manifest.get("replace").is_some()
        })
    }) {
        return Err("Cargo patch/replace dependency provenance is unresolved".to_owned());
    }

    for config in cargo_config_paths_for_contexts(inventory, locks, relevant_manifests) {
        if !inventory.get(&config).is_some_and(is_live_regular_entry) {
            return Err(format!(
                "Cargo source configuration {config} is not a regular file"
            ));
        }
        let RevisionRead::Bytes(bytes) = source
            .read(&config)
            .map_err(|error| format!("cannot read Cargo source configuration {config}: {error}"))?
        else {
            return Err(format!("cannot read Cargo source configuration {config}"));
        };
        let text = String::from_utf8(bytes.bytes)
            .map_err(|_| format!("Cargo source configuration {config} is not UTF-8"))?;
        let config_value: toml::Value = toml::from_str(&text)
            .map_err(|error| format!("Cargo source configuration {config} is invalid: {error}"))?;
        if ["patch", "replace", "source", "paths"]
            .into_iter()
            .any(|key| config_value.get(key).is_some())
        {
            return Err(format!(
                "Cargo source replacement in {config} has unresolved dependency provenance"
            ));
        }
    }
    Ok(())
}

fn reject_unresolved_revision_symlinks(
    inventory: &BTreeMap<String, RevisionEntry>,
) -> Result<(), String> {
    if let Some((path, _)) = inventory.iter().find(|(_, entry)| {
        let symlink = super::revision_source::RevisionEntryKind::Symlink;
        (entry.kind == symlink
            && !matches!(
                entry.state,
                super::revision_source::RevisionEntryState::Deleted
                    | super::revision_source::RevisionEntryState::Renamed { .. }
            ))
            || matches!(
                entry.state,
                super::revision_source::RevisionEntryState::NonRegular { kind } if kind == symlink
            )
    }) {
        return Err(format!(
            "tracked symlink {path} has unresolved target provenance"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExternalDependencySpec {
    package: String,
    version_requirement: Option<String>,
    source: ExternalDependencySource,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ExternalDependencySource {
    DefaultRegistry,
    AlternateRegistry(String),
    Git {
        url: String,
        selector: Option<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LockedExternalPackage {
    version: semver::Version,
    source: String,
    checksum: Option<String>,
}

fn cargo_lock_external_package_versions(
    source: &dyn RevisionFileSource,
    locks: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<LockedExternalPackage>>, String> {
    let mut packages_by_name = BTreeMap::<String, BTreeSet<LockedExternalPackage>>::new();
    for lock in locks {
        let RevisionRead::Bytes(bytes) = source
            .read(lock)
            .map_err(|error| format!("cannot read effective lock {lock}: {error}"))?
        else {
            return Err(format!("cannot read effective lock {lock}"));
        };
        let text = String::from_utf8(bytes.bytes)
            .map_err(|_| format!("effective lock {lock} is not UTF-8"))?;
        let value: toml::Value = toml::from_str(&text)
            .map_err(|error| format!("effective lock {lock} is invalid: {error}"))?;
        let Some(packages) = value.get("package").and_then(toml::Value::as_array) else {
            continue;
        };
        for package in packages {
            let Some(package) = package.as_table() else {
                return Err(format!(
                    "effective lock {lock} contains a package that is not a table"
                ));
            };
            let Some(name) = package.get("name").and_then(toml::Value::as_str) else {
                return Err(format!(
                    "effective lock {lock} contains a package without a string name"
                ));
            };
            let Some(version) = package.get("version").and_then(toml::Value::as_str) else {
                return Err(format!(
                    "effective lock {lock} contains package {name} without a string version"
                ));
            };
            let version = semver::Version::parse(version).map_err(|error| {
                format!("effective lock {lock} contains invalid version for {name}: {error}")
            })?;
            let Some(source_value) = package.get("source") else {
                // Cargo omits `source` for workspace/path packages. Such a
                // same-name local package must never prove that an external
                // registry/git dependency is present in the effective lock.
                continue;
            };
            let Some(package_source) = source_value.as_str().filter(|value| !value.is_empty())
            else {
                return Err(format!(
                    "effective lock {lock} contains package {name} without a valid source"
                ));
            };
            let checksum = match package.get("checksum") {
                Some(value) => Some(
                    value
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            format!(
                                "effective lock {lock} contains package {name} with an invalid checksum"
                            )
                        })?
                        .to_owned(),
                ),
                None => None,
            };
            packages_by_name
                .entry(name.to_owned())
                .or_default()
                .insert(LockedExternalPackage {
                    version,
                    source: package_source.to_owned(),
                    checksum,
                });
        }
    }
    Ok(packages_by_name)
}

fn format_external_dependency(dependency: &ExternalDependencySpec) -> String {
    match dependency.version_requirement.as_deref() {
        Some(requirement) => format!("{} {requirement}", dependency.package),
        None => dependency.package.clone(),
    }
}

fn external_dependency_is_locked(
    dependency: &ExternalDependencySpec,
    locked_packages: &BTreeMap<String, BTreeSet<LockedExternalPackage>>,
) -> Result<bool, String> {
    let Some(packages) = locked_packages.get(&dependency.package) else {
        return Ok(false);
    };
    let version_requirement = dependency
        .version_requirement
        .as_deref()
        .map(semver::VersionReq::parse)
        .transpose()
        .map_err(|error| {
            format!(
                "dependency {} has an invalid version requirement: {error}",
                dependency.package
            )
        })?;
    let source_matches = |locked: &LockedExternalPackage| -> Result<bool, String> {
        match &dependency.source {
            ExternalDependencySource::DefaultRegistry => {
                Ok(matches!(
                    locked.source.as_str(),
                    "registry+https://github.com/rust-lang/crates.io-index"
                        | "registry+https://index.crates.io/"
                ) && locked.checksum.as_deref().is_some_and(|checksum| {
                    checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
                }))
            }
            ExternalDependencySource::AlternateRegistry(name) => Err(format!(
                "alternate registry {name:?} has unresolved index provenance"
            )),
            ExternalDependencySource::Git { url, selector } => {
                let Some(git_source) = locked.source.strip_prefix("git+") else {
                    return Ok(false);
                };
                let Some((without_commit, precise)) = git_source.rsplit_once('#') else {
                    return Ok(false);
                };
                if !matches!(precise.len(), 40 | 64)
                    || !precise.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Ok(false);
                }
                let (locked_url, query) = without_commit
                    .split_once('?')
                    .map_or((without_commit, None), |(url, query)| (url, Some(query)));
                if locked_url.trim_end_matches('/') != url.trim_end_matches('/') {
                    return Ok(false);
                }
                Ok(match selector {
                    None => query.is_none(),
                    Some((key, value)) => query.is_some_and(|query| {
                        query.split('&').any(|pair| {
                            pair.split_once('=')
                                .is_some_and(|(found_key, found_value)| {
                                    found_key == key
                                        && percent_decode_query_component(found_value)
                                            .is_some_and(|decoded| decoded == *value)
                                })
                        })
                    }),
                })
            }
        }
    };
    for locked in packages {
        if version_requirement
            .as_ref()
            .is_none_or(|requirement| requirement.matches(&locked.version))
            && source_matches(locked)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn percent_decode_query_component(value: &str) -> Option<String> {
    fn hex_digit(value: u8) -> Option<u8> {
        match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            b'A'..=b'F' => Some(value - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_digit(*bytes.get(index + 1)?)?;
            let low = hex_digit(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn ensure_external_dependencies_locked(
    source: &dyn RevisionFileSource,
    locks: &BTreeSet<String>,
    dependencies: &BTreeSet<ExternalDependencySpec>,
) -> Result<(), String> {
    let locked_packages = cargo_lock_external_package_versions(source, locks)?;
    let mut missing = Vec::new();
    for dependency in dependencies {
        if !external_dependency_is_locked(dependency, &locked_packages)? {
            missing.push(format_external_dependency(dependency));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "effective Cargo.lock does not contain dependency candidates: {}",
            missing.join(", ")
        ))
    }
}

fn external_dependencies_for_manifests(
    parsed: &BTreeMap<String, toml::Value>,
    manifests: &BTreeSet<String>,
    authorities: &[String],
) -> Result<BTreeSet<ExternalDependencySpec>, String> {
    let mut workspace_specs = BTreeMap::new();
    for authority in authorities {
        let manifest = parsed
            .get(authority)
            .ok_or_else(|| format!("Cargo authority {authority} is unreadable or invalid"))?;
        for (name, dependency) in workspace_external_dependencies(manifest)
            .map_err(|error| format!("{authority}: {error}"))?
        {
            if let Some(existing) = workspace_specs.get(&name)
                && existing != &dependency
            {
                return Err(format!(
                    "workspace dependency {name} has competing package authorities"
                ));
            }
            workspace_specs.insert(name, dependency);
        }
    }
    let mut external = BTreeSet::new();
    for path in manifests {
        let manifest = parsed
            .get(path)
            .ok_or_else(|| format!("dependency manifest {path} is unreadable or invalid"))?;
        external.extend(
            manifest_external_dependencies(manifest, &workspace_specs)
                .map_err(|error| format!("{path}: {error}"))?,
        );
    }
    Ok(external)
}

fn external_dependency_source(
    spec: &toml::Table,
    context: &str,
) -> Result<ExternalDependencySource, String> {
    let git = spec
        .get("git")
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("{context}.git must be a non-empty string"))
        })
        .transpose()?;
    let registry = spec
        .get("registry")
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("{context}.registry must be a non-empty string"))
        })
        .transpose()?;
    if git.is_some() && registry.is_some() {
        return Err(format!("{context} cannot declare both git and registry"));
    }
    if let Some(url) = git {
        let selectors = ["rev", "tag", "branch"]
            .into_iter()
            .filter_map(|key| spec.get(key).map(|value| (key, value)))
            .map(|(key, value)| {
                value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(|value| (key.to_owned(), value.to_owned()))
                    .ok_or_else(|| format!("{context}.{key} must be a non-empty string"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if selectors.len() > 1 {
            return Err(format!(
                "{context} must declare at most one of rev, tag, or branch"
            ));
        }
        return Ok(ExternalDependencySource::Git {
            url,
            selector: selectors.into_iter().next(),
        });
    }
    if let Some(registry) = registry {
        return Ok(ExternalDependencySource::AlternateRegistry(registry));
    }
    Ok(ExternalDependencySource::DefaultRegistry)
}

fn workspace_external_dependencies(
    workspace_manifest: &toml::Value,
) -> Result<BTreeMap<String, Option<ExternalDependencySpec>>, String> {
    let Some(dependencies) = workspace_manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("dependencies"))
    else {
        return Ok(BTreeMap::new());
    };
    let Some(dependencies) = dependencies.as_table() else {
        return Err("workspace.dependencies must be a table".to_owned());
    };
    dependencies
        .iter()
        .map(|(name, dependency)| {
            if let Some(requirement) = dependency.as_str() {
                return Ok((
                    name.clone(),
                    Some(ExternalDependencySpec {
                        package: name.clone(),
                        version_requirement: Some(requirement.to_owned()),
                        source: ExternalDependencySource::DefaultRegistry,
                    }),
                ));
            }
            let Some(spec) = dependency.as_table() else {
                return Err(format!(
                    "workspace.dependencies.{name} must be a string or table"
                ));
            };
            if spec.get("path").is_some() {
                return Ok((name.clone(), None));
            }
            let package = match spec.get("package") {
                Some(package) => package
                    .as_str()
                    .ok_or_else(|| {
                        format!("workspace.dependencies.{name}.package must be a string")
                    })?
                    .to_owned(),
                None => name.clone(),
            };
            let version_requirement = match spec.get("version") {
                Some(version) => Some(
                    version
                        .as_str()
                        .ok_or_else(|| {
                            format!("workspace.dependencies.{name}.version must be a string")
                        })?
                        .to_owned(),
                ),
                None => None,
            };
            Ok((
                name.clone(),
                Some(ExternalDependencySpec {
                    package,
                    version_requirement,
                    source: external_dependency_source(
                        spec,
                        &format!("workspace.dependencies.{name}"),
                    )?,
                }),
            ))
        })
        .collect()
}

fn manifest_external_dependencies(
    manifest: &toml::Value,
    workspace_dependencies: &BTreeMap<String, Option<ExternalDependencySpec>>,
) -> Result<BTreeSet<ExternalDependencySpec>, String> {
    fn collect(
        value: Option<&toml::Value>,
        context: &str,
        workspace_dependencies: &BTreeMap<String, Option<ExternalDependencySpec>>,
        dependencies: &mut BTreeSet<ExternalDependencySpec>,
    ) -> Result<(), String> {
        let Some(value) = value else {
            return Ok(());
        };
        let Some(table) = value.as_table() else {
            return Err(format!("{context} must be a dependency table"));
        };
        for (name, dependency) in table {
            if let Some(requirement) = dependency.as_str() {
                dependencies.insert(ExternalDependencySpec {
                    package: name.clone(),
                    version_requirement: Some(requirement.to_owned()),
                    source: ExternalDependencySource::DefaultRegistry,
                });
                continue;
            }
            let Some(spec) = dependency.as_table() else {
                return Err(format!("{context}.{name} must be a string or table"));
            };
            if spec.get("path").is_some() {
                continue;
            }
            if spec.get("workspace").is_some() {
                if spec.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
                    return Err(format!("{context}.{name}.workspace must be true"));
                }
                let Some(package) = workspace_dependencies.get(name) else {
                    return Err(format!(
                        "{context}.{name} inherits a missing workspace dependency"
                    ));
                };
                if let Some(dependency) = package {
                    dependencies.insert(dependency.clone());
                }
                continue;
            }
            let package = match spec.get("package") {
                Some(package) => package
                    .as_str()
                    .ok_or_else(|| format!("{context}.{name}.package must be a string"))?,
                None => name,
            };
            let version_requirement = match spec.get("version") {
                Some(version) => Some(
                    version
                        .as_str()
                        .ok_or_else(|| format!("{context}.{name}.version must be a string"))?
                        .to_owned(),
                ),
                None => None,
            };
            dependencies.insert(ExternalDependencySpec {
                package: package.to_owned(),
                version_requirement,
                source: external_dependency_source(spec, &format!("{context}.{name}"))?,
            });
        }
        Ok(())
    }

    let mut dependencies = BTreeSet::new();
    for key in ["dependencies", "build-dependencies"] {
        collect(
            manifest.get(key),
            key,
            workspace_dependencies,
            &mut dependencies,
        )?;
    }
    if let Some(targets) = manifest.get("target") {
        let Some(targets) = targets.as_table() else {
            return Err("target must be a table".to_owned());
        };
        for (target_name, target) in targets {
            let Some(target) = target.as_table() else {
                return Err(format!("target.{target_name} must be a table"));
            };
            for key in ["dependencies", "build-dependencies"] {
                collect(
                    target.get(key),
                    &format!("target.{target_name}.{key}"),
                    workspace_dependencies,
                    &mut dependencies,
                )?;
            }
        }
    }
    Ok(dependencies)
}

fn normalize_cargo_relative_path(value: &str) -> Result<String, String> {
    let windows_prefix = value.len() >= 2
        && value.as_bytes()[1] == b':'
        && value.as_bytes()[0].is_ascii_alphabetic();
    let path = Path::new(value);
    if path.is_absolute() || value.starts_with('\\') || windows_prefix {
        return Err(format!(
            "invalid Cargo workspace path {value:?}: absolute paths are unsupported"
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(format!(
                        "invalid Cargo workspace path {value:?}: non-UTF-8 component"
                    ));
                };
                parts.push(part.to_owned());
            }
            std::path::Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_owned());
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "invalid Cargo workspace path {value:?}: absolute paths are unsupported"
                ));
            }
        }
    }
    Ok(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

fn cargo_glob_matches(pattern: &str, path: &str) -> bool {
    let Ok(pattern) = normalize_cargo_relative_path(pattern) else {
        return false;
    };
    let Ok(path) = normalize_cargo_relative_path(path) else {
        return false;
    };
    glob::Pattern::new(&pattern).is_ok_and(|pattern| {
        pattern.matches_path_with(
            Path::new(&path),
            glob::MatchOptions {
                require_literal_separator: true,
                ..glob::MatchOptions::new()
            },
        )
    })
}

fn cargo_pattern_has_meta(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

fn cargo_path_prefix(prefix: &str, path: &str) -> bool {
    let (Ok(prefix), Ok(path)) = (
        normalize_cargo_relative_path(prefix),
        normalize_cargo_relative_path(path),
    ) else {
        return false;
    };
    Path::new(&path).starts_with(Path::new(&prefix))
}

/// Cargo expands `workspace.members` as glob patterns, but applies `exclude`
/// as lexical path prefixes. An explicitly named member wins over an exclude
/// prefix; a member admitted only by a glob does not.
fn cargo_workspace_path_is_excluded(
    relative: &str,
    members: &[String],
    exclude: &[String],
) -> bool {
    let excluded = exclude
        .iter()
        .any(|prefix| cargo_path_prefix(prefix, relative));
    let explicitly_named = members
        .iter()
        .any(|member| cargo_path_prefix(member, relative));
    excluded && !explicitly_named
}

fn cargo_workspace_member_is_selected(
    relative: &str,
    members: &[String],
    exclude: &[String],
) -> bool {
    members
        .iter()
        .any(|pattern| cargo_glob_matches(pattern, relative))
        && !cargo_workspace_path_is_excluded(relative, members, exclude)
}

fn manifest_dependency_refs(
    manifest: &toml::Value,
    include_dev_dependencies: bool,
) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    fn collect(
        value: Option<&toml::Value>,
        context: &str,
        paths: &mut BTreeSet<String>,
        inherited: &mut BTreeSet<String>,
    ) -> Result<(), String> {
        let Some(value) = value else {
            return Ok(());
        };
        let Some(table) = value.as_table() else {
            return Err(format!("{context} must be a dependency table"));
        };
        for (name, dependency) in table {
            if dependency.is_str() {
                continue;
            }
            let Some(dependency) = dependency.as_table() else {
                return Err(format!("{context}.{name} must be a string or table"));
            };
            let path = match dependency.get("path") {
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| format!("{context}.{name}.path must be a string"))?,
                ),
                None => None,
            };
            let from_workspace = match dependency.get("workspace") {
                Some(value) => {
                    if value.as_bool() != Some(true) {
                        return Err(format!("{context}.{name}.workspace must be true"));
                    }
                    true
                }
                None => false,
            };
            if path.is_some() && from_workspace {
                return Err(format!(
                    "{context}.{name} cannot declare both path and workspace=true"
                ));
            }
            if let Some(path) = path {
                paths.insert(path.to_owned());
            } else if from_workspace {
                inherited.insert(name.to_owned());
            }
        }
        Ok(())
    }

    let mut paths = BTreeSet::new();
    let mut inherited = BTreeSet::new();
    let dependency_tables: &[&str] = if include_dev_dependencies {
        &["dependencies", "dev-dependencies", "build-dependencies"]
    } else {
        &["dependencies", "build-dependencies"]
    };
    for key in dependency_tables {
        collect(manifest.get(*key), key, &mut paths, &mut inherited)?;
    }
    if let Some(targets) = manifest.get("target") {
        let Some(targets) = targets.as_table() else {
            return Err("target must be a table".to_owned());
        };
        for (target_name, target) in targets {
            let Some(target) = target.as_table() else {
                return Err(format!("target.{target_name} must be a table"));
            };
            for key in dependency_tables {
                collect(
                    target.get(*key),
                    &format!("target.{target_name}.{key}"),
                    &mut paths,
                    &mut inherited,
                )?;
            }
        }
    }
    Ok((paths, inherited))
}

fn workspace_dependency_paths(
    workspace_manifest: &toml::Value,
) -> Result<BTreeMap<String, Option<String>>, String> {
    let Some(workspace) = workspace_manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
    else {
        return Ok(BTreeMap::new());
    };
    let Some(dependencies) = workspace.get("dependencies") else {
        return Ok(BTreeMap::new());
    };
    let Some(dependencies) = dependencies.as_table() else {
        return Err("workspace.dependencies must be a table".to_owned());
    };
    dependencies
        .iter()
        .map(|(name, dependency)| {
            if dependency.is_str() {
                return Ok((name.clone(), None));
            }
            let Some(dependency) = dependency.as_table() else {
                return Err(format!(
                    "workspace.dependencies.{name} must be a string or table"
                ));
            };
            let path = match dependency.get("path") {
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| {
                            format!("workspace.dependencies.{name}.path must be a string")
                        })?
                        .to_owned(),
                ),
                None => None,
            };
            Ok((name.clone(), path))
        })
        .collect()
}

fn workspace_relative_dir(workspace_dir: &str, package_dir: &str) -> Option<String> {
    if workspace_dir.is_empty() {
        Some(package_dir.to_owned())
    } else if package_dir == workspace_dir {
        Some(String::new())
    } else {
        package_dir
            .strip_prefix(&format!("{workspace_dir}/"))
            .map(str::to_owned)
    }
}

fn repo_relative_dir(base_dir: &str, target_dir: &str) -> String {
    let base: Vec<_> = base_dir
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let target: Vec<_> = target_dir
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let common = base
        .iter()
        .zip(&target)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = vec![".."; base.len() - common];
    relative.extend_from_slice(&target[common..]);
    if relative.is_empty() {
        ".".to_owned()
    } else {
        relative.join("/")
    }
}

fn live_inventory_directories(inventory: &BTreeMap<String, RevisionEntry>) -> BTreeSet<String> {
    let mut directories = BTreeSet::new();
    for (path, entry) in inventory {
        let live = matches!(
            entry.state,
            super::revision_source::RevisionEntryState::Present
                | super::revision_source::RevisionEntryState::Added
                | super::revision_source::RevisionEntryState::RenamedFrom { .. }
        );
        if !live || entry.kind == super::revision_source::RevisionEntryKind::Tree {
            continue;
        }
        let mut parent = Path::new(path).parent();
        while let Some(directory_path) = parent {
            let Some(directory) = directory_path.to_str() else {
                break;
            };
            let directory = directory.replace('\\', "/");
            if directory.is_empty() {
                break;
            }
            directories.insert(directory);
            parent = directory_path.parent();
        }
        if matches!(
            entry.kind,
            super::revision_source::RevisionEntryKind::Gitlink
                | super::revision_source::RevisionEntryKind::Symlink
        ) {
            directories.insert(path.clone());
        }
    }
    directories
}

#[derive(Debug, Clone, Copy)]
enum CargoMembershipSource {
    ExplicitMember,
    PathDependency,
}

fn package_belongs_to_workspace(
    manifest_path: &str,
    manifest: &toml::Value,
    workspace_dir: &str,
    parsed: &[(&str, toml::Value)],
    source: CargoMembershipSource,
) -> Result<bool, String> {
    let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else {
        return Err("path dependency manifest has no valid package table".to_owned());
    };
    let manifest_dir = parent_manifest_dir(manifest_path);
    let owns_workspace = match manifest.get("workspace") {
        Some(workspace) if workspace.as_table().is_some() => true,
        Some(_) => return Err("workspace must be a table".to_owned()),
        None => false,
    };
    if owns_workspace && package.contains_key("workspace") {
        return Err("package cannot define both [workspace] and package.workspace".to_owned());
    }
    if owns_workspace {
        return Ok(manifest_dir == workspace_dir);
    }
    if let Some(workspace) = package.get("workspace") {
        let Some(workspace) = workspace.as_str() else {
            return Err("package.workspace must be a string".to_owned());
        };
        let declared_workspace = safe_join_repo_path(&manifest_dir, workspace)
            .map_err(|error| format!("package.workspace cannot be resolved: {error}"))?;
        let declared_manifest = safe_join_repo_path(&declared_workspace, "Cargo.toml")
            .map_err(|error| format!("package.workspace has no valid Cargo.toml: {error}"))?;
        let authority = parsed
            .iter()
            .find(|(path, _)| *path == declared_manifest)
            .map(|(_, manifest)| manifest)
            .ok_or_else(|| {
                format!("package.workspace points to unavailable {declared_manifest}")
            })?;
        if authority
            .get("workspace")
            .and_then(toml::Value::as_table)
            .is_none()
        {
            return Err(format!(
                "package.workspace target {declared_manifest} has no valid workspace table"
            ));
        }
        return Ok(declared_workspace == workspace_dir);
    }

    if matches!(source, CargoMembershipSource::PathDependency)
        && workspace_relative_dir(workspace_dir, &manifest_dir).is_some()
    {
        return Ok(true);
    }

    let nearest_workspace = parsed
        .iter()
        .filter(|(path, manifest)| {
            manifest.get("workspace").is_some()
                && workspace_relative_dir(&parent_manifest_dir(path), &manifest_dir).is_some()
        })
        .max_by_key(|(path, _)| parent_manifest_dir(path).len());
    match nearest_workspace {
        Some((path, manifest)) => {
            if manifest
                .get("workspace")
                .and_then(toml::Value::as_table)
                .is_none()
            {
                return Err(format!(
                    "nearest workspace authority {path} has no valid workspace table"
                ));
            }
            Ok(parent_manifest_dir(path) == workspace_dir)
        }
        None if matches!(source, CargoMembershipSource::PathDependency) => Ok(false),
        None => Err(format!(
            "{manifest_path} is outside workspace {workspace_dir:?} and declares no resolvable workspace authority"
        )),
    }
}

fn rootless_package_is_proven_non_competing(
    manifest_path: &str,
    manifest: &toml::Value,
    workspace_dir: &str,
    exclude: &[String],
    parsed: &[(&str, toml::Value)],
) -> bool {
    let package_dir = parent_manifest_dir(manifest_path);
    if let Some(relative) = workspace_relative_dir(workspace_dir, &package_dir) {
        // A package physically below the selected workspace is a nested
        // fixture/tool unless Cargo selected it through members or a path
        // dependency. An explicit exclude is the stronger form of the same
        // proof. Neither is a second rootless authority.
        return !relative.is_empty()
            || exclude
                .iter()
                .any(|prefix| cargo_path_prefix(prefix, &relative));
    }
    matches!(
        package_belongs_to_workspace(
            manifest_path,
            manifest,
            workspace_dir,
            parsed,
            CargoMembershipSource::ExplicitMember,
        ),
        Ok(false)
    )
}

fn validate_declared_package_workspace(
    manifest_path: &str,
    manifest: &toml::Value,
    parsed: &[(&str, toml::Value)],
) -> Result<(), String> {
    let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else {
        return Err("manifest has no valid package table".to_owned());
    };
    let Some(workspace) = package.get("workspace") else {
        return Ok(());
    };
    let Some(workspace) = workspace.as_str() else {
        return Err("package.workspace must be a string".to_owned());
    };
    let package_dir = parent_manifest_dir(manifest_path);
    let workspace_dir = safe_join_repo_path(&package_dir, workspace)
        .map_err(|error| format!("package.workspace cannot be resolved: {error}"))?;
    let workspace_manifest_path = safe_join_repo_path(&workspace_dir, "Cargo.toml")
        .map_err(|error| format!("package.workspace has no valid Cargo.toml path: {error}"))?;
    let workspace_manifest = parsed
        .iter()
        .find(|(path, _)| *path == workspace_manifest_path)
        .map(|(_, manifest)| manifest)
        .ok_or_else(|| {
            format!("package.workspace points to unavailable {workspace_manifest_path}")
        })?;
    let workspace_table = workspace_manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| {
            format!(
                "package.workspace target {workspace_manifest_path} has no valid workspace table"
            )
        })?;
    let members = toml_string_array(workspace_table, "members")?;
    let exclude = toml_string_array(workspace_table, "exclude")?;
    for member in &members {
        let normalized = normalize_cargo_relative_path(member)?;
        glob::Pattern::new(&normalized).map_err(|error| {
            format!("workspace.members contains invalid glob {member:?}: {error}")
        })?;
    }
    for excluded in &exclude {
        normalize_cargo_relative_path(excluded)?;
    }
    Ok(())
}

fn include_unavailable_explicit_members(
    manifests: &[String],
    inventory_dirs: &BTreeSet<String>,
    unresolved: &[&str],
    workspace_dir: &str,
    members: &[String],
    exclude: &[String],
    allowed: &mut BTreeSet<String>,
) -> BTreeSet<String> {
    let manifest_paths: BTreeSet<_> = manifests.iter().map(String::as_str).collect();
    let mut errors = BTreeSet::new();

    for manifest_path in unresolved {
        let package_dir = parent_manifest_dir(manifest_path);
        let relative = repo_relative_dir(workspace_dir, &package_dir);
        if cargo_workspace_member_is_selected(&relative, members, exclude) {
            allowed.insert((*manifest_path).to_owned());
            errors.insert(format!(
                "{manifest_path}: explicit workspace member manifest is unreadable or invalid"
            ));
        }
    }

    for member in members
        .iter()
        .filter(|member| !cargo_pattern_has_meta(member))
    {
        let Ok(member_dir) = safe_join_repo_path(workspace_dir, member) else {
            errors.insert(format!(
                "workspace member {member} resolves outside the repository"
            ));
            continue;
        };
        let relative = repo_relative_dir(workspace_dir, &member_dir);
        if cargo_workspace_path_is_excluded(&relative, members, exclude) {
            continue;
        }
        let Ok(manifest_path) = safe_join_repo_path(&member_dir, "Cargo.toml") else {
            errors.insert(format!(
                "workspace member {member} has an invalid manifest path"
            ));
            continue;
        };
        if !manifest_paths.contains(manifest_path.as_str()) {
            errors.insert(format!("workspace member {member} has no {manifest_path}"));
        }
    }

    for member in members
        .iter()
        .filter(|member| cargo_pattern_has_meta(member))
    {
        let matched_directory = inventory_dirs.iter().any(|directory| {
            let relative = repo_relative_dir(workspace_dir, directory);
            cargo_glob_matches(member, &relative)
        });
        if !matched_directory {
            errors.insert(format!(
                "workspace member glob {member} matched no repository directory"
            ));
        }
    }

    for directory in inventory_dirs {
        let relative = repo_relative_dir(workspace_dir, directory);
        let matched_glob = members.iter().any(|pattern| {
            cargo_pattern_has_meta(pattern) && cargo_glob_matches(pattern, &relative)
        });
        if !matched_glob || cargo_workspace_path_is_excluded(&relative, members, exclude) {
            continue;
        }
        let Ok(manifest_path) = safe_join_repo_path(directory, "Cargo.toml") else {
            continue;
        };
        if !manifest_paths.contains(manifest_path.as_str()) {
            errors.insert(format!(
                "workspace member glob matched existing directory {relative} without {manifest_path}"
            ));
        }
    }

    errors
}

fn include_implicit_path_dependency_members(
    manifests: &[String],
    parsed: &[(&str, toml::Value)],
    workspace_manifest: &toml::Value,
    workspace_dir: &str,
    members: &[String],
    exclude: &[String],
    allowed: &mut BTreeSet<String>,
) -> BTreeSet<String> {
    let manifest_paths: BTreeSet<_> = manifests.iter().map(String::as_str).collect();
    let mut errors = BTreeSet::new();
    let workspace_dependencies = match workspace_dependency_paths(workspace_manifest) {
        Ok(dependencies) => dependencies,
        Err(error) => {
            errors.insert(error);
            BTreeMap::new()
        }
    };

    loop {
        let mut discovered = BTreeSet::new();
        for manifest_path in allowed.iter() {
            let Some((_, manifest)) = parsed
                .iter()
                .find(|(path, _)| *path == manifest_path.as_str())
            else {
                continue;
            };
            let manifest_dir = parent_manifest_dir(manifest_path);
            let (direct_paths, inherited) = match manifest_dependency_refs(manifest, true) {
                Ok(dependencies) => dependencies,
                Err(error) => {
                    errors.insert(format!("{manifest_path}: {error}"));
                    continue;
                }
            };
            let mut dependency_paths: Vec<_> = direct_paths
                .into_iter()
                .map(|path| (manifest_dir.clone(), path, "path dependency".to_owned()))
                .collect();
            for name in inherited {
                match workspace_dependencies.get(&name) {
                    Some(Some(path)) => dependency_paths.push((
                        workspace_dir.to_owned(),
                        path.clone(),
                        format!("workspace dependency {name}"),
                    )),
                    Some(None) => {}
                    None => {
                        errors.insert(format!(
                            "{manifest_path}: inherited workspace dependency {name} is not declared"
                        ));
                    }
                }
            }
            for (base_dir, dependency_path, dependency_kind) in dependency_paths {
                let Ok(dependency_dir) = safe_join_repo_path(&base_dir, &dependency_path) else {
                    // A path dependency outside the repository cannot be a
                    // member of this repository-backed workspace snapshot.
                    continue;
                };
                if workspace_relative_dir(workspace_dir, &dependency_dir).is_some_and(|relative| {
                    cargo_workspace_path_is_excluded(&relative, members, exclude)
                }) {
                    continue;
                }
                let Ok(dependency_manifest) = safe_join_repo_path(&dependency_dir, "Cargo.toml")
                else {
                    continue;
                };
                if !manifest_paths.contains(dependency_manifest.as_str()) {
                    errors.insert(format!(
                        "{manifest_path}: {dependency_kind} {dependency_path} has no {dependency_manifest}"
                    ));
                    continue;
                }
                let Some((_, dependency)) = parsed
                    .iter()
                    .find(|(path, _)| *path == dependency_manifest.as_str())
                else {
                    discovered.insert(dependency_manifest.clone());
                    errors.insert(format!(
                        "{manifest_path}: {dependency_kind} {dependency_path} points to unreadable or invalid {dependency_manifest}"
                    ));
                    continue;
                };
                let dependency_inside_workspace =
                    workspace_relative_dir(workspace_dir, &dependency_dir).is_some();
                match package_belongs_to_workspace(
                    &dependency_manifest,
                    dependency,
                    workspace_dir,
                    parsed,
                    CargoMembershipSource::PathDependency,
                ) {
                    Ok(true) => {
                        discovered.insert(dependency_manifest);
                    }
                    Ok(false) if dependency_inside_workspace => {
                        errors.insert(format!(
                            "{manifest_path}: {dependency_kind} {dependency_path} points to {dependency_manifest}, which declares a different package.workspace"
                        ));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        errors.insert(format!("{dependency_manifest}: {error}"));
                    }
                }
            }
        }
        let previous_len = allowed.len();
        allowed.extend(discovered);
        if allowed.len() == previous_len {
            break;
        }
    }
    errors
}

#[allow(clippy::too_many_arguments)]
fn select_workspace_authority(
    manifests: &[String],
    inventory_dirs: &BTreeSet<String>,
    parsed: &[(&str, toml::Value)],
    packages: &[&str],
    unresolved_authorities: &[&str],
    workspace_path: &str,
    workspace_dir: &str,
    members: &[String],
    exclude: &[String],
    has_package: bool,
    workspace_errors: &BTreeSet<String>,
    label: &str,
) -> (BTreeSet<String>, Option<String>) {
    if !workspace_errors.is_empty() {
        return (
            BTreeSet::new(),
            Some(format!(
                "{label} {workspace_path} is invalid: {}",
                workspace_errors
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        );
    }
    let member_patterns = if members.is_empty() {
        if has_package {
            vec![".".to_owned()]
        } else {
            Vec::new()
        }
    } else {
        members.to_vec()
    };
    let mut allowed = BTreeSet::new();
    if has_package {
        allowed.insert(workspace_path.to_owned());
    }
    let mut discovery_errors = BTreeSet::new();
    for package in packages {
        let package_dir = parent_manifest_dir(package);
        let relative = repo_relative_dir(workspace_dir, &package_dir);
        if cargo_workspace_member_is_selected(&relative, &member_patterns, exclude) {
            let package_manifest = parsed
                .iter()
                .find(|(path, _)| path == package)
                .map(|(_, manifest)| manifest)
                .expect("package path came from parsed manifests");
            match package_belongs_to_workspace(
                package,
                package_manifest,
                workspace_dir,
                parsed,
                CargoMembershipSource::ExplicitMember,
            ) {
                Ok(true) => {
                    allowed.insert((*package).to_owned());
                }
                Ok(false) => {
                    discovery_errors.insert(format!(
                        "{package}: explicit member is owned by another workspace authority"
                    ));
                }
                Err(error) => {
                    discovery_errors.insert(format!("{package}: {error}"));
                }
            }
        }
    }
    discovery_errors.extend(include_unavailable_explicit_members(
        manifests,
        inventory_dirs,
        unresolved_authorities,
        workspace_dir,
        &member_patterns,
        exclude,
        &mut allowed,
    ));
    let workspace_manifest = parsed
        .iter()
        .find(|(path, _)| *path == workspace_path)
        .map(|(_, manifest)| manifest)
        .expect("workspace path came from parsed manifests");
    discovery_errors.extend(include_implicit_path_dependency_members(
        manifests,
        parsed,
        workspace_manifest,
        workspace_dir,
        &member_patterns,
        exclude,
        &mut allowed,
    ));
    let evidence = (!discovery_errors.is_empty()).then(|| {
        format!(
            "{label} is incomplete: {}",
            discovery_errors
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        )
    });
    (allowed, evidence)
}

/// Product crates are workspace members (minus `exclude`), or the root package
/// of a non-workspace repo. Nested fixture/tool packages are not API surface.
fn api_crate_manifests(
    source: &dyn RevisionFileSource,
    manifests: &[String],
    inventory_dirs: &BTreeSet<String>,
) -> (BTreeSet<String>, Option<String>) {
    let mut parsed = Vec::new();
    let mut unresolved = Vec::new();
    for path in manifests {
        match peek_manifest_toml(source, path) {
            Some(value) => parsed.push((path.as_str(), value)),
            None => unresolved.push(path.as_str()),
        }
    }
    let mut workspaces = Vec::new();
    let mut packages = Vec::new();
    let mut semantically_invalid = Vec::new();
    for (path, manifest) in &parsed {
        let dir = parent_manifest_dir(path);
        if let Some(workspace_value) = manifest.get("workspace") {
            let mut errors = BTreeSet::new();
            if manifest
                .get("package")
                .and_then(toml::Value::as_table)
                .is_some_and(|package| package.contains_key("workspace"))
            {
                errors.insert(
                    "package.workspace cannot be specified in a manifest that defines [workspace]"
                        .to_owned(),
                );
            }
            let (members, exclude) = if let Some(workspace) = workspace_value.as_table() {
                let members = match toml_string_array(workspace, "members") {
                    Ok(members) => members,
                    Err(error) => {
                        errors.insert(error);
                        Vec::new()
                    }
                };
                let exclude = match toml_string_array(workspace, "exclude") {
                    Ok(exclude) => exclude,
                    Err(error) => {
                        errors.insert(error);
                        Vec::new()
                    }
                };
                for member in &members {
                    match normalize_cargo_relative_path(member) {
                        Ok(member) => {
                            if let Err(error) = glob::Pattern::new(&member) {
                                errors.insert(format!(
                                    "workspace.members contains invalid glob {member:?}: {error}"
                                ));
                            }
                        }
                        Err(error) => {
                            errors.insert(error);
                        }
                    }
                }
                for excluded in &exclude {
                    if let Err(error) = normalize_cargo_relative_path(excluded) {
                        errors.insert(error);
                    }
                }
                (members, exclude)
            } else {
                errors.insert("workspace must be a table".to_owned());
                (Vec::new(), Vec::new())
            };
            workspaces.push((
                *path,
                dir,
                members,
                exclude,
                manifest.get("package").is_some(),
                errors,
            ));
        }
        if manifest.get("package").is_some() {
            packages.push(*path);
        }
        if manifest.get("package").is_none() && manifest.get("workspace").is_none() {
            semantically_invalid.push(*path);
        }
    }
    let unresolved_authorities = unresolved
        .iter()
        .copied()
        .chain(semantically_invalid.iter().copied())
        .collect::<Vec<_>>();
    if manifests.iter().any(|path| path == "Cargo.toml")
        && !parsed.iter().any(|(path, _)| *path == "Cargo.toml")
    {
        // A malformed or non-UTF-8 root manifest is still the repository's
        // authority. Keep it selected so the snapshot emits its typed unknown
        // instead of silently falling through to a nested fixture package.
        return (BTreeSet::from(["Cargo.toml".to_owned()]), None);
    }
    if let Some((_, root)) = parsed.iter().find(|(path, _)| *path == "Cargo.toml") {
        if root.get("workspace").is_none() {
            return if root.get("package").is_some() {
                let declared_workspace = root
                    .get("package")
                    .and_then(toml::Value::as_table)
                    .and_then(|package| package.get("workspace"));
                if declared_workspace.is_none() {
                    (BTreeSet::from(["Cargo.toml".to_owned()]), None)
                } else if let Err(error) =
                    validate_declared_package_workspace("Cargo.toml", root, &parsed)
                {
                    (
                        BTreeSet::from(["Cargo.toml".to_owned()]),
                        Some(format!(
                            "root package workspace authority is invalid: {error}"
                        )),
                    )
                } else {
                    let workspace = declared_workspace
                        .and_then(toml::Value::as_str)
                        .expect("validated package.workspace is a string");
                    let workspace_dir = safe_join_repo_path("", workspace)
                        .expect("validated package.workspace is repository-local");
                    let workspace_path = safe_join_repo_path(&workspace_dir, "Cargo.toml")
                        .expect("validated package.workspace has a Cargo.toml path");
                    let Some((_, _, members, exclude, has_package, workspace_errors)) = workspaces
                        .iter()
                        .find(|(path, _, _, _, _, _)| *path == workspace_path)
                    else {
                        return (
                            BTreeSet::from(["Cargo.toml".to_owned()]),
                            Some(format!(
                                "root package workspace authority {workspace_path} was not classified as a workspace"
                            )),
                        );
                    };
                    let (allowed, evidence) = select_workspace_authority(
                        manifests,
                        inventory_dirs,
                        &parsed,
                        &packages,
                        &unresolved_authorities,
                        &workspace_path,
                        &workspace_dir,
                        members,
                        exclude,
                        *has_package,
                        workspace_errors,
                        "declared root package workspace discovery",
                    );
                    if allowed.contains("Cargo.toml") {
                        (allowed, evidence)
                    } else {
                        let missing = format!(
                            "declared root package workspace {workspace_path} does not select Cargo.toml as a member"
                        );
                        let evidence = Some(match evidence {
                            Some(existing) => format!("{existing}; {missing}"),
                            None => missing,
                        });
                        (allowed, evidence)
                    }
                }
            } else {
                // A parseable root manifest that is not a package or a
                // workspace is still the repository authority. Keep it
                // selected so discover_crates emits ManifestParse instead of
                // certifying an empty surface or falling through to fixtures.
                (BTreeSet::from(["Cargo.toml".to_owned()]), None)
            };
        }

        let (_, _, members, exclude, _, workspace_errors) = workspaces
            .iter()
            .find(|(path, _, _, _, _, _)| *path == "Cargo.toml")
            .expect("root workspace candidate was recorded");
        let mut allowed = BTreeSet::new();
        if root.get("package").is_some() {
            // A package declared by the workspace root is always a workspace
            // member; nested workspaces must not displace it as API authority.
            allowed.insert("Cargo.toml".to_owned());
        }
        if !workspace_errors.is_empty() {
            return (
                allowed,
                Some(format!(
                    "root workspace authority is invalid: {}",
                    workspace_errors
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )),
            );
        }
        let mut discovery_errors = BTreeSet::new();
        for package in &packages {
            if *package == "Cargo.toml" {
                continue;
            }
            let package_dir = parent_manifest_dir(package);
            let relative = repo_relative_dir("", &package_dir);
            if cargo_workspace_member_is_selected(&relative, members, exclude) {
                let package_manifest = parsed
                    .iter()
                    .find(|(path, _)| path == package)
                    .map(|(_, manifest)| manifest)
                    .expect("package path came from parsed manifests");
                match package_belongs_to_workspace(
                    package,
                    package_manifest,
                    "",
                    &parsed,
                    CargoMembershipSource::ExplicitMember,
                ) {
                    Ok(true) => {
                        allowed.insert((*package).to_owned());
                    }
                    Ok(false) => {
                        discovery_errors.insert(format!(
                            "{package}: explicit member is owned by another workspace authority"
                        ));
                    }
                    Err(error) => {
                        discovery_errors.insert(format!("{package}: {error}"));
                    }
                }
            }
        }
        discovery_errors.extend(include_unavailable_explicit_members(
            manifests,
            inventory_dirs,
            &unresolved_authorities,
            "",
            members,
            exclude,
            &mut allowed,
        ));
        discovery_errors.extend(include_implicit_path_dependency_members(
            manifests,
            &parsed,
            root,
            "",
            members,
            exclude,
            &mut allowed,
        ));
        let evidence = (!discovery_errors.is_empty()).then(|| {
            format!(
                "root workspace discovery is incomplete: {}",
                discovery_errors
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        });
        return (allowed, evidence);
    }

    if !unresolved_authorities.is_empty() {
        // Without a root Cargo.toml, every discovered manifest is a candidate
        // authority. A malformed, unreadable, non-UTF-8, or parseable but
        // semantically invalid candidate cannot be discarded in favour of
        // whichever fixture happens to parse.
        return (
            BTreeSet::new(),
            Some(format!(
                "rootless workspace authority cannot be established because manifests are unreadable or invalid: {}",
                unresolved_authorities.join(", ")
            )),
        );
    }

    // Compatibility fallback for revision sources rooted below the repository
    // root (for example a caller-provided single-crate source). A real repo with
    // Cargo.toml above never reaches this branch.
    if workspaces.is_empty() {
        return if packages.len() <= 1 {
            let evidence = packages.first().and_then(|manifest_path| {
                parsed
                    .iter()
                    .find(|(path, _)| path == manifest_path)
                    .and_then(|(_, manifest)| {
                        validate_declared_package_workspace(manifest_path, manifest, &parsed).err()
                    })
                    .map(|error| {
                        format!("rootless package workspace authority is invalid: {error}")
                    })
            });
            (packages.into_iter().map(str::to_owned).collect(), evidence)
        } else {
            (
                BTreeSet::new(),
                Some(format!(
                    "multiple package manifests without a root Cargo.toml or workspace authority: {}",
                    packages.join(", ")
                )),
            )
        };
    }
    if workspaces.len() != 1 {
        return (
            BTreeSet::new(),
            Some(format!(
                "multiple workspace authorities without a root Cargo.toml: {}",
                workspaces
                    .iter()
                    .map(|(_, dir, _, _, _, _)| { if dir.is_empty() { "." } else { dir.as_str() } })
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        );
    }
    let (workspace_path, dir, members, exclude, has_package, workspace_errors) = &workspaces[0];
    let (allowed, evidence) = select_workspace_authority(
        manifests,
        inventory_dirs,
        &parsed,
        &packages,
        &unresolved_authorities,
        workspace_path,
        dir,
        members,
        exclude,
        *has_package,
        workspace_errors,
        "rootless workspace discovery",
    );
    let competing = packages
        .iter()
        .filter(|package| !allowed.contains(**package))
        .filter(|package| {
            let manifest = parsed
                .iter()
                .find(|(path, _)| path == *package)
                .map(|(_, manifest)| manifest)
                .expect("package path came from parsed manifests");
            !rootless_package_is_proven_non_competing(package, manifest, dir, exclude, &parsed)
        })
        .copied()
        .collect::<Vec<_>>();
    if competing.is_empty() {
        (allowed, evidence)
    } else {
        let conflict = format!(
            "rootless workspace authority {workspace_path} competes with unowned package manifests: {}",
            competing.join(", ")
        );
        (
            BTreeSet::new(),
            Some(match evidence {
                Some(existing) => format!("{existing}; {conflict}"),
                None => conflict,
            }),
        )
    }
}

/// Bind opaque-return uncertainty to revision-backed Rust sources, build
/// inputs, manifests, and the effective dependency lock. The aggregate is a
/// conservative safety floor until compiler-backed hidden-type analysis exists.
fn opaque_implementation_digest(
    source: &dyn RevisionFileSource,
    inventory: &BTreeMap<String, RevisionEntry>,
    manifests: &[String],
    allowed: &BTreeSet<String>,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let parsed = manifests
        .iter()
        .filter_map(|path| peek_manifest_toml(source, path).map(|value| (path.clone(), value)))
        .collect::<BTreeMap<_, _>>();
    let locks = effective_cargo_lock_paths(inventory, &parsed, allowed)?;
    let authorities = cargo_authority_manifest_paths(&parsed, allowed)?;
    let external_dependencies =
        external_dependencies_for_manifests(&parsed, allowed, &authorities)?;
    ensure_external_dependencies_locked(source, &locks, &external_dependencies)?;
    let mut relevant_manifests = allowed.clone();
    relevant_manifests.extend(authorities);
    reject_unresolved_cargo_source_overrides(
        source,
        inventory,
        &parsed,
        &relevant_manifests,
        &locks,
    )?;
    reject_unresolved_revision_symlinks(inventory)?;

    // Rust sources are canonicalized so a non-opaque public body or a generic
    // binder rename does not manufacture uncertainty. Every other live Git
    // entry contributes its cheap object identity: include!/#[path] inputs,
    // build-script assets, and nonstandard generated-code inputs can affect an
    // opaque hidden type even when their filename has no familiar extension.
    let mut rows = Vec::new();
    for (path, entry) in inventory.iter().filter(|(_, entry)| {
        entry.kind != super::revision_source::RevisionEntryKind::Tree
            && matches!(
                entry.state,
                super::revision_source::RevisionEntryState::Present
                    | super::revision_source::RevisionEntryState::Added
                    | super::revision_source::RevisionEntryState::RenamedFrom { .. }
            )
    }) {
        let rust_source = is_live_regular_entry(entry)
            && Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("rs");
        if rust_source {
            let bytes = match source
                .read(path)
                .map_err(|error| format!("cannot read {path}: {error}"))?
            {
                RevisionRead::Bytes(bytes) => bytes.bytes,
                other => return Err(format!("cannot digest {path}: {other:?}")),
            };
            rows.push(format!(
                "{path}\0rust-sha256:{}\0{}\0{:?}",
                hex::encode(Sha256::digest(opaque_source_canonical_bytes(&bytes))),
                entry.mode,
                entry.state
            ));
        } else {
            rows.push(format!(
                "{path}\0git-object:{}\0{}\0{:?}\0{:?}",
                entry
                    .baseline_object_id
                    .as_deref()
                    .unwrap_or("overlay-only"),
                entry.mode,
                entry.kind,
                entry.state
            ));
        }
    }
    if let RevisionProvenance::WorkingTreeOverlay { dirty_digest, .. } = source.provenance() {
        rows.push(format!("working-tree-overlay\0{dirty_digest}"));
    }
    Ok(format!("sha256:{:x}", Sha256::digest(rows.join("\n"))))
}

fn macro_invocation_implementation_digest(
    proc_macro_digest: &Result<String, String>,
    opaque_digest: &Result<String, String>,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let opaque = opaque_digest
        .as_ref()
        .map_err(|reason| format!("opaque invocation substrate: {reason}"))?;
    let mut rows = vec![format!("raw-local-and-lock-substrate\0{opaque}")];
    match proc_macro_digest {
        Ok(digest) => rows.push(format!("reachable-transformer-substrate\0{digest}")),
        Err(reason) if reason == "no reachable transformer dependency candidate" => {}
        Err(reason) => return Err(format!("reachable transformer substrate: {reason}")),
    }
    Ok(format!("sha256:{:x}", Sha256::digest(rows.join("\n"))))
}

/// Bind transform uncertainty to the revision-backed implementation substrate
/// that can change proc-macro expansion. Exact attribute-to-crate resolution is
/// intentionally deferred; the aggregate is conservative across all reachable
/// local proc-macro closures and dependency identities in the product manifests.
fn proc_macro_implementation_digest(
    source: &dyn RevisionFileSource,
    inventory: &BTreeMap<String, RevisionEntry>,
    manifests: &[String],
    allowed: &BTreeSet<String>,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    let parsed: BTreeMap<String, toml::Value> = manifests
        .iter()
        .filter_map(|path| peek_manifest_toml(source, path).map(|value| (path.clone(), value)))
        .collect();
    for manifest in allowed {
        if !parsed.contains_key(manifest) {
            return Err(format!(
                "product manifest {manifest} is unreadable or invalid"
            ));
        }
    }

    let cargo_authorities = cargo_authority_manifest_paths(&parsed, allowed)?;
    let mut workspace_dependencies: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    let mut workspace_external_specs: BTreeMap<String, Option<ExternalDependencySpec>> =
        BTreeMap::new();
    for manifest_path in &cargo_authorities {
        let manifest = parsed
            .get(manifest_path)
            .expect("Cargo authority came from parsed manifests");
        let base_dir = parent_manifest_dir(manifest_path);
        for (name, path) in workspace_dependency_paths(manifest)
            .map_err(|error| format!("{manifest_path}: {error}"))?
        {
            let candidate = (base_dir.clone(), path);
            if let Some(existing) = workspace_dependencies.get(&name)
                && existing != &candidate
            {
                return Err(format!(
                    "workspace dependency {name} has competing path authorities"
                ));
            }
            workspace_dependencies.insert(name, candidate);
        }
        for (name, package) in workspace_external_dependencies(manifest)
            .map_err(|error| format!("{manifest_path}: {error}"))?
        {
            if let Some(existing) = workspace_external_specs.get(&name)
                && existing != &package
            {
                return Err(format!(
                    "workspace dependency {name} has competing package authorities"
                ));
            }
            workspace_external_specs.insert(name, package);
        }
    }

    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut reachable = allowed.clone();
    let mut queue = allowed.iter().cloned().collect::<Vec<_>>();
    while let Some(manifest_path) = queue.pop() {
        if graph.contains_key(&manifest_path) {
            continue;
        }
        let manifest = parsed.get(&manifest_path).ok_or_else(|| {
            format!("dependency manifest {manifest_path} is unreadable or invalid")
        })?;
        let manifest_dir = parent_manifest_dir(&manifest_path);
        let (direct_paths, inherited) = manifest_dependency_refs(manifest, false)
            .map_err(|error| format!("{manifest_path}: {error}"))?;
        let mut dependencies = BTreeSet::new();
        for path in direct_paths {
            let dependency_dir = safe_join_repo_path(&manifest_dir, &path).map_err(|reason| {
                format!("{manifest_path}: local path dependency {path:?}: {reason}")
            })?;
            let dependency_manifest = safe_join_repo_path(&dependency_dir, "Cargo.toml")
                .map_err(|reason| format!("{manifest_path}: {reason}"))?;
            if !parsed.contains_key(&dependency_manifest) {
                return Err(format!(
                    "{manifest_path}: local path dependency {path:?} has no readable {dependency_manifest}"
                ));
            }
            dependencies.insert(dependency_manifest);
        }
        for name in inherited {
            let Some((base_dir, Some(path))) = workspace_dependencies.get(&name) else {
                // Registry/workspace dependencies have no repo-backed source
                // closure; their manifest/lock identity is digested below.
                continue;
            };
            let dependency_dir = safe_join_repo_path(base_dir, path).map_err(|reason| {
                format!("{manifest_path}: workspace dependency {name}: {reason}")
            })?;
            let dependency_manifest = safe_join_repo_path(&dependency_dir, "Cargo.toml")
                .map_err(|reason| format!("{manifest_path}: {reason}"))?;
            if !parsed.contains_key(&dependency_manifest) {
                return Err(format!(
                    "{manifest_path}: workspace dependency {name} has no readable {dependency_manifest}"
                ));
            }
            dependencies.insert(dependency_manifest);
        }
        for dependency in &dependencies {
            if reachable.insert(dependency.clone()) {
                queue.push(dependency.clone());
            }
        }
        graph.insert(manifest_path, dependencies);
    }

    let is_proc_macro = |manifest: &toml::Value| {
        manifest
            .get("lib")
            .and_then(toml::Value::as_table)
            .is_some_and(|lib| {
                lib.get("proc-macro").and_then(toml::Value::as_bool) == Some(true)
                    || lib
                        .get("crate-type")
                        .and_then(toml::Value::as_array)
                        .is_some_and(|types| {
                            types
                                .iter()
                                .any(|value| value.as_str() == Some("proc-macro"))
                        })
            })
    };
    let mut closure: BTreeSet<String> = reachable
        .iter()
        .filter(|path| parsed.get(*path).is_some_and(&is_proc_macro))
        .cloned()
        .collect();
    let mut closure_queue = closure.iter().cloned().collect::<Vec<_>>();
    while let Some(manifest_path) = closure_queue.pop() {
        for dependency in graph.get(&manifest_path).into_iter().flatten() {
            if closure.insert(dependency.clone()) {
                closure_queue.push(dependency.clone());
            }
        }
    }

    let locks = effective_cargo_lock_paths(inventory, &parsed, allowed)?;
    let mut external_candidates = BTreeSet::new();
    for manifest_path in &reachable {
        let manifest = parsed.get(manifest_path).ok_or_else(|| {
            format!("dependency manifest {manifest_path} is unreadable or invalid")
        })?;
        external_candidates.extend(
            manifest_external_dependencies(manifest, &workspace_external_specs)
                .map_err(|error| format!("{manifest_path}: {error}"))?,
        );
    }
    if closure.is_empty() && external_candidates.is_empty() {
        return Err("no reachable transformer dependency candidate".to_owned());
    }
    ensure_external_dependencies_locked(source, &locks, &external_candidates)?;
    let mut source_authorities = reachable.clone();
    source_authorities.extend(cargo_authority_manifest_paths(&parsed, allowed)?);
    reject_unresolved_cargo_source_overrides(
        source,
        inventory,
        &parsed,
        &source_authorities,
        &locks,
    )?;
    reject_unresolved_revision_symlinks(inventory)?;

    let mut digest_paths = source_authorities.clone();
    digest_paths.extend(cargo_config_paths_for_contexts(
        inventory,
        &locks,
        &source_authorities,
    ));
    digest_paths.extend(locks);
    if !closure.is_empty() {
        // Until attribute-to-crate resolution exists, the only honest local
        // safety floor is the full revision-backed inventory. Object identities
        // capture nonstandard lib.path/#[path]/build assets outside package
        // dirs without rereading every tracked blob.
        let mut rows = inventory
            .iter()
            .filter(|(_, entry)| {
                entry.kind != super::revision_source::RevisionEntryKind::Tree
                    && matches!(
                        entry.state,
                        super::revision_source::RevisionEntryState::Present
                            | super::revision_source::RevisionEntryState::Added
                            | super::revision_source::RevisionEntryState::RenamedFrom { .. }
                    )
            })
            .map(|(path, entry)| {
                format!(
                    "{path}\0{}\0{}\0{:?}",
                    entry
                        .baseline_object_id
                        .as_deref()
                        .unwrap_or("overlay-only"),
                    entry.mode,
                    entry.state
                )
            })
            .collect::<Vec<_>>();
        if let RevisionProvenance::WorkingTreeOverlay { dirty_digest, .. } = source.provenance() {
            rows.push(format!("working-tree-overlay\0{dirty_digest}"));
        }
        return Ok(format!("sha256:{:x}", Sha256::digest(rows.join("\n"))));
    }

    let mut rows = Vec::new();
    for path in digest_paths {
        let bytes = match source
            .read(&path)
            .map_err(|error| format!("cannot read {path}: {error}"))?
        {
            RevisionRead::Bytes(bytes) => bytes.bytes,
            other => return Err(format!("cannot digest {path}: {other:?}")),
        };
        rows.push(format!("{path}\0{}", hex::encode(Sha256::digest(bytes))));
    }
    Ok(format!("sha256:{:x}", Sha256::digest(rows.join("\n"))))
}

fn private_alias_graph(
    private_uses: &[UseEdge],
    self_crate_aliases: &[SelfCrateAlias],
    declarations: &[RustApiDeclaration],
) -> (
    BTreeMap<PrivateTypeKey, Vec<GuardedPrivateTypeTarget>>,
    BTreeMap<PrivateModuleAliasKey, Vec<GuardedPrivateModuleTarget>>,
) {
    let mut private_aliases: BTreeMap<PrivateTypeKey, Vec<GuardedPrivateTypeTarget>> =
        BTreeMap::new();
    let mut private_module_aliases: BTreeMap<
        PrivateModuleAliasKey,
        Vec<GuardedPrivateModuleTarget>,
    > = BTreeMap::new();

    for edge in private_uses {
        for leaf in &edge.leaves {
            if leaf.glob {
                for target_module in use_candidate_paths(&edge.module_path, &leaf.segments) {
                    for declaration in declarations.iter().filter(|declaration| {
                        declaration.key.crate_name == edge.crate_name
                            && declaration.key.module_path.starts_with(&target_module)
                    }) {
                        let suffix = &declaration.key.module_path[target_module.len()..];
                        let mut source_module = edge.module_path.clone();
                        source_module.extend_from_slice(suffix);
                        private_aliases
                            .entry((
                                edge.crate_name.clone(),
                                source_module,
                                declaration.key.external_name.clone(),
                            ))
                            .or_default()
                            .push((
                                (
                                    edge.crate_name.clone(),
                                    declaration.key.module_path.clone(),
                                    declaration.key.external_name.clone(),
                                ),
                                edge.cfg_guard.clone(),
                            ));
                    }
                }
                continue;
            }

            let alias_name = normalize_identifier(&leaf.alias);
            for path in use_candidate_paths(&edge.module_path, &leaf.segments) {
                let Some((target_name, target_module)) = path.split_last() else {
                    continue;
                };
                private_aliases
                    .entry((
                        edge.crate_name.clone(),
                        edge.module_path.clone(),
                        alias_name.clone(),
                    ))
                    .or_default()
                    .push((
                        (
                            edge.crate_name.clone(),
                            target_module.to_vec(),
                            normalize_identifier(target_name),
                        ),
                        edge.cfg_guard.clone(),
                    ));

                let mut alias_path = edge.module_path.clone();
                alias_path.push(alias_name.clone());
                private_module_aliases
                    .entry((edge.crate_name.clone(), alias_path))
                    .or_default()
                    .push((path, edge.cfg_guard.clone()));
            }
        }
    }

    // `extern crate self as alias` binds a module path back to the current
    // crate root. It participates in private type dependency resolution even
    // when the binding itself is not public; otherwise `alias::Hidden` is
    // mistaken for an unrelated local module and a private layout/auto-trait
    // change can disappear behind stable placeholder evidence.
    for alias in self_crate_aliases {
        private_module_aliases
            .entry((alias.crate_name.clone(), alias.alias_path.clone()))
            .or_default()
            .push((Vec::new(), alias.cfg_guard.clone()));
    }

    for declaration in declarations
        .iter()
        .filter(|declaration| declaration.kind == RustApiItemKind::TypeAlias)
    {
        let Ok(Item::Type(alias)) = syn::parse_str::<Item>(&declaration.contract) else {
            continue;
        };
        let Some((target_module, target_name)) =
            resolve_impl_self_owner(&declaration.key.module_path, alias.ty.as_ref())
        else {
            continue;
        };
        let alias_key = (
            declaration.key.crate_name.clone(),
            declaration.key.module_path.clone(),
            declaration.key.external_name.clone(),
        );
        private_aliases.entry(alias_key).or_default().push((
            (
                declaration.key.crate_name.clone(),
                target_module,
                target_name,
            ),
            declaration.cfg_guard.clone(),
        ));
    }

    for targets in private_aliases.values_mut() {
        targets.sort();
        targets.dedup();
    }
    for targets in private_module_aliases.values_mut() {
        targets.sort();
        targets.dedup();
    }
    (private_aliases, private_module_aliases)
}

fn resolve_private_type_alias_keys(
    initial: PrivateTypeKey,
    guard: &[String],
    private_aliases: &BTreeMap<PrivateTypeKey, Vec<GuardedPrivateTypeTarget>>,
    private_module_aliases: &BTreeMap<PrivateModuleAliasKey, Vec<GuardedPrivateModuleTarget>>,
) -> PrivateAliasResolution {
    let node_count = private_aliases
        .len()
        .saturating_add(private_module_aliases.len())
        .saturating_add(1);
    let edge_count = private_aliases
        .values()
        .map(Vec::len)
        .chain(private_module_aliases.values().map(Vec::len))
        .sum::<usize>();
    // Guarded states can legitimately revisit one nominal key under distinct
    // cfg regions. Size the finite bound from both nodes and guarded edges;
    // cycles still dedupe the complete (key, effective guard) state.
    let budget = node_count
        .saturating_add(edge_count)
        .saturating_mul(8)
        .clamp(64, 16_384);
    let base_depth = private_aliases
        .keys()
        .map(|key| key.1.len())
        .chain(
            private_aliases
                .values()
                .flatten()
                .map(|(key, _)| key.1.len()),
        )
        .chain(private_module_aliases.keys().map(|(_, path)| path.len()))
        .chain(
            private_module_aliases
                .values()
                .flatten()
                .map(|(path, _)| path.len()),
        )
        .max()
        .unwrap_or(0);
    let max_depth = base_depth
        .saturating_add(private_module_aliases.len().min(256))
        .saturating_add(2);

    let initial_state = GuardedPrivateTypeKey {
        key: initial,
        cfg_guard: combined_guards(guard, &[]),
    };
    let mut resolved = BTreeSet::new();
    let mut terminals = BTreeSet::new();
    let mut pending = BTreeSet::from([initial_state.clone()]);
    let mut exhausted = false;
    let mut visited = 0usize;
    while let Some(state) = pending.pop_first() {
        if !resolved.insert(state.clone()) {
            continue;
        }
        visited = visited.saturating_add(1);
        if visited > budget {
            exhausted = true;
            break;
        }
        let mut successors = BTreeSet::new();
        if let Some(targets) = private_aliases.get(&state.key) {
            for (target, target_guard) in targets
                .iter()
                .filter(|(_, target_guard)| !guards_proven_disjoint(target_guard, &state.cfg_guard))
            {
                if target.1.len() <= max_depth {
                    successors.insert(GuardedPrivateTypeKey {
                        key: target.clone(),
                        cfg_guard: combined_guards(&state.cfg_guard, target_guard),
                    });
                } else {
                    exhausted = true;
                }
            }
        }
        for ((crate_name, alias_path), targets) in private_module_aliases {
            if crate_name != &state.key.0 || !state.key.1.starts_with(alias_path) {
                continue;
            }
            let suffix = &state.key.1[alias_path.len()..];
            for (target_path, target_guard) in targets
                .iter()
                .filter(|(_, target_guard)| !guards_proven_disjoint(target_guard, &state.cfg_guard))
            {
                let mut module_path = target_path.clone();
                module_path.extend_from_slice(suffix);
                if module_path.len() <= max_depth {
                    successors.insert(GuardedPrivateTypeKey {
                        key: (state.key.0.clone(), module_path, state.key.2.clone()),
                        cfg_guard: combined_guards(&state.cfg_guard, target_guard),
                    });
                } else {
                    exhausted = true;
                }
            }
        }
        if successors.is_empty() {
            terminals.insert(state);
        } else {
            pending.extend(successors);
        }
    }
    if !pending.is_empty() {
        exhausted = true;
    }
    let exhaustion_digest = exhausted.then(|| {
        private_alias_graph_digest(&initial_state, private_aliases, private_module_aliases)
    });
    PrivateAliasResolution {
        states: resolved,
        terminals,
        exhausted,
        exhaustion_digest,
    }
}

fn guarded_private_type_evidence(label: &str, state: &GuardedPrivateTypeKey) -> String {
    format!(
        "{label}:{}::{:?}::{}\neffective-cfg:{:?}",
        state.key.0, state.key.1, state.key.2, state.cfg_guard
    )
}

fn private_alias_graph_digest(
    initial: &GuardedPrivateTypeKey,
    private_aliases: &BTreeMap<PrivateTypeKey, Vec<GuardedPrivateTypeTarget>>,
    private_module_aliases: &BTreeMap<PrivateModuleAliasKey, Vec<GuardedPrivateModuleTarget>>,
) -> String {
    use sha2::{Digest, Sha256};

    let mut rows = vec![guarded_private_type_evidence("initial", initial)];
    for (source, targets) in private_aliases {
        for (target, guard) in targets {
            rows.push(format!(
                "type-edge:{source:?}->{target:?}\ncfg:{:?}",
                combined_guards(guard, &[])
            ));
        }
    }
    for (source, targets) in private_module_aliases {
        for (target, guard) in targets {
            rows.push(format!(
                "module-edge:{source:?}->{target:?}\ncfg:{:?}",
                combined_guards(guard, &[])
            ));
        }
    }
    rows.sort();
    rows.dedup();
    format!("sha256:{:x}", Sha256::digest(rows.join("\n--\n")))
}

fn impl_path_is_external_public(crate_name: &str, module_path: &[String]) -> bool {
    let first = module_path.first().map(String::as_str);
    match first {
        Some("std" | "core" | "alloc") => true,
        Some("crate" | "self" | "super") => false,
        Some(segment) if segment == crate_name => false,
        Some(_) => true,
        // An unqualified path may have entered scope through `use`, including
        // an external crate import. Source-only analysis cannot resolve that
        // binding safely. Callers use local declarations first, then retain the
        // unresolved impl as typed uncertainty rather than maintaining an
        // incomplete allowlist of external names.
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

fn parsed_manifest_authorities(
    source: &dyn RevisionFileSource,
    manifests: &[String],
) -> BTreeMap<String, toml::Value> {
    manifests
        .iter()
        .filter_map(|path| {
            let RevisionRead::Bytes(bytes) = source.read(path).ok()? else {
                return None;
            };
            let text = std::str::from_utf8(&bytes.bytes).ok()?;
            toml::from_str(text)
                .ok()
                .map(|manifest| (path.clone(), manifest))
        })
        .collect()
}

fn validate_edition(edition: &str) -> Result<String, String> {
    matches!(edition, "2015" | "2018" | "2021" | "2024")
        .then(|| edition.to_owned())
        .ok_or_else(|| format!("unsupported Rust edition {edition:?}"))
}

fn effective_library_edition(
    manifest_path: &str,
    manifest: &toml::Value,
    lib: Option<&toml::Table>,
    parsed: &BTreeMap<String, toml::Value>,
) -> Result<String, String> {
    if let Some(value) = lib.and_then(|table| table.get("edition")) {
        let edition = value
            .as_str()
            .ok_or_else(|| "lib.edition must be a string".to_owned())?;
        return validate_edition(edition);
    }
    let package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "package must be a table".to_owned())?;
    let Some(value) = package.get("edition") else {
        return Ok("2015".to_owned());
    };
    if let Some(edition) = value.as_str() {
        return validate_edition(edition);
    }
    let inherited = value
        .as_table()
        .and_then(|table| table.get("workspace"))
        .and_then(toml::Value::as_bool);
    if inherited != Some(true) {
        return Err("package.edition must be a string or { workspace = true }".to_owned());
    }

    let manifest_dir = parent_repo_path(manifest_path);
    let authority_path = if let Some(workspace) = package.get("workspace") {
        let workspace = workspace
            .as_str()
            .ok_or_else(|| "package.workspace must be a string".to_owned())?;
        let workspace_dir = safe_join_repo_path(&manifest_dir, workspace)
            .map_err(|reason| format!("package.workspace cannot be resolved: {reason}"))?;
        safe_join_repo_path(&workspace_dir, "Cargo.toml")
            .map_err(|reason| format!("workspace manifest cannot be resolved: {reason}"))?
    } else {
        parsed
            .iter()
            .filter(|(path, candidate)| {
                candidate
                    .get("workspace")
                    .and_then(toml::Value::as_table)
                    .is_some()
                    && workspace_relative_dir(&parent_manifest_dir(path), &manifest_dir).is_some()
            })
            .max_by_key(|(path, _)| parent_manifest_dir(path).len())
            .map(|(path, _)| path.clone())
            .ok_or_else(|| {
                format!(
                    "package.edition inherits from a workspace, but no workspace authority contains {manifest_path}"
                )
            })?
    };
    let authority = parsed.get(&authority_path).ok_or_else(|| {
        format!("package.edition workspace authority {authority_path} is unavailable")
    })?;
    let edition = authority
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("package"))
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("edition"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            format!("workspace authority {authority_path} has no workspace.package.edition string")
        })?;
    validate_edition(edition)
}

fn default_autolib(manifest: &toml::Value, edition: &str) -> bool {
    edition != "2015"
        || !["bin", "example", "test", "bench"]
            .iter()
            .any(|target| manifest.get(*target).is_some())
}

fn package_has_active_build_script(
    package: &toml::Table,
    manifest_dir: &str,
    inventory: &BTreeMap<String, RevisionEntry>,
) -> Result<bool, String> {
    let declared_build_script = |path: &str| {
        let path = safe_join_repo_path(manifest_dir, path)
            .map_err(|reason| format!("package.build cannot be resolved: {reason}"))?;
        inventory
            .get(&path)
            .is_some_and(is_live_regular_entry)
            .then_some(true)
            .ok_or_else(|| format!("declared build script {path} is unavailable"))
    };
    match package.get("build") {
        Some(toml::Value::Boolean(false)) => Ok(false),
        Some(toml::Value::Boolean(true)) => declared_build_script("build.rs"),
        Some(toml::Value::String(path)) => declared_build_script(path),
        Some(_) => Err("package.build must be a string or boolean".to_owned()),
        None => safe_join_repo_path(manifest_dir, "build.rs")
            .map(|path| inventory.get(&path).is_some_and(is_live_regular_entry)),
    }
}

fn repository_cargo_cfg_authority(
    source: &dyn RevisionFileSource,
    inventory: &BTreeMap<String, RevisionEntry>,
    package_links: Option<&str>,
) -> Result<bool, String> {
    // Cargo gives the legacy extensionless file precedence when both names
    // exist in the same directory. Mirroring that rule matters here: binding
    // the lower-precedence file could manufacture cfg authority Cargo ignores.
    let path = [".cargo/config", ".cargo/config.toml"]
        .into_iter()
        .find(|path| inventory.get(*path).is_some_and(is_live_regular_entry));
    let Some(path) = path else {
        return Ok(false);
    };
    let RevisionRead::Bytes(bytes) = source
        .read(path)
        .map_err(|reason| format!("cannot read {path}: {reason}"))?
    else {
        return Err(format!("{path} is not revision-backed regular bytes"));
    };
    let text =
        std::str::from_utf8(&bytes.bytes).map_err(|_| format!("{path} is not valid UTF-8"))?;
    let config: toml::Value =
        toml::from_str(text).map_err(|reason| format!("cannot parse {path}: {reason}"))?;
    cargo_config_can_define_custom_cfg(&config, package_links)
}

fn cargo_config_can_define_custom_cfg(
    value: &toml::Value,
    package_links: Option<&str>,
) -> Result<bool, String> {
    fn rustflags_define_cfg(value: &toml::Value) -> Result<bool, String> {
        let tokens = match value {
            toml::Value::String(value) => shlex::split(value)
                .ok_or_else(|| "Cargo config rustflags string has invalid quoting".to_owned())?,
            toml::Value::Array(values) => values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        "Cargo config rustflags array must contain strings".to_owned()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("Cargo config rustflags must be a string or string array".to_owned()),
        };
        Ok(tokens.iter().enumerate().any(|(index, token)| {
            token == "--cfg" && tokens.get(index + 1).is_some() || token.starts_with("--cfg=")
        }))
    }

    fn rustc_cfg_is_configured(value: &toml::Value) -> Result<bool, String> {
        match value {
            toml::Value::String(value) => Ok(!value.trim().is_empty()),
            toml::Value::Array(values) => {
                for value in values {
                    if value.as_str().is_none() {
                        return Err(
                            "Cargo target override rustc-cfg must contain strings".to_owned()
                        );
                    }
                }
                Ok(!values.is_empty())
            }
            _ => Err("Cargo target override rustc-cfg must be a string or string array".to_owned()),
        }
    }

    let table = value
        .as_table()
        .ok_or_else(|| "Cargo config root must be a table".to_owned())?;
    if table.contains_key("include") {
        return Err("Cargo config include authority is not resolved".to_owned());
    }

    if let Some(build) = table.get("build") {
        let build = build
            .as_table()
            .ok_or_else(|| "Cargo config build section must be a table".to_owned())?;
        if let Some(rustflags) = build.get("rustflags")
            && rustflags_define_cfg(rustflags)?
        {
            return Ok(true);
        }
    }

    if let Some(targets) = table.get("target") {
        let targets = targets
            .as_table()
            .ok_or_else(|| "Cargo config target section must be a table".to_owned())?;
        for (target_key, target) in targets {
            let target = target
                .as_table()
                .ok_or_else(|| "Cargo config target entry must be a table".to_owned())?;
            if let Some(rustflags) = target.get("rustflags")
                && rustflags_define_cfg(rustflags)?
            {
                return Ok(true);
            }
            if !target_key.trim_start().starts_with("cfg(")
                && let Some(links) = package_links
                && let Some(override_value) = target.get(links)
            {
                let override_value = override_value.as_table().ok_or_else(|| {
                    format!("Cargo config target link override {links} must be a table")
                })?;
                if let Some(rustc_cfg) = override_value.get("rustc-cfg")
                    && rustc_cfg_is_configured(rustc_cfg)?
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn revision_cfg_authority_digest(inventory: &BTreeMap<String, RevisionEntry>) -> String {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    for (path, entry) in inventory {
        digest.update(path.as_bytes());
        digest.update([0]);
        digest.update(entry.baseline_object_id.as_deref().unwrap_or("<overlay>"));
        digest.update([0]);
        digest.update(format!(
            "{:o}:{:?}:{:?}",
            entry.mode, entry.kind, entry.state
        ));
        digest.update([0]);
        if let RevisionProvenance::WorkingTreeOverlay { dirty_digest, .. } = &entry.provenance {
            digest.update(dirty_digest.as_bytes());
        }
        digest.update([0xff]);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn lib_crate_types(lib: Option<&toml::Table>, proc_macro: bool) -> Result<Vec<String>, String> {
    let Some(value) = lib.and_then(|table| table.get("crate-type")) else {
        return Ok(vec![
            if proc_macro { "proc-macro" } else { "lib" }.to_owned(),
        ]);
    };
    let mut types = match value {
        toml::Value::Array(kinds) => kinds
            .iter()
            .map(|kind| {
                kind.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "lib.crate-type must contain only strings".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("lib.crate-type must be an array of strings".to_owned()),
    };
    types.sort();
    types.dedup();
    if types.is_empty() {
        return Err("lib.crate-type must not be empty".to_owned());
    }
    const SUPPORTED: &[&str] = &[
        "bin",
        "lib",
        "rlib",
        "dylib",
        "cdylib",
        "staticlib",
        "proc-macro",
    ];
    if let Some(kind) = types
        .iter()
        .find(|kind| !SUPPORTED.contains(&kind.as_str()))
    {
        return Err(format!("unsupported lib.crate-type {kind:?}"));
    }
    if (proc_macro || types.iter().any(|kind| kind == "proc-macro"))
        && types.as_slice() != ["proc-macro"]
    {
        return Err(
            "proc-macro library targets require crate-type to be omitted or ['proc-macro']"
                .to_owned(),
        );
    }
    Ok(types)
}

fn opaque_source_canonical_bytes(bytes: &[u8]) -> Vec<u8> {
    let Ok(source) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    let Ok(file) = syn::parse_file(source) else {
        return bytes.to_vec();
    };
    canonical_tokens(OpaqueSubstrateNormalizer.fold_file(file).to_token_stream()).into_bytes()
}

struct OpaqueSubstrateNormalizer;

impl OpaqueSubstrateNormalizer {
    fn normalize_function(&mut self, signature: &mut syn::Signature, block: &mut syn::Block) {
        signature.ident = syn::Ident::new("__prview_name", signature.ident.span());
        if signature_has_body_dependent_opaque_return(signature) {
            SignatureAlphaNormalizer::with_occupied(FreeIdentifierCollector::signature(signature))
                .normalize_signature_and_block(signature, block);
        } else {
            alpha_normalize_signature(signature);
            *block = syn::parse_quote!({});
        }
        trim_signature_punctuation(signature);
    }
}

impl Fold for OpaqueSubstrateNormalizer {
    fn fold_item_fn(&mut self, mut function: syn::ItemFn) -> syn::ItemFn {
        if is_public(&function.vis) {
            self.normalize_function(&mut function.sig, &mut function.block);
            function
        } else {
            function
        }
    }

    fn fold_impl_item_fn(&mut self, mut function: syn::ImplItemFn) -> syn::ImplItemFn {
        if is_public(&function.vis) {
            self.normalize_function(&mut function.sig, &mut function.block);
            function
        } else {
            function
        }
    }

    fn fold_trait_item_fn(&mut self, mut function: syn::TraitItemFn) -> syn::TraitItemFn {
        function.sig.ident = syn::Ident::new("__prview_name", function.sig.ident.span());
        if let Some(block) = &mut function.default {
            self.normalize_function(&mut function.sig, block);
        } else {
            alpha_normalize_signature(&mut function.sig);
            trim_signature_punctuation(&mut function.sig);
        }
        function
    }
}

#[derive(Default)]
struct ImplTraitDetector {
    found: bool,
}

impl Fold for ImplTraitDetector {
    fn fold_type_impl_trait(&mut self, ty: syn::TypeImplTrait) -> syn::TypeImplTrait {
        self.found = true;
        syn::fold::fold_type_impl_trait(self, ty)
    }
}

fn signature_has_body_dependent_opaque_return(signature: &syn::Signature) -> bool {
    if signature.asyncness.is_some() {
        return true;
    }
    let syn::ReturnType::Type(_, ty) = &signature.output else {
        return false;
    };
    let mut detector = ImplTraitDetector::default();
    let _ = detector.fold_type((**ty).clone());
    detector.found
}

fn opaque_return_evidence(
    label: &str,
    signature: &syn::Signature,
    body: &syn::Block,
) -> Option<String> {
    if !signature_has_body_dependent_opaque_return(signature) {
        return None;
    }
    use sha2::{Digest, Sha256};

    let mut signature = signature.clone();
    signature.ident = syn::Ident::new("__prview_name", signature.ident.span());
    let mut body = body.clone();
    SignatureAlphaNormalizer::with_occupied(FreeIdentifierCollector::signature(&signature))
        .normalize_signature_and_block(&mut signature, &mut body);
    trim_signature_punctuation(&mut signature);
    let body = canonical_tokens(body.to_token_stream());
    Some(format!(
        "opaque-return:{label}\nsignature:{}\nbody-digest:sha256:{:x}",
        canonical_tokens(signature.to_token_stream()),
        Sha256::digest(body)
    ))
}

fn public_item_opaque_return_evidence(item: &Item) -> Vec<OpaqueReturnEvidence> {
    match item {
        Item::Fn(function) => opaque_return_evidence("function", &function.sig, &function.block)
            .into_iter()
            .map(|evidence| OpaqueReturnEvidence {
                cfg_guard: Vec::new(),
                evidence,
            })
            .collect(),
        Item::Trait(item_trait) => item_trait
            .items
            .iter()
            .filter_map(|item| {
                let syn::TraitItem::Fn(function) = item else {
                    return None;
                };
                let body = function.default.as_ref()?;
                let member_cfg = canonical_cfg(&function.attrs);
                opaque_return_evidence(
                    &format!("trait-default:{}", function.sig.ident),
                    &function.sig,
                    body,
                )
                .map(|evidence| OpaqueReturnEvidence {
                    cfg_guard: member_cfg.guards,
                    evidence: format!(
                        "{evidence}\ntrait-member-cfg:{}",
                        trait_member_cfg_key(&function.attrs)
                    ),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn associated_opaque_return_evidence(item: &syn::ImplItem) -> Vec<String> {
    match item {
        syn::ImplItem::Fn(function) => opaque_return_evidence(
            &format!("inherent:{}", function.sig.ident),
            &function.sig,
            &function.block,
        )
        .into_iter()
        .collect(),
        _ => Vec::new(),
    }
}

fn trait_impl_opaque_return_evidence(item_impl: &syn::ItemImpl) -> Vec<String> {
    item_impl
        .items
        .iter()
        .filter_map(|item| {
            let syn::ImplItem::Fn(function) = item else {
                return None;
            };
            opaque_return_evidence(
                &format!("trait-impl:{}", function.sig.ident),
                &function.sig,
                &function.block,
            )
        })
        .collect()
}

fn include_literal_path(macro_call: &syn::Macro) -> Option<String> {
    syn::parse2::<syn::LitStr>(macro_call.tokens.clone())
        .ok()
        .map(|lit| lit.value())
}

fn macro_is_include(macro_call: &syn::Macro) -> bool {
    macro_call
        .path
        .segments
        .last()
        .map(|segment| normalize_identifier(segment.ident.to_string()))
        .is_some_and(|name| matches!(name.as_str(), "include" | "include_str" | "include_bytes"))
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[Attribute] {
    match item {
        syn::TraitItem::Const(value) => &value.attrs,
        syn::TraitItem::Fn(value) => &value.attrs,
        syn::TraitItem::Type(value) => &value.attrs,
        syn::TraitItem::Macro(value) => &value.attrs,
        _ => &[],
    }
}

fn trait_transform_boundaries(item_trait: &syn::ItemTrait) -> Vec<String> {
    let mut boundaries = Vec::new();
    for item in &item_trait.items {
        boundaries.extend(transforming_attrs(trait_item_attrs(item)).map(|attribute| {
            format!(
                "transform-boundary:attribute\n{}",
                canonical_tokens(attribute.to_token_stream())
            )
        }));
        boundaries.extend(
            transforming_cfg_attrs(trait_item_attrs(item))
                .into_iter()
                .map(|evidence| format!("transform-boundary:attribute\n{evidence}")),
        );
        if let syn::TraitItem::Macro(value) = item
            && !macro_is_include(&value.mac)
        {
            boundaries.push(format!(
                "transform-boundary:declarative-macro\n{}",
                canonical_tokens(value.mac.to_token_stream())
            ));
        }
    }
    boundaries.sort();
    boundaries.dedup();
    boundaries
}

fn impl_transform_boundaries(item_impl: &syn::ItemImpl) -> Vec<String> {
    let mut boundaries = Vec::new();
    for item in &item_impl.items {
        let attrs: &[Attribute] = match item {
            syn::ImplItem::Const(value) => &value.attrs,
            syn::ImplItem::Fn(value) => &value.attrs,
            syn::ImplItem::Type(value) => &value.attrs,
            syn::ImplItem::Macro(value) => &value.attrs,
            _ => &[],
        };
        boundaries.extend(transforming_attrs(attrs).map(|attribute| {
            format!(
                "transform-boundary:attribute\n{}",
                canonical_tokens(attribute.to_token_stream())
            )
        }));
        boundaries.extend(
            transforming_cfg_attrs(attrs)
                .into_iter()
                .map(|evidence| format!("transform-boundary:attribute\n{evidence}")),
        );
        if let syn::ImplItem::Macro(value) = item
            && !macro_is_include(&value.mac)
        {
            boundaries.push(format!(
                "transform-boundary:declarative-macro\n{}",
                canonical_tokens(value.mac.to_token_stream())
            ));
        }
    }
    boundaries.sort();
    boundaries.dedup();
    boundaries
}

#[derive(Default)]
struct ContractIncludeCollector {
    macros: Vec<syn::Macro>,
}

impl Fold for ContractIncludeCollector {
    fn fold_macro(&mut self, macro_call: syn::Macro) -> syn::Macro {
        let name = macro_call
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string());
        if name
            .as_deref()
            .is_some_and(|name| matches!(name, "include" | "include_str" | "include_bytes"))
        {
            self.macros.push(macro_call.clone());
        }
        syn::fold::fold_macro(self, macro_call)
    }
}

fn public_contract_include_macros(item: &Item) -> Vec<syn::Macro> {
    let mut collector = ContractIncludeCollector::default();
    match item {
        Item::Fn(value) if is_public(&value.vis) => {
            collector.fold_signature(value.sig.clone());
        }
        Item::Struct(value) if is_public(&value.vis) => {
            collector.fold_generics(value.generics.clone());
            for field in &value.fields {
                collector.fold_type(field.ty.clone());
            }
        }
        Item::Union(value) if is_public(&value.vis) => {
            collector.fold_generics(value.generics.clone());
            for field in &value.fields.named {
                collector.fold_type(field.ty.clone());
            }
        }
        Item::Enum(value) if is_public(&value.vis) => {
            collector.fold_generics(value.generics.clone());
            for variant in &value.variants {
                for field in &variant.fields {
                    collector.fold_type(field.ty.clone());
                }
                if let Some((_, discriminant)) = &variant.discriminant {
                    collector.fold_expr(discriminant.clone());
                }
            }
        }
        Item::Trait(value) if is_public(&value.vis) => {
            collector.fold_generics(value.generics.clone());
            for bound in &value.supertraits {
                collector.fold_type_param_bound(bound.clone());
            }
            for trait_item in &value.items {
                match trait_item {
                    syn::TraitItem::Const(value) => {
                        collector.fold_generics(value.generics.clone());
                        collector.fold_type(value.ty.clone());
                        if let Some((_, default)) = &value.default {
                            collector.fold_expr(default.clone());
                        }
                    }
                    syn::TraitItem::Fn(value) => {
                        collector.fold_signature(value.sig.clone());
                    }
                    syn::TraitItem::Type(value) => {
                        collector.fold_generics(value.generics.clone());
                        for bound in &value.bounds {
                            collector.fold_type_param_bound(bound.clone());
                        }
                        if let Some((_, default)) = &value.default {
                            collector.fold_type(default.clone());
                        }
                    }
                    syn::TraitItem::Macro(value) => {
                        collector.fold_macro(value.mac.clone());
                    }
                    _ => {}
                }
            }
        }
        Item::Type(value) if is_public(&value.vis) => {
            collector.fold_generics(value.generics.clone());
            collector.fold_type((*value.ty).clone());
        }
        Item::Const(value) if is_public(&value.vis) => {
            collector.fold_type((*value.ty).clone());
            collector.fold_expr((*value.expr).clone());
        }
        Item::Static(value) if is_public(&value.vis) => {
            collector.fold_type((*value.ty).clone());
            collector.fold_expr((*value.expr).clone());
        }
        _ => {}
    }
    collector.macros
}

fn foreign_contract_include_macros(item: &syn::ForeignItem) -> Vec<syn::Macro> {
    let mut collector = ContractIncludeCollector::default();
    match item {
        syn::ForeignItem::Fn(value) if is_public(&value.vis) => {
            collector.fold_signature(value.sig.clone());
        }
        syn::ForeignItem::Static(value) if is_public(&value.vis) => {
            collector.fold_type((*value.ty).clone());
        }
        syn::ForeignItem::Macro(value) => {
            collector.fold_macro(value.mac.clone());
        }
        _ => {}
    }
    collector.macros
}

fn associated_contract_include_macros(item: &syn::ImplItem) -> Vec<syn::Macro> {
    let mut collector = ContractIncludeCollector::default();
    match item {
        syn::ImplItem::Fn(value) if is_public(&value.vis) => {
            collector.fold_signature(value.sig.clone());
        }
        syn::ImplItem::Const(value) if is_public(&value.vis) => {
            collector.fold_generics(value.generics.clone());
            collector.fold_type(value.ty.clone());
            collector.fold_expr(value.expr.clone());
        }
        syn::ImplItem::Macro(value) => {
            collector.fold_macro(value.mac.clone());
        }
        _ => {}
    }
    collector.macros
}

fn trait_impl_contract_include_macros(item_impl: &syn::ItemImpl) -> Vec<syn::Macro> {
    let mut collector = ContractIncludeCollector::default();
    collector.fold_generics(item_impl.generics.clone());
    collector.fold_type((*item_impl.self_ty).clone());
    if let Some((_, trait_path, _)) = &item_impl.trait_ {
        collector.fold_path(trait_path.clone());
    }
    for item in &item_impl.items {
        match item {
            syn::ImplItem::Fn(value) => {
                collector.fold_signature(value.sig.clone());
            }
            syn::ImplItem::Const(value) => {
                collector.fold_generics(value.generics.clone());
                collector.fold_type(value.ty.clone());
                collector.fold_expr(value.expr.clone());
            }
            syn::ImplItem::Type(value) => {
                collector.fold_generics(value.generics.clone());
                collector.fold_type(value.ty.clone());
            }
            syn::ImplItem::Macro(value) => {
                collector.fold_macro(value.mac.clone());
            }
            _ => {}
        }
    }
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
    if syn::parse_str::<syn::Ident>(&crate_name).is_err()
        && !is_raw_identifier_compatible_keyword(&crate_name)
    {
        return Err(format!(
            "package.name does not normalize to a Rust crate identifier or keyword: {name:?}"
        ));
    }
    Ok(())
}

fn validate_lib_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("lib.name must not be empty".to_owned());
    }
    if syn::parse_str::<syn::Ident>(name).is_err() && !is_raw_identifier_compatible_keyword(name) {
        return Err(format!(
            "lib.name must be a Rust crate identifier or keyword: {name:?}"
        ));
    }
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

struct LocalTypeDependencyCollector {
    crate_name: String,
    module_path: Vec<String>,
    dependencies: BTreeSet<(String, Vec<String>, String)>,
}

impl LocalTypeDependencyCollector {
    fn new(crate_name: &str, module_path: &[String]) -> Self {
        Self {
            crate_name: crate_name.to_owned(),
            module_path: module_path.to_vec(),
            dependencies: BTreeSet::new(),
        }
    }

    fn collect_item_types(
        crate_name: &str,
        module_path: &[String],
        item: &Item,
    ) -> BTreeSet<(String, Vec<String>, String)> {
        let mut collector = Self::new(crate_name, module_path);
        collector.fold_item(item.clone());
        collector.dependencies
    }

    fn collect_impl_types(
        crate_name: &str,
        module_path: &[String],
        item: &syn::ItemImpl,
    ) -> BTreeSet<(String, Vec<String>, String)> {
        let mut collector = Self::new(crate_name, module_path);
        collector.fold_item_impl(item.clone());
        collector.dependencies
    }

    fn record_path(&mut self, path: &syn::Path) {
        if let Some((module_path, name)) = resolve_impl_owner(&self.module_path, path) {
            self.dependencies
                .insert((self.crate_name.clone(), module_path, name));
        }
    }
}

impl Fold for LocalTypeDependencyCollector {
    fn fold_path(&mut self, path: syn::Path) -> syn::Path {
        self.record_path(&path);
        syn::fold::fold_path(self, path)
    }
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

fn resolve_impl_self_owner(current: &[String], ty: &syn::Type) -> Option<(Vec<String>, String)> {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => resolve_impl_owner(current, &path.path),
        syn::Type::Reference(reference) => resolve_impl_self_owner(current, &reference.elem),
        syn::Type::Ptr(pointer) => resolve_impl_self_owner(current, &pointer.elem),
        syn::Type::Slice(slice) => resolve_impl_self_owner(current, &slice.elem),
        syn::Type::Array(array) => resolve_impl_self_owner(current, &array.elem),
        syn::Type::Paren(paren) => resolve_impl_self_owner(current, &paren.elem),
        syn::Type::Group(group) => resolve_impl_self_owner(current, &group.elem),
        _ => None,
    }
}

fn external_owner_projections(
    item: &RustApiItem,
    owner_module_path: &[String],
    owner_name: &str,
    private_aliases: &BTreeMap<PrivateTypeKey, Vec<GuardedPrivateTypeTarget>>,
    private_module_aliases: &BTreeMap<PrivateModuleAliasKey, Vec<GuardedPrivateModuleTarget>>,
) -> Vec<ExternalOwnerProjection> {
    let direct = item.origin_module_path == owner_module_path && item.origin_name == owner_name;
    if item.kind != RustApiItemKind::TypeAlias {
        return direct
            .then(|| ExternalOwnerProjection {
                external_module_path: item.key.module_path.clone(),
                external_name: item.key.external_name.clone(),
                cfg_guard: item.cfg_guard.clone(),
                alias_uncertain: false,
                resolution_evidence: None,
            })
            .into_iter()
            .collect();
    }
    let mut projections = BTreeSet::new();
    if direct {
        projections.insert(ExternalOwnerProjection {
            external_module_path: item.key.module_path.clone(),
            external_name: item.key.external_name.clone(),
            cfg_guard: item.cfg_guard.clone(),
            alias_uncertain: true,
            resolution_evidence: None,
        });
    }
    let resolution = resolve_private_type_alias_keys(
        (
            item.key.crate_name.clone(),
            item.origin_module_path.clone(),
            item.origin_name.clone(),
        ),
        &item.cfg_guard,
        private_aliases,
        private_module_aliases,
    );
    let exhaustion_evidence = resolution.exhaustion_digest.clone();
    for state in resolution.states.into_iter().filter(|state| {
        state.key.0 == item.key.crate_name
            && state.key.1 == owner_module_path
            && state.key.2 == owner_name
    }) {
        projections.insert(ExternalOwnerProjection {
            external_module_path: item.key.module_path.clone(),
            external_name: item.key.external_name.clone(),
            cfg_guard: state.cfg_guard,
            alias_uncertain: true,
            resolution_evidence: exhaustion_evidence.clone(),
        });
    }
    if projections.is_empty()
        && let Some(digest) = exhaustion_evidence
    {
        projections.insert(ExternalOwnerProjection {
            external_module_path: item.key.module_path.clone(),
            external_name: item.key.external_name.clone(),
            cfg_guard: item.cfg_guard.clone(),
            alias_uncertain: true,
            resolution_evidence: Some(format!("exhausted:{digest}")),
        });
    }
    projections.into_iter().collect()
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
    ) || left.iter().any(|left_guard| {
        right.iter().any(|right_guard| {
            direct_cfg_negation(left_guard).is_some_and(|atom| atom == right_guard)
                || direct_cfg_negation(right_guard).is_some_and(|atom| atom == left_guard)
        })
    })
}

fn direct_cfg_negation(predicate: &str) -> Option<&str> {
    let atom = predicate.strip_prefix("not(")?.strip_suffix(')')?;
    (!atom.starts_with("all(")
        && !atom.starts_with("any(")
        && !atom.starts_with("not(")
        && !atom.contains(','))
    .then_some(atom)
}

fn proven_target_family(guards: &[String]) -> Option<ProvenTargetFamily> {
    guards.iter().find_map(|guard| match guard.as_str() {
        "unix" | "target_family = \"unix\"" => Some(ProvenTargetFamily::Unix),
        "windows" | "target_family = \"windows\"" => Some(ProvenTargetFamily::Windows),
        _ => None,
    })
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
        if name.as_deref() == Some("unsafe") {
            return !is_known_unsafe_attr(&attr.meta);
        }
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
                    | "derive"
                    | "no_mangle"
                    | "export_name"
                    | "link_name"
                    | "link_section"
                    | "path"
                    | "cold"
                    | "inline"
                    | "track_caller"
                    | "test"
                    | "bench"
                    | "ignore"
                    | "should_panic"
            )
        )
    })
}

fn is_builtin_derive_name(name: &str) -> bool {
    matches!(
        name,
        "Clone" | "Copy" | "Debug" | "Default" | "Eq" | "Hash" | "Ord" | "PartialEq" | "PartialOrd"
    )
}

fn is_proven_builtin_derive(path: &syn::Path, ambiguity: &DeriveNameAmbiguity) -> bool {
    path.segments.len() == 1
        && path.segments.first().is_some_and(|segment| {
            let name = normalize_identifier(segment.ident.to_string());
            is_builtin_derive_name(&name) && !ambiguity.may_shadow(&name)
        })
}

fn derive_contains_custom(list: &syn::MetaList, ambiguity: &DeriveNameAmbiguity) -> bool {
    let parser = Punctuated::<syn::Path, Token![,]>::parse_terminated;
    parser.parse2(list.tokens.clone()).map_or(true, |paths| {
        paths
            .iter()
            .any(|path| !is_proven_builtin_derive(path, ambiguity))
    })
}

fn custom_derive_evidence(attrs: &[Attribute], ambiguity: &DeriveNameAmbiguity) -> Vec<String> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("derive"))
        .filter(|attr| {
            let Meta::List(list) = &attr.meta else {
                return true;
            };
            derive_contains_custom(list, ambiguity)
        })
        .map(|attr| canonical_tokens(attr.to_token_stream()))
        .collect()
}

fn conditional_custom_derive_evidence(
    attrs: &[Attribute],
    ambiguity: &DeriveNameAmbiguity,
) -> Vec<String> {
    fn contains_custom(meta: &Meta, ambiguity: &DeriveNameAmbiguity) -> bool {
        let Meta::List(list) = meta else {
            return false;
        };
        if list.path.is_ident("derive") {
            return derive_contains_custom(list, ambiguity);
        }
        if !list.path.is_ident("cfg_attr") {
            return false;
        }
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        parser.parse2(list.tokens.clone()).map_or(true, |parts| {
            parts
                .iter()
                .skip(1)
                .any(|nested| contains_custom(nested, ambiguity))
        })
    }

    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg_attr"))
        .filter(|attr| contains_custom(&attr.meta, ambiguity))
        .map(|attr| canonical_tokens(attr.to_token_stream()))
        .collect()
}

fn meta_contains_macro_use(meta: &Meta) -> bool {
    if meta.path().is_ident("macro_use") {
        return true;
    }
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    parser
        .parse2(list.tokens.clone())
        .is_ok_and(|parts| parts.iter().any(meta_contains_macro_use))
}

fn collect_derive_name_ambiguity(items: &[Item], ambiguity: &mut DeriveNameAmbiguity) {
    for item in items {
        match item {
            Item::Use(item_use) => {
                let mut leaves = Vec::new();
                flatten_use_tree(&item_use.tree, Vec::new(), &mut leaves);
                for leaf in leaves {
                    if leaf.glob {
                        ambiguity.all_unqualified = true;
                    } else if is_builtin_derive_name(&leaf.alias) {
                        ambiguity.names.insert(leaf.alias);
                    }
                }
            }
            Item::ExternCrate(item_extern)
                if item_extern
                    .attrs
                    .iter()
                    .any(|attr| meta_contains_macro_use(&attr.meta)) =>
            {
                ambiguity.macro_use = true;
                ambiguity.all_unqualified = true;
            }
            Item::Mod(module) => {
                if module
                    .attrs
                    .iter()
                    .any(|attr| meta_contains_macro_use(&attr.meta))
                {
                    ambiguity.macro_use = true;
                    ambiguity.all_unqualified = true;
                }
            }
            _ => {}
        }
    }
}

fn derive_name_ambiguity_for_items(items: &[Item]) -> DeriveNameAmbiguity {
    let mut ambiguity = DeriveNameAmbiguity::default();
    collect_derive_name_ambiguity(items, &mut ambiguity);
    ambiguity
}

fn sanitize_derive_meta(meta: &mut Meta, ambiguity: &DeriveNameAmbiguity) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<syn::Path, Token![,]>::parse_terminated;
    let Ok(paths) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    let paths = paths
        .into_iter()
        .filter(|path| is_proven_builtin_derive(path, ambiguity))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return false;
    }
    list.tokens = quote!(#(#paths),*);
    true
}

fn sanitize_cfg_attr_meta(meta: &mut Meta, ambiguity: &DeriveNameAmbiguity) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    let mut parts = parts.into_iter();
    let Some(predicate) = parts.next() else {
        return false;
    };
    let mut retained = Vec::new();
    for mut nested in parts {
        let name = nested
            .path()
            .segments
            .first()
            .map(|segment| normalize_identifier(segment.ident.to_string()));
        let keep = match name.as_deref() {
            Some("derive") => sanitize_derive_meta(&mut nested, ambiguity),
            Some("cfg_attr") => sanitize_cfg_attr_meta(&mut nested, ambiguity),
            Some(_) => is_known_contract_meta(&nested),
            None => false,
        };
        if keep {
            retained.push(nested);
        }
    }
    if retained.is_empty() {
        return false;
    }
    list.tokens = quote!(#predicate, #(#retained),*);
    true
}

fn sanitize_confirmed_attrs(attrs: &mut Vec<Attribute>, ambiguity: &DeriveNameAmbiguity) {
    attrs.retain_mut(|attr| {
        let name = attr
            .path()
            .segments
            .first()
            .map(|segment| normalize_identifier(segment.ident.to_string()));
        match name.as_deref() {
            Some("derive") => sanitize_derive_meta(&mut attr.meta, ambiguity),
            Some("cfg_attr") => sanitize_cfg_attr_meta(&mut attr.meta, ambiguity),
            Some(_) => is_known_contract_meta(&attr.meta),
            None => false,
        }
    });
}

#[derive(Default)]
struct BuiltinDefaultCoverage {
    guards: BTreeSet<Vec<String>>,
}

impl BuiltinDefaultCoverage {
    fn any(&self) -> bool {
        !self.guards.is_empty()
    }

    fn covers(&self, effective_guard: &[String]) -> bool {
        self.guards
            .iter()
            .any(|proof_guard| guard_lineage_contains(effective_guard, proof_guard))
    }
}

fn meta_has_proven_default_derive(meta: &Meta, ambiguity: &DeriveNameAmbiguity) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if !list.path.is_ident("derive") {
        return false;
    }
    let parser = Punctuated::<syn::Path, Token![,]>::parse_terminated;
    parser.parse2(list.tokens.clone()).is_ok_and(|paths| {
        paths
            .iter()
            .any(|path| path.is_ident("Default") && is_proven_builtin_derive(path, ambiguity))
    })
}

fn canonical_default_predicate(meta: &Meta) -> Option<String> {
    let Meta::List(list) = meta else {
        return canonical_meta_checked(meta).ok();
    };
    let name = canonical_tokens(list.path.to_token_stream());
    if !matches!(name.as_str(), "all" | "any") {
        return canonical_meta_checked(meta).ok();
    }
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let parts = parser.parse2(list.tokens.clone()).ok()?;
    let mut operands = parts
        .iter()
        .map(canonical_default_predicate)
        .collect::<Option<Vec<_>>>()?;
    operands.sort();
    operands.dedup();
    match operands.as_slice() {
        [only] => Some(only.clone()),
        _ => Some(format!("{name}({})", operands.join(","))),
    }
}

fn builtin_default_coverage(
    attrs: &[Attribute],
    ambiguity: &DeriveNameAmbiguity,
) -> BuiltinDefaultCoverage {
    fn collect(
        meta: &Meta,
        ambiguity: &DeriveNameAmbiguity,
        inherited_guard: &[String],
        coverage: &mut BuiltinDefaultCoverage,
    ) {
        if meta_has_proven_default_derive(meta, ambiguity) {
            coverage.guards.insert(inherited_guard.to_vec());
            return;
        }
        let Meta::List(list) = meta else {
            return;
        };
        if !list.path.is_ident("cfg_attr") {
            return;
        }
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let Ok(parts) = parser.parse2(list.tokens.clone()) else {
            return;
        };
        let mut parts = parts.iter();
        let Some(predicate) = parts.next().and_then(canonical_default_predicate) else {
            return;
        };
        let mut effective_guard = inherited_guard.to_vec();
        effective_guard.push(predicate);
        effective_guard.sort();
        effective_guard.dedup();
        for nested in parts {
            collect(nested, ambiguity, &effective_guard, coverage);
        }
    }

    let mut coverage = BuiltinDefaultCoverage::default();
    for attr in attrs {
        collect(&attr.meta, ambiguity, &[], &mut coverage);
    }
    coverage
}

fn unresolved_builtin_default_evidence(
    item: &Item,
    ambiguity: &DeriveNameAmbiguity,
) -> Vec<String> {
    fn collect_uncovered(
        meta: &Meta,
        inherited_guard: &[String],
        coverage: &BuiltinDefaultCoverage,
        uncovered: &mut BTreeSet<Vec<String>>,
    ) {
        if matches!(meta, Meta::Path(path) if path.is_ident("default")) {
            if !coverage.covers(inherited_guard) {
                uncovered.insert(inherited_guard.to_vec());
            }
            return;
        }
        let Meta::List(list) = meta else {
            return;
        };
        if !list.path.is_ident("cfg_attr") {
            return;
        }
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let Ok(parts) = parser.parse2(list.tokens.clone()) else {
            return;
        };
        let mut parts = parts.iter();
        let Some(predicate) = parts.next().and_then(canonical_default_predicate) else {
            return;
        };
        let mut effective_guard = inherited_guard.to_vec();
        effective_guard.push(predicate);
        effective_guard.sort();
        effective_guard.dedup();
        for nested in parts {
            collect_uncovered(nested, &effective_guard, coverage, uncovered);
        }
    }

    let Item::Enum(item_enum) = item else {
        return Vec::new();
    };
    let coverage = builtin_default_coverage(&item_enum.attrs, ambiguity);
    if !coverage.any() {
        return Vec::new();
    }
    let mut uncovered = BTreeSet::new();
    for variant in &item_enum.variants {
        for attr in &variant.attrs {
            collect_uncovered(&attr.meta, &[], &coverage, &mut uncovered);
        }
    }
    uncovered
        .into_iter()
        .map(|guard| {
            format!(
                "conditional-default-coverage-unresolved:{}\ninput:{}",
                guard.join(" && "),
                canonical_tokens(item.to_token_stream())
            )
        })
        .collect()
}

fn sanitize_variant_cfg_attr_meta(
    meta: &mut Meta,
    ambiguity: &DeriveNameAmbiguity,
    coverage: &BuiltinDefaultCoverage,
    inherited_guard: &[String],
) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    let mut parts = parts.into_iter();
    let Some(predicate) = parts.next() else {
        return false;
    };
    let predicate_key = canonical_default_predicate(&predicate);
    let mut effective_guard = inherited_guard.to_vec();
    if let Some(predicate) = &predicate_key {
        effective_guard.push(predicate.clone());
        effective_guard.sort();
        effective_guard.dedup();
    }
    let mut retained = Vec::new();
    for mut nested in parts {
        let name = nested
            .path()
            .segments
            .first()
            .map(|segment| normalize_identifier(segment.ident.to_string()));
        let keep = match name.as_deref() {
            Some("default") => {
                matches!(nested, Meta::Path(ref path) if path.is_ident("default"))
                    && predicate_key.is_some()
                    && coverage.covers(&effective_guard)
            }
            Some("derive") => sanitize_derive_meta(&mut nested, ambiguity),
            Some("cfg_attr") => {
                sanitize_variant_cfg_attr_meta(&mut nested, ambiguity, coverage, &effective_guard)
            }
            Some(_) => is_known_contract_meta(&nested),
            None => false,
        };
        if keep {
            retained.push(nested);
        }
    }
    if retained.is_empty() {
        return false;
    }
    list.tokens = quote!(#predicate, #(#retained),*);
    true
}

fn sanitize_variant_attrs(
    attrs: &mut Vec<Attribute>,
    ambiguity: &DeriveNameAmbiguity,
    coverage: &BuiltinDefaultCoverage,
) {
    attrs.retain_mut(|attr| {
        let name = attr
            .path()
            .segments
            .first()
            .map(|segment| normalize_identifier(segment.ident.to_string()));
        match name.as_deref() {
            Some("default") => {
                coverage.covers(&[])
                    && matches!(&attr.meta, Meta::Path(path) if path.is_ident("default"))
            }
            Some("derive") => sanitize_derive_meta(&mut attr.meta, ambiguity),
            Some("cfg_attr") => {
                sanitize_variant_cfg_attr_meta(&mut attr.meta, ambiguity, coverage, &[])
            }
            Some(_) => is_known_contract_meta(&attr.meta),
            None => false,
        }
    });
}

fn sanitize_confirmed_contract_attrs(item: &mut Item, ambiguity: &DeriveNameAmbiguity) {
    let default_coverage = match item {
        Item::Enum(value) => builtin_default_coverage(&value.attrs, ambiguity),
        _ => BuiltinDefaultCoverage::default(),
    };
    if let Some(attrs) = item_attrs_mut(item) {
        sanitize_confirmed_attrs(attrs, ambiguity);
    }
    match item {
        Item::Struct(value) => {
            for field in &mut value.fields {
                sanitize_confirmed_attrs(&mut field.attrs, ambiguity);
            }
        }
        Item::Union(value) => {
            for field in &mut value.fields.named {
                sanitize_confirmed_attrs(&mut field.attrs, ambiguity);
            }
        }
        Item::Enum(value) => {
            for variant in &mut value.variants {
                sanitize_variant_attrs(&mut variant.attrs, ambiguity, &default_coverage);
                for field in &mut variant.fields {
                    sanitize_confirmed_attrs(&mut field.attrs, ambiguity);
                }
            }
        }
        Item::Trait(value) => {
            for trait_item in &mut value.items {
                match trait_item {
                    syn::TraitItem::Const(value) => {
                        sanitize_confirmed_attrs(&mut value.attrs, ambiguity)
                    }
                    syn::TraitItem::Fn(value) => {
                        sanitize_confirmed_attrs(&mut value.attrs, ambiguity)
                    }
                    syn::TraitItem::Type(value) => {
                        sanitize_confirmed_attrs(&mut value.attrs, ambiguity)
                    }
                    syn::TraitItem::Macro(value) => {
                        sanitize_confirmed_attrs(&mut value.attrs, ambiguity)
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn meta_contains_derive(meta: &Meta) -> bool {
    if meta.path().is_ident("derive") {
        return true;
    }
    let Meta::List(list) = meta else {
        return false;
    };
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    parser
        .parse2(list.tokens.clone())
        .is_ok_and(|parts| parts.iter().any(meta_contains_derive))
}

fn bind_transform_evidence<T: ToTokens>(
    evidence: String,
    input: &T,
    implementation_digest: Option<&str>,
) -> String {
    format!(
        "transform:{evidence}\ninput:{}\nmacro-implementation-digest:{}",
        canonical_tokens(input.to_token_stream()),
        implementation_digest.unwrap_or("unresolved:not-captured")
    )
}

fn bind_associated_native_transform_evidence(
    boundary: &str,
    member_name: &str,
    member: &syn::ImplItem,
    owner_contract: &str,
    implementation_digest: Option<&str>,
) -> String {
    let mut evidence = bind_transform_evidence(boundary.to_owned(), member, implementation_digest);
    evidence.push_str("\nnative-export-associated-function:");
    evidence.push_str(member_name);
    evidence.push_str("\nassociated-owner-contract:");
    evidence.push_str(owner_contract);
    evidence
}

fn bind_additive_derive_evidence<T: ToTokens>(
    evidence: String,
    input: &T,
    implementation_digest: Option<&str>,
) -> String {
    format!(
        "transform-kind:additive-derive\n{}",
        bind_transform_evidence(evidence, input, implementation_digest)
    )
}

fn transforming_cfg_attrs(attrs: &[Attribute]) -> Vec<String> {
    fn contains_transformer(meta: &Meta) -> bool {
        let Meta::List(list) = meta else {
            return !is_known_contract_meta(meta);
        };
        if !list.path.is_ident("cfg_attr") {
            return !is_known_contract_meta(meta);
        }
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        parser
            .parse2(list.tokens.clone())
            .map_or(true, |parts| parts.iter().skip(1).any(contains_transformer))
    }

    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg_attr"))
        .filter(|attr| contains_transformer(&attr.meta))
        .map(|attr| canonical_tokens(attr.to_token_stream()))
        .collect()
}

fn is_known_contract_meta(meta: &Meta) -> bool {
    let name = meta
        .path()
        .segments
        .first()
        .map(|segment| normalize_identifier(segment.ident.to_string()));
    match name.as_deref() {
        Some("unsafe") => is_known_unsafe_attr(meta),
        Some(name) => is_known_contract_attr(name),
        None => false,
    }
}

fn is_known_unsafe_attr(meta: &Meta) -> bool {
    let Meta::List(list) = meta else {
        return false;
    };
    if !list.path.is_ident("unsafe") {
        return false;
    }
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    let Ok(parts) = parser.parse2(list.tokens.clone()) else {
        return false;
    };
    if parts.len() != 1 {
        return false;
    }
    match parts.first().expect("one unsafe attribute") {
        Meta::Path(path) => path.is_ident("no_mangle"),
        Meta::NameValue(value)
            if value.path.is_ident("export_name") || value.path.is_ident("link_section") =>
        {
            matches!(
                &value.value,
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(_),
                    ..
                })
            )
        }
        _ => false,
    }
}

fn is_binary_export_meta(meta: &Meta) -> bool {
    if meta.path().is_ident("no_mangle") || meta.path().is_ident("export_name") {
        return true;
    }
    let Meta::List(list) = meta else {
        return false;
    };
    if !list.path.is_ident("unsafe") {
        return false;
    }
    let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
    parser.parse2(list.tokens.clone()).is_ok_and(|parts| {
        parts.len() == 1
            && parts.first().is_some_and(|meta| {
                meta.path().is_ident("no_mangle") || meta.path().is_ident("export_name")
            })
    })
}

fn binary_export_attributes(attrs: &[Attribute]) -> Vec<(Vec<String>, String)> {
    fn collect(meta: &Meta, inherited_guard: &[String], exports: &mut Vec<(Vec<String>, String)>) {
        if is_binary_export_meta(meta) {
            exports.push((
                inherited_guard.to_vec(),
                canonical_tokens(meta.to_token_stream()),
            ));
            return;
        }
        let Meta::List(list) = meta else {
            return;
        };
        if !list.path.is_ident("cfg_attr") {
            return;
        }
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let Ok(parts) = parser.parse2(list.tokens.clone()) else {
            return;
        };
        let mut parts = parts.iter();
        let Some(predicate) = parts
            .next()
            .and_then(|meta| canonical_meta_checked(meta).ok())
        else {
            return;
        };
        let mut effective_guard = inherited_guard.to_vec();
        effective_guard.push(predicate);
        effective_guard.sort();
        effective_guard.dedup();
        for nested in parts {
            collect(nested, &effective_guard, exports);
        }
    }

    let mut exports = Vec::new();
    for attr in attrs {
        collect(&attr.meta, &[], &mut exports);
    }
    exports
}

fn binary_exports(
    item: &Item,
    include_public_direct: bool,
) -> Vec<(String, bool, Vec<String>, String)> {
    let direct = |name: String, attrs: &[Attribute], public: bool, input: String| {
        if public && !include_public_direct {
            return Vec::new();
        }
        binary_export_attributes(attrs)
            .into_iter()
            .map(|(guard, export)| {
                (
                    name.clone(),
                    public,
                    guard,
                    format!("{export}\ninput:{input}"),
                )
            })
            .collect()
    };

    match item {
        Item::Fn(value) => direct(
            normalize_identifier(value.sig.ident.to_string()),
            &value.attrs,
            is_public(&value.vis),
            canonical_tokens(item.to_token_stream()),
        ),
        Item::Static(value) => direct(
            normalize_identifier(value.ident.to_string()),
            &value.attrs,
            is_public(&value.vis),
            canonical_tokens(item.to_token_stream()),
        ),
        Item::Impl(item_impl) => {
            let normalized_impl =
                CanonicalFold.fold_item_impl(normalized_trait_impl_item(item_impl));
            let owner = if let Some((_, trait_path, _)) = &normalized_impl.trait_ {
                format!(
                    "<{} as {}>",
                    canonical_tokens(normalized_impl.self_ty.to_token_stream()),
                    canonical_tokens(trait_path.to_token_stream())
                )
            } else {
                canonical_tokens(normalized_impl.self_ty.to_token_stream())
            };
            item_impl
                .items
                .iter()
                .filter_map(|member| {
                    let syn::ImplItem::Fn(function) = member else {
                        return None;
                    };
                    let public = is_public(&function.vis);
                    let member_cfg = canonical_cfg(&function.attrs);
                    if !member_cfg.errors.is_empty() {
                        return None;
                    }
                    let name = format!(
                        "{owner}::{}",
                        normalize_identifier(function.sig.ident.to_string())
                    );
                    let input = normalized_associated_contract(item_impl, member, false);
                    Some(binary_export_attributes(&function.attrs).into_iter().map(
                        move |(guard, export)| {
                            (
                                name.clone(),
                                public,
                                combined_guards(&member_cfg.guards, &guard),
                                format!("{export}\ninput:{input}"),
                            )
                        },
                    ))
                })
                .flatten()
                .collect()
        }
        _ => Vec::new(),
    }
}

fn macro_export_guards(attrs: &[Attribute]) -> Vec<Vec<String>> {
    fn collect(meta: &Meta, inherited_guard: &[String], exports: &mut BTreeSet<Vec<String>>) {
        if matches!(meta, Meta::Path(path) if path.is_ident("macro_export")) {
            exports.insert(inherited_guard.to_vec());
            return;
        }
        let Meta::List(list) = meta else {
            return;
        };
        if !list.path.is_ident("cfg_attr") {
            return;
        }
        let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
        let Ok(parts) = parser.parse2(list.tokens.clone()) else {
            return;
        };
        let mut parts = parts.iter();
        let Some(predicate) = parts
            .next()
            .and_then(|meta| canonical_meta_checked(meta).ok())
        else {
            return;
        };
        let mut effective_guard = inherited_guard.to_vec();
        effective_guard.push(predicate);
        effective_guard.sort();
        effective_guard.dedup();
        for nested in parts {
            collect(nested, &effective_guard, exports);
        }
    }

    let mut exports = BTreeSet::new();
    for attr in attrs {
        collect(&attr.meta, &[], &mut exports);
    }
    exports.into_iter().collect()
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
            | "derive"
            | "no_mangle"
            | "export_name"
            | "link_name"
            | "link_section"
            | "path"
            | "cold"
            | "inline"
            | "track_caller"
            | "test"
            | "bench"
            | "ignore"
            | "should_panic"
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

fn is_raw_identifier_compatible_keyword(value: &str) -> bool {
    is_rust_keyword(value) && !matches!(value, "crate" | "self" | "Self" | "super")
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
    use sha2::Digest;
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
                    baseline_object_id: Some(
                        self.files
                            .get(&path)
                            .map(|bytes| hex::encode(sha2::Sha256::digest(bytes)))
                            .unwrap_or_else(|| "fixture-object".to_owned()),
                    ),
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
    fn rust_api_snapshot_normalizes_external_private_field_visibility() {
        let inherited = snapshot_rust_api(&source(
            "pub mod api { pub struct Named { inner: u8 } pub struct Tuple(u16); }",
        ));
        let restricted = snapshot_rust_api(&source(
            "pub mod api { pub struct Named { pub(crate) inner: u8 } pub struct Tuple(pub(super) u16); }",
        ));
        assert_eq!(
            inherited.items, restricted.items,
            "inherited and restricted fields have the same external-private identity"
        );

        let named_type_changed = snapshot_rust_api(&source(
            "pub mod api { pub struct Named { pub(crate) inner: String } pub struct Tuple(pub(super) u16); }",
        ));
        assert_ne!(
            find_item(&inherited, "Named").contract,
            find_item(&named_type_changed, "Named").contract,
            "private named-field types remain observable after visibility normalization"
        );
        assert_eq!(
            find_item(&inherited, "Tuple").contract,
            find_item(&named_type_changed, "Tuple").contract,
            "the unchanged tuple control remains stable"
        );

        let tuple_type_changed = snapshot_rust_api(&source(
            "pub mod api { pub struct Named { pub(crate) inner: u8 } pub struct Tuple(pub(super) u32); }",
        ));
        assert_eq!(
            find_item(&inherited, "Named").contract,
            find_item(&tuple_type_changed, "Named").contract,
            "the unchanged named-field control remains stable"
        );
        assert_ne!(
            find_item(&inherited, "Tuple").contract,
            find_item(&tuple_type_changed, "Tuple").contract,
            "private tuple-field types remain observable after visibility normalization"
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
            !snapshot_rust_api(&missing_default_root)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::MissingLibRoot),
            "a package without [lib] or src/lib.rs has no implicit library target"
        );

        let missing_explicit_default_root = MemorySource::new(&[(
            "Cargo.toml",
            b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\n",
        )]);
        assert!(
            snapshot_rust_api(&missing_explicit_default_root)
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::MissingLibRoot),
            "an explicit [lib] retains its default src/lib.rs requirement"
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

    #[test]
    fn include_proofs_do_not_cross_disjoint_cfg_declarations() {
        let fixture = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                b"#[cfg(unix)] pub type X = [u8; include!(\"u.rs\")];\n#[cfg(windows)] pub type X = [u8; include!(\"w.rs\")];\n",
            ),
            ("src/u.rs", b"1\n"),
            ("src/w.rs", b"2\n"),
        ]);
        let snapshot = snapshot_rust_api(&fixture);
        let include_proofs: Vec<_> = snapshot
            .unknowns
            .iter()
            .filter(|unknown| unknown.kind == RustApiUnknownKind::IncludeMacro)
            .collect();
        assert_eq!(
            include_proofs.len(),
            2,
            "one proof per reachable cfg region"
        );
        assert!(include_proofs.iter().all(|unknown| {
            !(unknown.cfg_guard.iter().any(|guard| guard.contains("unix"))
                && unknown
                    .cfg_guard
                    .iter()
                    .any(|guard| guard.contains("windows")))
        }));
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

        let send = snapshot_rust_api(&source(
            "pub struct Public<T>(pub T); impl<T> Public<T> where T: Send { pub fn value(&self) {} }",
        ));
        let sync = snapshot_rust_api(&source(
            "pub struct Public<T>(pub T); impl<T> Public<T> where T: Sync { pub fn value(&self) {} }",
        ));
        assert_ne!(
            find_item(&send, "Public::value").contract,
            find_item(&sync, "Public::value").contract,
            "the impl where-clause controls associated-item availability"
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

        let one_rootless_workspace = MemorySource::new(&[
            ("backend/Cargo.toml", b"[workspace]\nmembers=['api']\n"),
            (
                "backend/api/Cargo.toml",
                b"[package]\nname='backend-api'\nversion='0.0.0'\n",
            ),
            ("backend/api/src/lib.rs", b"pub fn backend() {}"),
        ]);
        let snapshot = snapshot_rust_api(&one_rootless_workspace);
        assert_eq!(snapshot.crates[0].name, "backend_api");
        assert!(snapshot.unknowns.is_empty());

        let ambiguous_rootless_workspaces = MemorySource::new(&[
            ("backend/Cargo.toml", b"[workspace]\nmembers=['api']\n"),
            (
                "backend/api/Cargo.toml",
                b"[package]\nname='backend-api'\nversion='0.0.0'\n",
            ),
            ("backend/api/src/lib.rs", b"pub fn backend() {}"),
            (
                "tests/fixtures/Cargo.toml",
                b"[workspace]\nmembers=['sample']\n",
            ),
            (
                "tests/fixtures/sample/Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            ("tests/fixtures/sample/src/lib.rs", b"pub fn fixture() {}"),
        ]);
        let snapshot = snapshot_rust_api(&ambiguous_rootless_workspaces);
        assert!(snapshot.crates.is_empty());
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("backend")
                && unknown.evidence.contains("tests/fixtures")
        }));

        for rootless in [
            MemorySource::new(&[
                ("broken/Cargo.toml", b"[package\n"),
                (
                    "fixture/Cargo.toml",
                    b"[package]\nname='fixture'\nversion='0.0.0'\n",
                ),
                ("fixture/src/lib.rs", b"pub fn fixture() {}"),
            ]),
            MemorySource::new(&[
                ("binary/Cargo.toml", &[0xff, 0xfe]),
                (
                    "fixture/Cargo.toml",
                    b"[package]\nname='fixture'\nversion='0.0.0'\n",
                ),
                ("fixture/src/lib.rs", b"pub fn fixture() {}"),
            ]),
            MemorySource::new(&[("broken/Cargo.toml", b"[package\n")]),
        ] {
            let snapshot = snapshot_rust_api(&rootless);
            assert!(
                snapshot.crates.is_empty(),
                "an unresolved rootless authority must not select a parseable sibling"
            );
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                    && unknown.evidence.contains("rootless workspace authority")
            }));
        }

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
    fn rust_api_snapshot_includes_implicit_path_dependency_workspace_members() {
        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/app']\nexclude=['crates/excluded']\n",
            ),
            (
                "crates/app/Cargo.toml",
                b"[package]\nname='app'\nversion='0.0.0'\n[dependencies]\nimplicit={path='../implicit'}\nexcluded={path='../excluded'}\n",
            ),
            ("crates/app/src/lib.rs", b"pub fn app() {}"),
            (
                "crates/implicit/Cargo.toml",
                b"[package]\nname='implicit'\nversion='0.0.0'\n[build-dependencies]\nleaf={path='../leaf'}\n",
            ),
            ("crates/implicit/src/lib.rs", b"pub fn implicit() {}"),
            (
                "crates/leaf/Cargo.toml",
                b"[package]\nname='leaf'\nversion='0.0.0'\n",
            ),
            ("crates/leaf/src/lib.rs", b"pub fn leaf() {}"),
            (
                "crates/excluded/Cargo.toml",
                b"[package]\nname='excluded'\nversion='0.0.0'\n",
            ),
            ("crates/excluded/src/lib.rs", b"pub fn excluded() {}"),
            (
                "tools/unrelated/Cargo.toml",
                b"[package]\nname='unrelated'\nversion='0.0.0'\n",
            ),
            ("tools/unrelated/src/lib.rs", b"pub fn unrelated() {}"),
        ]);
        let snapshot = snapshot_rust_api(&workspace);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app", "implicit", "leaf"],
            "Cargo path dependencies inside the workspace become members transitively, while exclude and unrelated packages remain out"
        );
    }

    #[test]
    fn rust_api_snapshot_accepts_cargo_build_true_and_keyword_crate_names() {
        let valid = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='type'\nversion='0.0.0'\nbuild=true\n[lib]\nname='type'\npath='src/lib.rs'\n",
            ),
            ("build.rs", b"fn main() {}"),
            ("src/lib.rs", b"pub fn api() {}"),
        ]);
        let snapshot = snapshot_rust_api(&valid);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["type"]
        );
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| { item.key.crate_name == "type" && item.key.external_name == "api" })
        );
        assert!(snapshot.unknowns.iter().all(|unknown| {
            unknown.kind != RustApiUnknownKind::ManifestParse
                || !unknown.evidence.contains("package.build")
        }));

        let missing_declared_build = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nbuild=true\n",
            ),
            ("src/lib.rs", b"pub fn api() {}"),
        ]);
        let snapshot = snapshot_rust_api(&missing_declared_build);
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::ManifestParse
                && unknown
                    .evidence
                    .contains("declared build script build.rs is unavailable")
        }));
    }

    #[test]
    fn rust_api_snapshot_respects_library_projection_modes() {
        for crate_type in ["cdylib", "staticlib", "bin"] {
            let manifest = format!(
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['{crate_type}']\n"
            );
            let source = MemorySource::new(&[
                ("Cargo.toml", manifest.as_bytes()),
                (
                    "src/lib.rs",
                    b"pub fn internal(value: u8) {} #[unsafe(no_mangle)] pub extern \"C\" fn exported() {}",
                ),
            ]);
            let snapshot = snapshot_rust_api(&source);
            assert!(
                snapshot.items.is_empty(),
                "{crate_type}-only targets are not Rust dependency surfaces: {:?}",
                snapshot.items
            );
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::UnsupportedExternResolution
                    && unknown.evidence.contains("exported")
                    && unknown.evidence.contains("public-binary-export:")
            }));
        }

        let associated_exports = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
            ),
            (
                "src/lib.rs",
                b"pub struct Api; pub trait Exported { extern \"C\" fn call(); } #[cfg(unix)] impl Api { #[cfg_attr(feature = \"ffi\", unsafe(no_mangle))] pub extern \"C\" fn exported() {} } impl Exported for Api { #[unsafe(export_name=\"trait_call\")] extern \"C\" fn call() {} }",
            ),
        ]);
        let snapshot = snapshot_rust_api(&associated_exports);
        assert!(snapshot.items.is_empty());
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::UnsupportedExternResolution
                && unknown.evidence.contains("Api::exported")
                && unknown.cfg_guard.iter().any(|guard| guard == "unix")
                && unknown
                    .cfg_guard
                    .iter()
                    .any(|guard| guard == "feature = \"ffi\"")
        }));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::UnsupportedExternResolution
                && unknown.evidence.contains("<Api as Exported>::call")
                && unknown.evidence.contains("trait_call")
        }));

        let mixed = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib','cdylib']\n",
            ),
            ("src/lib.rs", b"pub fn visible(value: u8) {}"),
        ]);
        assert!(
            snapshot_rust_api(&mixed)
                .items
                .iter()
                .any(|item| item.key.external_name == "visible")
        );

        let proc_macro = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n",
            ),
            (
                "src/lib.rs",
                b"#[proc_macro] pub fn bang(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input } pub struct Internal;",
            ),
        ]);
        let snapshot = snapshot_rust_api(&proc_macro);
        assert!(snapshot.items.iter().any(|item| {
            item.key.namespace == RustNamespace::Crate
                && item.contract.contains("crate_types=[\"proc-macro\"]")
        }));
        assert!(snapshot.items.iter().any(|item| {
            item.key.namespace == RustNamespace::Macro && item.key.external_name == "bang"
        }));
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.key.external_name == "Internal")
        );

        let crate_type_proc_macro = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['proc-macro']\n",
            ),
            (
                "src/lib.rs",
                b"#[proc_macro] pub fn bang(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input } pub struct Internal;",
            ),
        ]);
        let snapshot = snapshot_rust_api(&crate_type_proc_macro);
        assert!(snapshot.items.iter().any(|item| {
            item.key.namespace == RustNamespace::Macro && item.key.external_name == "bang"
        }));
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| item.key.external_name == "Internal")
        );
        assert!(!snapshot.unknowns.iter().any(|unknown| {
            unknown
                .evidence
                .contains("native-only library target is not a Rust dependency surface")
        }));
    }

    #[test]
    fn cargo_cfg_authority_respects_target_schema_and_package_links() {
        let parses = |text: &str| toml::from_str::<toml::Value>(text).expect("valid Cargo config");

        assert!(
            cargo_config_can_define_custom_cfg(
                &parses("[build]\nrustflags=['--cfg','public_api']\n"),
                None,
            )
            .expect("build rustflags authority")
        );
        assert!(
            cargo_config_can_define_custom_cfg(
                &parses("[target.'cfg(unix)']\nrustflags=['--cfg','public_api']\n"),
                None,
            )
            .expect("target rustflags authority")
        );
        let links_override =
            parses("[target.x86_64-unknown-linux-gnu.native]\nrustc-cfg=['public_api']\n");
        assert!(
            cargo_config_can_define_custom_cfg(&links_override, Some("native"))
                .expect("matching links override")
        );
        assert!(
            !cargo_config_can_define_custom_cfg(&links_override, Some("other"))
                .expect("mismatched links override")
        );
        assert!(
            !cargo_config_can_define_custom_cfg(&links_override, None)
                .expect("links override without package.links")
        );
        assert!(
            !cargo_config_can_define_custom_cfg(
                &parses("[target.'cfg(unix)'.native]\nrustc-cfg=['public_api']\n"),
                Some("native"),
            )
            .expect("cfg selector cannot host links override")
        );
        assert!(
            !cargo_config_can_define_custom_cfg(
                &parses("[env]\nRUSTFLAGS='--cfg public_api'\n"),
                None,
            )
            .expect("child-process environment is not Cargo rustflags authority")
        );

        let invalid_links = snapshot_rust_api(&MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nlinks='native'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", b"#[cfg(public_api)] pub fn api() {}"),
        ]));
        assert!(invalid_links.crates.is_empty());
        assert!(invalid_links.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::ManifestParse
                && unknown
                    .evidence
                    .contains("package.links requires a live build script")
        }));
    }

    #[test]
    fn rust_api_snapshot_binds_nested_custom_cfg_and_trait_member_proofs() {
        let source = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "build.rs",
                b"fn main() { println!(\"cargo::rustc-cfg=public_api\"); }",
            ),
            (
                "src/lib.rs",
                b"pub struct Holder { #[cfg_attr(unix, cfg(public_api))] pub field: u8 } pub trait Contract { #[cfg(feature = \"a\")] async fn fetch() {} #[cfg(not(feature = \"a\"))] async fn fetch() {} }",
            ),
        ]);
        let snapshot = snapshot_rust_api(&source);
        assert!(
            snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::CfgPredicate
                    && unknown.evidence.contains("public_api")
                    && unknown.evidence.contains("cfg-authority-digest:sha256:")
            }),
            "nested custom cfg authority proof is missing: {:?}",
            snapshot.unknowns
        );

        let opaque_proofs: Vec<_> = snapshot
            .unknowns
            .iter()
            .filter(|unknown| unknown.kind == RustApiUnknownKind::OpaqueReturnAutoTraits)
            .collect();
        assert_eq!(
            opaque_proofs.len(),
            2,
            "disjoint cfg-qualified trait methods must retain distinct opaque proofs: {opaque_proofs:?}"
        );
        assert!(opaque_proofs.iter().any(|unknown| {
            unknown
                .evidence
                .contains("trait-member-cfg:feature = \"a\"")
        }));
        assert!(opaque_proofs.iter().any(|unknown| {
            unknown
                .evidence
                .contains("trait-member-cfg:not(feature = \"a\")")
        }));
    }

    #[test]
    fn rust_api_workspace_discovery_matches_cargo_globs_and_exclude_precedence() {
        let bracket_glob = MemorySource::new(&[
            ("Cargo.toml", b"[workspace]\nmembers=['crates/[ab]*']\n"),
            (
                "crates/apple/Cargo.toml",
                b"[package]\nname='apple'\nversion='0.0.0'\n",
            ),
            ("crates/apple/src/lib.rs", b"pub fn apple() {}"),
            (
                "crates/banana/Cargo.toml",
                b"[package]\nname='banana'\nversion='0.0.0'\n",
            ),
            ("crates/banana/src/lib.rs", b"pub fn banana() {}"),
            (
                "crates/cherry/Cargo.toml",
                b"[package]\nname='cherry'\nversion='0.0.0'\n",
            ),
            ("crates/cherry/src/lib.rs", b"pub fn cherry() {}"),
        ]);
        assert_eq!(
            snapshot_rust_api(&bracket_glob)
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["apple", "banana"],
            "workspace member matching must use Cargo's glob grammar, including bracket classes"
        );

        let explicit_override = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/app']\nexclude=['crates']\n",
            ),
            (
                "crates/app/Cargo.toml",
                b"[package]\nname='app'\nversion='0.0.0'\n",
            ),
            ("crates/app/src/lib.rs", b"pub fn app() {}"),
        ]);
        assert_eq!(
            snapshot_rust_api(&explicit_override)
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app"],
            "an explicitly named Cargo member wins over an exclude path prefix"
        );

        let invalid =
            MemorySource::new(&[("Cargo.toml", b"[workspace]\nmembers=['crates/[broken']\n")]);
        assert!(snapshot_rust_api(&invalid).unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("invalid glob")
        }));

        let zero_matches =
            MemorySource::new(&[("Cargo.toml", b"[workspace]\nmembers=['crates/missing*']\n")]);
        assert!(
            snapshot_rust_api(&zero_matches)
                .unknowns
                .iter()
                .any(|unknown| {
                    unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                        && unknown.evidence.contains("matched no repository directory")
                }),
            "a valid Cargo member glob with zero matches must fail closed"
        );

        assert!(cargo_path_prefix("crates/app", "crates/app/tool"));
        assert!(!cargo_path_prefix("crates/app", "crates/application"));
        assert!(cargo_glob_matches("crates/*", "crates/app"));
        assert!(
            !cargo_glob_matches("crates/*", "crates/group/app"),
            "Cargo's single-star member glob cannot cross a path separator"
        );
        assert!(cargo_glob_matches("crates/**", "crates/group/app"));
        assert!(cargo_glob_matches("../shared", "../shared"));
    }

    #[test]
    fn rust_api_workspace_rejects_explicit_member_owned_by_nested_workspace() {
        let snapshot = snapshot_rust_api(&MemorySource::new(&[
            ("Cargo.toml", b"[workspace]\nmembers=['nested/member']\n"),
            ("nested/Cargo.toml", b"[workspace]\nmembers=['member']\n"),
            (
                "nested/member/Cargo.toml",
                b"[package]\nname='member'\nversion='0.0.0'\n",
            ),
            ("nested/member/src/lib.rs", b"pub fn member() {}"),
        ]));
        assert!(snapshot.crates.is_empty());
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("nested/member/Cargo.toml")
                && unknown
                    .evidence
                    .contains("explicit member is owned by another workspace authority")
        }));
    }

    #[test]
    fn rust_api_snapshot_includes_inherited_workspace_path_dependencies() {
        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/app']\n[workspace.dependencies]\nimplicit={path='crates/implicit'}\n",
            ),
            (
                "crates/app/Cargo.toml",
                b"[package]\nname='app'\nversion='0.0.0'\n[target.'cfg(unix)'.dev-dependencies]\nimplicit={workspace=true}\n",
            ),
            ("crates/app/src/lib.rs", b"pub fn app() {}"),
            (
                "crates/implicit/Cargo.toml",
                b"[package]\nname='implicit'\nversion='0.0.0'\n",
            ),
            ("crates/implicit/src/lib.rs", b"pub fn implicit() {}"),
        ]);
        let workspace_snapshot = snapshot_rust_api(&workspace);
        assert_eq!(
            workspace_snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app", "implicit"]
        );
    }

    #[test]
    fn rust_api_snapshot_rootless_package_workspace_keeps_its_root_package() {
        let workspace = MemorySource::new(&[
            (
                "backend/Cargo.toml",
                b"[package]\nname='backend-root'\nversion='0.0.0'\n[workspace]\nmembers=['member']\n",
            ),
            ("backend/src/lib.rs", b"pub fn root() {}"),
            (
                "backend/member/Cargo.toml",
                b"[package]\nname='member'\nversion='0.0.0'\n",
            ),
            ("backend/member/src/lib.rs", b"pub fn member() {}"),
        ]);
        assert_eq!(
            snapshot_rust_api(&workspace)
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["backend_root", "member"]
        );
    }

    #[test]
    fn rust_api_snapshot_rootless_workspace_can_own_an_explicit_sibling_member() {
        let workspace = MemorySource::new(&[
            (
                "backend/Cargo.toml",
                b"[workspace]\nmembers=['../shared']\n",
            ),
            (
                "shared/Cargo.toml",
                b"[package]\nname='shared'\nversion='0.0.0'\nworkspace='../backend'\n",
            ),
            ("shared/src/lib.rs", b"pub fn shared() {}"),
        ]);
        assert_eq!(
            snapshot_rust_api(&workspace)
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["shared"]
        );
    }

    #[test]
    fn rust_api_snapshot_rootless_workspace_rejects_nonreciprocal_sibling_member() {
        let snapshot = snapshot_rust_api(&MemorySource::new(&[
            (
                "backend/Cargo.toml",
                b"[workspace]\nmembers=['../shared']\n",
            ),
            (
                "shared/Cargo.toml",
                b"[package]\nname='shared'\nversion='0.0.0'\n",
            ),
            ("shared/src/lib.rs", b"pub fn shared() {}"),
        ]));
        assert!(snapshot.crates.is_empty());
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("shared/Cargo.toml")
                && unknown.evidence.contains("outside workspace")
        }));
    }

    #[test]
    fn rust_api_snapshot_rootless_workspace_rejects_unowned_sibling_package() {
        let snapshot = snapshot_rust_api(&MemorySource::new(&[
            ("backend/Cargo.toml", b"[workspace]\nmembers=[]\n"),
            (
                "fixture/Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            ("fixture/src/lib.rs", b"pub fn fixture() {}"),
        ]));
        assert!(
            snapshot.crates.is_empty(),
            "competing rootless authorities must not certify either API surface"
        );
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("backend/Cargo.toml")
                && unknown.evidence.contains("fixture/Cargo.toml")
                && unknown.evidence.contains("competes")
        }));
    }

    #[test]
    fn rust_api_snapshot_respects_package_workspace_override() {
        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/app']\n",
            ),
            (
                "crates/app/Cargo.toml",
                b"[package]\nname='app'\nversion='0.0.0'\n[dependencies]\nowned={path='../owned'}\nforeign={path='../foreign'}\n",
            ),
            ("crates/app/src/lib.rs", b"pub fn app() {}"),
            (
                "crates/owned/Cargo.toml",
                b"[package]\nname='owned'\nversion='0.0.0'\nworkspace='../..'\n",
            ),
            ("crates/owned/src/lib.rs", b"pub fn owned() {}"),
            (
                "crates/foreign/Cargo.toml",
                b"[package]\nname='foreign'\nversion='0.0.0'\nworkspace='../../other-workspace'\n",
            ),
            ("crates/foreign/src/lib.rs", b"pub fn foreign() {}"),
            (
                "other-workspace/Cargo.toml",
                b"[workspace]\nmembers=['../crates/foreign']\n",
            ),
        ]);
        let workspace_snapshot = snapshot_rust_api(&workspace);
        assert_eq!(
            workspace_snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["app", "owned"]
        );
        assert!(workspace_snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("crates/foreign/Cargo.toml")
                && unknown.evidence.contains("different package.workspace")
        }));
    }

    #[test]
    fn rust_api_snapshot_validates_declared_package_workspace_authority() {
        for source in [
            MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='root'\nversion='0.0.0'\nworkspace='missing'\n",
                ),
                ("src/lib.rs", b"pub fn root() {}"),
            ]),
            MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
                ),
                ("src/lib.rs", b"pub fn root() {}"),
                (
                    "authority/Cargo.toml",
                    b"[package]\nname='not-a-workspace'\nversion='0.0.0'\n",
                ),
            ]),
            MemorySource::new(&[
                (
                    "backend/Cargo.toml",
                    b"[package]\nname='backend'\nversion='0.0.0'\nworkspace='missing'\n",
                ),
                ("backend/src/lib.rs", b"pub fn backend() {}"),
            ]),
            MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
                ),
                ("src/lib.rs", b"pub fn root() {}"),
                (
                    "authority/Cargo.toml",
                    b"[workspace]\nmembers=['..', 'missing*']\n",
                ),
            ]),
            MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
                ),
                ("src/lib.rs", b"pub fn root() {}"),
                (
                    "authority/Cargo.toml",
                    b"[package]\nname='invalid-authority'\nversion='0.0.0'\nworkspace='.'\n[workspace]\nmembers=['..']\n",
                ),
            ]),
        ] {
            let snapshot = snapshot_rust_api(&source);
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                    && unknown.evidence.contains("workspace")
            }));
        }

        let valid = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
            ),
            ("src/lib.rs", b"pub fn root() {}"),
            ("authority/Cargo.toml", b"[workspace]\nmembers=['..']\n"),
        ]);
        let snapshot = snapshot_rust_api(&valid);
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::WorkspaceDiscovery),
            "a readable reciprocal package.workspace authority remains confirmed"
        );
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );

        let implicit = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
            ),
            ("src/lib.rs", b"pub fn root() {}"),
            ("authority/Cargo.toml", b"[workspace]\nmembers=['host']\n"),
            (
                "authority/host/Cargo.toml",
                b"[package]\nname='host'\nversion='0.0.0'\n[dependencies]\nroot={path='../..'}\n",
            ),
            ("authority/host/src/lib.rs", b"pub fn host() {}"),
        ]);
        let snapshot = snapshot_rust_api(&implicit);
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::WorkspaceDiscovery),
            "a root package reached through an implicit path member belongs to its declared workspace"
        );
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["host", "root"]
        );

        let unowned = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
            ),
            ("src/lib.rs", b"pub fn root() {}"),
            ("authority/Cargo.toml", b"[workspace]\nmembers=['host']\n"),
            (
                "authority/host/Cargo.toml",
                b"[package]\nname='host'\nversion='0.0.0'\n",
            ),
            ("authority/host/src/lib.rs", b"pub fn host() {}"),
        ]);
        let snapshot = snapshot_rust_api(&unowned);
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown
                    .evidence
                    .contains("does not select Cargo.toml as a member")
        }));
        assert!(
            snapshot
                .crates
                .iter()
                .all(|crate_snap| crate_snap.name != "root"),
            "a structurally valid but unowned package must not be certified as a workspace member"
        );

        let foreign_dependency = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
            ),
            ("src/lib.rs", b"pub fn root() {}"),
            (
                "authority/Cargo.toml",
                b"[workspace]\nmembers=['host']\n",
            ),
            (
                "authority/host/Cargo.toml",
                b"[package]\nname='host'\nversion='0.0.0'\n[dependencies]\nroot={path='../..'}\nforeign={path='../../foreign'}\n",
            ),
            ("authority/host/src/lib.rs", b"pub fn host() {}"),
            (
                "foreign/Cargo.toml",
                b"[package]\nname='foreign'\nversion='0.0.0'\n[workspace]\nmembers=[]\n",
            ),
            ("foreign/src/lib.rs", b"pub fn foreign() {}"),
        ]);
        let snapshot = snapshot_rust_api(&foreign_dependency);
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::WorkspaceDiscovery),
            "an outside path dependency with its own proven workspace authority is foreign, not an invalid member"
        );
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["host", "root"]
        );

        let no_authority_dependency = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='root'\nversion='0.0.0'\nworkspace='authority'\n",
            ),
            ("src/lib.rs", b"pub fn root() {}"),
            (
                "authority/Cargo.toml",
                b"[workspace]\nmembers=['host']\n",
            ),
            (
                "authority/host/Cargo.toml",
                b"[package]\nname='host'\nversion='0.0.0'\n[dependencies]\nroot={path='../..'}\nunknown={path='../../unknown'}\n",
            ),
            ("authority/host/src/lib.rs", b"pub fn host() {}"),
            (
                "unknown/Cargo.toml",
                b"[package]\nname='unknown'\nversion='0.0.0'\n",
            ),
            ("unknown/src/lib.rs", b"pub fn unknown() {}"),
        ]);
        let snapshot = snapshot_rust_api(&no_authority_dependency);
        assert!(
            !snapshot
                .unknowns
                .iter()
                .any(|unknown| unknown.kind == RustApiUnknownKind::WorkspaceDiscovery),
            "Cargo treats an outside path dependency with no workspace root as foreign"
        );
        assert!(
            snapshot
                .crates
                .iter()
                .all(|crate_snap| crate_snap.name != "unknown")
        );

        let contradictory_dependency = MemorySource::new(&[
            ("Cargo.toml", b"[workspace]\nmembers=['app']\n"),
            (
                "app/Cargo.toml",
                b"[package]\nname='app'\nversion='0.0.0'\n[dependencies]\nbad={path='../bad'}\n",
            ),
            ("app/src/lib.rs", b"pub fn app() {}"),
            (
                "bad/Cargo.toml",
                b"[package]\nname='bad'\nversion='0.0.0'\nworkspace='..'\n[workspace]\nmembers=[]\n",
            ),
            ("bad/src/lib.rs", b"pub fn bad() {}"),
        ]);
        let snapshot = snapshot_rust_api(&contradictory_dependency);
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("bad/Cargo.toml")
                && unknown
                    .evidence
                    .contains("both [workspace] and package.workspace")
        }));
        assert!(
            snapshot
                .crates
                .iter()
                .all(|crate_snap| crate_snap.name != "bad"),
            "a Cargo-invalid path dependency must never enter the certified crate set"
        );
    }

    #[test]
    fn rust_api_snapshot_rejects_malformed_workspace_member_lists() {
        for (manifest, expected) in [
            ("[workspace]\nmembers='crates/*'\n", "workspace.members"),
            (
                "[workspace]\nmembers=['crates/app']\nexclude=['crates/skip', 1]\n",
                "workspace.exclude",
            ),
        ] {
            let source = MemorySource::new(&[
                ("Cargo.toml", manifest.as_bytes()),
                (
                    "crates/app/Cargo.toml",
                    b"[package]\nname='app'\nversion='0.0.0'\n",
                ),
                ("crates/app/src/lib.rs", b"pub fn app() {}"),
            ]);
            let snapshot = snapshot_rust_api(&source);
            assert!(snapshot.crates.is_empty());
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                    && unknown.evidence.contains(expected)
            }));
        }
    }

    #[test]
    fn rust_api_snapshot_fails_closed_for_semantically_invalid_authorities() {
        let rooted = snapshot_rust_api(&MemorySource::new(&[(
            "Cargo.toml",
            b"[dependencies]\nserde='1'\n",
        )]));
        assert!(rooted.crates.is_empty());
        assert!(rooted.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::ManifestParse
                && unknown.source_path == "Cargo.toml"
                && unknown.evidence.contains("neither a package table")
        }));

        let rootless = snapshot_rust_api(&MemorySource::new(&[
            ("broken/Cargo.toml", b"[dependencies]\nserde='1'\n"),
            (
                "fixture/Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            ("fixture/src/lib.rs", b"pub fn fixture() {}"),
        ]));
        assert!(
            rootless.crates.is_empty(),
            "a parseable invalid authority must not select a valid sibling"
        );
        assert!(rootless.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("broken/Cargo.toml")
                && unknown.evidence.contains("unreadable or invalid")
        }));

        for source in [
            MemorySource::new(&[
                (
                    "Cargo.toml",
                    b"[package]\nname='root'\nversion='0.0.0'\nworkspace='.'\n[workspace]\nmembers=[]\n",
                ),
                ("src/lib.rs", b"pub fn root() {}"),
            ]),
            MemorySource::new(&[
                (
                    "backend/Cargo.toml",
                    b"[package]\nname='root'\nversion='0.0.0'\nworkspace='.'\n[workspace]\nmembers=[]\n",
                ),
                ("backend/src/lib.rs", b"pub fn root() {}"),
            ]),
        ] {
            let snapshot = snapshot_rust_api(&source);
            assert!(snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                    && unknown
                        .evidence
                        .contains("package.workspace cannot be specified")
            }));
        }
    }

    #[test]
    fn rust_api_snapshot_fails_closed_for_unavailable_explicit_members() {
        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/broken', 'crates/missing']\n",
            ),
            ("crates/broken/Cargo.toml", b"[package\n"),
        ]);
        let snapshot = snapshot_rust_api(&workspace);
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("crates/broken/Cargo.toml")
                && unknown.evidence.contains("crates/missing/Cargo.toml")
        }));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::ManifestParse
                && unknown.source_path == "crates/broken/Cargo.toml"
        }));
    }

    #[test]
    fn rust_api_snapshot_fails_closed_for_globbed_member_directory_without_manifest() {
        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/*']\nexclude=['crates/excluded']\n",
            ),
            (
                "crates/good/Cargo.toml",
                b"[package]\nname='good'\nversion='0.0.0'\n",
            ),
            ("crates/good/src/lib.rs", b"pub fn good() {}"),
            ("crates/missing/src/lib.rs", b"pub fn missing() {}"),
            ("crates/excluded/src/lib.rs", b"pub fn excluded() {}"),
            ("docs/guide/README.md", b"not a workspace member"),
        ]);
        let snapshot = snapshot_rust_api(&workspace);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["good"]
        );
        let discovery = snapshot
            .unknowns
            .iter()
            .find(|unknown| unknown.kind == RustApiUnknownKind::WorkspaceDiscovery)
            .expect("globbed package-like directory without Cargo.toml must fail closed");
        assert!(discovery.evidence.contains("crates/missing/Cargo.toml"));
        assert!(!discovery.evidence.contains("crates/excluded"));
        assert!(!discovery.evidence.contains("docs/guide"));
    }

    #[test]
    fn rust_api_snapshot_rootless_workspace_detects_globbed_member_without_manifest() {
        let workspace = MemorySource::new(&[
            ("backend/Cargo.toml", b"[workspace]\nmembers=['crates/*']\n"),
            (
                "backend/crates/good/Cargo.toml",
                b"[package]\nname='good'\nversion='0.0.0'\nworkspace='../..'\n",
            ),
            ("backend/crates/good/src/lib.rs", b"pub fn good() {}"),
            ("backend/crates/missing/src/lib.rs", b"pub fn missing() {}"),
        ]);
        let snapshot = snapshot_rust_api(&workspace);
        assert_eq!(
            snapshot
                .crates
                .iter()
                .map(|crate_snap| crate_snap.name.as_str())
                .collect::<Vec<_>>(),
            vec!["good"]
        );
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown
                    .evidence
                    .contains("backend/crates/missing/Cargo.toml")
        }));
    }

    #[test]
    fn rust_api_snapshot_fails_closed_for_unavailable_path_dependency_manifests() {
        let workspace = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['crates/app']\n",
            ),
            (
                "crates/app/Cargo.toml",
                b"[package]\nname='app'\nversion='0.0.0'\n[dependencies]\nmissing={path='../missing'}\nbroken={path='../broken'}\n",
            ),
            ("crates/app/src/lib.rs", b"pub fn app() {}"),
            ("crates/broken/Cargo.toml", b"[package\n"),
        ]);
        let snapshot = snapshot_rust_api(&workspace);
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::WorkspaceDiscovery
                && unknown.evidence.contains("crates/missing/Cargo.toml")
                && unknown.evidence.contains("crates/broken/Cargo.toml")
        }));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::ManifestParse
                && unknown.source_path == "crates/broken/Cargo.toml"
        }));
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
    fn private_alias_resolution_retains_cfg_target_pairs_and_accumulates_guards() {
        let hidden = ("fixture".to_owned(), Vec::new(), "Hidden".to_owned());
        let a = (
            "fixture".to_owned(),
            vec!["a".to_owned()],
            "Item".to_owned(),
        );
        let b = (
            "fixture".to_owned(),
            vec!["b".to_owned()],
            "Item".to_owned(),
        );
        let mut aliases = BTreeMap::new();
        aliases.insert(
            hidden.clone(),
            vec![
                (a.clone(), vec!["unix".to_owned()]),
                (b.clone(), vec!["windows".to_owned()]),
            ],
        );
        let resolved =
            resolve_private_type_alias_keys(hidden.clone(), &[], &aliases, &BTreeMap::new());
        assert!(resolved.states.contains(&GuardedPrivateTypeKey {
            key: a.clone(),
            cfg_guard: vec!["unix".to_owned()],
        }));
        assert!(resolved.states.contains(&GuardedPrivateTypeKey {
            key: b.clone(),
            cfg_guard: vec!["windows".to_owned()],
        }));

        let mid = ("fixture".to_owned(), Vec::new(), "Mid".to_owned());
        aliases.clear();
        aliases.insert(
            hidden.clone(),
            vec![(mid.clone(), vec!["feature = \"x\"".to_owned()])],
        );
        aliases.insert(
            mid,
            vec![(a.clone(), vec!["not(feature = \"x\")".to_owned()])],
        );
        let impossible = resolve_private_type_alias_keys(hidden, &[], &aliases, &BTreeMap::new());
        assert!(
            impossible.states.iter().all(|state| state.key != a),
            "a later alias edge must be checked against the accumulated path guard"
        );
        assert!(!impossible.exhausted);

        let growing_initial = (
            "fixture".to_owned(),
            vec!["a".to_owned()],
            "Item".to_owned(),
        );
        let growing = |segment: &str| {
            BTreeMap::from([(
                ("fixture".to_owned(), vec!["a".to_owned()]),
                vec![(vec!["a".to_owned(), segment.to_owned()], Vec::new())],
            )])
        };
        let left = resolve_private_type_alias_keys(
            growing_initial.clone(),
            &[],
            &BTreeMap::new(),
            &growing("b"),
        );
        let right =
            resolve_private_type_alias_keys(growing_initial, &[], &BTreeMap::new(), &growing("c"));
        assert!(left.exhausted && right.exhausted);
        assert_ne!(
            left.exhaustion_digest, right.exhaustion_digest,
            "different exhausted alias graphs need different fail-closed evidence"
        );
    }

    fn private_dependency_evidence(snapshot: &RustApiSnapshot, public_name: &str) -> String {
        snapshot
            .unknowns
            .iter()
            .find(|unknown| {
                unknown.kind == RustApiUnknownKind::PrivateTypeDependency
                    && unknown.evidence.contains(public_name)
            })
            .unwrap_or_else(|| {
                panic!(
                    "missing private dependency evidence for {public_name}: {:#?}",
                    snapshot.unknowns
                )
            })
            .evidence
            .clone()
    }

    #[test]
    fn private_dependency_digest_preserves_cfg_alias_target_correlation() {
        let prefix = "mod a { pub struct Item; } mod b { pub struct Item; } ";
        let base = snapshot_rust_api(&source(&format!(
            "{prefix} #[cfg(unix)] use a::Item as Hidden; \
             #[cfg(windows)] use b::Item as Hidden; \
             pub fn make() -> Hidden {{ todo!() }}"
        )));
        let swapped = snapshot_rust_api(&source(&format!(
            "{prefix} #[cfg(unix)] use b::Item as Hidden; \
             #[cfg(windows)] use a::Item as Hidden; \
             pub fn make() -> Hidden {{ todo!() }}"
        )));
        let reordered = snapshot_rust_api(&source(&format!(
            "{prefix} #[cfg(windows)] use b::Item as Hidden; \
             #[cfg(unix)] use a::Item as Hidden; \
             pub fn make() -> Hidden {{ todo!() }}"
        )));

        let base_evidence = private_dependency_evidence(&base, "make");
        assert_ne!(
            base_evidence,
            private_dependency_evidence(&swapped, "make"),
            "swapping cfg-selected alias targets changes private type semantics"
        );
        assert_eq!(
            base_evidence,
            private_dependency_evidence(&reordered, "make"),
            "source order must not change canonical private dependency evidence"
        );
    }

    #[test]
    fn private_dependency_fingerprints_unresolved_external_alias_targets() {
        let dep_a = snapshot_rust_api(&source(
            "use dep_a::Type as Hidden; pub fn make() -> Hidden { todo!() }",
        ));
        let dep_b = snapshot_rust_api(&source(
            "use dep_b::Type as Hidden; pub fn make() -> Hidden { todo!() }",
        ));
        assert_ne!(
            private_dependency_evidence(&dep_a, "make"),
            private_dependency_evidence(&dep_b, "make"),
            "a private alias retarget must not disappear when neither external terminal has a local declaration"
        );
    }

    #[test]
    fn private_dependency_resolves_extern_crate_self_alias_to_the_real_root_type() {
        let base = snapshot_rust_api(&source(
            "extern crate self as alias; struct Hidden(u8); \
             pub struct Api(pub alias::Hidden);",
        ));
        let changed = snapshot_rust_api(&source(
            "extern crate self as alias; struct Hidden(std::rc::Rc<()>); \
             pub struct Api(pub alias::Hidden);",
        ));

        let base_evidence = private_dependency_evidence(&base, "Api");
        let changed_evidence = private_dependency_evidence(&changed, "Api");

        assert_ne!(
            base_evidence, changed_evidence,
            "a private type change behind extern crate self must not become a false-green delta"
        );
    }

    #[test]
    fn private_impl_evidence_preserves_cfg_selected_owner_region() {
        let base = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; } trait Local {} \
             #[cfg(unix)] use a::Item as Owner; \
             #[cfg(windows)] use b::Item as Owner; \
             impl Local for Owner {} \
             pub fn expose_a() -> a::Item { todo!() }",
        ));
        let swapped = snapshot_rust_api(&source(
            "mod a { pub struct Item; } mod b { pub struct Item; } trait Local {} \
             #[cfg(unix)] use b::Item as Owner; \
             #[cfg(windows)] use a::Item as Owner; \
             impl Local for Owner {} \
             pub fn expose_a() -> a::Item { todo!() }",
        ));
        assert_ne!(
            private_dependency_evidence(&base, "expose_a"),
            private_dependency_evidence(&swapped, "expose_a"),
            "impl evidence must keep the effective cfg region of its canonical owner"
        );
    }

    #[test]
    fn private_impl_evidence_preserves_joint_trait_and_owner_cfg_region() {
        let prefix = "mod a { pub struct Item; } mod b { pub struct Item; } \
                      trait TraitA {} trait TraitB {} \
                      #[cfg(unix)] use a::Item as Owner; \
                      #[cfg(windows)] use b::Item as Owner; ";
        let base = snapshot_rust_api(&source(&format!(
            "{prefix} #[cfg(unix)] use TraitA as Local; \
             #[cfg(windows)] use TraitB as Local; \
             impl Local for Owner {{}} \
             pub fn expose_a() -> a::Item {{ todo!() }}"
        )));
        let swapped = snapshot_rust_api(&source(&format!(
            "{prefix} #[cfg(unix)] use TraitB as Local; \
             #[cfg(windows)] use TraitA as Local; \
             impl Local for Owner {{}} \
             pub fn expose_a() -> a::Item {{ todo!() }}"
        )));

        assert_ne!(
            private_dependency_evidence(&base, "expose_a"),
            private_dependency_evidence(&swapped, "expose_a"),
            "trait evidence must stay correlated with the cfg-selected owner instead of becoming one global set"
        );
    }

    #[test]
    fn trait_impl_visibility_requires_one_overlapping_cfg_region() {
        let snapshot = snapshot_rust_api(&source(
            "#[cfg(unix)] pub trait Marker {} \
             #[cfg(windows)] trait Marker {} \
             #[cfg(windows)] pub struct Value; \
             #[cfg(unix)] struct Value; \
             impl Marker for Value {}",
        ));
        assert!(
            snapshot
                .unknowns
                .iter()
                .all(|unknown| unknown.kind != RustApiUnknownKind::TraitImplResolution),
            "a public trait and public owner in disjoint cfg regions are not one observable impl: {:#?}",
            snapshot.unknowns
        );
    }

    #[test]
    fn private_dependency_digest_is_alias_spelling_independent() {
        let alias = snapshot_rust_api(&source(
            "mod model { pub struct Hidden; } use model::Hidden as Alias; \
             trait Local {} impl Local for Alias {} pub fn make() -> Alias { Alias }",
        ));
        let canonical = snapshot_rust_api(&source(
            "mod model { pub struct Hidden; } use model::Hidden as Alias; \
             trait Local {} impl Local for Alias {} \
             pub fn make() -> model::Hidden { model::Hidden }",
        ));
        let evidence = |snapshot: &RustApiSnapshot| {
            snapshot
                .unknowns
                .iter()
                .filter(|unknown| unknown.kind == RustApiUnknownKind::PrivateTypeDependency)
                .map(|unknown| unknown.evidence.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            evidence(&alias),
            evidence(&canonical),
            "one impl reached through an alias and its canonical owner must contribute once"
        );
    }

    #[test]
    fn private_dependency_follows_private_type_aliases_and_use_aliases() {
        let snapshot = snapshot_rust_api(&source(
            "mod model { pub struct Hidden; pub trait Local {} } \
             type TypeAlias = model::Hidden; \
             use model::{Hidden as UseAlias, Local as TraitAlias}; \
             impl TraitAlias for TypeAlias {} \
             pub fn make() -> UseAlias { model::Hidden }",
        ));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::PrivateTypeDependency
                && unknown.evidence.contains("non-public local type semantics")
        }));
        assert!(
            !snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::TraitImplResolution
                    && !unknown.evidence.contains("finite graph bound")
            }),
            "wholly local trait and owner aliases must not be mistaken for external impls"
        );
    }

    #[test]
    fn private_dependency_glob_alias_resolves_without_path_growth() {
        let snapshot = snapshot_rust_api(&source(
            "mod model { pub struct Hidden; } use model::*; \
             trait Local {} impl Local for Hidden {} \
             pub fn make() -> model::Hidden { model::Hidden }",
        ));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::PrivateTypeDependency
                && unknown.evidence.contains("non-public local type semantics")
        }));
        assert!(
            snapshot
                .unknowns
                .iter()
                .all(|unknown| !unknown.evidence.contains("finite graph bound"))
        );
    }

    #[test]
    fn private_alias_path_growth_is_bounded_and_fails_closed() {
        let snapshot = snapshot_rust_api(&source(
            "mod a { pub mod b { pub struct T; } } use a::b as a; \
             trait Local {} impl Local for a::T {}",
        ));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            matches!(
                unknown.kind,
                RustApiUnknownKind::PrivateTypeDependency | RustApiUnknownKind::TraitImplResolution
            ) && unknown.evidence.contains("finite graph bound")
        }));
    }

    #[test]
    fn private_dependency_proves_direct_cfg_atom_negation() {
        let snapshot = snapshot_rust_api(&source(
            "#[cfg(feature = \"public\")] pub struct Hidden(u8); \
             #[cfg(not(feature = \"public\"))] struct Hidden(u16); \
             #[cfg(not(feature = \"public\"))] pub fn make() -> Hidden { Hidden(0) }",
        ));
        assert!(guards_proven_disjoint(
            &["feature = \"public\"".to_owned()],
            &["not(feature = \"public\")".to_owned()]
        ));
        assert!(snapshot.unknowns.iter().any(|unknown| {
            unknown.kind == RustApiUnknownKind::PrivateTypeDependency
                && unknown.cfg_guard == ["not(feature = \"public\")"]
        }));
    }

    #[test]
    fn private_dependency_keeps_unproven_composite_cfg_complement() {
        let public_guard = vec!["all(unix, feature = \"x\")".to_owned()];
        let private_source_guard = vec!["not(all(unix, feature = \"x\"))".to_owned()];
        assert!(
            !guards_proven_disjoint(&public_guard, &private_source_guard),
            "the bounded solver deliberately does not prove nested boolean complements"
        );

        let snapshot = snapshot_rust_api(&source(
            "#[cfg(all(unix, feature = \"x\"))] pub struct Hidden(u8); \
             #[cfg(not(all(unix, feature = \"x\")))] struct Hidden(std::rc::Rc<()>); \
             #[cfg(not(all(unix, feature = \"x\")))] \
             pub struct Api { pub field: Hidden }",
        ));
        assert!(
            snapshot.unknowns.iter().any(|unknown| {
                unknown.kind == RustApiUnknownKind::PrivateTypeDependency
                    && unknown.cfg_guard == ["not(all(feature = \"x\",unix))"]
                    && unknown.evidence.contains("non-public local type semantics")
            }),
            "unknowns: {:#?}",
            snapshot.unknowns
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

    #[test]
    fn opaque_substrate_digest_is_alpha_stable() {
        let manifest = b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let base_source = b"pub async fn api<T: Default>() { let _: T = T::default(); }\n";
        let target_source = b"pub async fn api<U: Default>() { let _: U = U::default(); }\n";
        assert_eq!(
            opaque_source_canonical_bytes(base_source),
            opaque_source_canonical_bytes(target_source)
        );

        let base = MemorySource::new(&[
            ("Cargo.toml", manifest),
            ("Cargo.lock", b"version = 4\n"),
            ("src/lib.rs", base_source),
        ]);
        let target = MemorySource::new(&[
            ("Cargo.toml", manifest),
            ("Cargo.lock", b"version = 4\n"),
            ("src/lib.rs", target_source),
        ]);
        let manifests = vec!["Cargo.toml".to_owned()];
        let allowed = BTreeSet::from(["Cargo.toml".to_owned()]);
        let digest = |source: &MemorySource| {
            let inventory = source
                .entries()
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect();
            opaque_implementation_digest(source, &inventory, &manifests, &allowed).unwrap()
        };
        assert_eq!(digest(&base), digest(&target));

        let temp = tempfile::tempdir().unwrap();
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("Cargo.toml"), manifest).unwrap();
        fs::write(temp.path().join("Cargo.lock"), "version = 4\n").unwrap();
        fs::write(temp.path().join("src/lib.rs"), base_source).unwrap();
        let base_oid = commit(temp.path(), "base");
        fs::write(temp.path().join("src/lib.rs"), target_source).unwrap();
        let target_oid = commit(temp.path(), "target");
        let repo = Repository::open(temp.path()).unwrap();
        let git_digest = |oid: &str| {
            let source = GitTree::new(&repo, oid).unwrap();
            let inventory = source
                .entries()
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect();
            opaque_implementation_digest(&source, &inventory, &manifests, &allowed).unwrap()
        };
        assert_eq!(git_digest(&base_oid), git_digest(&target_oid));
    }

    #[test]
    fn effective_lock_follows_declared_workspace_authority() {
        let source = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[package]\nname='fixture'\nversion='0.0.0'\nworkspace='authority'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "authority/Cargo.toml",
                b"[workspace]\nmembers=['..']\nresolver='2'\n",
            ),
            ("authority/Cargo.lock", b"version = 4\n"),
            ("src/lib.rs", b"pub async fn api() {}\n"),
        ]);
        let inventory = source
            .entries()
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        let parsed = ["Cargo.toml", "authority/Cargo.toml"]
            .into_iter()
            .map(|path| (path.to_owned(), peek_manifest_toml(&source, path).unwrap()))
            .collect();
        assert_eq!(
            effective_cargo_lock_paths(
                &inventory,
                &parsed,
                &BTreeSet::from(["Cargo.toml".to_owned()])
            )
            .unwrap(),
            BTreeSet::from(["authority/Cargo.lock".to_owned()])
        );
    }

    #[test]
    fn implementation_proofs_reject_unresolved_tracked_symlinks() {
        let source = MemorySource::new(&[
            (
                "Cargo.toml",
                b"[workspace]\nmembers=['api','macros']\nresolver='2'\n",
            ),
            ("Cargo.lock", b"version = 4\n"),
            (
                "api/Cargo.toml",
                b"[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n",
            ),
            (
                "api/src/lib.rs",
                b"#[macros::expose] pub struct Api;\npub async fn future() {}\n",
            ),
            (
                "macros/Cargo.toml",
                b"[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n",
            ),
            (
                "macros/src/lib.rs",
                b"#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n",
            ),
            ("macros/schema.json", b"../../outside-repo/schema.json"),
        ]);
        let mut inventory = source
            .entries()
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>();
        let symlink = inventory.get_mut("macros/schema.json").unwrap();
        symlink.kind = RevisionEntryKind::Symlink;
        symlink.mode = 0o120000;
        symlink.state = RevisionEntryState::NonRegular {
            kind: RevisionEntryKind::Symlink,
        };
        let manifests = vec![
            "Cargo.toml".to_owned(),
            "api/Cargo.toml".to_owned(),
            "macros/Cargo.toml".to_owned(),
        ];
        let allowed = BTreeSet::from(["api/Cargo.toml".to_owned(), "macros/Cargo.toml".to_owned()]);
        let macro_error =
            proc_macro_implementation_digest(&source, &inventory, &manifests, &allowed)
                .unwrap_err();
        assert!(macro_error.contains("tracked symlink macros/schema.json"));
        let opaque_error =
            opaque_implementation_digest(&source, &inventory, &manifests, &allowed).unwrap_err();
        assert!(opaque_error.contains("tracked symlink macros/schema.json"));
    }
}
