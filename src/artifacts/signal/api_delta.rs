//! Canonical comparison of two revision-backed Rust API snapshots.
//!
//! This module is intentionally side-effect free. It computes one typed delta
//! from exact repository revisions and exposes deterministic projections used
//! by both production Rust API artifacts.

use super::api_surface::{
    RustApiDeclaration, RustApiItem, RustApiItemKey, RustApiSnapshot, RustApiUnknown,
    RustApiUnknownKind, RustNamespace, RustSourceCertainty, guards_proven_disjoint,
};
use super::revision_source::RevisionProvenance;
use crate::git::{Diff, Repository};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

pub const REPO_BACKED_RUST_API_SOURCE: &str = "repo_backed_rust_api";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiDeltaKind {
    Added,
    Removed,
    Changed,
    Relocated,
    VisibilityChanged,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiDeltaConfidence {
    Confirmed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiSnapshotSide {
    Base,
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ApiUnknownSource {
    pub side: ApiSnapshotSide,
    pub source_path: String,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ApiIdentity {
    pub crate_name: String,
    pub module_path: Vec<String>,
    pub namespace: String,
    pub name: String,
    pub cfg_region: Vec<String>,
}

impl ApiIdentity {
    pub fn external_path(&self) -> String {
        if self.module_path.is_empty() {
            "crate".to_owned()
        } else {
            format!("crate::{}", self.module_path.join("::"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ApiFactSide {
    pub identity: ApiIdentity,
    pub contract: String,
    pub source_path: String,
    pub evidence: String,
    pub provenance: String,
    pub declared_public: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ApiDeltaFinding {
    pub id: String,
    pub kind: ApiDeltaKind,
    pub identity: ApiIdentity,
    pub before: Option<ApiFactSide>,
    pub after: Option<ApiFactSide>,
    pub analysis_source: &'static str,
    pub confidence: ApiDeltaConfidence,
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_source: Option<ApiUnknownSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ApiDelta {
    pub analysis_source: &'static str,
    pub base_revision: String,
    pub target_revision: String,
    pub added: Vec<ApiDeltaFinding>,
    pub removed: Vec<ApiDeltaFinding>,
    pub changed: Vec<ApiDeltaFinding>,
    pub relocated: Vec<ApiDeltaFinding>,
    pub visibility_changed: Vec<ApiDeltaFinding>,
    pub unknown: Vec<ApiDeltaFinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiArtifactViewKind {
    BreakingChanges,
    PublicApiDiff,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ApiDeltaCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub relocated: usize,
    pub visibility_changed: usize,
    pub unknown: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ApiArtifactView {
    pub view: ApiArtifactViewKind,
    pub analysis_source: &'static str,
    pub base_revision: String,
    pub target_revision: String,
    pub counts: ApiDeltaCounts,
    pub findings: Vec<ApiDeltaFinding>,
}

struct SnapshotEvidence<'a> {
    base_declarations: &'a [RustApiDeclaration],
    target_declarations: &'a [RustApiDeclaration],
    base_unknowns: &'a [RustApiUnknown],
    target_unknowns: &'a [RustApiUnknown],
}

impl ApiDelta {
    pub fn counts(&self) -> ApiDeltaCounts {
        ApiDeltaCounts {
            added: self.added.len(),
            removed: self.removed.len(),
            changed: self.changed.len(),
            relocated: self.relocated.len(),
            visibility_changed: self.visibility_changed.len(),
            unknown: self.unknown.len(),
        }
    }

    pub fn findings(&self) -> Vec<ApiDeltaFinding> {
        let mut findings = Vec::new();
        findings.extend(self.added.iter().cloned());
        findings.extend(self.removed.iter().cloned());
        findings.extend(self.changed.iter().cloned());
        findings.extend(self.relocated.iter().cloned());
        findings.extend(self.visibility_changed.iter().cloned());
        findings.extend(self.unknown.iter().cloned());
        findings.sort();
        findings
    }
}

/// Compare the base and target snapshots once. Every fact is admitted to one
/// primary bucket only; artifact projections never repeat pairing.
pub fn compare_rust_api(base: &RustApiSnapshot, target: &RustApiSnapshot) -> ApiDelta {
    let mut delta = ApiDelta {
        analysis_source: REPO_BACKED_RUST_API_SOURCE,
        base_revision: provenance_id(&base.provenance),
        target_revision: provenance_id(&target.provenance),
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
        relocated: Vec::new(),
        visibility_changed: Vec::new(),
        unknown: snapshot_unknown_findings(base, target),
    };

    let base_items: Vec<_> = base.items.iter().map(item_side).collect();
    let target_items: Vec<_> = target.items.iter().map(item_side).collect();
    let mut base_used = vec![false; base_items.len()];
    let mut target_used = vec![false; target_items.len()];

    pair_exact_identities(
        &base_items,
        &target_items,
        &base.unknowns,
        &target.unknowns,
        &mut base_used,
        &mut target_used,
        &mut delta,
    );

    // A cfg-region change may still be paired when the external key has one
    // remaining fact on each side. Anything wider is ambiguous, not guessed.
    pair_cfg_changes(
        &base_items,
        &target_items,
        &base.unknowns,
        &target.unknowns,
        &mut base_used,
        &mut target_used,
        &mut delta,
    );

    // Visibility changes require a parsed non-public counterpart in a parent
    // module that is itself externally reachable. Absence alone is removal.
    pair_visibility_changes(
        &base_items,
        &target_items,
        &SnapshotEvidence {
            base_declarations: &base.declarations,
            target_declarations: &target.declarations,
            base_unknowns: &base.unknowns,
            target_unknowns: &target.unknowns,
        },
        &mut base_used,
        &mut target_used,
        &mut delta,
    );

    // Relocation is deliberately late: only unmatched, semantically identical,
    // one-to-one facts may move. Ambiguous groups become typed unknowns and are
    // consumed so they cannot leak as add+remove pairs.
    pair_relocations(
        &base_items,
        &target_items,
        &base.unknowns,
        &target.unknowns,
        &mut base_used,
        &mut target_used,
        &mut delta,
    );

    consume_one_sided_ambiguities(
        &base_items,
        &target_items,
        &mut base_used,
        &mut target_used,
        &mut delta,
    );

    for (index, before) in base_items.iter().enumerate() {
        if base_used[index] {
            continue;
        }
        if region_is_unknown(&target.unknowns, &before.identity) {
            delta.unknown.push(pairing_unknown(
                before.identity.clone(),
                Some(before.clone()),
                None,
                "target counterpart is unprovable in an unknown snapshot region",
            ));
        } else {
            delta.removed.push(known_finding(
                ApiDeltaKind::Removed,
                before.identity.clone(),
                Some(before.clone()),
                None,
            ));
        }
    }
    for (index, after) in target_items.iter().enumerate() {
        if target_used[index] {
            continue;
        }
        if region_is_unknown(&base.unknowns, &after.identity) {
            delta.unknown.push(pairing_unknown(
                after.identity.clone(),
                None,
                Some(after.clone()),
                "base counterpart is unprovable in an unknown snapshot region",
            ));
        } else {
            delta.added.push(known_finding(
                ApiDeltaKind::Added,
                after.identity.clone(),
                None,
                Some(after.clone()),
            ));
        }
    }

    normalize_delta(&mut delta);
    delta
}

/// Build the production Rust API delta from the exact Git trees that own each
/// artifact diff. Every multi-base comparison retains its own base OID; no
/// checkout, working tree, patch text, or `diffs.first()` fallback participates.
/// Target snapshots are reused by exact OID, while every base/target pair is
/// compared exactly once and then folded into one deterministic delta consumed
/// by both artifact views.
pub fn compare_rust_api_revisions(repo: &Repository, diffs: &[Diff]) -> Result<Option<ApiDelta>> {
    use super::api_surface::snapshot_rust_api;
    use super::revision_source::GitTree;

    if diffs.is_empty() {
        return Ok(None);
    }

    let unique_diffs = unique_exact_revision_pairs(diffs);
    let mut target_snapshots = BTreeMap::new();
    let mut comparisons = Vec::with_capacity(diffs.len());
    for diff in unique_diffs {
        let base = snapshot_rust_api(&GitTree::new(repo, &diff.base_commit_id)?);
        if !target_snapshots.contains_key(&diff.target_commit_id) {
            let target = snapshot_rust_api(&GitTree::new(repo, &diff.target_commit_id)?);
            target_snapshots.insert(diff.target_commit_id.clone(), target);
        }
        let target = target_snapshots
            .get(&diff.target_commit_id)
            .expect("target snapshot inserted for exact OID");
        comparisons.push(compare_rust_api(&base, target));
    }

    Ok(Some(merge_comparisons(comparisons)))
}

fn unique_exact_revision_pairs(diffs: &[Diff]) -> Vec<&Diff> {
    let mut seen = BTreeSet::new();
    diffs
        .iter()
        .filter(|diff| seen.insert((diff.base_commit_id.as_str(), diff.target_commit_id.as_str())))
        .collect()
}

fn merge_comparisons(mut comparisons: Vec<ApiDelta>) -> ApiDelta {
    debug_assert!(!comparisons.is_empty());
    if comparisons.len() == 1 {
        return comparisons.pop().expect("one comparison");
    }

    let base_revisions = comparisons
        .iter()
        .map(|delta| delta.base_revision.as_str())
        .collect::<BTreeSet<_>>();
    let target_revisions = comparisons
        .iter()
        .map(|delta| delta.target_revision.as_str())
        .collect::<BTreeSet<_>>();
    let mut merged = ApiDelta {
        analysis_source: REPO_BACKED_RUST_API_SOURCE,
        base_revision: format!(
            "multiple:[{}]",
            base_revisions.into_iter().collect::<Vec<_>>().join(",")
        ),
        target_revision: format!(
            "multiple:[{}]",
            target_revisions.into_iter().collect::<Vec<_>>().join(",")
        ),
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
        relocated: Vec::new(),
        visibility_changed: Vec::new(),
        unknown: Vec::new(),
    };
    for mut delta in comparisons {
        let comparison = format!("{} -> {}", delta.base_revision, delta.target_revision);
        for bucket in [
            &mut delta.added,
            &mut delta.removed,
            &mut delta.changed,
            &mut delta.relocated,
            &mut delta.visibility_changed,
            &mut delta.unknown,
        ] {
            for finding in bucket {
                finding
                    .evidence
                    .push(format!("revision comparison: {comparison}"));
                finding.id = format!("{}|comparison:{comparison}", finding.id);
            }
        }
        merged.added.extend(delta.added);
        merged.removed.extend(delta.removed);
        merged.changed.extend(delta.changed);
        merged.relocated.extend(delta.relocated);
        merged.visibility_changed.extend(delta.visibility_changed);
        merged.unknown.extend(delta.unknown);
    }
    normalize_delta(&mut merged);
    merged
}

pub fn breaking_changes_view(delta: &ApiDelta) -> ApiArtifactView {
    project(delta, ApiArtifactViewKind::BreakingChanges)
}

pub fn public_api_diff_view(delta: &ApiDelta) -> ApiArtifactView {
    project(delta, ApiArtifactViewKind::PublicApiDiff)
}

fn project(delta: &ApiDelta, view: ApiArtifactViewKind) -> ApiArtifactView {
    ApiArtifactView {
        view,
        analysis_source: delta.analysis_source,
        base_revision: delta.base_revision.clone(),
        target_revision: delta.target_revision.clone(),
        counts: delta.counts(),
        findings: delta.findings(),
    }
}

fn item_side(item: &RustApiItem) -> ApiFactSide {
    ApiFactSide {
        identity: identity_from_key(&item.key, &item.cfg_guard),
        contract: item.contract.clone(),
        source_path: item.source_path.clone(),
        evidence: item.evidence.clone(),
        provenance: provenance_id(&item.provenance),
        declared_public: true,
    }
}

fn declaration_side(declaration: &RustApiDeclaration) -> ApiFactSide {
    ApiFactSide {
        identity: identity_from_key(&declaration.key, &declaration.cfg_guard),
        contract: declaration.contract.clone(),
        source_path: declaration.source_path.clone(),
        evidence: declaration.evidence.clone(),
        provenance: provenance_id(&declaration.provenance),
        declared_public: declaration.declared_public,
    }
}

fn push_contract_changes(delta: &mut ApiDelta, before: &ApiFactSide, after: &ApiFactSide) {
    match (
        public_enum_contract(&before.contract),
        public_enum_contract(&after.contract),
    ) {
        (Some(before_enum), Some(after_enum)) => {
            let additive_non_exhaustive = before_enum.non_exhaustive
                && after_enum.non_exhaustive
                && before_enum.header == after_enum.header
                && after_enum.variants.len() > before_enum.variants.len()
                && before_enum
                    .variants
                    .iter()
                    .all(|(name, contract)| after_enum.variants.get(name) == Some(contract));
            if additive_non_exhaustive {
                for (name, contract) in after_enum
                    .variants
                    .iter()
                    .filter(|(name, _)| !before_enum.variants.contains_key(*name))
                {
                    let after_variant = variant_side(after, name, contract);
                    delta.added.push(known_finding(
                        ApiDeltaKind::Added,
                        after_variant.identity.clone(),
                        None,
                        Some(after_variant),
                    ));
                }
            } else {
                delta.changed.push(known_finding(
                    ApiDeltaKind::Changed,
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after.clone()),
                ));
            }
            return;
        }
        (Some(_), None) | (None, Some(_)) => {
            delta.changed.push(known_finding(
                ApiDeltaKind::Changed,
                before.identity.clone(),
                Some(before.clone()),
                Some(after.clone()),
            ));
            return;
        }
        (None, None) => {}
    }

    let Some(before_struct) = public_struct_contract(&before.contract) else {
        delta.changed.push(known_finding(
            ApiDeltaKind::Changed,
            before.identity.clone(),
            Some(before.clone()),
            Some(after.clone()),
        ));
        return;
    };
    let Some(after_struct) = public_struct_contract(&after.contract) else {
        delta.changed.push(known_finding(
            ApiDeltaKind::Changed,
            before.identity.clone(),
            Some(before.clone()),
            Some(after.clone()),
        ));
        return;
    };
    let field_added = after_struct
        .fields
        .keys()
        .any(|name| !before_struct.fields.contains_key(name));
    let exhaustive_field_added = !before_struct.non_exhaustive && field_added;
    let layout_field_added = before_struct.layout_sensitive && field_added;
    let parent_policy_changed = before_struct.non_exhaustive != after_struct.non_exhaustive;
    let mut emitted = exhaustive_field_added || parent_policy_changed || layout_field_added;
    if emitted {
        delta.changed.push(known_finding(
            ApiDeltaKind::Changed,
            before.identity.clone(),
            Some(before.clone()),
            Some(after.clone()),
        ));
    }
    for name in before_struct
        .fields
        .keys()
        .chain(after_struct.fields.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        let before_field = before_struct
            .fields
            .get(&name)
            .map(|contract| field_side(before, &name, contract));
        let after_field = after_struct
            .fields
            .get(&name)
            .map(|contract| field_side(after, &name, contract));
        let kind = match (&before_field, &after_field) {
            (Some(left), Some(right)) if left.contract == right.contract => continue,
            (Some(_), Some(_)) => ApiDeltaKind::Changed,
            (Some(_), None) => ApiDeltaKind::Removed,
            // Existing exhaustive structs are constructed and matched by
            // downstream callers. Their added field is represented by the
            // parent Changed finding above, not by an informational field add.
            (None, Some(_)) if exhaustive_field_added || layout_field_added => continue,
            (None, Some(_)) => ApiDeltaKind::Added,
            (None, None) => unreachable!(),
        };
        emitted = true;
        let identity = before_field
            .as_ref()
            .or(after_field.as_ref())
            .expect("field side exists")
            .identity
            .clone();
        push_finding(
            delta,
            kind,
            known_finding(kind, identity, before_field, after_field),
        );
    }
    if !emitted {
        delta.changed.push(known_finding(
            ApiDeltaKind::Changed,
            before.identity.clone(),
            Some(before.clone()),
            Some(after.clone()),
        ));
    }
}

struct PublicEnumContract {
    variants: BTreeMap<String, String>,
    non_exhaustive: bool,
    header: String,
}

fn public_enum_contract(contract: &str) -> Option<PublicEnumContract> {
    let syn::Item::Enum(item) = syn::parse_str::<syn::Item>(contract).ok()? else {
        return None;
    };
    let non_exhaustive = item
        .attrs
        .iter()
        .any(|attribute| attribute.path().is_ident("non_exhaustive"));
    let variants = item
        .variants
        .iter()
        .map(|variant| {
            (
                variant.ident.to_string(),
                quote::ToTokens::to_token_stream(variant).to_string(),
            )
        })
        .collect();
    let mut header = item.clone();
    header.variants.clear();
    Some(PublicEnumContract {
        variants,
        non_exhaustive,
        header: quote::ToTokens::to_token_stream(&header).to_string(),
    })
}

fn variant_side(parent: &ApiFactSide, name: &str, contract: &str) -> ApiFactSide {
    field_side(parent, name, contract)
}

struct PublicStructContract {
    fields: BTreeMap<String, String>,
    non_exhaustive: bool,
    layout_sensitive: bool,
}

fn public_struct_contract(contract: &str) -> Option<PublicStructContract> {
    let syn::Item::Struct(item) = syn::parse_str::<syn::Item>(contract).ok()? else {
        return None;
    };
    let non_exhaustive = item
        .attrs
        .iter()
        .any(|attribute| attribute.path().is_ident("non_exhaustive"));
    let layout_sensitive = item.attrs.iter().any(attr_is_layout_sensitive_repr);
    let syn::Fields::Named(fields) = item.fields else {
        return None;
    };
    if fields.named.iter().any(|field| {
        field
            .ident
            .as_ref()
            .is_some_and(|name| name.to_string().starts_with("__prview_private_field_"))
    }) {
        // A repr-sensitive private layout delta belongs to the parent type.
        // Do not leak synthetic private field identities into public artifacts.
        return None;
    }
    Some(PublicStructContract {
        fields: fields
            .named
            .into_iter()
            .filter_map(|field| {
                let name = field.ident.as_ref()?.to_string();
                if name == "__prview_has_private_fields" {
                    return None;
                }
                Some((name, quote::ToTokens::to_token_stream(&field).to_string()))
            })
            .collect(),
        non_exhaustive,
        layout_sensitive,
    })
}

fn attr_is_layout_sensitive_repr(attribute: &syn::Attribute) -> bool {
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
}

fn field_side(parent: &ApiFactSide, name: &str, contract: &str) -> ApiFactSide {
    let mut identity = parent.identity.clone();
    identity.module_path.push(parent.identity.name.clone());
    identity.name = name.to_owned();
    identity.namespace = "value".to_owned();
    ApiFactSide {
        identity,
        contract: contract.to_owned(),
        source_path: parent.source_path.clone(),
        evidence: contract.to_owned(),
        provenance: parent.provenance.clone(),
        declared_public: parent.declared_public,
    }
}

fn identity_from_key(key: &RustApiItemKey, cfg_guard: &[String]) -> ApiIdentity {
    ApiIdentity {
        crate_name: key.crate_name.clone(),
        module_path: key.module_path.clone(),
        namespace: namespace_name(key.namespace).to_owned(),
        name: key.external_name.clone(),
        cfg_region: cfg_guard.to_vec(),
    }
}

fn namespace_name(namespace: RustNamespace) -> &'static str {
    match namespace {
        RustNamespace::Type => "type",
        RustNamespace::Value => "value",
        RustNamespace::Macro => "macro",
        RustNamespace::Module => "module",
        RustNamespace::Crate => "crate",
        RustNamespace::CargoFeature => "cargo_feature",
    }
}

fn provenance_id(provenance: &RevisionProvenance) -> String {
    match provenance {
        RevisionProvenance::GitTree { commit_oid } => format!("git_tree:{commit_oid}"),
        RevisionProvenance::WorkingTreeOverlay {
            target_oid,
            dirty_digest,
        } => format!("working_tree_overlay:{target_oid}:{dirty_digest}"),
    }
}

fn key_without_cfg(identity: &ApiIdentity) -> (String, Vec<String>, String, String) {
    (
        identity.crate_name.clone(),
        identity.module_path.clone(),
        identity.namespace.clone(),
        identity.name.clone(),
    )
}

fn relocation_key(side: &ApiFactSide) -> (String, String, String, String, Vec<String>) {
    (
        side.identity.crate_name.clone(),
        side.identity.namespace.clone(),
        side.identity.name.clone(),
        side.contract.clone(),
        side.identity.cfg_region.clone(),
    )
}

fn pair_is_certain(
    before: &ApiFactSide,
    after: &ApiFactSide,
    base_unknowns: &[RustApiUnknown],
    target_unknowns: &[RustApiUnknown],
) -> bool {
    ![&before.identity, &after.identity]
        .into_iter()
        .any(|identity| {
            region_is_unknown(base_unknowns, identity)
                || region_is_unknown(target_unknowns, identity)
        })
}

fn pair_exact_identities(
    base: &[ApiFactSide],
    target: &[ApiFactSide],
    base_unknowns: &[RustApiUnknown],
    target_unknowns: &[RustApiUnknown],
    base_used: &mut [bool],
    target_used: &mut [bool],
    delta: &mut ApiDelta,
) {
    let mut base_groups: BTreeMap<&ApiIdentity, Vec<usize>> = BTreeMap::new();
    let mut target_groups: BTreeMap<&ApiIdentity, Vec<usize>> = BTreeMap::new();
    for (index, side) in base.iter().enumerate() {
        base_groups.entry(&side.identity).or_default().push(index);
    }
    for (index, side) in target.iter().enumerate() {
        target_groups.entry(&side.identity).or_default().push(index);
    }

    for identity in base_groups
        .keys()
        .filter(|candidate| target_groups.contains_key(*candidate))
        .copied()
        .collect::<Vec<_>>()
    {
        let left = &base_groups[identity];
        let right = &target_groups[identity];
        if left.len() == 1 && right.len() == 1 {
            let base_index = left[0];
            let target_index = right[0];
            base_used[base_index] = true;
            target_used[target_index] = true;
            let before = &base[base_index];
            let after = &target[target_index];
            if before.contract == after.contract {
                continue;
            }
            if !pair_is_certain(before, after, base_unknowns, target_unknowns) {
                delta.unknown.push(pairing_unknown(
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after.clone()),
                    "changed contract intersects an unknown snapshot region",
                ));
            } else {
                push_contract_changes(delta, before, after);
            }
            continue;
        }

        for (ordinal, index) in left.iter().enumerate() {
            base_used[*index] = true;
            delta.unknown.push(pairing_unknown(
                base[*index].identity.clone(),
                Some(base[*index].clone()),
                None,
                &format!(
                    "exact identity candidates are not one-to-one: base candidate {} of {}",
                    ordinal + 1,
                    left.len()
                ),
            ));
        }
        for (ordinal, index) in right.iter().enumerate() {
            target_used[*index] = true;
            delta.unknown.push(pairing_unknown(
                target[*index].identity.clone(),
                None,
                Some(target[*index].clone()),
                &format!(
                    "exact identity candidates are not one-to-one: target candidate {} of {}",
                    ordinal + 1,
                    right.len()
                ),
            ));
        }
    }
}

fn pair_cfg_changes(
    base: &[ApiFactSide],
    target: &[ApiFactSide],
    base_unknowns: &[RustApiUnknown],
    target_unknowns: &[RustApiUnknown],
    base_used: &mut [bool],
    target_used: &mut [bool],
    delta: &mut ApiDelta,
) {
    let mut base_groups: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    let mut target_groups: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    for (index, side) in base.iter().enumerate().filter(|(i, _)| !base_used[*i]) {
        base_groups
            .entry(key_without_cfg(&side.identity))
            .or_default()
            .push(index);
    }
    for (index, side) in target.iter().enumerate().filter(|(i, _)| !target_used[*i]) {
        target_groups
            .entry(key_without_cfg(&side.identity))
            .or_default()
            .push(index);
    }
    for group_key in base_groups
        .keys()
        .filter(|candidate| target_groups.contains_key(*candidate))
        .cloned()
        .collect::<Vec<_>>()
    {
        let left = &base_groups[&group_key];
        let right = &target_groups[&group_key];
        if left.len() == 1 && right.len() == 1 {
            let base_index = left[0];
            let target_index = right[0];
            let before = &base[base_index];
            let after = &target[target_index];
            if guards_proven_disjoint(&before.identity.cfg_region, &after.identity.cfg_region) {
                continue;
            }
            base_used[base_index] = true;
            target_used[target_index] = true;
            if !pair_is_certain(before, after, base_unknowns, target_unknowns) {
                delta.unknown.push(pairing_unknown(
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after.clone()),
                    "cfg-region pairing intersects an unknown snapshot region",
                ));
            } else {
                delta.changed.push(known_finding(
                    ApiDeltaKind::Changed,
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after.clone()),
                ));
            }
        } else {
            for (ordinal, index) in left.iter().enumerate() {
                base_used[*index] = true;
                delta.unknown.push(pairing_unknown(
                    base[*index].identity.clone(),
                    Some(base[*index].clone()),
                    None,
                    &format!(
                        "identity/cfg candidates are not one-to-one: base candidate {} of {}",
                        ordinal + 1,
                        left.len()
                    ),
                ));
            }
            for (ordinal, index) in right.iter().enumerate() {
                target_used[*index] = true;
                delta.unknown.push(pairing_unknown(
                    target[*index].identity.clone(),
                    None,
                    Some(target[*index].clone()),
                    &format!(
                        "identity/cfg candidates are not one-to-one: target candidate {} of {}",
                        ordinal + 1,
                        right.len()
                    ),
                ));
            }
        }
    }
}

fn pair_visibility_changes(
    base: &[ApiFactSide],
    target: &[ApiFactSide],
    evidence: &SnapshotEvidence<'_>,
    base_used: &mut [bool],
    target_used: &mut [bool],
    delta: &mut ApiDelta,
) {
    for (index, before) in base.iter().enumerate() {
        if base_used[index] {
            continue;
        }
        let matches: Vec<_> = evidence
            .target_declarations
            .iter()
            .filter(|declaration| {
                !declaration.declared_public
                    && declaration.parent_externally_reachable
                    && declaration.certainty == RustSourceCertainty::Confirmed
                    && identity_from_key(&declaration.key, &declaration.cfg_guard)
                        == before.identity
            })
            .collect();
        if matches.len() == 1 {
            base_used[index] = true;
            let after = declaration_side(matches[0]);
            if !pair_is_certain(
                before,
                &after,
                evidence.base_unknowns,
                evidence.target_unknowns,
            ) {
                delta.unknown.push(pairing_unknown(
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after),
                    "visibility transition intersects an unknown snapshot region",
                ));
            } else {
                delta.visibility_changed.push(known_finding(
                    ApiDeltaKind::VisibilityChanged,
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after),
                ));
            }
        } else if matches.len() > 1 {
            base_used[index] = true;
            delta.unknown.push(pairing_unknown(
                before.identity.clone(),
                Some(before.clone()),
                None,
                "visibility counterparts are not one-to-one",
            ));
        }
    }
    // The reverse transition is normally already visible as an addition. Prove
    // it only when the base snapshot retained one exact private declaration.
    for (index, after) in target.iter().enumerate() {
        if target_used[index] {
            continue;
        }
        let matches: Vec<_> = evidence
            .base_declarations
            .iter()
            .filter(|declaration| {
                !declaration.declared_public
                    && declaration.parent_externally_reachable
                    && declaration.certainty == RustSourceCertainty::Confirmed
                    && identity_from_key(&declaration.key, &declaration.cfg_guard) == after.identity
            })
            .collect();
        if matches.len() == 1 {
            target_used[index] = true;
            let before = declaration_side(matches[0]);
            if !pair_is_certain(
                &before,
                after,
                evidence.base_unknowns,
                evidence.target_unknowns,
            ) {
                delta.unknown.push(pairing_unknown(
                    after.identity.clone(),
                    Some(before),
                    Some(after.clone()),
                    "visibility transition intersects an unknown snapshot region",
                ));
            } else {
                delta.visibility_changed.push(known_finding(
                    ApiDeltaKind::VisibilityChanged,
                    after.identity.clone(),
                    Some(before),
                    Some(after.clone()),
                ));
            }
        } else if matches.len() > 1 {
            target_used[index] = true;
            delta.unknown.push(pairing_unknown(
                after.identity.clone(),
                None,
                Some(after.clone()),
                "visibility counterparts are not one-to-one",
            ));
        }
    }
}

fn pair_relocations(
    base: &[ApiFactSide],
    target: &[ApiFactSide],
    base_unknowns: &[RustApiUnknown],
    target_unknowns: &[RustApiUnknown],
    base_used: &mut [bool],
    target_used: &mut [bool],
    delta: &mut ApiDelta,
) {
    let mut base_groups: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    let mut target_groups: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    for (index, side) in base.iter().enumerate().filter(|(i, _)| !base_used[*i]) {
        base_groups
            .entry(relocation_key(side))
            .or_default()
            .push(index);
    }
    for (index, side) in target.iter().enumerate().filter(|(i, _)| !target_used[*i]) {
        target_groups
            .entry(relocation_key(side))
            .or_default()
            .push(index);
    }
    for key in base_groups
        .keys()
        .filter(|candidate| target_groups.contains_key(*candidate))
        .cloned()
        .collect::<Vec<_>>()
    {
        let left = &base_groups[&key];
        let right = &target_groups[&key];
        if left.len() == 1 && right.len() == 1 {
            let base_index = left[0];
            let target_index = right[0];
            if base[base_index].identity.module_path == target[target_index].identity.module_path {
                continue;
            }
            base_used[base_index] = true;
            target_used[target_index] = true;
            let before = &base[base_index];
            let after = &target[target_index];
            if !pair_is_certain(before, after, base_unknowns, target_unknowns) {
                delta.unknown.push(pairing_unknown(
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after.clone()),
                    "relocation intersects an unknown snapshot region",
                ));
            } else {
                delta.relocated.push(known_finding(
                    ApiDeltaKind::Relocated,
                    before.identity.clone(),
                    Some(before.clone()),
                    Some(after.clone()),
                ));
            }
        } else {
            for (ordinal, index) in left.iter().enumerate() {
                base_used[*index] = true;
                delta.unknown.push(pairing_unknown(
                    base[*index].identity.clone(),
                    Some(base[*index].clone()),
                    None,
                    &format!(
                        "relocation candidates are not one-to-one: base candidate {} of {}",
                        ordinal + 1,
                        left.len()
                    ),
                ));
            }
            for (ordinal, index) in right.iter().enumerate() {
                target_used[*index] = true;
                delta.unknown.push(pairing_unknown(
                    target[*index].identity.clone(),
                    None,
                    Some(target[*index].clone()),
                    &format!(
                        "relocation candidates are not one-to-one: target candidate {} of {}",
                        ordinal + 1,
                        right.len()
                    ),
                ));
            }
        }
    }
}

fn consume_one_sided_ambiguities(
    base: &[ApiFactSide],
    target: &[ApiFactSide],
    base_used: &mut [bool],
    target_used: &mut [bool],
    delta: &mut ApiDelta,
) {
    let mut base_groups: BTreeMap<&ApiIdentity, Vec<usize>> = BTreeMap::new();
    let mut target_groups: BTreeMap<&ApiIdentity, Vec<usize>> = BTreeMap::new();
    for (index, side) in base.iter().enumerate().filter(|(i, _)| !base_used[*i]) {
        base_groups.entry(&side.identity).or_default().push(index);
    }
    for (index, side) in target.iter().enumerate().filter(|(i, _)| !target_used[*i]) {
        target_groups.entry(&side.identity).or_default().push(index);
    }
    for indices in base_groups.values().filter(|indices| indices.len() > 1) {
        for (ordinal, index) in indices.iter().enumerate() {
            base_used[*index] = true;
            delta.unknown.push(pairing_unknown(
                base[*index].identity.clone(),
                Some(base[*index].clone()),
                None,
                &format!(
                    "unmatched base identity is not unique: candidate {} of {}",
                    ordinal + 1,
                    indices.len()
                ),
            ));
        }
    }
    for indices in target_groups.values().filter(|indices| indices.len() > 1) {
        for (ordinal, index) in indices.iter().enumerate() {
            target_used[*index] = true;
            delta.unknown.push(pairing_unknown(
                target[*index].identity.clone(),
                None,
                Some(target[*index].clone()),
                &format!(
                    "unmatched target identity is not unique: candidate {} of {}",
                    ordinal + 1,
                    indices.len()
                ),
            ));
        }
    }
}

fn region_is_unknown(unknowns: &[RustApiUnknown], identity: &ApiIdentity) -> bool {
    unknowns.iter().any(|unknown| {
        !matches!(
            unknown.kind,
            RustApiUnknownKind::PathNonUtf8 | RustApiUnknownKind::TraitImplResolution
        ) && unknown
            .crate_name
            .as_ref()
            .is_none_or(|crate_name| crate_name == &identity.crate_name)
            && (unknown.module_path.is_empty()
                || identity.module_path.starts_with(&unknown.module_path)
                || unknown.module_path.starts_with(&identity.module_path))
            && guards_may_overlap(&unknown.cfg_guard, &identity.cfg_region)
    })
}

fn guards_may_overlap(left: &[String], right: &[String]) -> bool {
    !guards_proven_disjoint(left, right)
}

fn snapshot_unknown_findings(
    base: &RustApiSnapshot,
    target: &RustApiSnapshot,
) -> Vec<ApiDeltaFinding> {
    let mut target_used = vec![false; target.unknowns.len()];
    let mut findings = Vec::new();

    for unknown in &base.unknowns {
        let counterpart = target
            .unknowns
            .iter()
            .enumerate()
            .find(|(index, candidate)| {
                !target_used[*index] && unknown_proofs_match(base, unknown, target, candidate)
            })
            .map(|(index, _)| index);
        if let Some(index) = counterpart {
            target_used[index] = true;
        } else {
            findings.push(snapshot_unknown_finding(unknown, ApiSnapshotSide::Base));
        }
    }

    findings.extend(
        target
            .unknowns
            .iter()
            .enumerate()
            .filter(|(index, _)| !target_used[*index])
            .map(|(_, unknown)| snapshot_unknown_finding(unknown, ApiSnapshotSide::Target)),
    );
    findings
}

fn unknown_proofs_match(
    base: &RustApiSnapshot,
    left: &RustApiUnknown,
    target: &RustApiSnapshot,
    right: &RustApiUnknown,
) -> bool {
    left.kind != RustApiUnknownKind::PathNonUtf8
        && left.kind == right.kind
        && left.crate_name == right.crate_name
        && left.module_path == right.module_path
        && left.source_path == right.source_path
        && left.cfg_guard == right.cfg_guard
        && left.evidence == right.evidence
        // Revision ids necessarily differ across the comparison. What must not
        // differ is the provenance class, and each proof must still belong to
        // the snapshot that supplied it; an overlay is not silently equated to
        // a Git tree and a detached proof is never neutralized.
        && left.provenance == base.provenance
        && right.provenance == target.provenance
        && same_provenance_class(&left.provenance, &right.provenance)
        && include_dependent_source_is_proven(left)
}

fn include_dependent_source_is_proven(unknown: &RustApiUnknown) -> bool {
    unknown.kind != RustApiUnknownKind::IncludeMacro
        || !unknown.evidence.contains("included-digest:unresolved")
}

fn same_provenance_class(left: &RevisionProvenance, right: &RevisionProvenance) -> bool {
    matches!(
        (left, right),
        (
            RevisionProvenance::GitTree { .. },
            RevisionProvenance::GitTree { .. }
        ) | (
            RevisionProvenance::WorkingTreeOverlay { .. },
            RevisionProvenance::WorkingTreeOverlay { .. }
        )
    )
}

fn snapshot_unknown_finding(unknown: &RustApiUnknown, side: ApiSnapshotSide) -> ApiDeltaFinding {
    let identity = ApiIdentity {
        crate_name: unknown
            .crate_name
            .clone()
            .unwrap_or_else(|| "<unknown-crate>".to_owned()),
        module_path: unknown.module_path.clone(),
        namespace: "unknown".to_owned(),
        name: format!("{:?}", unknown.kind),
        cfg_region: unknown.cfg_guard.clone(),
    };
    let side_name = match side {
        ApiSnapshotSide::Base => "base",
        ApiSnapshotSide::Target => "target",
    };
    let reason = format!(
        "{side_name} snapshot {:?}: {}",
        unknown.kind, unknown.evidence
    );
    let mut finding = ApiDeltaFinding {
        id: String::new(),
        kind: ApiDeltaKind::Unknown,
        identity,
        before: None,
        after: None,
        analysis_source: REPO_BACKED_RUST_API_SOURCE,
        confidence: ApiDeltaConfidence::Unknown,
        evidence: vec![unknown.evidence.clone()],
        unknown_reason: Some(reason),
        unknown_source: Some(ApiUnknownSource {
            side,
            source_path: unknown.source_path.clone(),
            provenance: provenance_id(&unknown.provenance),
        }),
    };
    finding.id = stable_finding_id(&finding);
    finding
}

fn known_finding(
    kind: ApiDeltaKind,
    identity: ApiIdentity,
    before: Option<ApiFactSide>,
    after: Option<ApiFactSide>,
) -> ApiDeltaFinding {
    let evidence = before
        .iter()
        .chain(after.iter())
        .map(|side| side.evidence.clone())
        .collect();
    let mut finding = ApiDeltaFinding {
        id: String::new(),
        kind,
        identity,
        before,
        after,
        analysis_source: REPO_BACKED_RUST_API_SOURCE,
        confidence: ApiDeltaConfidence::Confirmed,
        evidence,
        unknown_reason: None,
        unknown_source: None,
    };
    finding.id = stable_finding_id(&finding);
    finding
}

fn pairing_unknown(
    identity: ApiIdentity,
    before: Option<ApiFactSide>,
    after: Option<ApiFactSide>,
    reason: &str,
) -> ApiDeltaFinding {
    let kind = ApiDeltaKind::Unknown;
    let mut finding = ApiDeltaFinding {
        id: String::new(),
        kind,
        identity,
        before,
        after,
        analysis_source: REPO_BACKED_RUST_API_SOURCE,
        confidence: ApiDeltaConfidence::Unknown,
        evidence: vec![reason.to_owned()],
        unknown_reason: Some(reason.to_owned()),
        unknown_source: None,
    };
    finding.id = stable_finding_id(&finding);
    finding
}

#[derive(serde::Serialize)]
struct FindingIdMaterial<'a> {
    kind: ApiDeltaKind,
    identity: &'a ApiIdentity,
    before_identity: Option<&'a ApiIdentity>,
    before_contract: Option<&'a str>,
    before_declared_public: Option<bool>,
    after_identity: Option<&'a ApiIdentity>,
    after_contract: Option<&'a str>,
    after_declared_public: Option<bool>,
    unknown_reason: Option<&'a str>,
    unknown_source: Option<&'a ApiUnknownSource>,
}

fn stable_finding_id(finding: &ApiDeltaFinding) -> String {
    let material = FindingIdMaterial {
        kind: finding.kind,
        identity: &finding.identity,
        before_identity: finding.before.as_ref().map(|side| &side.identity),
        before_contract: finding.before.as_ref().map(|side| side.contract.as_str()),
        before_declared_public: finding.before.as_ref().map(|side| side.declared_public),
        after_identity: finding.after.as_ref().map(|side| &side.identity),
        after_contract: finding.after.as_ref().map(|side| side.contract.as_str()),
        after_declared_public: finding.after.as_ref().map(|side| side.declared_public),
        unknown_reason: finding.unknown_reason.as_deref(),
        unknown_source: finding.unknown_source.as_ref(),
    };
    format!(
        "api-delta:{}",
        serde_json::to_string(&material).expect("finding ID material is serializable")
    )
}

fn push_finding(delta: &mut ApiDelta, bucket: ApiDeltaKind, finding: ApiDeltaFinding) {
    if finding.kind == ApiDeltaKind::Unknown {
        delta.unknown.push(finding);
        return;
    }
    match bucket {
        ApiDeltaKind::Added => delta.added.push(finding),
        ApiDeltaKind::Removed => delta.removed.push(finding),
        ApiDeltaKind::Changed => delta.changed.push(finding),
        ApiDeltaKind::Relocated => delta.relocated.push(finding),
        ApiDeltaKind::VisibilityChanged => delta.visibility_changed.push(finding),
        ApiDeltaKind::Unknown => delta.unknown.push(finding),
    }
}

fn normalize_delta(delta: &mut ApiDelta) {
    for bucket in [
        &mut delta.added,
        &mut delta.removed,
        &mut delta.changed,
        &mut delta.relocated,
        &mut delta.visibility_changed,
        &mut delta.unknown,
    ] {
        bucket.sort();
        bucket.dedup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::signal::api_surface::snapshot_rust_api;
    use crate::artifacts::signal::breaking::{
        BreakingKind, analyze_all_breaking_changes, historical_scenarios,
        write_breaking_changes_with_api,
    };
    use crate::artifacts::signal::public_api::api_surface_corpus_contract::{
        ApiDeltaKind as ExpectedKind, CorpusExpectation, CorpusExpected, CorpusManifest,
    };
    use crate::artifacts::signal::public_api::{
        analyze_js_ts_public_api_diff, generate_public_api_diff, write_public_api_diff,
    };
    use crate::artifacts::signal::revision_source::{
        RevisionBytes, RevisionContentKind, RevisionEntry, RevisionEntryKind, RevisionEntryState,
        RevisionFileSource, RevisionRead, RevisionSourceError,
    };
    use crate::artifacts::signal::test_helpers::{make_diff_with_ids, make_test_repo};
    use crate::git::git_cmd;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    #[derive(Clone)]
    struct MemorySource {
        provenance: RevisionProvenance,
        files: BTreeMap<String, Vec<u8>>,
    }

    impl MemorySource {
        fn from_root(root: &Path, revision: &str) -> Self {
            let mut files = BTreeMap::new();
            for relative in ["Cargo.toml", "src/lib.rs"] {
                files.insert(relative.to_owned(), fs::read(root.join(relative)).unwrap());
            }
            Self {
                provenance: RevisionProvenance::GitTree {
                    commit_oid: revision.to_owned(),
                },
                files,
            }
        }

        fn source(text: &str, revision: &str) -> Self {
            Self {
                provenance: RevisionProvenance::GitTree {
                    commit_oid: revision.to_owned(),
                },
                files: BTreeMap::from([
                    (
                        "Cargo.toml".to_owned(),
                        b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n"
                            .to_vec(),
                    ),
                    ("src/lib.rs".to_owned(), text.as_bytes().to_vec()),
                ]),
            }
        }
    }

    impl RevisionFileSource for MemorySource {
        fn provenance(&self) -> &RevisionProvenance {
            &self.provenance
        }

        fn entries(&self) -> Vec<RevisionEntry> {
            self.files
                .keys()
                .map(|path| RevisionEntry {
                    path: path.clone(),
                    baseline_object_id: Some(format!("fixture:{path}")),
                    mode: 0o100644,
                    kind: RevisionEntryKind::RegularFile,
                    state: RevisionEntryState::Present,
                    provenance: self.provenance.clone(),
                })
                .collect()
        }

        fn read(&self, path: &str) -> Result<RevisionRead, RevisionSourceError> {
            Ok(match self.files.get(path) {
                Some(bytes) => RevisionRead::Bytes(RevisionBytes {
                    bytes: bytes.clone(),
                    content_kind: RevisionContentKind::Utf8Text,
                    provenance: self.provenance.clone(),
                }),
                None => RevisionRead::Missing {
                    provenance: self.provenance.clone(),
                },
            })
        }
    }

    fn corpus_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api_surface")
    }

    fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn fixture_delta(cell: &str) -> ApiDelta {
        let root = corpus_root().join(cell);
        let base = snapshot_rust_api(&MemorySource::from_root(
            &root.join("base"),
            &format!("fixture://{cell}/base"),
        ));
        let target = snapshot_rust_api(&MemorySource::from_root(
            &root.join("head"),
            &format!("fixture://{cell}/head"),
        ));
        compare_rust_api(&base, &target)
    }

    fn repository_delta(files: &[(&str, &str, &str)]) -> ApiDelta {
        let (_tmp, repo, base, target) = make_test_repo(files);
        compare_rust_api_revisions(&repo, &[make_diff_with_ids(base, target, Vec::new())])
            .expect("repository-backed comparison")
            .expect("Rust revisions")
    }

    fn git_with_input(repo: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
        let mut child = git_cmd()
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "prview test")
            .env("GIT_AUTHOR_EMAIL", "prview@example.test")
            .env("GIT_COMMITTER_NAME", "prview test")
            .env("GIT_COMMITTER_EMAIL", "prview@example.test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git object command");
        child
            .stdin
            .take()
            .expect("git stdin")
            .write_all(input)
            .expect("write git object input");
        let output = child
            .wait_with_output()
            .expect("wait for git object command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn repository_with_unchanged_non_utf8_entry()
    -> (tempfile::TempDir, crate::git::Repository, String, String) {
        let (tmp, _repo, _initial, seed) = make_test_repo(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", "pub fn stable() {}\n", "pub fn stable() {}\n"),
        ]);

        let mut tree_records = git_with_input(
            tmp.path(),
            &["ls-tree", "-z", &format!("{seed}^{{tree}}")],
            &[],
        );
        let blob = String::from_utf8(git_with_input(
            tmp.path(),
            &["hash-object", "-w", "--stdin"],
            b"pub fn hidden_by_path_encoding() {}\n",
        ))
        .expect("blob oid")
        .trim()
        .to_owned();
        tree_records.extend_from_slice(format!("100644 blob {blob}\t").as_bytes());
        tree_records.extend_from_slice(b"non_utf8_\xff.rs\0");
        let tree = String::from_utf8(git_with_input(tmp.path(), &["mktree", "-z"], &tree_records))
            .expect("tree oid")
            .trim()
            .to_owned();
        let base = String::from_utf8(git_with_input(
            tmp.path(),
            &["commit-tree", &tree, "-p", &seed],
            b"base with non-UTF-8 path\n",
        ))
        .expect("commit oid")
        .trim()
        .to_owned();
        let target_lib_blob = String::from_utf8(git_with_input(
            tmp.path(),
            &["hash-object", "-w", "--stdin"],
            b"pub fn stable() {}\npub fn valid_sibling() {}\n",
        ))
        .expect("target lib blob oid")
        .trim()
        .to_owned();
        let src_tree_record = format!("100644 blob {target_lib_blob}\tlib.rs\0");
        let src_tree = String::from_utf8(git_with_input(
            tmp.path(),
            &["mktree", "-z"],
            src_tree_record.as_bytes(),
        ))
        .expect("target src tree oid")
        .trim()
        .to_owned();
        let base_root_records = git_with_input(
            tmp.path(),
            &["ls-tree", "-z", &format!("{base}^{{tree}}")],
            &[],
        );
        let mut replaced_src = false;
        let mut target_root_records = Vec::new();
        for record in base_root_records.split_inclusive(|byte| *byte == 0) {
            if record.ends_with(b"\tsrc\0") {
                target_root_records
                    .extend_from_slice(format!("040000 tree {src_tree}\tsrc\0").as_bytes());
                replaced_src = true;
            } else {
                target_root_records.extend_from_slice(record);
            }
        }
        assert!(replaced_src, "seed tree carries src/");
        let target_tree = String::from_utf8(git_with_input(
            tmp.path(),
            &["mktree", "-z"],
            &target_root_records,
        ))
        .expect("target root tree oid")
        .trim()
        .to_owned();
        let target = String::from_utf8(git_with_input(
            tmp.path(),
            &["commit-tree", &target_tree, "-p", &base],
            b"target with preserved non-UTF-8 path\n",
        ))
        .expect("target commit oid")
        .trim()
        .to_owned();
        let repo = crate::git::Repository::open(tmp.path()).unwrap();
        (tmp, repo, base, target)
    }

    fn expected_projection(expected: &CorpusExpected) -> BTreeSet<(ExpectedKind, String, String)> {
        expected
            .repo_backed_records
            .iter()
            .map(|record| {
                (
                    record.kind.clone(),
                    record.symbol.clone(),
                    record.namespace.clone(),
                )
            })
            .collect()
    }

    fn actual_projection(delta: &ApiDelta) -> BTreeSet<(ExpectedKind, String, String)> {
        delta
            .findings()
            .into_iter()
            .filter(|finding| finding.kind != ApiDeltaKind::Unknown)
            .map(|finding| {
                let external_path = match (&finding.before, &finding.after) {
                    (Some(before), Some(after)) if finding.kind == ApiDeltaKind::Relocated => {
                        let common_len = before
                            .identity
                            .module_path
                            .iter()
                            .zip(&after.identity.module_path)
                            .take_while(|(left, right)| left == right)
                            .count();
                        ApiIdentity {
                            module_path: before.identity.module_path[..common_len].to_vec(),
                            ..finding.identity.clone()
                        }
                        .external_path()
                    }
                    _ => finding.identity.external_path(),
                };
                let kind = match finding.kind {
                    ApiDeltaKind::Added => ExpectedKind::Added,
                    ApiDeltaKind::Removed => ExpectedKind::Removed,
                    ApiDeltaKind::Changed => ExpectedKind::Changed,
                    ApiDeltaKind::Relocated => ExpectedKind::Relocated,
                    ApiDeltaKind::VisibilityChanged => ExpectedKind::VisibilityChanged,
                    ApiDeltaKind::Unknown => ExpectedKind::Unknown,
                };
                (kind, finding.identity.name, external_path)
            })
            .collect()
    }

    #[test]
    fn api_delta_matches_every_frozen_w0_cell() {
        let root = corpus_root();
        let manifest: CorpusManifest = read_json(&root.join("manifest.json"));
        for cell in manifest.cells {
            let expected: CorpusExpected = read_json(&root.join(&cell.id).join("expected.json"));
            let delta = fixture_delta(&cell.id);
            assert_eq!(
                actual_projection(&delta),
                expected_projection(&expected),
                "{}",
                cell.id
            );
        }
    }

    #[test]
    fn api_delta_pairing_is_one_to_one_and_relocation_is_exclusive() {
        let delta = fixture_delta("move_relocated");
        assert_eq!(delta.relocated.len(), 1);
        assert_eq!(
            delta
                .added
                .iter()
                .map(|finding| finding.identity.name.as_str())
                .collect::<Vec<_>>(),
            ["new"]
        );
        assert_eq!(
            delta
                .removed
                .iter()
                .map(|finding| finding.identity.name.as_str())
                .collect::<Vec<_>>(),
            ["old"]
        );

        let base = snapshot_rust_api(&MemorySource::source(
            "pub mod a { pub fn item() {} }\npub mod b { pub fn item() {} }",
            "base",
        ));
        let target = snapshot_rust_api(&MemorySource::source(
            "pub mod c { pub fn item() {} }\npub mod d { pub fn item() {} }",
            "target",
        ));
        let ambiguous = compare_rust_api(&base, &target);
        assert_eq!(ambiguous.added.len(), 2);
        assert_eq!(ambiguous.removed.len(), 2);
        assert!(ambiguous.added.iter().all(|finding| {
            finding.identity.namespace == "module"
                && matches!(finding.identity.name.as_str(), "c" | "d")
        }));
        assert!(ambiguous.removed.iter().all(|finding| {
            finding.identity.namespace == "module"
                && matches!(finding.identity.name.as_str(), "a" | "b")
        }));
        assert!(ambiguous.relocated.is_empty());
        assert!(ambiguous.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.starts_with("relocation candidates are not one-to-one")
            })
        }));
    }

    #[test]
    fn api_delta_pairing_unknown_suppresses_confirmed_removal() {
        let base = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base"));
        let target = snapshot_rust_api(&MemorySource::source(
            "mod donor { pub fn item() {} }\npub use donor::*;",
            "target",
        ));
        let delta = compare_rust_api(&base, &target);
        assert!(delta.removed.is_empty());
        assert!(delta.unknown.iter().any(|finding| {
            finding.identity.name == "item"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("target counterpart"))
        }));
    }

    #[test]
    fn unchanged_unknown_proof_is_neutral_but_non_equivalent_proof_is_not() {
        let unchanged = "mod donor { pub fn item() {} } pub use donor::*;";
        let base = snapshot_rust_api(&MemorySource::source(unchanged, "base"));
        let target = snapshot_rust_api(&MemorySource::source(
            "mod donor { pub fn item() {} } pub use donor::*; fn unrelated_private_change() {}",
            "target",
        ));
        let neutral = compare_rust_api(&base, &target);
        assert!(
            neutral.unknown.is_empty(),
            "the same unsupported proof on both revisions is not a delta: {:?}",
            neutral.unknown
        );

        let changed = compare_rust_api(
            &base,
            &snapshot_rust_api(&MemorySource::source(
                "mod donor { pub fn item() {} } pub use donor::*; include!(\"extra.rs\");",
                "target",
            )),
        );
        assert!(
            !changed.unknown.is_empty(),
            "changed unknown evidence remains review-required"
        );

        let unilateral = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source("fn private() {}", "base")),
            &snapshot_rust_api(&MemorySource::source(
                "fn private() {} include!(\"generated.rs\");",
                "target",
            )),
        );
        assert_eq!(
            unilateral.unknown.len(),
            1,
            "one-sided unknown proof must remain visible"
        );

        let mut overlay = snapshot_rust_api(&MemorySource::source(unchanged, "target"));
        let overlay_provenance = RevisionProvenance::WorkingTreeOverlay {
            target_oid: "target".to_owned(),
            dirty_digest: "dirty".to_owned(),
        };
        overlay.provenance = overlay_provenance.clone();
        for unknown in &mut overlay.unknowns {
            unknown.provenance = overlay_provenance.clone();
        }
        let provenance_changed = compare_rust_api(&base, &overlay);
        assert!(
            !provenance_changed.unknown.is_empty(),
            "a provenance-class change is evidence, not a neutral pair"
        );
    }

    #[test]
    fn added_field_policy_distinguishes_exhaustive_non_exhaustive_and_new_structs() {
        let exhaustive = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "pub struct Options { pub a: u8 }",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "pub struct Options { pub a: u8, pub b: u8 }",
                "target",
            )),
        );
        assert!(
            exhaustive.changed.iter().any(|finding| {
                finding.identity.name == "Options" && finding.identity.module_path.is_empty()
            }),
            "adding a field to an exhaustive struct must change the parent contract"
        );
        assert!(
            !exhaustive
                .added
                .iter()
                .any(|finding| finding.identity.name == "b"),
            "the breaking addition must not survive only in the informational bucket"
        );

        let non_exhaustive = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[non_exhaustive] pub struct Options { pub a: u8 }",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[non_exhaustive] pub struct Options { pub a: u8, pub b: u8 }",
                "target",
            )),
        );
        assert!(non_exhaustive.changed.is_empty());
        assert!(
            non_exhaustive
                .added
                .iter()
                .any(|finding| finding.identity.name == "b"),
            "non_exhaustive opts downstream callers out of exhaustive construction"
        );

        let new_item = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source("fn private() {}", "base")),
            &snapshot_rust_api(&MemorySource::source(
                "fn private() {} pub struct Options { pub a: u8, pub b: u8 }",
                "target",
            )),
        );
        assert!(new_item.changed.is_empty());
        assert!(new_item.added.iter().any(|finding| {
            finding.identity.name == "Options" && finding.identity.module_path.is_empty()
        }));

        let repr_non_exhaustive = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[repr(C)] #[non_exhaustive] pub struct Options { pub a: u8 }",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[repr(C)] #[non_exhaustive] pub struct Options { pub a: u8, pub b: u8 }",
                "target",
            )),
        );
        assert!(
            repr_non_exhaustive.changed.iter().any(|finding| {
                finding.identity.name == "Options" && finding.identity.module_path.is_empty()
            }),
            "repr(C) field addition is an ABI break even when the struct is non_exhaustive"
        );
        assert!(
            !repr_non_exhaustive
                .added
                .iter()
                .any(|finding| finding.identity.name == "b"),
            "the ABI break must not survive only as an informational field add"
        );
    }

    #[test]
    fn repository_backed_t1_t3_contracts_are_non_vacuous() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Options { pub a: u8 }\n#[non_exhaustive] pub struct Flexible { pub a: u8 }\npub fn parse<'a, T>(value: &'a T) -> &'a T { value }\n",
                "pub struct Options { pub a: u8, pub b: u8 }\n#[non_exhaustive] pub struct Flexible { pub a: u8, pub b: u8 }\npub struct New { pub value: u8 }\npub fn parse<'value, U>(input: &'value U) -> &'value U { input }\n",
            ),
        ]);
        assert!(delta.changed.iter().any(|finding| {
            finding.identity.name == "Options" && finding.identity.namespace == "type"
        }));
        assert!(delta.added.iter().any(|finding| {
            finding.identity.name == "b" && finding.identity.module_path == ["Flexible".to_owned()]
        }));
        assert!(
            delta
                .added
                .iter()
                .any(|finding| finding.identity.name == "New")
        );
        assert!(
            delta
                .findings()
                .iter()
                .all(|finding| finding.identity.name != "parse"),
            "parameter, generic, and lifetime alpha-renames stay neutral through exact Git trees"
        );

        for (base, target) in [
            (
                "pub fn changed(value: u8) {}",
                "pub fn changed(value: u16) {}",
            ),
            (
                "pub extern \"C\" fn changed(value: u8) {}",
                "pub extern \"system\" fn changed(value: u8) {}",
            ),
            (
                "pub fn changed<'a, 'b>(left: &'a str, right: &'b str) -> &'a str { left }",
                "pub fn changed<'x, 'y>(left: &'x str, right: &'y str) -> &'y str { right }",
            ),
        ] {
            let mutation = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", base, target),
            ]);
            assert!(
                mutation
                    .changed
                    .iter()
                    .any(|finding| finding.identity.name == "changed"),
                "type, ABI, and lifetime-relation controls must remain observable: {base} -> {target}"
            );
        }
    }

    #[test]
    fn repository_backed_t2_tuple_private_tail_changes_parent_contract() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Tuple(pub u8);\npub struct Stable(pub u8, pub u16);\n",
                "pub struct Tuple(pub u8, u16);\npub struct Stable(pub u8, pub u16);\n",
            ),
        ]);
        assert!(delta.changed.iter().any(|finding| {
            finding.identity.name == "Tuple" && finding.identity.namespace == "type"
        }));
        assert!(
            delta
                .findings()
                .iter()
                .all(|finding| finding.identity.name != "Stable"),
            "an unchanged public-only tuple is a stable control"
        );
    }

    #[test]
    fn repository_backed_t4_non_exhaustive_enum_addition_is_informational() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[non_exhaustive] pub enum Flexible { A }\npub enum Strict { A }\n",
                "#[non_exhaustive] pub enum Flexible { A, B }\npub enum Strict { A, B }\n",
            ),
        ]);
        assert!(delta.added.iter().any(|finding| {
            finding.identity.name == "B" && finding.identity.module_path == ["Flexible".to_owned()]
        }));
        assert!(!delta.changed.iter().any(|finding| {
            finding.identity.name == "Flexible" && finding.identity.namespace == "type"
        }));
        assert!(delta.changed.iter().any(|finding| {
            finding.identity.name == "Strict" && finding.identity.namespace == "type"
        }));
    }

    #[test]
    fn git_object_t5_non_utf8_entry_preserves_valid_api_siblings() {
        let (_tmp, repo, base, target) = repository_with_unchanged_non_utf8_entry();
        let delta =
            compare_rust_api_revisions(&repo, &[make_diff_with_ids(base, target, Vec::new())])
                .expect("a legal non-UTF-8 Git path must not abort API analysis")
                .expect("Rust revisions");
        assert!(
            delta
                .added
                .iter()
                .any(|finding| finding.identity.name == "valid_sibling"),
            "valid siblings remain analyzable"
        );
        let path_unknowns: Vec<_> = delta
            .unknown
            .iter()
            .filter(|finding| finding.identity.name == "PathNonUtf8")
            .collect();
        assert_eq!(
            path_unknowns.len(),
            2,
            "both exact revisions retain typed path uncertainty instead of becoming clean"
        );
        assert!(path_unknowns.iter().all(|finding| {
            finding
                .unknown_source
                .as_ref()
                .is_some_and(|source| source.source_path.contains("git-path-bytes"))
        }));
    }

    #[test]
    fn repository_backed_module_feature_and_crate_contracts_are_observable() {
        let module_and_feature = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[features]\ndefault=[]\nlegacy=[]\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[features]\ndefault=[]\n",
            ),
            ("src/lib.rs", "pub mod empty {}\n", "fn private() {}\n"),
        ]);
        assert!(module_and_feature.removed.iter().any(|finding| {
            finding.identity.name == "empty" && finding.identity.namespace == "module"
        }));
        assert!(module_and_feature.removed.iter().any(|finding| {
            finding.identity.name == "legacy" && finding.identity.namespace == "cargo_feature"
        }));

        let crate_removed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nautolib=false\n",
            ),
            ("src/lib.rs", "", ""),
        ]);
        assert!(crate_removed.removed.iter().any(|finding| {
            finding.identity.name == "fixture" && finding.identity.namespace == "crate"
        }));
    }

    #[test]
    fn repository_backed_repr_layout_and_data_binders_are_semantic() {
        let binder_rename = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Wrapper<'a, T, const N: usize> { pub value: &'a [T; N] }\n",
                "pub struct Wrapper<'value, U, const M: usize> { pub value: &'value [U; M] }\n",
            ),
        ]);
        assert!(
            binder_rename.findings().is_empty(),
            "public data-type binder spelling is not caller-observable"
        );

        let repr_layout = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(C)] pub struct Layout { pub tag: u8, hidden: u8 }\n",
                "#[repr(C)] pub struct Layout { pub tag: u8, hidden: u16 }\n",
            ),
        ]);
        assert!(
            repr_layout
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Layout"),
            "repr(C) makes private field layout part of the observable contract"
        );
    }

    #[test]
    fn repository_backed_implicit_optional_features_and_crate_types_are_contracts() {
        let implicit_feature = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde = { version = '1', optional = true }\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", "pub fn stable() {}\n", "pub fn stable() {}\n"),
        ]);
        assert!(
            implicit_feature.removed.iter().any(|finding| {
                finding.identity.name == "serde" && finding.identity.namespace == "cargo_feature"
            }),
            "removing an optional dependency without an explicit [features] table must drop the implicit feature"
        );

        let crate_type = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib']\n",
            ),
            ("src/lib.rs", "pub fn stable() {}\n", "pub fn stable() {}\n"),
        ]);
        assert!(
            crate_type.changed.iter().any(|finding| {
                finding.identity.name == "fixture" && finding.identity.namespace == "crate"
            }),
            "cdylib to rlib is a binary-contract change even when public items are unchanged"
        );

        let root_move = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='lib.rs'\n",
            ),
            ("src/lib.rs", "pub fn stable() {}\n", "pub fn stable() {}\n"),
            ("lib.rs", "pub fn stable() {}\n", "pub fn stable() {}\n"),
        ]);
        assert!(
            !root_move.changed.iter().any(|finding| {
                finding.identity.name == "fixture" && finding.identity.namespace == "crate"
            }),
            "an internal library-root move is not a caller-observable crate contract change"
        );
    }

    #[test]
    fn repository_backed_include_tracks_dependent_source_bytes() {
        let unchanged = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "include!(\"api.rs\");\n",
                "include!(\"api.rs\");\n",
            ),
            ("src/api.rs", "pub fn item() {}\n", "pub fn item() {}\n"),
        ]);
        assert!(
            unchanged.unknown.is_empty(),
            "an unchanged include and unchanged included file neutralize"
        );

        let included_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "include!(\"api.rs\");\n",
                "include!(\"api.rs\");\n",
            ),
            ("src/api.rs", "pub fn item() {}\n", "pub fn extra() {}\n"),
        ]);
        assert!(
            included_changed.unknown.iter().any(|finding| {
                finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("IncludeMacro"))
            }),
            "changing the included file must keep the include unknown active"
        );
    }

    #[test]
    fn repository_backed_public_trait_impl_change_is_typed() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker {}\npub struct Value;\n",
                "pub trait Marker {}\npub struct Value;\nimpl Marker for Value {}\n",
            ),
        ]);
        assert!(delta.unknown.iter().any(|finding| {
            finding.identity.name == "TraitImplResolution"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("impl Marker for Value"))
        }));

        let private_control = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "trait PrivateMarker {}\nstruct PrivateValue;\n",
                "trait PrivateMarker {}\nstruct PrivateValue;\nimpl PrivateMarker for PrivateValue {}\n",
            ),
        ]);
        assert!(
            private_control.findings().is_empty(),
            "an impl whose trait and owner are both private is not public API uncertainty"
        );

        let private_module = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker {}\npub struct Value;\n",
                "pub trait Marker {}\npub struct Value;\nmod helper { impl crate::Marker for crate::Value {} }\n",
            ),
        ]);
        assert!(private_module.unknown.iter().any(|finding| {
            finding.identity.name == "TraitImplResolution"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("impl"))
        }));
    }

    #[test]
    fn repository_backed_trait_impl_binder_rename_is_not_unknown_delta() {
        let renamed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker<T> {}\npub struct Wrapper<T>(pub T);\nimpl<T> Marker<T> for Wrapper<T> {}\n",
                "pub trait Marker<T> {}\npub struct Wrapper<T>(pub T);\nimpl<U> Marker<U> for Wrapper<U> {}\n",
            ),
        ]);
        assert!(
            renamed.findings().is_empty(),
            "renaming an impl-level binder is caller-equivalent, got {:?}",
            renamed.findings()
        );

        let retargeted = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker<T> {}\npub struct Wrapper<T>(pub T);\nimpl Marker<u8> for Wrapper<u8> {}\n",
                "pub trait Marker<T> {}\npub struct Wrapper<T>(pub T);\nimpl Marker<u16> for Wrapper<u16> {}\n",
            ),
        ]);
        assert!(
            retargeted
                .unknown
                .iter()
                .any(|finding| { finding.identity.name == "TraitImplResolution" }),
            "changing the impl's observable type arguments must keep TraitImplResolution active"
        );
    }

    #[test]
    fn repository_backed_public_reexport_retarget_is_changed() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub mod a { pub struct A; }\npub mod b { pub struct B; }\npub use a::A as Public;\n",
                "pub mod a { pub struct A; }\npub mod b { pub struct B; }\npub use b::B as Public;\n",
            ),
        ]);
        assert!(
            delta
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Public"),
            "retargeting a public name between two still-public types is a contract change"
        );

        let private_donor = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod donor { pub struct Old; }\npub use donor::Old as Public;\n",
                "mod donor { pub struct New; }\npub use donor::New as Public;\n",
            ),
        ]);
        assert!(
            private_donor
                .findings()
                .iter()
                .all(|finding| finding.identity.name != "Public"),
            "renaming a private reexport donor is not a public contract change"
        );
    }

    #[test]
    fn api_delta_ids_preserve_case_and_cfg_identity_without_panicking() {
        let base = snapshot_rust_api(&MemorySource::source(
            "#[cfg(unix)] pub fn Variant(x: u8) {}\n#[cfg(windows)] pub fn Variant(x: u8) {}\npub fn Foo(x: u8) {}\npub fn foo(x: u8) {}",
            "base",
        ));
        let target = snapshot_rust_api(&MemorySource::source(
            "#[cfg(unix)] pub fn Variant(x: u16) {}\n#[cfg(windows)] pub fn Variant(x: u16) {}\npub fn Foo(x: u16) {}\npub fn foo(x: u16) {}",
            "target",
        ));
        let delta = compare_rust_api(&base, &target);
        let variants: Vec<_> = delta
            .changed
            .iter()
            .filter(|finding| finding.identity.name == "Variant")
            .collect();
        assert_eq!(variants.len(), 2);
        assert_ne!(variants[0].id, variants[1].id);
        let upper = delta
            .changed
            .iter()
            .find(|finding| finding.identity.name == "Foo")
            .unwrap();
        let lower = delta
            .changed
            .iter()
            .find(|finding| finding.identity.name == "foo")
            .unwrap();
        assert_ne!(upper.id, lower.id);
        assert!(upper.id.contains("Foo"));
        assert!(lower.id.contains("foo"));
    }

    #[test]
    fn api_delta_cfg_pairing_is_conservative_and_unknowns_overlap_by_default() {
        let disjoint = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(unix)] pub fn gated(x: u8) {}",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(windows)] pub fn gated(x: u16) {}",
                "target",
            )),
        );
        assert!(disjoint.changed.is_empty());
        assert_eq!(disjoint.removed.len(), 1);
        assert_eq!(disjoint.added.len(), 1);

        let overlapping = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(feature = \"a\")] pub fn gated(x: u8) {}",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(feature = \"b\")] pub fn gated(x: u16) {}",
                "target",
            )),
        );
        assert_eq!(overlapping.changed.len(), 1);

        for target in [
            "include!(\"generated.rs\");",
            "mod donor { pub fn item() {} }\npub use donor::*;",
        ] {
            let delta = compare_rust_api(
                &snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base")),
                &snapshot_rust_api(&MemorySource::source(target, "target")),
            );
            assert!(delta.removed.is_empty());
            assert!(delta.unknown.iter().any(|finding| {
                finding.identity.name == "item"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("counterpart"))
            }));
        }
    }

    #[test]
    fn api_delta_pair_certainty_checks_both_unknown_sets_against_both_regions() {
        let forward = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(feature = \"a\")] pub fn gated(x: u8) {}",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(unix)] pub fn gated(x: u16) {}\n#[cfg(windows)] include!(\"generated.rs\");",
                "target",
            )),
        );
        assert!(forward.changed.is_empty());
        assert!(forward.unknown.iter().any(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("cfg-region pairing"))
        }));

        let reverse = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(unix)] pub fn gated(x: u8) {}\n#[cfg(windows)] include!(\"generated.rs\");",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[cfg(feature = \"a\")] pub fn gated(x: u16) {}",
                "target",
            )),
        );
        assert!(reverse.changed.is_empty());
        assert!(reverse.unknown.iter().any(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("cfg-region pairing"))
        }));
    }

    #[test]
    fn api_delta_unknowns_block_relocation_and_visibility_confirmation() {
        let relocation = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "pub mod old { pub fn moved() {} }",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "pub mod new { pub fn moved() {} }\nmod donor { pub fn item() {} }\npub use donor::*;",
                "target",
            )),
        );
        assert!(relocation.relocated.is_empty());
        assert!(relocation.unknown.iter().any(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("relocation intersects"))
        }));

        let visibility = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source("pub struct Public;", "base")),
            &snapshot_rust_api(&MemorySource::source(
                "struct Public;\nmod donor { pub struct Other; }\npub use donor::*;",
                "target",
            )),
        );
        assert!(visibility.visibility_changed.is_empty());
        assert!(visibility.unknown.iter().any(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("visibility transition intersects"))
        }));
    }

    #[test]
    fn api_delta_relocation_checks_unknowns_at_source_and_destination() {
        for (base, target) in [
            (
                "pub mod old { pub fn moved() {} }",
                "pub mod new { pub fn moved() {} include!(\"generated.rs\"); }",
            ),
            (
                "pub mod old { pub fn moved() {} include!(\"generated.rs\"); }",
                "pub mod new { pub fn moved() {} }",
            ),
        ] {
            let delta = compare_rust_api(
                &snapshot_rust_api(&MemorySource::source(base, "base")),
                &snapshot_rust_api(&MemorySource::source(target, "target")),
            );
            assert!(delta.relocated.is_empty());
            assert!(delta.unknown.iter().any(|finding| {
                finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("relocation intersects"))
            }));
        }
    }

    #[test]
    fn api_delta_exact_pairing_requires_uniqueness_on_both_sides() {
        let mut base = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base"));
        base.items.push(base.items[0].clone());
        let target = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "target"));
        let delta = compare_rust_api(&base, &target);
        assert!(delta.removed.is_empty());
        assert!(delta.added.is_empty());
        assert!(delta.changed.is_empty());
        assert!(delta.unknown.iter().any(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("exact identity candidates"))
        }));
    }

    #[test]
    fn api_delta_unmatched_duplicate_identity_is_typed_ambiguity_on_each_side() {
        let empty = snapshot_rust_api(&MemorySource::source("", "empty"));

        let mut target = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "target"));
        target.items.push(target.items[0].clone());
        let target_only = compare_rust_api(&empty, &target);
        assert!(target_only.added.is_empty());
        assert_eq!(
            target_only
                .unknown
                .iter()
                .filter(|finding| finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("unmatched target identity")))
                .count(),
            2
        );

        let mut base = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base"));
        base.items.push(base.items[0].clone());
        let base_only = compare_rust_api(&base, &empty);
        assert!(base_only.removed.is_empty());
        assert_eq!(
            base_only
                .unknown
                .iter()
                .filter(|finding| finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("unmatched base identity")))
                .count(),
            2
        );
    }

    #[test]
    fn api_delta_artifact_parity_projects_identical_shared_facts() {
        for cell in [
            "function_added",
            "function_removed",
            "declaration_pairing_changed",
            "move_relocated",
            "item_opener_removed",
            "reexport_changed",
        ] {
            let delta = fixture_delta(cell);
            let breaking = breaking_changes_view(&delta);
            let public = public_api_diff_view(&delta);
            assert_eq!(breaking.analysis_source, public.analysis_source, "{cell}");
            assert_eq!(breaking.counts, public.counts, "{cell}");
            assert_eq!(breaking.findings, public.findings, "{cell}");
        }
    }

    #[test]
    fn confirmed_public_relocations_use_existing_breaking_escalation() {
        use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

        for cell in ["module_scope_changed", "move_relocated"] {
            let view = breaking_changes_view(&fixture_delta(cell));
            assert_eq!(view.counts.relocated, 1, "{cell}");
            let mut confidence = AnalysisStatus::Complete;
            let mut merge = MergeRecommendation::Approve;
            crate::artifacts::apply_rust_api_delta_outcome(
                true,
                Some(&view),
                &mut confidence,
                &mut merge,
            );
            assert_eq!(confidence, AnalysisStatus::Complete, "{cell}");
            assert_eq!(merge, MergeRecommendation::ReviewRequired, "{cell}");
        }
    }

    #[test]
    fn unknown_api_delta_degrades_without_blocking_or_policy_default_change() {
        use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

        let base = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base"));
        let target = snapshot_rust_api(&MemorySource::source(
            "mod donor { pub fn item() {} }\npub use donor::*;",
            "target",
        ));
        let view = breaking_changes_view(&compare_rust_api(&base, &target));
        assert!(view.counts.unknown > 0);
        let mut confidence = AnalysisStatus::Complete;
        let mut merge = MergeRecommendation::Approve;
        crate::artifacts::apply_rust_api_delta_outcome(
            false,
            Some(&view),
            &mut confidence,
            &mut merge,
        );
        assert_eq!(confidence, AnalysisStatus::Degraded);
        assert_eq!(merge, MergeRecommendation::ReviewRequired);
    }

    #[test]
    fn api_delta_artifact_schema_is_additive_and_explicit_about_unknowns() {
        let known =
            serde_json::to_value(public_api_diff_view(&fixture_delta("function_added"))).unwrap();
        let finding = &known["findings"][0];
        assert_eq!(known["analysis_source"], REPO_BACKED_RUST_API_SOURCE);
        assert_eq!(
            known["base_revision"],
            "git_tree:fixture://function_added/base"
        );
        assert_eq!(
            known["target_revision"],
            "git_tree:fixture://function_added/head"
        );
        assert_eq!(finding["analysis_source"], REPO_BACKED_RUST_API_SOURCE);
        assert_eq!(finding["confidence"], "confirmed");
        assert!(finding["evidence"].is_array());
        assert!(finding.get("unknown_reason").is_none());

        let base = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base"));
        let target = snapshot_rust_api(&MemorySource::source(
            "mod donor { pub fn item() {} }\npub use donor::*;",
            "target",
        ));
        let unknown =
            serde_json::to_value(public_api_diff_view(&compare_rust_api(&base, &target))).unwrap();
        let finding = unknown["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding["confidence"] == "unknown")
            .unwrap();
        assert!(
            finding["unknown_reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty())
        );
        assert!(
            finding["evidence"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
        let standalone = unknown["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|finding| finding.get("unknown_source").is_some())
            .unwrap();
        assert_eq!(standalone["unknown_source"]["side"], "target");
        assert_eq!(standalone["unknown_source"]["source_path"], "src/lib.rs");
        assert!(standalone["unknown_source"]["provenance"].is_string());
    }

    #[test]
    fn production_api_artifacts_serialize_the_same_lossless_delta() {
        let tmp = tempfile::tempdir().unwrap();
        let delta = fixture_delta("move_relocated");
        let public_view = public_api_diff_view(&delta);
        let breaking_view = breaking_changes_view(&delta);

        write_public_api_diff(
            tmp.path(),
            analyze_js_ts_public_api_diff(&[]),
            Some(&public_view),
        )
        .unwrap();
        write_breaking_changes_with_api(tmp.path(), Some(&breaking_view), &[]).unwrap();

        let public: serde_json::Value = serde_json::from_slice(
            &std::fs::read(tmp.path().join("PUBLIC_API_DIFF.json")).unwrap(),
        )
        .unwrap();
        let breaking: serde_json::Value = serde_json::from_slice(
            &std::fs::read(tmp.path().join("BREAKING_CHANGES.json")).unwrap(),
        )
        .unwrap();
        assert!(public["added"].is_array());
        assert!(public["removed"].is_array());
        assert!(public["changed"].is_array());
        assert_eq!(
            public["rust_api_delta"],
            serde_json::to_value(&public_view).unwrap()
        );
        assert_eq!(breaking, serde_json::to_value(&breaking_view).unwrap());
        assert_eq!(public["rust_api_delta"]["findings"], breaking["findings"]);
        assert_eq!(public["rust_api_delta"]["counts"], breaking["counts"]);
    }

    #[test]
    fn added_only_api_touch_is_informational_and_policy_neutral() {
        use crate::checks::CheckStatus;
        use crate::policy::engine::{AnalysisStatus, MergeRecommendation};

        let tmp = tempfile::tempdir().unwrap();
        let view = public_api_diff_view(&fixture_delta("function_added"));
        let check =
            write_public_api_diff(tmp.path(), analyze_js_ts_public_api_diff(&[]), Some(&view))
                .unwrap()
                .unwrap();
        assert_eq!(check.status, CheckStatus::Passed);
        let mut confidence = AnalysisStatus::Complete;
        let mut merge = MergeRecommendation::Approve;
        crate::artifacts::apply_rust_api_delta_outcome(
            true,
            Some(&view),
            &mut confidence,
            &mut merge,
        );
        assert_eq!(confidence, AnalysisStatus::Complete);
        assert_eq!(merge, MergeRecommendation::Approve);
    }

    #[test]
    fn operator_policy_api_matrix() {
        use crate::policy::engine::{AnalysisStatus, EnforcementDisposition, MergeRecommendation};

        let added = public_api_diff_view(&fixture_delta("function_added"));
        let confirmed = public_api_diff_view(&fixture_delta("function_removed"));
        let base = snapshot_rust_api(&MemorySource::source("pub fn item() {}", "base"));
        let target = snapshot_rust_api(&MemorySource::source(
            "mod donor { pub fn item() {} }\npub use donor::*;",
            "target",
        ));
        let unknown = public_api_diff_view(&compare_rust_api(&base, &target));

        let rows = [
            (
                "addition",
                &added,
                EnforcementDisposition::Clean,
                AnalysisStatus::Complete,
                MergeRecommendation::Approve,
            ),
            (
                "confirmed breaking",
                &confirmed,
                EnforcementDisposition::ReviewRequired,
                AnalysisStatus::Complete,
                MergeRecommendation::ReviewRequired,
            ),
            (
                "unknown",
                &unknown,
                EnforcementDisposition::ReviewRequired,
                AnalysisStatus::Degraded,
                MergeRecommendation::ReviewRequired,
            ),
        ];

        for (name, view, expected_disposition, expected_analysis, expected_merge) in rows {
            let mut analysis = AnalysisStatus::Complete;
            let mut merge = MergeRecommendation::Approve;
            let disposition = crate::artifacts::apply_rust_api_delta_outcome(
                true,
                Some(view),
                &mut analysis,
                &mut merge,
            );
            assert_eq!(disposition, expected_disposition, "{name}");
            assert_eq!(analysis, expected_analysis, "{name}");
            assert_eq!(merge, expected_merge, "{name}");
        }

        // The public breaking-escalation opt-out remains effective: facts stay
        // serialized, but a confirmed break does not create a hidden exit-only
        // failure when policy explicitly leaves the verdict informational.
        let mut analysis = AnalysisStatus::Complete;
        let mut merge = MergeRecommendation::Approve;
        let disposition = crate::artifacts::apply_rust_api_delta_outcome(
            false,
            Some(&confirmed),
            &mut analysis,
            &mut merge,
        );
        assert_eq!(disposition, EnforcementDisposition::Clean);
        assert_eq!(analysis, AnalysisStatus::Complete);
        assert_eq!(merge, MergeRecommendation::Approve);
    }

    #[test]
    fn operator_policy_language_boundary() {
        // Rust reads revision snapshots through the existing language-neutral
        // source contract; no patch-only backend is introduced for the 0.8
        // Rust-first slice.
        let base = MemorySource::source("pub fn old_api() {}", "base");
        let target = MemorySource::source("pub fn new_api() {}", "target");
        let delta = compare_rust_api(&snapshot_rust_api(&base), &snapshot_rust_api(&target));
        assert_eq!(base.entries().len(), target.entries().len());
        assert!(
            delta
                .removed
                .iter()
                .any(|finding| finding.identity.name == "old_api")
        );
        assert!(
            delta
                .added
                .iter()
                .any(|finding| finding.identity.name == "new_api")
        );

        // The unchanged JS/TS compatibility path still consumes patch sections
        // and never observes Rust lines in the same patch.
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +0,0 @@\n-pub fn rust_only() {}\ndiff --git a/src/api.ts b/src/api.ts\n--- a/src/api.ts\n+++ b/src/api.ts\n@@ -1 +0,0 @@\n-export function js_only() {}\n";
        let legacy = analyze_js_ts_public_api_diff(&[patch.to_owned()]);
        assert_eq!(legacy.removed.len(), 1);
        assert_eq!(legacy.removed[0].file, "src/api.ts");
        assert!(legacy.removed[0].signature.contains("js_only"));
    }

    #[test]
    fn multi_base_merge_preserves_each_comparison_provenance_without_dedup() {
        let target = snapshot_rust_api(&MemorySource::source(
            "pub fn added_for_each_base() {}",
            "target",
        ));
        let base_a = snapshot_rust_api(&MemorySource::source("", "base-a"));
        let base_b = snapshot_rust_api(&MemorySource::source("", "base-b"));
        let merged = merge_comparisons(vec![
            compare_rust_api(&base_a, &target),
            compare_rust_api(&base_b, &target),
        ]);
        assert_eq!(merged.added.len(), 2);
        assert_ne!(merged.added[0].id, merged.added[1].id);
        let evidence = merged
            .added
            .iter()
            .flat_map(|finding| finding.evidence.iter())
            .cloned()
            .collect::<Vec<_>>();
        assert!(evidence.iter().any(|line| line.contains("base-a")));
        assert!(evidence.iter().any(|line| line.contains("base-b")));
        assert!(merged.base_revision.contains("base-a"));
        assert!(merged.base_revision.contains("base-b"));
    }

    #[test]
    fn duplicate_exact_revision_pair_is_compared_and_emitted_once() {
        let (_tmp, repo, base, target) = make_test_repo(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", "", "pub fn added_once() {}\n"),
        ]);
        let diff = make_diff_with_ids(base, target, Vec::new());
        assert_eq!(
            unique_exact_revision_pairs(&[diff.clone(), diff.clone()]).len(),
            1,
            "dedup must happen before either snapshot/comparison is executed"
        );
        let delta = compare_rust_api_revisions(&repo, &[diff.clone(), diff])
            .unwrap()
            .unwrap();
        assert_eq!(delta.added.len(), 1);
        assert!(!delta.base_revision.starts_with("multiple:"));
        assert!(!delta.added[0].id.contains("|comparison:"));
    }

    #[test]
    fn different_ref_names_converging_to_one_exact_pair_emit_once() {
        let (_tmp, repo, base, target) = make_test_repo(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", "", "pub fn converged() {}\n"),
        ]);
        let mut main = make_diff_with_ids(base.clone(), target.clone(), Vec::new());
        main.base = "main".to_owned();
        let mut release = make_diff_with_ids(base, target, Vec::new());
        release.base = "release".to_owned();
        assert_eq!(
            unique_exact_revision_pairs(&[main.clone(), release.clone()]).len(),
            1
        );
        let delta = compare_rust_api_revisions(&repo, &[main, release])
            .unwrap()
            .unwrap();
        assert_eq!(delta.added.len(), 1);
        assert!(!delta.base_revision.starts_with("multiple:"));
    }

    #[test]
    fn distinct_exact_revision_pairs_remain_separate_in_production_merge() {
        let (tmp, _repo, base, middle) = make_test_repo(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "fn private_base() {}\n",
                "fn private_middle() {}\n",
            ),
        ]);
        fs::write(tmp.path().join("src/lib.rs"), "pub fn shared_target() {}\n").unwrap();
        let head = {
            let git = git2::Repository::open(tmp.path()).unwrap();
            let mut index = git.index().unwrap();
            index.add_path(Path::new("src/lib.rs")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = git.find_tree(tree_id).unwrap();
            let parent = git.find_commit(middle.parse().unwrap()).unwrap();
            let signature = git2::Signature::now("Test", "test@test.com").unwrap();
            git.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "head",
                &tree,
                &[&parent],
            )
            .unwrap()
            .to_string()
        };
        let repo = crate::git::Repository::open(tmp.path()).unwrap();
        let diffs = [
            make_diff_with_ids(base, head.clone(), Vec::new()),
            make_diff_with_ids(middle, head, Vec::new()),
        ];
        assert_eq!(unique_exact_revision_pairs(&diffs).len(), 2);
        let delta = compare_rust_api_revisions(&repo, &diffs).unwrap().unwrap();
        assert_eq!(delta.added.len(), 2);
        assert_ne!(delta.added[0].id, delta.added[1].id);
        assert!(
            delta
                .added
                .iter()
                .all(|finding| finding.id.contains("|comparison:"))
        );
    }

    fn fixture_patch(cell: &str) -> String {
        let root = corpus_root().join(cell);
        patch_between_files(&root.join("base/src/lib.rs"), &root.join("head/src/lib.rs"))
    }

    fn source_patch(base: &str, target: &str) -> String {
        let root = tempfile::tempdir().unwrap();
        let base_path = root.path().join("base.rs");
        let target_path = root.path().join("target.rs");
        fs::write(&base_path, base).unwrap();
        fs::write(&target_path, target).unwrap();
        patch_between_files(&base_path, &target_path)
    }

    fn patch_between_files(base: &Path, target: &Path) -> String {
        let output = git_cmd()
            .args([
                "diff",
                "--no-index",
                "--",
                base.to_str().unwrap(),
                target.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "git diff --no-index exited with {:?}",
            output.status.code()
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                if line.starts_with("diff --git ") {
                    "diff --git a/src/lib.rs b/src/lib.rs"
                } else if line.starts_with("--- ") {
                    "--- a/src/lib.rs"
                } else if line.starts_with("+++ ") {
                    "+++ b/src/lib.rs"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum LegacyBreakingKind {
        RemovedSymbol { symbol_type: String },
        RelocatedSymbol { symbol_type: String },
        ChangedSignature { before: String, after: String },
        NewEnvRequirement { variable: String },
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
    struct LegacyBreakingFact {
        file: String,
        kind: LegacyBreakingKind,
        line: String,
        risk_level: String,
    }

    fn legacy_breaking_facts(patch: &str) -> Vec<LegacyBreakingFact> {
        legacy_breaking_facts_from_findings(analyze_all_breaking_changes(&[patch.to_owned()]))
    }

    fn legacy_breaking_facts_from_findings(
        findings: Vec<crate::artifacts::signal::breaking::BreakingFinding>,
    ) -> Vec<LegacyBreakingFact> {
        let mut facts: Vec<_> = findings
            .into_iter()
            .map(|finding| LegacyBreakingFact {
                file: finding.file,
                kind: match finding.kind {
                    BreakingKind::RemovedSymbol { symbol_type } => {
                        LegacyBreakingKind::RemovedSymbol { symbol_type }
                    }
                    BreakingKind::RelocatedSymbol { symbol_type } => {
                        LegacyBreakingKind::RelocatedSymbol { symbol_type }
                    }
                    BreakingKind::ChangedSignature { before, after } => {
                        LegacyBreakingKind::ChangedSignature { before, after }
                    }
                    BreakingKind::NewEnvRequirement { variable } => {
                        LegacyBreakingKind::NewEnvRequirement { variable }
                    }
                },
                line: finding.line,
                risk_level: format!("{:?}", finding.risk_level).to_lowercase(),
            })
            .collect();
        facts.sort();
        facts
    }

    fn historical_fact_kinds(
        facts: &[LegacyBreakingFact],
    ) -> Vec<historical_scenarios::HistoricalFactKind> {
        let mut kinds: Vec<_> = facts
            .iter()
            .map(|fact| match &fact.kind {
                LegacyBreakingKind::RemovedSymbol { .. } => {
                    historical_scenarios::HistoricalFactKind::Removed
                }
                LegacyBreakingKind::RelocatedSymbol { .. } => {
                    historical_scenarios::HistoricalFactKind::Relocated
                }
                LegacyBreakingKind::ChangedSignature { .. } => {
                    historical_scenarios::HistoricalFactKind::Changed
                }
                LegacyBreakingKind::NewEnvRequirement { .. } => {
                    historical_scenarios::HistoricalFactKind::NewEnvRequirement
                }
            })
            .collect();
        kinds.sort();
        kinds
    }

    fn legacy_public_artifact(patch: &str) -> Option<serde_json::Value> {
        let dir = tempfile::tempdir().unwrap();
        let _ = generate_public_api_diff(dir.path(), &[patch.to_owned()]).unwrap();
        let path = dir.path().join("PUBLIC_API_DIFF.json");
        if !path.exists() {
            return None;
        }
        Some(read_json(&path))
    }

    fn public_artifact_is_positive(artifact: Option<&serde_json::Value>) -> bool {
        artifact.is_some_and(|value| {
            ["added", "removed", "changed"].iter().any(|key| {
                value[*key]
                    .as_array()
                    .is_some_and(|facts| !facts.is_empty())
            })
        })
    }

    #[derive(serde::Serialize)]
    struct HistoricalBinding {
        test_id: historical_scenarios::HistoricalTestId,
        expected_breaking_kinds: Vec<historical_scenarios::HistoricalFactKind>,
        actual_breaking_facts: Vec<LegacyBreakingFact>,
    }

    #[derive(serde::Serialize)]
    struct ParityRow {
        corpus_cell: String,
        base_revision: String,
        target_revision: String,
        declared_legacy_expectation: String,
        legacy_breaking_facts: Vec<LegacyBreakingFact>,
        legacy_public_api_artifact: Option<serde_json::Value>,
        repo_backed_facts: Vec<ApiDeltaFinding>,
        current_legacy_breaking_positive: bool,
        current_legacy_public_positive: bool,
        historical_binding: Option<HistoricalBinding>,
        legacy_relationship: String,
        classification: String,
        rationale: String,
        recommended_disposition: String,
        status: String,
        phase_a_operator_effect: &'static str,
        phase_b_operator_effect: String,
    }

    #[derive(serde::Serialize)]
    struct ControlledParityCase {
        id: &'static str,
        base_revision: String,
        target_revision: String,
        legacy_breaking_facts: Vec<LegacyBreakingFact>,
        legacy_public_api_artifact: Option<serde_json::Value>,
        repo_backed_facts: Vec<ApiDeltaFinding>,
    }

    #[derive(serde::Serialize)]
    struct ParityLedger {
        schema: &'static str,
        rows: Vec<ParityRow>,
        controlled_cases: Vec<ControlledParityCase>,
    }

    fn expectation_name(expectation: &CorpusExpectation) -> &'static str {
        match expectation {
            CorpusExpectation::Positive => "positive",
            CorpusExpectation::Negative => "negative",
            CorpusExpectation::AcceptedZero => "accepted_zero",
        }
    }

    fn controlled_case(id: &'static str, base: &str, target: &str) -> ControlledParityCase {
        let patch = source_patch(base, target);
        let delta = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(base, &format!("{id}/base"))),
            &snapshot_rust_api(&MemorySource::source(target, &format!("{id}/target"))),
        );
        ControlledParityCase {
            id,
            base_revision: delta.base_revision.clone(),
            target_revision: delta.target_revision.clone(),
            legacy_breaking_facts: legacy_breaking_facts(&patch),
            legacy_public_api_artifact: legacy_public_artifact(&patch),
            repo_backed_facts: delta.findings(),
        }
    }

    fn controlled_parity_cases() -> Vec<ControlledParityCase> {
        vec![
            controlled_case(
                "multiplicity_namespace_cfg",
                "",
                "pub struct Same;\n#[cfg(unix)] pub fn Variant() {}\n#[cfg(windows)] pub fn Variant() {}\n#[macro_export] macro_rules! Exported { () => {} }",
            ),
            controlled_case(
                "include_macro_unknown",
                "pub fn item() {}",
                "include!(\"generated.rs\");",
            ),
            controlled_case(
                "glob_reexport_unknown",
                "pub fn item() {}",
                "mod donor { pub fn item() {} }\npub use donor::*;",
            ),
            controlled_case("source_parse_unknown", "pub fn item() {}", "pub fn broken("),
            controlled_case(
                "no_op_provenance",
                "pub fn stable() {}",
                "pub fn stable() {}",
            ),
        ]
    }

    #[test]
    fn api_delta_parity_ledger_preserves_full_controlled_facts() {
        let cases = controlled_parity_cases();
        let multiplicity = &cases[0].repo_backed_facts;
        assert_eq!(
            multiplicity
                .iter()
                .filter(|finding| finding.identity.name == "Variant")
                .count(),
            2
        );
        assert!(multiplicity.iter().any(|finding| {
            finding.identity.name == "Same" && finding.identity.namespace == "type"
        }));
        assert!(multiplicity.iter().any(|finding| {
            finding.identity.name == "Same" && finding.identity.namespace == "value"
        }));
        assert!(multiplicity.iter().all(|finding| {
            !finding.id.is_empty()
                && finding.confidence == ApiDeltaConfidence::Confirmed
                && !finding.evidence.is_empty()
                && finding
                    .after
                    .as_ref()
                    .is_some_and(|side| side.provenance.contains("/target"))
        }));
        for (case, unknown_name) in
            cases[1..]
                .iter()
                .zip(["IncludeMacro", "GlobReexport", "SourceParse"])
        {
            assert!(case.repo_backed_facts.iter().any(|finding| {
                finding.identity.name == unknown_name
                    && finding.confidence == ApiDeltaConfidence::Unknown
                    && finding.unknown_reason.is_some()
                    && !finding.evidence.is_empty()
                    && finding.unknown_source.as_ref().is_some_and(|source| {
                        source.side == ApiSnapshotSide::Target
                            && source.source_path == "src/lib.rs"
                            && source.provenance.contains("/target")
                    })
            }));
            assert!(!case.legacy_breaking_facts.is_empty());
            assert!(case.legacy_public_api_artifact.is_some());
        }
        assert!(
            cases
                .iter()
                .all(|case| { !case.base_revision.is_empty() && !case.target_revision.is_empty() })
        );
        assert!(cases[4].repo_backed_facts.is_empty());
        assert!(cases[4].base_revision.contains("no_op_provenance/base"));
        assert!(cases[4].target_revision.contains("no_op_provenance/target"));
        let encoded = serde_json::to_value(&cases).unwrap();
        assert!(encoded[1]["legacy_breaking_facts"][0]["file"].is_string());
        assert!(encoded[1]["legacy_breaking_facts"][0]["line"].is_string());
        assert!(encoded[1]["legacy_breaking_facts"][0]["risk_level"].is_string());
        assert!(encoded[1]["legacy_public_api_artifact"]["removed"][0]["signature"].is_string());
    }

    #[test]
    fn api_delta_legacy_parity_covers_every_w0_cell_and_sibling() {
        let root = corpus_root();
        let manifest: CorpusManifest = read_json(&root.join("manifest.json"));
        let mut rows = Vec::new();
        for cell in manifest.cells {
            let expected: CorpusExpected = read_json(&root.join(&cell.id).join("expected.json"));
            let patch = fixture_patch(&cell.id);
            let breaking = legacy_breaking_facts(&patch);
            let public = legacy_public_artifact(&patch);
            let delta = fixture_delta(&cell.id);
            let repo_backed = delta.findings();
            let legacy_breaking_positive = !breaking.is_empty();
            let legacy_public_positive = public_artifact_is_positive(public.as_ref());
            let repo_positive = repo_backed
                .iter()
                .any(|finding| finding.kind != ApiDeltaKind::Unknown);
            let declared_legacy_positive =
                expected.legacy_expectation == CorpusExpectation::Positive;
            let observation_mismatch = legacy_breaking_positive != declared_legacy_positive;
            let historical_binding = cell.historical_test_id.map(|test_id| {
                let scenario = historical_scenarios::scenario(test_id);
                assert_eq!(scenario.id, test_id, "{}", cell.id);
                assert_eq!(
                    scenario.expected_kinds, cell.historical_expected_breaking_kinds,
                    "{}: manifest expectation must match the shared historical scenario",
                    cell.id
                );
                let actual_breaking_facts = legacy_breaking_facts_from_findings(
                    analyze_all_breaking_changes(&scenario.patches),
                );
                assert_eq!(
                    historical_fact_kinds(&actual_breaking_facts),
                    cell.historical_expected_breaking_kinds,
                    "{}: shared historical scenario actual facts",
                    cell.id
                );
                HistoricalBinding {
                    test_id,
                    expected_breaking_kinds: cell.historical_expected_breaking_kinds.clone(),
                    actual_breaking_facts,
                }
            });
            let relationship = match (observation_mismatch, historical_binding.is_some()) {
                (true, true) => "fixture_mapping_mismatch",
                (true, false) => "genuine_legacy_blind_spot",
                (false, _) => "current_fixture_matches_mapping",
            };
            if historical_binding.is_some() {
                assert!(
                    observation_mismatch,
                    "{}: historical binding is only needed for an actual W0 mapping mismatch",
                    cell.id
                );
            }
            let (classification, rationale, recommended_disposition, status, phase_b_effect) =
                if relationship == "fixture_mapping_mismatch"
                    || relationship == "genuine_legacy_blind_spot"
                {
                    (
                        relationship.to_owned(),
                        if relationship == "fixture_mapping_mismatch" {
                            "the shared historical scenario matches its typed expectation, while current execution of the distinct W0 fixture does not match the transferred legacy polarity".to_owned()
                        } else {
                            format!(
                                "current W0 legacy breaking polarity is {legacy_breaking_positive}, declared polarity is {declared_legacy_positive}, and no historical mapping substitute is involved"
                            )
                        },
                        cell.recommended_disposition
                            .clone()
                            .expect("operator row disposition"),
                        "operator_decision_required".to_owned(),
                        cell.phase_b_operator_effect
                            .clone()
                            .expect("operator row Phase B effect"),
                    )
                } else if expected.legacy_expectation == CorpusExpectation::AcceptedZero
                    && repo_positive
                {
                    (
                        "proven_truth_improvement".to_owned(),
                        expected
                            .legacy_delta_rationale
                            .clone()
                            .expect("accepted-zero rationale"),
                        "already accepted by W0/D2".to_owned(),
                        "accepted_by_w0_d2".to_owned(),
                        "canonical Rust artifacts gain the W0-proven semantic fact".to_owned(),
                    )
                } else if legacy_public_positive != repo_positive {
                    (
                        "proven_truth_improvement".to_owned(),
                        "repo-backed semantic polarity matches the frozen W0 record and removes a legacy public-API heuristic false positive/negative".to_owned(),
                        "already accepted by W0/D2".to_owned(),
                        "accepted_by_w0_d2".to_owned(),
                        "canonical Rust artifacts replace the legacy public-API heuristic polarity".to_owned(),
                    )
                } else {
                    (
                        "compatibility_preserving_presentation".to_owned(),
                        "legacy polarity and repo-backed semantic polarity agree".to_owned(),
                        "preserve semantic polarity".to_owned(),
                        "accepted_by_w0_d2".to_owned(),
                        "only the structured presentation changes".to_owned(),
                    )
                };
            rows.push(ParityRow {
                corpus_cell: cell.id,
                base_revision: delta.base_revision,
                target_revision: delta.target_revision,
                declared_legacy_expectation: expectation_name(&expected.legacy_expectation)
                    .to_owned(),
                legacy_breaking_facts: breaking,
                legacy_public_api_artifact: public,
                repo_backed_facts: repo_backed,
                current_legacy_breaking_positive: legacy_breaking_positive,
                current_legacy_public_positive: legacy_public_positive,
                historical_binding,
                legacy_relationship: relationship.to_owned(),
                classification,
                rationale,
                recommended_disposition,
                status,
                phase_a_operator_effect: "none: shadow-only, no verdict or policy change",
                phase_b_operator_effect: phase_b_effect,
            });
        }
        assert_eq!(rows.len(), 32);
        assert!(
            rows.iter()
                .all(|row| !row.base_revision.is_empty() && !row.target_revision.is_empty()),
            "every parity row must preserve both revision provenances"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.historical_binding.is_some())
                .count(),
            6,
            "the exact six fixture-mapping mismatches must stay mechanically bound"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.legacy_relationship == "genuine_legacy_blind_spot")
                .count(),
            3
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.legacy_relationship == "fixture_mapping_mismatch")
                .count(),
            6
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.legacy_relationship == "current_fixture_matches_mapping")
                .count(),
            23
        );
        let ledger = ParityLedger {
            schema: "prview.api_delta_phase_a_parity.v3",
            rows,
            controlled_cases: controlled_parity_cases(),
        };
        let json = serde_json::to_string_pretty(&ledger).unwrap();
        let rendered = format!("{json}\n");
        let ledger_path = root.join("phase_a_parity.json");
        if std::env::var_os("PRVIEW_UPDATE_API_DELTA_LEDGER").is_some() {
            fs::write(&ledger_path, &rendered).unwrap();
        } else if ledger_path.exists() {
            let expected_json = fs::read(&ledger_path).unwrap();
            assert_eq!(rendered.as_bytes(), expected_json.as_slice());
        }
        println!("API_DELTA_PARITY_LEDGER_BEGIN\n{json}\nAPI_DELTA_PARITY_LEDGER_END");
    }
}
