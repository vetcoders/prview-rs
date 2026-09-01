//! Canonical comparison of two revision-backed Rust API snapshots.
//!
//! This module is intentionally side-effect free. It computes one typed delta
//! from exact repository revisions and exposes deterministic projections used
//! by both production Rust API artifacts.

use super::api_surface::{
    NON_NEUTRALIZABLE_SYMLINK_ROOT, RustApiDeclaration, RustApiItem, RustApiItemKey,
    RustApiSnapshot, RustApiUnknown, RustApiUnknownKind, RustNamespace, RustSourceCertainty,
    guards_proven_disjoint, trait_member_cfg_key,
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
    if trait_default_addition_is_compatible(&before.contract, &after.contract) {
        return;
    }
    match (
        public_enum_contract(&before.contract),
        public_enum_contract(&after.contract),
    ) {
        (Some(before_enum), Some(after_enum)) => {
            let new_variants = after_enum
                .variants
                .keys()
                .filter(|name| !before_enum.variants.contains_key(*name))
                .cloned()
                .collect::<Vec<_>>();
            let outer_variant_addition_allowed = new_variants.is_empty()
                || (before_enum.non_exhaustive
                    && after_enum.non_exhaustive
                    && after_enum
                        .variant_order
                        .starts_with(&before_enum.variant_order)
                    && new_variants.iter().all(|name| {
                        after_enum
                            .variants
                            .get(name)
                            .is_some_and(|contract| variant_is_fieldless(contract))
                    }));
            let existing_variants_compatible = before_enum.variants.iter().all(|(name, before)| {
                let Some(after) = after_enum.variants.get(name) else {
                    return false;
                };
                before == after
            });
            let additive_non_exhaustive = !before_enum.layout_sensitive
                && !after_enum.layout_sensitive
                && before_enum.header == after_enum.header
                && outer_variant_addition_allowed
                && existing_variants_compatible
                && !new_variants.is_empty();
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
    let parent_policy_changed = before_struct.non_exhaustive != after_struct.non_exhaustive;
    let parent_contract_changed = before_struct.parent_contract != after_struct.parent_contract;
    // `#[non_exhaustive]` prevents downstream construction and exhaustive
    // matching, but a newly added field still contributes to compiler-derived
    // auto traits of the parent type. Without compiler-backed proof for that
    // input, every field addition to an existing public struct is Changed.
    let mut emitted = field_added || parent_policy_changed || parent_contract_changed;
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
            // An added field is represented by the parent Changed finding
            // above, never only by an informational child fact.
            (None, Some(_)) if field_added => continue,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum TraitDefaultSlot {
    Method { has_default: bool },
    Const { default: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraitDefaultContract {
    skeleton: String,
    slots: Vec<TraitDefaultSlot>,
}

fn trait_default_contract(contract: &str) -> Option<TraitDefaultContract> {
    let contract = contract
        .split_once("\nreexport-origin:")
        .map_or(contract, |(item, _)| item);
    let syn::Item::Trait(mut item) = syn::parse_str::<syn::Item>(contract).ok()? else {
        return None;
    };
    let mut slots = Vec::new();
    for trait_item in &mut item.items {
        match trait_item {
            syn::TraitItem::Fn(function) => {
                let has_default = function
                    .attrs
                    .iter()
                    .any(|attribute| attribute.path().is_ident("prview_trait_default"));
                function
                    .attrs
                    .retain(|attribute| !attribute.path().is_ident("prview_trait_default"));
                slots.push(TraitDefaultSlot::Method { has_default });
            }
            syn::TraitItem::Const(value) => {
                let default = value.default.take().map(|(_, expression)| {
                    quote::ToTokens::to_token_stream(&expression).to_string()
                });
                slots.push(TraitDefaultSlot::Const { default });
            }
            _ => {}
        }
    }
    Some(TraitDefaultContract {
        skeleton: quote::ToTokens::to_token_stream(&item).to_string(),
        slots,
    })
}

fn trait_default_addition_is_compatible(before: &str, after: &str) -> bool {
    let Some(before) = trait_default_contract(before) else {
        return false;
    };
    let Some(after) = trait_default_contract(after) else {
        return false;
    };
    if before.skeleton != after.skeleton || before.slots.len() != after.slots.len() {
        return false;
    }
    let mut added_default = false;
    for (before_slot, after_slot) in before.slots.iter().zip(&after.slots) {
        match (before_slot, after_slot) {
            (
                TraitDefaultSlot::Method {
                    has_default: before,
                },
                TraitDefaultSlot::Method { has_default: after },
            ) => match (*before, *after) {
                (false, true) => added_default = true,
                (left, right) if left == right => {}
                _ => return false,
            },
            (
                TraitDefaultSlot::Const { default: before },
                TraitDefaultSlot::Const { default: after },
            ) => match (before, after) {
                (None, Some(_)) => added_default = true,
                (left, right) if left == right => {}
                _ => return false,
            },
            _ => return false,
        }
    }
    added_default
}

fn trait_default_methods(contract: &str) -> Option<BTreeSet<String>> {
    let contract = contract
        .split_once("\nreexport-origin:")
        .map_or(contract, |(item, _)| item);
    let syn::Item::Trait(item) = syn::parse_str::<syn::Item>(contract).ok()? else {
        return None;
    };
    Some(
        item.items
            .iter()
            .filter_map(|trait_item| {
                let syn::TraitItem::Fn(function) = trait_item else {
                    return None;
                };
                function
                    .attrs
                    .iter()
                    .any(|attribute| attribute.path().is_ident("prview_trait_default"))
                    .then(|| {
                        format!(
                            "{}\u{1f}{}",
                            function.sig.ident,
                            trait_member_cfg_key(&function.attrs)
                        )
                    })
            })
            .collect(),
    )
}

fn variant_is_fieldless(contract: &str) -> bool {
    syn::parse_str::<syn::ItemEnum>(&format!("enum __Prview {{ {contract} }}"))
        .ok()
        .and_then(|item| item.variants.into_iter().next())
        .is_some_and(|variant| matches!(variant.fields, syn::Fields::Unit))
}

struct PublicEnumContract {
    variants: BTreeMap<String, String>,
    variant_order: Vec<String>,
    non_exhaustive: bool,
    layout_sensitive: bool,
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
    let layout_sensitive = item.attrs.iter().any(attr_is_layout_sensitive_repr);
    let variant_order = item
        .variants
        .iter()
        .map(|variant| variant.ident.to_string())
        .collect();
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
        variant_order,
        non_exhaustive,
        layout_sensitive,
        header: quote::ToTokens::to_token_stream(&header).to_string(),
    })
}

fn variant_side(parent: &ApiFactSide, name: &str, contract: &str) -> ApiFactSide {
    field_side(parent, name, contract)
}

struct PublicStructContract {
    fields: BTreeMap<String, String>,
    non_exhaustive: bool,
    /// The complete parent contract after removing only externally public
    /// fields. It retains attrs, generics and normalized private-field
    /// semantics, so a field addition cannot mask an unrelated parent change.
    parent_contract: String,
}

fn public_struct_contract(contract: &str) -> Option<PublicStructContract> {
    let syn::Item::Struct(item) = syn::parse_str::<syn::Item>(contract).ok()? else {
        return None;
    };
    let non_exhaustive = item
        .attrs
        .iter()
        .any(|attribute| attribute.path().is_ident("non_exhaustive"));
    let syn::Fields::Named(fields) = &item.fields else {
        return None;
    };
    let public_fields = fields
        .named
        .iter()
        .filter(|field| matches!(field.vis, syn::Visibility::Public(_)))
        .filter_map(|field| {
            Some((
                field.ident.as_ref()?.to_string(),
                quote::ToTokens::to_token_stream(field).to_string(),
            ))
        })
        .collect();

    let mut parent = item.clone();
    let syn::Fields::Named(parent_fields) = &mut parent.fields else {
        unreachable!("named struct clone remains named");
    };
    let mut private_index = 0usize;
    parent_fields.named = parent_fields
        .named
        .clone()
        .into_iter()
        .filter_map(|mut field| {
            if matches!(field.vis, syn::Visibility::Public(_)) {
                return None;
            }
            if let Some(ident) = &mut field.ident {
                *ident = syn::Ident::new(
                    &format!("__prview_parent_private_field_{private_index}"),
                    ident.span(),
                );
            }
            private_index += 1;
            Some(field)
        })
        .collect();
    Some(PublicStructContract {
        fields: public_fields,
        non_exhaustive,
        parent_contract: quote::ToTokens::to_token_stream(&parent).to_string(),
    })
}

fn attr_is_layout_sensitive_repr(attribute: &syn::Attribute) -> bool {
    fn meta_contains_layout_sensitive_repr(meta: &syn::Meta) -> bool {
        if meta.path().is_ident("repr") {
            let syn::Meta::List(list) = meta else {
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
        let syn::Meta::List(list) = meta else {
            return false;
        };
        let Ok(parts) = list.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        ) else {
            return false;
        };
        parts
            .iter()
            .skip(1)
            .any(meta_contains_layout_sensitive_repr)
    }

    meta_contains_layout_sensitive_repr(&attribute.meta)
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
        !(matches!(
            unknown.kind,
            RustApiUnknownKind::PathNonUtf8
                | RustApiUnknownKind::TraitImplResolution
                | RustApiUnknownKind::PrivateTypeDependency
                | RustApiUnknownKind::OpaqueReturnAutoTraits
        ) || unknown.kind == RustApiUnknownKind::MacroGeneratedItems
            && unknown
                .evidence
                .lines()
                .any(|line| line == "transform-kind:additive-derive"))
            && unknown
                .crate_name
                .as_ref()
                .is_none_or(|crate_name| crate_name == &identity.crate_name)
            && (unknown.module_path.is_empty()
                || identity.module_path.starts_with(&unknown.module_path)
                || unknown.module_path.starts_with(&identity.module_path))
            && guards_may_overlap(&unknown.cfg_guard, &identity.cfg_region)
            && transform_scope_may_cover(unknown, identity)
    })
}

fn transform_scope_may_cover(unknown: &RustApiUnknown, identity: &ApiIdentity) -> bool {
    let owner = unknown.evidence.lines().find_map(|line| {
        line.strip_prefix("public-owner:")
            .or_else(|| line.strip_prefix("public-type-alias:"))
    });
    if let Some(owner) = owner {
        return identity.name == owner
            || identity
                .name
                .strip_prefix(owner)
                .is_some_and(|suffix| suffix.starts_with("::"));
    }
    if let Some(owner) = unknown
        .evidence
        .lines()
        .find_map(|line| line.strip_prefix("trait-owner:"))
    {
        return identity.name == owner;
    }
    true
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
        } else if unmatched_unknown_is_observable(base, target, unknown) {
            findings.push(snapshot_unknown_finding(unknown, ApiSnapshotSide::Base));
        }
    }

    findings.extend(
        target
            .unknowns
            .iter()
            .enumerate()
            .filter(|(index, unknown)| {
                !target_used[*index] && unmatched_unknown_is_observable(base, target, unknown)
            })
            .map(|(_, unknown)| snapshot_unknown_finding(unknown, ApiSnapshotSide::Target)),
    );
    findings
}

fn unmatched_unknown_is_observable(
    base: &RustApiSnapshot,
    target: &RustApiSnapshot,
    unknown: &RustApiUnknown,
) -> bool {
    unknown.kind != RustApiUnknownKind::OpaqueReturnAutoTraits
        || (opaque_origin_exists(base, unknown)
            && opaque_origin_exists(target, unknown)
            && !trait_default_only_transition(base, target, unknown))
}

fn opaque_origin_exists(snapshot: &RustApiSnapshot, unknown: &RustApiUnknown) -> bool {
    opaque_origin_item(snapshot, unknown).is_some()
}

fn opaque_origin_item<'a>(
    snapshot: &'a RustApiSnapshot,
    unknown: &RustApiUnknown,
) -> Option<&'a RustApiItem> {
    let origin = unknown
        .evidence
        .lines()
        .find_map(|line| line.strip_prefix("origin:"))?;
    let (namespace, external_path) = origin.split_once(':')?;
    snapshot.items.iter().find(|item| {
        let item_path = if item.key.module_path.is_empty() {
            item.key.external_name.clone()
        } else {
            format!(
                "{}::{}",
                item.key.module_path.join("::"),
                item.key.external_name
            )
        };
        item.key.crate_name == unknown.crate_name.as_deref().unwrap_or_default()
            && format!("{:?}", item.key.namespace) == namespace
            && item_path == external_path
    })
}

fn trait_default_only_transition(
    base: &RustApiSnapshot,
    target: &RustApiSnapshot,
    unknown: &RustApiUnknown,
) -> bool {
    let Some(method) = unknown
        .evidence
        .lines()
        .find_map(|line| line.strip_prefix("opaque-return:trait-default:"))
    else {
        return false;
    };
    let Some(member_cfg) = unknown
        .evidence
        .lines()
        .find_map(|line| line.strip_prefix("trait-member-cfg:"))
    else {
        return false;
    };
    let method_key = format!("{method}\u{1f}{member_cfg}");
    let (Some(before), Some(after)) = (
        opaque_origin_item(base, unknown),
        opaque_origin_item(target, unknown),
    ) else {
        return false;
    };
    let Some(before_defaults) = trait_default_methods(&before.contract) else {
        return false;
    };
    let Some(after_defaults) = trait_default_methods(&after.contract) else {
        return false;
    };
    before_defaults.contains(&method_key) != after_defaults.contains(&method_key)
        && (trait_default_addition_is_compatible(&before.contract, &after.contract)
            || trait_default_addition_is_compatible(&after.contract, &before.contract))
}

fn unknown_proofs_match(
    base: &RustApiSnapshot,
    left: &RustApiUnknown,
    target: &RustApiSnapshot,
    right: &RustApiUnknown,
) -> bool {
    if left.evidence.contains(NON_NEUTRALIZABLE_SYMLINK_ROOT)
        || right.evidence.contains(NON_NEUTRALIZABLE_SYMLINK_ROOT)
    {
        return false;
    }
    if left.kind == RustApiUnknownKind::CfgPredicate
        && (left.evidence.contains("cfg-authority-digest:unresolved:")
            || right.evidence.contains("cfg-authority-digest:unresolved:"))
    {
        return false;
    }
    !matches!(
        left.kind,
        RustApiUnknownKind::PathNonUtf8 | RustApiUnknownKind::WorkspaceDiscovery
    )
        // A finite resolver stopped before it could inspect the complete
        // semantic substrate. Equality of the partial graph proof therefore
        // cannot prove equality of declarations or impls beyond the frontier.
        // Keep both sides visible instead of manufacturing a green delta.
        && !alias_resolution_exhausted(left)
        && !alias_resolution_exhausted(right)
        && left.kind == right.kind
        && left.crate_name == right.crate_name
        && left.module_path == right.module_path
        // String/byte include proofs bind their complete output by digest, so
        // their private declaration file is provenance rather than identity.
        // Plain include! remains path-sensitive: identical source bytes may
        // expand differently after a move through file!() or nested includes.
        && (left.source_path == right.source_path
            || (include_output_is_digest_bound(left) && include_output_is_digest_bound(right)))
        && left.cfg_guard == right.cfg_guard
        && left.evidence == right.evidence
        && macro_implementation_is_proven(left)
        && opaque_implementation_is_proven(left)
        // Revision ids necessarily differ across the comparison. What must not
        // differ is the provenance class, and each proof must still belong to
        // the snapshot that supplied it; an overlay is not silently equated to
        // a Git tree and a detached proof is never neutralized.
        && left.provenance == base.provenance
        && right.provenance == target.provenance
        && same_provenance_class(&left.provenance, &right.provenance)
        && include_dependent_source_is_proven(left)
}

fn opaque_implementation_is_proven(unknown: &RustApiUnknown) -> bool {
    !unknown
        .evidence
        .lines()
        .any(|line| line.starts_with("opaque-return:"))
        || unknown
            .evidence
            .lines()
            .any(|line| line.starts_with("opaque-implementation-digest:sha256:"))
}

fn macro_implementation_is_proven(unknown: &RustApiUnknown) -> bool {
    let boundaries = unknown
        .evidence
        .lines()
        .filter_map(|line| {
            line.strip_prefix("transform-boundary:")
                .or_else(|| line.strip_prefix("transform:transform-boundary:"))
        })
        .collect::<BTreeSet<_>>();
    let proc_macro_proven = unknown
        .evidence
        .lines()
        .any(|line| line.starts_with("macro-implementation-digest:sha256:"));
    let declarative_proven = unknown
        .evidence
        .lines()
        .any(|line| line.starts_with("declarative-implementation-digest:sha256:"));
    if boundaries.is_empty() {
        return unknown.kind != RustApiUnknownKind::MacroGeneratedItems || proc_macro_proven;
    }
    (!boundaries.contains("attribute") || proc_macro_proven)
        && (!boundaries.contains("declarative-macro") || declarative_proven)
        && (!boundaries.contains("macro-invocation") || declarative_proven)
}

fn alias_resolution_exhausted(unknown: &RustApiUnknown) -> bool {
    matches!(
        unknown.kind,
        RustApiUnknownKind::TraitImplResolution | RustApiUnknownKind::PrivateTypeDependency
    ) && unknown.resolution_exhausted
}

fn include_dependent_source_is_proven(unknown: &RustApiUnknown) -> bool {
    let evidence_is_resolved = !unknown
        .evidence
        .lines()
        .any(|line| line == "included-digest:unresolved");
    let include_kinds: Vec<_> = unknown
        .evidence
        .lines()
        .filter_map(|line| line.strip_prefix("include-kind:"))
        .collect();
    let carries_include_digest = unknown
        .evidence
        .lines()
        .any(|line| line.starts_with("included-digest:"));
    evidence_is_resolved
        && ((!carries_include_digest && include_kinds.is_empty())
            || include_kinds
                .iter()
                .all(|kind| matches!(*kind, "include_str" | "include_bytes")))
}

fn include_output_is_digest_bound(unknown: &RustApiUnknown) -> bool {
    unknown.kind == RustApiUnknownKind::IncludeMacro
        && unknown.evidence.lines().any(|line| {
            matches!(
                line,
                "include-kind:include_str" | "include-kind:include_bytes"
            )
        })
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
    use sha2::Digest;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

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
                .iter()
                .map(|(path, bytes)| RevisionEntry {
                    path: path.clone(),
                    baseline_object_id: Some(hex::encode(sha2::Sha256::digest(bytes))),
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

        let tree_records = git_with_input(
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
        let mut records = tree_records
            .split_inclusive(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        let mut printable_collision = format!("100644 blob {blob}\t").into_bytes();
        printable_collision.extend_from_slice(b"<git-path-bytes:ff>\0");
        records.push(printable_collision);
        let mut non_utf8 = format!("100644 blob {blob}\t").into_bytes();
        non_utf8.extend_from_slice(b"\xff\0");
        records.push(non_utf8);
        fn record_name(record: &[u8]) -> &[u8] {
            let start = record
                .iter()
                .position(|byte| *byte == b'\t')
                .map_or(record.len(), |index| index + 1);
            let path = &record[start..];
            path.strip_suffix(&[0]).unwrap_or(path)
        }
        records.sort_by(|left, right| record_name(left).cmp(record_name(right)));
        let tree_records = records.into_iter().flatten().collect::<Vec<_>>();
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
        assert!(
            non_exhaustive
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Options"),
            "a new field can change compiler-derived auto traits even when construction is non-exhaustive"
        );
        assert!(
            !non_exhaustive
                .added
                .iter()
                .any(|finding| finding.identity.name == "b"),
            "the auto-trait risk must not survive only as an informational field"
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
    fn non_exhaustive_field_addition_cannot_mask_parent_contract_changes() {
        for (label, base, target) in [
            (
                "repr transition",
                "#[non_exhaustive] pub struct Options<T> { pub a: T }",
                "#[repr(C)] #[non_exhaustive] pub struct Options<T> { pub a: T, pub b: T }",
            ),
            (
                "generic bound",
                "#[non_exhaustive] pub struct Options<T> { pub a: T }",
                "#[non_exhaustive] pub struct Options<T: Clone> { pub a: T, pub b: T }",
            ),
            (
                "private auto-trait input",
                "#[non_exhaustive] pub struct Options { pub a: u8, hidden: u8 }",
                "#[non_exhaustive] pub struct Options { pub a: u8, pub b: u8, hidden: std::rc::Rc<()> }",
            ),
        ] {
            let delta = compare_rust_api(
                &snapshot_rust_api(&MemorySource::source(base, "base")),
                &snapshot_rust_api(&MemorySource::source(target, "target")),
            );
            assert!(
                delta.changed.iter().any(|finding| {
                    finding.identity.name == "Options" && finding.identity.module_path.is_empty()
                }),
                "{label} must retain a parent Changed finding: {:?}",
                delta.findings()
            );
        }
    }

    #[test]
    fn legal_public_private_marker_name_remains_a_public_field() {
        let delta = compare_rust_api(
            &snapshot_rust_api(&MemorySource::source(
                "#[non_exhaustive] pub struct Options { pub a: u8, hidden: bool }",
                "base",
            )),
            &snapshot_rust_api(&MemorySource::source(
                "#[non_exhaustive] pub struct Options { pub a: u8, pub __prview_private_field_2: u8, hidden: bool }",
                "target",
            )),
        );
        assert!(delta.changed.iter().any(|finding| {
            finding.identity.name == "Options" && finding.identity.module_path.is_empty()
        }));
        assert!(
            !delta
                .added
                .iter()
                .any(|finding| finding.identity.name == "__prview_private_field_2")
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
        assert!(delta.changed.iter().any(|finding| {
            finding.identity.name == "Flexible" && finding.identity.namespace == "type"
        }));
        assert!(!delta.added.iter().any(|finding| {
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

        let payload_delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[non_exhaustive] pub enum Flexible { A }\n",
                "#[non_exhaustive] pub enum Flexible { A, B(std::rc::Rc<()>) }\n",
            ),
        ]);
        assert!(payload_delta.changed.iter().any(|finding| {
            finding.identity.name == "Flexible" && finding.identity.namespace == "type"
        }));
        assert!(!payload_delta.added.iter().any(|finding| {
            finding.identity.name == "B" && finding.identity.module_path == ["Flexible".to_owned()]
        }));

        let inserted_unit_delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[non_exhaustive] pub enum Flexible { A, B }\n",
                "#[non_exhaustive] pub enum Flexible { A, Inserted, B }\n",
            ),
        ]);
        assert!(inserted_unit_delta.changed.iter().any(|finding| {
            finding.identity.name == "Flexible" && finding.identity.namespace == "type"
        }));
        assert!(!inserted_unit_delta.added.iter().any(|finding| {
            finding.identity.name == "Inserted"
                && finding.identity.module_path == ["Flexible".to_owned()]
        }));

        let repr_delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(C)] #[non_exhaustive] pub enum Abi { A(u8) }\n",
                "#[repr(C)] #[non_exhaustive] pub enum Abi { A(u8), B([u8; 128]) }\n",
            ),
        ]);
        assert!(repr_delta.changed.iter().any(|finding| {
            finding.identity.name == "Abi" && finding.identity.namespace == "type"
        }));
        assert!(
            !repr_delta.added.iter().any(|finding| {
                finding.identity.name == "B" && finding.identity.module_path == ["Abi".to_owned()]
            }),
            "ABI-sensitive enums stay on the parent Changed path"
        );

        let primitive_repr_delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(u8)] #[non_exhaustive] pub enum Abi { A(u8) }\n",
                "#[repr(u8)] #[non_exhaustive] pub enum Abi { A(u8), B(u16) }\n",
            ),
        ]);
        assert!(primitive_repr_delta.changed.iter().any(|finding| {
            finding.identity.name == "Abi" && finding.identity.namespace == "type"
        }));
        assert!(
            !primitive_repr_delta.added.iter().any(|finding| {
                finding.identity.name == "B" && finding.identity.module_path == ["Abi".to_owned()]
            }),
            "primitive integer repr enums stay on the parent Changed path"
        );

        for repr in ["C", "u8"] {
            let before = format!(
                "#[cfg_attr(feature = \"ffi\", repr({repr}))] #[non_exhaustive] pub enum ConditionalAbi {{ A(u8) }}\n"
            );
            let after = format!(
                "#[cfg_attr(feature = \"ffi\", repr({repr}))] #[non_exhaustive] pub enum ConditionalAbi {{ A(u8), B([u8; 128]) }}\n"
            );
            let conditional_repr_delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", &before, &after),
            ]);
            assert!(conditional_repr_delta.changed.iter().any(|finding| {
                finding.identity.name == "ConditionalAbi" && finding.identity.namespace == "type"
            }));
            assert!(
                !conditional_repr_delta.added.iter().any(|finding| {
                    finding.identity.name == "B"
                        && finding.identity.module_path == ["ConditionalAbi".to_owned()]
                }),
                "cfg_attr repr({repr}) must keep the ABI-sensitive enum on the parent Changed path"
            );
        }

        let repr_rust_delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(Rust)] #[non_exhaustive] pub enum Flexible { A }\n",
                "#[repr(Rust)] #[non_exhaustive] pub enum Flexible { A, B }\n",
            ),
        ]);
        assert!(repr_rust_delta.added.iter().any(|finding| {
            finding.identity.name == "B" && finding.identity.module_path == ["Flexible".to_owned()]
        }));
        assert!(!repr_rust_delta.changed.iter().any(|finding| {
            finding.identity.name == "Flexible" && finding.identity.namespace == "type"
        }));

        let conditional_repr_rust_delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[cfg_attr(feature = \"ffi\", repr(Rust))] #[non_exhaustive] pub enum Flexible { A }\n",
                "#[cfg_attr(feature = \"ffi\", repr(Rust))] #[non_exhaustive] pub enum Flexible { A, B }\n",
            ),
        ]);
        assert!(conditional_repr_rust_delta.added.iter().any(|finding| {
            finding.identity.name == "B" && finding.identity.module_path == ["Flexible".to_owned()]
        }));
        assert!(!conditional_repr_rust_delta.changed.iter().any(|finding| {
            finding.identity.name == "Flexible" && finding.identity.namespace == "type"
        }));
    }

    #[test]
    fn variant_level_non_exhaustive_named_field_addition_preserves_auto_trait_risk() {
        let changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub enum Message { #[non_exhaustive] Data { value: u8 } }\n",
                "pub enum Message { #[non_exhaustive] Data { value: u8, extra: std::rc::Rc<()> } }\n",
            ),
        ]);
        assert!(changed.changed.iter().any(|finding| {
            finding.identity.name == "Message" && finding.identity.namespace == "type"
        }));
        assert!(
            !changed
                .added
                .iter()
                .any(|finding| finding.identity.name == "extra")
        );

        for (before, after) in [
            (
                "pub enum Message { Data { value: u8 } }\n",
                "pub enum Message { Data { value: u8, extra: u16 } }\n",
            ),
            (
                "pub enum Message { #[non_exhaustive] Data { value: u8 } }\n",
                "pub enum Message { #[non_exhaustive] Data { value: u16 } }\n",
            ),
        ] {
            let breaking = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", before, after),
            ]);
            assert!(breaking.changed.iter().any(|finding| {
                finding.identity.name == "Message" && finding.identity.namespace == "type"
            }));
        }
    }

    #[test]
    fn git_object_t5_non_utf8_entry_preserves_valid_api_siblings() {
        let (_tmp, repo, base, target) = repository_with_unchanged_non_utf8_entry();
        let tree = super::super::revision_source::GitTree::new(&repo, &base)
            .expect("exact tree with colliding printable names");
        let collision_entries = tree
            .entries()
            .into_iter()
            .filter(|entry| crate::git::display_git_path(&entry.path) == "<git-path-bytes:ff>")
            .collect::<Vec<_>>();
        assert_eq!(
            collision_entries.len(),
            2,
            "raw ff and a legal literal surrogate name must keep distinct inventory identities"
        );
        assert_eq!(
            collision_entries
                .iter()
                .filter(|entry| entry.path.starts_with('\0'))
                .count(),
            1,
            "only the raw-byte entry uses the impossible Git-path key"
        );
        assert!(matches!(
            tree.read("<git-path-bytes:ff>")
                .expect("literal UTF-8 path remains readable"),
            RevisionRead::Bytes(_)
        ));

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
    fn ambiguous_rootless_workspace_authority_remains_typed_unknown() {
        let delta = repository_delta(&[
            (
                "backend/Cargo.toml",
                "[workspace]\nmembers=['api']\n",
                "[workspace]\nmembers=['api']\n",
            ),
            (
                "backend/api/Cargo.toml",
                "[package]\nname='backend-api'\nversion='0.0.0'\n",
                "[package]\nname='backend-api'\nversion='0.0.0'\n",
            ),
            (
                "backend/api/src/lib.rs",
                "pub fn backend() {}\n",
                "pub fn backend() {}\n",
            ),
            (
                "tests/fixtures/Cargo.toml",
                "[workspace]\nmembers=['sample']\n",
                "[workspace]\nmembers=['sample']\n",
            ),
            (
                "tests/fixtures/sample/Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n",
            ),
            (
                "tests/fixtures/sample/src/lib.rs",
                "pub fn fixture() {}\n",
                "pub fn fixture() {}\n",
            ),
        ]);
        assert_eq!(
            delta
                .unknown
                .iter()
                .filter(|finding| finding.identity.name == "WorkspaceDiscovery")
                .count(),
            2,
            "workspace authority uncertainty is side-specific and never neutralized"
        );
        assert!(delta.added.is_empty() && delta.removed.is_empty() && delta.changed.is_empty());
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

        let conditional_repr_layout = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[cfg_attr(feature = \"ffi\", repr(C))] pub struct Layout { pub tag: u8, hidden: u8 }\n",
                "#[cfg_attr(feature = \"ffi\", repr(C))] pub struct Layout { pub tag: u8, hidden: u16 }\n",
            ),
        ]);
        assert!(
            conditional_repr_layout
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Layout"),
            "conditional repr(C) must preserve private ABI layout in the contract"
        );
    }

    #[test]
    fn repository_backed_member_order_follows_struct_union_and_enum_layout_rules() {
        let union_reordered = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(C)] pub union Value { pub small: u8, pub large: u32 }\n",
                "#[repr(C)] pub union Value { pub large: u32, pub small: u8 }\n",
            ),
        ]);
        assert!(
            union_reordered.findings().is_empty(),
            "repr(C) union members all start at offset zero, so order is neutral: {:?}",
            union_reordered.findings()
        );

        let union_type_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(C)] pub union Value { hidden: u8, pub tag: u8 }\n",
                "#[repr(C)] pub union Value { hidden: u64, pub tag: u8 }\n",
            ),
        ]);
        assert!(
            union_type_changed
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Value"),
            "union member types still determine size, alignment and auto traits"
        );

        let rust_enum_reordered = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub enum Event { Value { small: u8, large: u32 } }\n",
                "pub enum Event { Value { large: u32, small: u8 } }\n",
            ),
        ]);
        assert!(
            rust_enum_reordered.findings().is_empty(),
            "named repr(Rust) variant fields are addressed by name, not order"
        );

        let c_enum_reordered = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[repr(C)] pub enum Event { Value { small: u8, large: u32 } }\n",
                "#[repr(C)] pub enum Event { Value { large: u32, small: u8 } }\n",
            ),
        ]);
        assert!(
            c_enum_reordered
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Event"),
            "repr(C) enum payload layout remains declaration-order-sensitive"
        );
    }

    #[test]
    fn repository_backed_repr_field_order_tracks_only_layouts_that_define_it() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        for attribute in [
            "#[repr(transparent)]",
            "#[repr(align(8))]",
            "#[repr(packed)]",
            "#[cfg_attr(feature = \"ffi\", repr(transparent))]",
        ] {
            let base = format!(
                "struct First; struct Second; {attribute} pub struct Wrapper {{ pub value: u8, first: First, second: Second }}\n"
            );
            let target = format!(
                "struct First; struct Second; {attribute} pub struct Wrapper {{ pub value: u8, second: Second, first: First }}\n"
            );
            let delta = repository_delta(&[
                ("Cargo.toml", manifest, manifest),
                ("src/lib.rs", &base, &target),
            ]);
            assert!(
                delta.findings().is_empty(),
                "{attribute} does not define named-field declaration order: {:?}",
                delta.findings()
            );
        }

        let transparent_marker_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "struct First; struct Second; #[repr(transparent)] pub struct Wrapper { pub value: u8, marker: First }\n",
                "struct First; struct Second; #[repr(transparent)] pub struct Wrapper { pub value: u8, marker: Second }\n",
            ),
        ]);
        assert!(
            transparent_marker_changed
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Wrapper"),
            "private marker types still affect transparent layout and auto traits"
        );

        for attribute in [
            "#[repr(C)]",
            "#[repr(C, packed)]",
            "#[repr(C, align(8))]",
            "#[cfg_attr(feature = \"ffi\", repr(C))]",
        ] {
            let base = format!("{attribute} pub struct Layout {{ first: u8, second: u32 }}\n");
            let target = format!("{attribute} pub struct Layout {{ second: u32, first: u8 }}\n");
            let delta = repository_delta(&[
                ("Cargo.toml", manifest, manifest),
                ("src/lib.rs", &base, &target),
            ]);
            assert!(
                delta
                    .changed
                    .iter()
                    .any(|finding| finding.identity.name == "Layout"),
                "{attribute} defines field order: {:?}",
                delta.findings()
            );
        }

        let transparent_enum = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "struct First; struct Second; #[repr(transparent)] pub enum Wrapper { Value { carrier: u8, first: First, second: Second } }\n",
                "struct First; struct Second; #[repr(transparent)] pub enum Wrapper { Value { carrier: u8, second: Second, first: First } }\n",
            ),
        ]);
        assert!(
            transparent_enum.findings().is_empty(),
            "a transparent single-variant enum follows its carrier rather than private ZST declaration order: {:?}",
            transparent_enum.findings()
        );

        let primitive_enum = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[repr(u8)] pub enum Event { Value { first: u8, second: u32 } }\n",
                "#[repr(u8)] pub enum Event { Value { second: u32, first: u8 } }\n",
            ),
        ]);
        assert!(
            primitive_enum
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Event"),
            "primitive enum repr keeps payload field order ABI-sensitive"
        );
    }

    #[test]
    fn alpha_normalization_never_collides_with_public_identifiers() {
        let swapped = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct __PrviewT0_0_0; pub fn f<T>(x: T, y: __PrviewT0_0_0) {}\n",
                "pub struct __PrviewT0_0_0; pub fn f<T>(x: __PrviewT0_0_0, y: T) {}\n",
            ),
        ]);
        assert!(
            swapped
                .changed
                .iter()
                .any(|finding| finding.identity.name == "f"),
            "a real public type must not collapse into a synthetic binder: {:?}",
            swapped.findings()
        );

        let raw_swapped = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct r#__PrviewT0_0_0; pub fn f<T>(x: T, y: r#__PrviewT0_0_0) {}\n",
                "pub struct r#__PrviewT0_0_0; pub fn f<T>(x: r#__PrviewT0_0_0, y: T) {}\n",
            ),
        ]);
        assert!(
            raw_swapped
                .changed
                .iter()
                .any(|finding| finding.identity.name == "f"),
            "a raw public identifier must reserve the same semantic name as its canonical spelling: {:?}",
            raw_swapped.findings()
        );

        for (base, target) in [
            (
                "pub fn f<T>(__PrviewT0_0_0: T) {}\n",
                "pub fn f<U>(other: U) {}\n",
            ),
            (
                "pub fn f<__PrviewT0_0_0>(x: __PrviewT0_0_0) {}\n",
                "pub fn f<T>(x: T) {}\n",
            ),
            (
                "pub struct __PrviewT0_0_0; pub fn f<T>(x: T, y: __PrviewT0_0_0) {}\n",
                "pub struct __PrviewT0_0_0; pub fn f<U>(x: U, y: __PrviewT0_0_0) {}\n",
            ),
            (
                "pub fn f<T>(callback: for<'a> fn(&'a T) -> &'a T) where T: for<'b> Fn(&'b u8) + for<'c> Fn(&'c u16) {}\n",
                "pub fn f<U>(callback: for<'value> fn(&'value U) -> &'value U) where U: for<'right> Fn(&'right u16) + for<'left> Fn(&'left u8) {}\n",
            ),
            (
                "pub const __PRVIEW_C0_0_0: usize = 1; pub fn f<const N: usize>(value: [u8; const { __PRVIEW_C0_0_0 + N }]) {}\n",
                "pub const __PRVIEW_C0_0_0: usize = 1; pub fn f<const M: usize>(value: [u8; const { __PRVIEW_C0_0_0 + M }]) {}\n",
            ),
            (
                "pub struct Api<T>(T); impl<T> Api<T> { pub const N: usize = core::mem::size_of::<T>(); }\n",
                "pub struct Api<U>(U); impl<U> Api<U> { pub const N: usize = core::mem::size_of::<U>(); }\n",
            ),
            (
                "pub trait Trait<T> { type Out<U>; } pub struct Api; impl<T> Trait<T> for Api { type Out<U> = (T, U); }\n",
                "pub trait Trait<W> { type Out<X>; } pub struct Api; impl<W> Trait<W> for Api { type Out<X> = (W, X); }\n",
            ),
        ] {
            let renamed = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", base, target),
            ]);
            assert!(
                renamed.findings().is_empty(),
                "fresh synthetic names must preserve alpha equivalence: {:?}",
                renamed.findings()
            );
        }

        let sibling_change = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Api<T>(T); pub struct Left; pub struct Right; impl<T> Api<T> { pub fn keep(value: T) {} pub fn sibling(value: Left) {} }\n",
                "pub struct Api<U>(U); pub struct Left; pub struct Right; impl<U> Api<U> { pub fn keep(value: U) {} pub fn sibling(value: Right) {} }\n",
            ),
        ]);
        let sibling_findings = sibling_change.findings();
        assert!(
            sibling_findings
                .iter()
                .any(|finding| finding.identity.name.ends_with("::sibling")),
            "the sibling method change must remain observable: {sibling_findings:?}"
        );
        assert!(
            !sibling_findings
                .iter()
                .any(|finding| finding.identity.name.ends_with("::keep")),
            "an unrelated associated-item identifier must not perturb an existing member contract: {:?}",
            sibling_findings
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
            crate_type.unknown.iter().any(|finding| {
                finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("native-only library target") && reason.contains("cdylib")
                })
            }) && crate_type.unknown.iter().any(|finding| {
                finding.identity.name == "fixture" && finding.identity.namespace == "crate"
            }),
            "cdylib to rlib must retain the removed native target uncertainty and refuse to over-confirm the Rust dependency surface: {:?}",
            crate_type.findings()
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
            !unchanged.unknown.is_empty(),
            "plain include! stays review-required until its transitive expansion is proven"
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

        let expression_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub const N: usize = include!(\"n.rs\");\n",
                "pub const N: usize = include!(\"n.rs\");\n",
            ),
            ("src/n.rs", "1\n", "2\n"),
        ]);
        assert!(expression_changed.unknown.iter().any(|finding| {
            finding.identity.name == "IncludeMacro"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("included-digest"))
        }));

        let expression_unchanged = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub static BYTES: &[u8] = include_bytes!(\"data.bin\");\n",
                "pub static BYTES: &[u8] = include_bytes!(\"data.bin\");\n",
            ),
            ("src/data.bin", "same\n", "same\n"),
        ]);
        assert!(
            expression_unchanged.unknown.is_empty(),
            "unchanged terminal include_bytes! proof neutralizes"
        );

        let nested_dependency_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub type Item = include!(\"outer.rs\");\n",
                "pub type Item = include!(\"outer.rs\");\n",
            ),
            (
                "src/outer.rs",
                "include!(\"inner.rs\")\n",
                "include!(\"inner.rs\")\n",
            ),
            ("src/inner.rs", "u8\n", "u16\n"),
        ]);
        assert!(
            nested_dependency_changed.unknown.iter().any(|finding| {
                finding.identity.name == "IncludeMacro"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("include-kind:include"))
            }),
            "a direct digest cannot neutralize plain include! with a changed nested dependency"
        );

        let root_nested_dependency_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "include!(\"outer.rs\");\n",
                "include!(\"outer.rs\");\n",
            ),
            (
                "src/outer.rs",
                "include!(\"inner.rs\");\n",
                "include!(\"inner.rs\");\n",
            ),
            ("src/inner.rs", "pub fn old() {}\n", "pub fn changed() {}\n"),
        ]);
        assert!(
            !root_nested_dependency_changed.unknown.is_empty(),
            "a root item-position include! also stays active when only its nested source changes"
        );

        let trait_impl_nested_dependency_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { type Assoc = include!(\"outer.rs\"); }\n",
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { type Assoc = include!(\"outer.rs\"); }\n",
            ),
            (
                "src/outer.rs",
                "include!(\"inner.rs\")\n",
                "include!(\"inner.rs\")\n",
            ),
            ("src/inner.rs", "u8\n", "u16\n"),
        ]);
        assert!(
            trait_impl_nested_dependency_changed
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "TraitImplResolution"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("include-kind:include")))
        );
    }

    #[test]
    fn repository_backed_include_tracks_every_public_contract_position() {
        let public_contracts = [
            "pub type Bytes = [u8; include!(\"n.rs\")];\n",
            "pub struct Packet { pub bytes: [u8; include!(\"n.rs\")] }\n",
            "pub fn packet() -> [u8; include!(\"n.rs\")] { todo!() }\n",
            "#[repr(u8)] pub enum Tag { Value = include!(\"n.rs\") }\n",
            "pub trait Contract { const N: usize = include!(\"n.rs\"); }\n",
            "pub struct Owner; impl Owner { pub const N: usize = include!(\"n.rs\"); }\n",
            "pub type Included = include!(\"type.rs\");\n",
        ];

        for source in public_contracts {
            let delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", source, source),
                ("src/n.rs", "1\n", "2\n"),
                ("src/type.rs", "u8\n", "u16\n"),
            ]);
            assert!(
                delta.unknown.iter().any(|finding| {
                    finding.identity.name == "IncludeMacro"
                        && finding
                            .unknown_reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("included-digest"))
                }),
                "a changed include dependency in {source:?} must stay review-required: {:?}",
                delta.findings()
            );
        }

        let nested_item_contracts = [
            (
                "pub trait Contract { include!(\"nested.rs\"); }\n",
                "type Assoc;\n",
                "type Changed;\n",
            ),
            (
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { include!(\"nested.rs\"); }\n",
                "type Assoc = u8;\n",
                "type Assoc = u16;\n",
            ),
            (
                "pub struct Owner; impl Owner { include!(\"nested.rs\"); }\n",
                "pub fn build() -> u8 { 1 }\n",
                "pub fn build() -> u16 { 1 }\n",
            ),
            (
                "unsafe extern \"C\" { include!(\"nested.rs\"); }\n",
                "pub fn old();\n",
                "pub fn new();\n",
            ),
            (
                "std::include!(\"nested.rs\");\n",
                "pub fn old() {}\n",
                "pub fn new() {}\n",
            ),
        ];
        for (source, base_nested, target_nested) in nested_item_contracts {
            let delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", source, source),
                ("src/nested.rs", base_nested, target_nested),
            ]);
            assert!(
                delta.unknown.iter().any(|finding| {
                    finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("included-digest"))
                }),
                "nested item-position include must bind included bytes for {source:?}: {:?}",
                delta.findings()
            );
        }

        let nested_unchanged = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Contract { include!(\"nested.rs\"); }\n",
                "pub trait Contract { include!(\"nested.rs\"); }\n",
            ),
            ("src/nested.rs", "type Assoc;\n", "type Assoc;\n"),
        ]);
        assert!(
            !nested_unchanged.unknown.is_empty(),
            "plain include! item expansion remains conservative without a transitive proof"
        );

        let private_foreign_reexport = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod hidden { unsafe extern \"C\" { include!(\"ffi.rs\"); } } pub use hidden::old;\n",
                "mod hidden { unsafe extern \"C\" { include!(\"ffi.rs\"); } } pub use hidden::old;\n",
            ),
            ("src/ffi.rs", "pub fn old();\n", "pub fn renamed();\n"),
        ]);
        assert!(private_foreign_reexport.unknown.iter().any(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("included-digest"))
        }));

        let private_inherent_owner_renamed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod hidden { pub struct Old; impl Old { pub const N: &str = include_str!(\"n.txt\"); include!(\"assoc.rs\"); } } pub use hidden::Old as Public;\n",
                "mod hidden { pub struct New; impl New { pub const N: &str = include_str!(\"n.txt\"); include!(\"assoc.rs\"); } } pub use hidden::New as Public;\n",
            ),
            ("src/n.txt", "same\n", "same\n"),
            (
                "src/assoc.rs",
                "pub fn build() -> u8 { 1 }\n",
                "pub fn build() -> u8 { 1 }\n",
            ),
        ]);
        assert!(
            !private_inherent_owner_renamed.unknown.is_empty()
                && private_inherent_owner_renamed
                    .unknown
                    .iter()
                    .all(|finding| {
                        finding
                            .evidence
                            .iter()
                            .all(|evidence| !evidence.contains("Old") && !evidence.contains("New"))
                    }),
            "a conservative plain-include finding must bind the stable public alias, not leak a private owner rename: {:?}",
            private_inherent_owner_renamed.findings()
        );

        let trait_impl = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { type Assoc = include!(\"type.rs\"); }\n",
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { type Assoc = include!(\"type.rs\"); }\n",
            ),
            ("src/type.rs", "u8\n", "u16\n"),
        ]);
        assert!(trait_impl.unknown.iter().any(|finding| {
            finding.identity.name == "TraitImplResolution"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("included-digest"))
        }));

        let trait_impl_reordered = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Contract { const A: &'static str; const B: &'static str; } pub struct Owner; impl Contract for Owner { const A: &'static str = include_str!(\"a.txt\"); const B: &'static str = include_str!(\"b.txt\"); }\n",
                "pub trait Contract { const A: &'static str; const B: &'static str; } pub struct Owner; impl Contract for Owner { const B: &'static str = include_str!(\"b.txt\"); const A: &'static str = include_str!(\"a.txt\"); }\n",
            ),
            ("src/a.txt", "a\n", "a\n"),
            ("src/b.txt", "b\n", "b\n"),
        ]);
        assert!(
            trait_impl_reordered.findings().is_empty(),
            "ordinary trait-impl members and their include proofs form an unordered set"
        );

        let unresolved_trait_impl = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { type Assoc = include!(concat!(\"type\", \".rs\")); }\n",
                "pub trait Contract { type Assoc; } pub struct Owner; impl Contract for Owner { type Assoc = include!(concat!(\"type\", \".rs\")); }\n",
            ),
        ]);
        assert!(unresolved_trait_impl.unknown.iter().any(|finding| {
            finding.identity.name == "TraitImplResolution"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("included-digest:unresolved"))
        }));

        let implementation_only_change = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub fn packet() -> [u8; include!(\"n.rs\")] { [1] }\n",
                "pub fn packet() -> [u8; include!(\"n.rs\")] { [2] }\n",
            ),
            ("src/n.rs", "1\n", "1\n"),
        ]);
        assert!(
            implementation_only_change.unknown.len() == 2
                && implementation_only_change.unknown[0].evidence
                    == implementation_only_change.unknown[1].evidence,
            "plain include! remains conservative, but its proof evidence must still exclude function bodies"
        );

        let private_unreachable = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod hidden { pub type Bytes = [u8; include!(\"n.rs\")]; }\n",
                "mod hidden { pub type Bytes = [u8; include!(\"n.rs\")]; }\n",
            ),
            ("src/n.rs", "1\n", "2\n"),
        ]);
        assert!(
            private_unreachable.findings().is_empty(),
            "an include owned only by an unreachable item is not public API"
        );

        let reexported = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod hidden { pub type Bytes = [u8; include!(\"n.rs\")]; } pub use hidden::Bytes;\n",
                "mod hidden { pub type Bytes = [u8; include!(\"n.rs\")]; } pub use hidden::Bytes;\n",
            ),
            ("src/n.rs", "1\n", "2\n"),
        ]);
        assert!(
            reexported
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "IncludeMacro"),
            "a reexport makes the private-module declaration externally reachable"
        );

        let body_only = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub fn calculate() { let _: usize = include!(\"n.rs\"); }\n",
                "pub fn calculate() { let _: usize = include!(\"n.rs\"); }\n",
            ),
            ("src/n.rs", "1\n", "2\n"),
        ]);
        assert!(
            body_only.findings().is_empty(),
            "ordinary function bodies are implementation, not public contracts"
        );
    }

    #[test]
    fn include_proof_survives_private_donor_file_move_under_stable_alias() {
        let manifest = b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let donor =
            b"pub struct Owner; impl Owner { pub const N: &str = include_str!(\"n.txt\"); }\n";
        let base = snapshot_rust_api(&MemorySource {
            provenance: RevisionProvenance::GitTree {
                commit_oid: "base".to_owned(),
            },
            files: BTreeMap::from([
                ("Cargo.toml".to_owned(), manifest.to_vec()),
                (
                    "src/lib.rs".to_owned(),
                    b"#[path=\"old/mod.rs\"] mod hidden; pub use hidden::Owner as Public;\n"
                        .to_vec(),
                ),
                ("src/old/mod.rs".to_owned(), donor.to_vec()),
                ("src/old/n.txt".to_owned(), b"same\n".to_vec()),
            ]),
        });
        let target = snapshot_rust_api(&MemorySource {
            provenance: RevisionProvenance::GitTree {
                commit_oid: "target".to_owned(),
            },
            files: BTreeMap::from([
                ("Cargo.toml".to_owned(), manifest.to_vec()),
                (
                    "src/lib.rs".to_owned(),
                    b"#[path=\"new/mod.rs\"] mod hidden; pub use hidden::Owner as Public;\n"
                        .to_vec(),
                ),
                ("src/new/mod.rs".to_owned(), donor.to_vec()),
                ("src/new/n.txt".to_owned(), b"same\n".to_vec()),
            ]),
        });
        let delta = compare_rust_api(&base, &target);
        assert!(
            delta.findings().is_empty(),
            "a private donor file move with the same public alias, contract, and included bytes is not an API change: {:?}",
            delta.findings()
        );

        let path_sensitive_donor =
            b"pub struct Owner; impl Owner { pub const PATH: &str = include!(\"path.rs\"); }\n";
        let base = snapshot_rust_api(&MemorySource {
            provenance: RevisionProvenance::GitTree {
                commit_oid: "base".to_owned(),
            },
            files: BTreeMap::from([
                ("Cargo.toml".to_owned(), manifest.to_vec()),
                (
                    "src/lib.rs".to_owned(),
                    b"#[path=\"old/mod.rs\"] mod hidden; pub use hidden::Owner as Public;\n"
                        .to_vec(),
                ),
                ("src/old/mod.rs".to_owned(), path_sensitive_donor.to_vec()),
                ("src/old/path.rs".to_owned(), b"file!()\n".to_vec()),
            ]),
        });
        let target = snapshot_rust_api(&MemorySource {
            provenance: RevisionProvenance::GitTree {
                commit_oid: "target".to_owned(),
            },
            files: BTreeMap::from([
                ("Cargo.toml".to_owned(), manifest.to_vec()),
                (
                    "src/lib.rs".to_owned(),
                    b"#[path=\"new/mod.rs\"] mod hidden; pub use hidden::Owner as Public;\n"
                        .to_vec(),
                ),
                ("src/new/mod.rs".to_owned(), path_sensitive_donor.to_vec()),
                ("src/new/path.rs".to_owned(), b"file!()\n".to_vec()),
            ]),
        });
        assert!(
            !compare_rust_api(&base, &target).unknown.is_empty(),
            "plain include! remains path-sensitive because file!() and nested relative includes may change expansion"
        );
    }

    #[test]
    fn repository_backed_transforming_attribute_proof_binds_the_input_item() {
        let lock = "version = 4\n";
        let changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[derive(Custom)] pub struct Api { pub value: u8 }\n",
                "#[derive(Custom)] pub struct Api { pub value: u16 }\n",
            ),
            ("Cargo.lock", lock, lock),
        ]);
        assert!(changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("input:"))
        }));

        let nested_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", custom(before)))] pub struct Api;\n",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", custom(after)))] pub struct Api;\n",
            ),
            ("Cargo.lock", lock, lock),
        ]);
        assert!(nested_changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("custom (before)") || reason.contains("custom (after)")
                })
        }));

        let unchanged = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[derive(Custom)] pub struct Api { pub value: u8 }\n",
                "#[derive(Custom)] pub struct Api { pub value: u8 }\n",
            ),
            ("Cargo.lock", lock, lock),
        ]);
        assert!(
            unchanged.unknown.len() == 2,
            "without a reachable local proc-macro implementation, a lock alone cannot resolve an arbitrary transformer: {:?}",
            unchanged.findings()
        );
    }

    #[test]
    fn repository_backed_derives_preserve_input_contracts_and_builtin_test_attrs_are_inert() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let built_in_field_add = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[derive(Debug)] pub struct Api { pub x: u8 }\n",
                "#[derive(Debug)] pub struct Api { pub x: u8, pub y: u8 }\n",
            ),
        ]);
        assert!(
            built_in_field_add
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Api"),
            "a built-in derive must not hide the confirmed input-item change: {:?}",
            built_in_field_add.findings()
        );
        assert!(
            !built_in_field_add
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );

        let built_in_unchanged = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[derive(Debug, Clone)] pub struct Api { pub x: u8 }\n",
                "#[derive(Debug, Clone)] pub struct Api { pub x: u8 }\n",
            ),
        ]);
        assert!(built_in_unchanged.findings().is_empty());

        let unrelated_globs_do_not_shadow_builtins = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "mod private { use unrelated::*; } #[derive(Debug)] pub struct Api;\n",
                "mod private { use unrelated::*; } #[derive(Debug)] pub struct Api;\n",
            ),
            (
                "tests/fixture.rs",
                "use another_unrelated::*;\n",
                "use another_unrelated::*;\n",
            ),
        ]);
        assert!(
            unrelated_globs_do_not_shadow_builtins.findings().is_empty(),
            "an unrelated file or sibling lexical scope must not shadow a builtin derive: {:?}",
            unrelated_globs_do_not_shadow_builtins.findings()
        );

        let custom_manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n";
        let custom_lock = "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n[[package]]\nname='serde_derive'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='1111111111111111111111111111111111111111111111111111111111111111'\n";
        let custom_field_add = repository_delta(&[
            ("Cargo.toml", custom_manifest, custom_manifest),
            ("Cargo.lock", custom_lock, custom_lock),
            (
                "src/lib.rs",
                "#[derive(serde::Serialize)] pub struct Api { pub x: u8 }\n",
                "#[derive(serde::Serialize)] pub struct Api { pub x: u8, pub y: u8 }\n",
            ),
        ]);
        assert!(
            custom_field_add
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Api"),
            "a custom derive must not hide a confirmed input-item field change: {:?}",
            custom_field_add.findings()
        );
        assert!(
            custom_field_add
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );

        let custom_derive_add = repository_delta(&[
            ("Cargo.toml", custom_manifest, custom_manifest),
            ("Cargo.lock", custom_lock, custom_lock),
            (
                "src/lib.rs",
                "pub struct Api { pub x: u8 }\n",
                "#[derive(serde::Serialize)] pub struct Api { pub x: u8 }\n",
            ),
        ]);
        assert!(
            custom_derive_add.changed.is_empty(),
            "an additive custom derive is uncertain generated output, not a confirmed input-item change: {:?}",
            custom_derive_add.findings()
        );
        assert!(
            custom_derive_add
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );

        let nested_custom_derive = repository_delta(&[
            ("Cargo.toml", custom_manifest, custom_manifest),
            ("Cargo.lock", custom_lock, custom_lock),
            (
                "src/lib.rs",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", derive(serde::Serialize)))] pub struct Api { pub x: u8 }\n",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", derive(serde::Serialize)))] pub struct Api { pub x: u16 }\n",
            ),
        ]);
        assert!(nested_custom_derive.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("transform-kind:additive-derive"))
        }));

        let helper_only_change = repository_delta(&[
            ("Cargo.toml", custom_manifest, custom_manifest),
            ("Cargo.lock", custom_lock, custom_lock),
            (
                "src/lib.rs",
                "#[derive(serde::Serialize)] pub struct Api { #[serde(rename=\"left\")] pub x: u8 }\n",
                "#[derive(serde::Serialize)] pub struct Api { #[serde(rename=\"right\")] pub x: u8 }\n",
            ),
        ]);
        assert!(
            helper_only_change.changed.is_empty(),
            "derive helper tokens belong to transform uncertainty, not the confirmed input contract: {:?}",
            helper_only_change.findings()
        );
        assert!(
            helper_only_change
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );

        let shadowed_builtin = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api','macros']\nresolver='2'\n",
                "[workspace]\nmembers=['api','macros']\nresolver='2'\n",
            ),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n",
            ),
            (
                "api/src/lib.rs",
                "use macros::Debug; #[derive(Debug)] pub struct Api;\n",
                "use macros::Debug; #[derive(Debug)] pub struct Api;\n",
            ),
            (
                "macros/Cargo.toml",
                "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n",
                "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n",
            ),
            (
                "macros/src/lib.rs",
                "#[proc_macro_derive(Debug)] pub fn debug(_: proc_macro::TokenStream) -> proc_macro::TokenStream { proc_macro::TokenStream::new() }\n",
                "#[proc_macro_derive(Debug)] pub fn debug(_: proc_macro::TokenStream) -> proc_macro::TokenStream { \"impl Api { pub fn generated() {} }\".parse().unwrap() }\n",
            ),
        ]);
        assert!(
            shadowed_builtin.changed.is_empty(),
            "an imported derive named like a compiler builtin must not become a confirmed builtin contract: {:?}",
            shadowed_builtin.findings()
        );
        assert!(shadowed_builtin.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("macro-implementation-digest:sha256:"))
        }));

        let inert_test_attrs = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[cfg(test)] #[test] #[ignore] #[should_panic] fn smoke() {}\n",
                "#[cfg(test)] #[test] #[ignore] #[should_panic] fn smoke() {}\n",
            ),
        ]);
        assert!(
            inert_test_attrs.findings().is_empty(),
            "built-in test harness attributes are not arbitrary API transformers: {:?}",
            inert_test_attrs.findings()
        );
    }

    #[test]
    fn repository_backed_builtin_default_and_rust_2024_unsafe_attrs_remain_confirmed_contracts() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let builtin_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[derive(Default)] pub enum Choice { #[default] A, B }\n",
                "#[derive(Default)] pub enum Choice { A, #[default] B }\n",
            ),
        ]);
        assert!(builtin_default.changed.iter().any(|finding| {
            finding.identity.name == "Choice" && finding.confidence == ApiDeltaConfidence::Confirmed
        }));

        let conditional_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[cfg_attr(feature=\"x\", derive(Default))] pub enum Choice { #[cfg_attr(feature=\"x\", default)] A, B }\n",
                "#[cfg_attr(feature=\"x\", derive(Default))] pub enum Choice { A, #[cfg_attr(feature=\"x\", default)] B }\n",
            ),
        ]);
        assert!(conditional_default.changed.iter().any(|finding| {
            finding.identity.name == "Choice" && finding.confidence == ApiDeltaConfidence::Confirmed
        }));

        let nested_conditional_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", derive(Default)))] pub enum Choice { #[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", default))] A, B }\n",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", derive(Default)))] pub enum Choice { A, #[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", default))] B }\n",
            ),
        ]);
        assert!(nested_conditional_default.changed.iter().any(|finding| {
            finding.identity.name == "Choice" && finding.confidence == ApiDeltaConfidence::Confirmed
        }));

        let equivalent_conditional_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[cfg_attr(feature=\"x\", derive(Default))] pub enum Choice { #[cfg_attr(all(feature=\"x\"), default)] A, B }\n",
                "#[cfg_attr(feature=\"x\", derive(Default))] pub enum Choice { A, #[cfg_attr(all(feature=\"x\"), default)] B }\n",
            ),
        ]);
        assert!(
            equivalent_conditional_default
                .changed
                .iter()
                .any(|finding| {
                    finding.identity.name == "Choice"
                        && finding.confidence == ApiDeltaConfidence::Confirmed
                })
        );

        let unresolved_conditional_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[cfg_attr(feature=\"x\", derive(Default))] pub enum Choice { #[cfg_attr(feature=\"y\", default)] A, B }\n",
                "#[cfg_attr(feature=\"x\", derive(Default))] pub enum Choice { A, #[cfg_attr(feature=\"y\", default)] B }\n",
            ),
        ]);
        assert!(
            unresolved_conditional_default
                .unknown
                .iter()
                .any(|finding| {
                    finding.identity.name == "CfgPredicate"
                        && finding.unknown_reason.as_deref().is_some_and(|reason| {
                            reason.contains("conditional-default-coverage-unresolved")
                        })
                })
        );

        let unsafe_export = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[unsafe(export_name=\"before\")] pub extern \"C\" fn api() {}\n",
                "#[unsafe(export_name=\"after\")] pub extern \"C\" fn api() {}\n",
            ),
        ]);
        assert!(unsafe_export.changed.iter().any(|finding| {
            finding.identity.name == "api" && finding.confidence == ApiDeltaConfidence::Confirmed
        }));
        assert!(
            !unsafe_export
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );

        let private_binary_export = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[unsafe(no_mangle)] extern \"C\" fn binary_api(_: u8) {}\n",
                "#[unsafe(no_mangle)] extern \"C\" fn binary_api(_: u16) {}\n",
            ),
        ]);
        assert!(private_binary_export.unknown.iter().any(|finding| {
            finding.identity.name == "UnsupportedExternResolution"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("private-binary-export"))
        }));

        for (before, after) in [
            (
                "#[cfg_attr(feature=\"ffi\", unsafe(no_mangle))] extern \"C\" fn binary_api(_: u8) {}\n",
                "#[cfg_attr(feature=\"ffi\", unsafe(no_mangle))] extern \"C\" fn binary_api(_: u16) {}\n",
            ),
            (
                "#[cfg_attr(feature=\"ffi\", cfg_attr(feature=\"named\", unsafe(export_name=\"binary_api\")))] extern \"C\" fn binary_api(_: u8) {}\n",
                "#[cfg_attr(feature=\"ffi\", cfg_attr(feature=\"named\", unsafe(export_name=\"binary_api\")))] extern \"C\" fn binary_api(_: u16) {}\n",
            ),
        ] {
            let conditional_binary_export = repository_delta(&[
                ("Cargo.toml", manifest, manifest),
                ("src/lib.rs", before, after),
            ]);
            assert!(conditional_binary_export.unknown.iter().any(|finding| {
                finding.identity.name == "UnsupportedExternResolution"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("private-binary-export"))
            }));
        }
    }

    #[test]
    fn repository_backed_conditional_macro_exports_and_macro_invocations_are_proven() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let lock = "version = 4\n";
        for (before, after) in [
            (
                "#[cfg_attr(feature=\"public\", macro_export)] macro_rules! api { () => { 1 } }\n",
                "#[cfg_attr(feature=\"public\", macro_export)] macro_rules! api { () => { 2 } }\n",
            ),
            (
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", macro_export))] macro_rules! api { () => { 1 } }\n",
                "#[cfg_attr(feature=\"a\", cfg_attr(feature=\"b\", macro_export))] macro_rules! api { () => { 2 } }\n",
            ),
        ] {
            let delta = repository_delta(&[
                ("Cargo.toml", manifest, manifest),
                ("Cargo.lock", lock, lock),
                ("src/lib.rs", before, after),
            ]);
            assert!(delta.changed.iter().any(|finding| {
                finding.identity.name == "api"
                    && finding.confidence == ApiDeltaConfidence::Confirmed
            }));
        }

        let unchanged_invocation = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } make!();\n",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } make!();\n",
            ),
        ]);
        assert!(
            unchanged_invocation.findings().is_empty(),
            "an unchanged invocation with a revision-backed implementation must neutralize: {:?}",
            unchanged_invocation.findings()
        );

        let changed_invocation = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } make!();\n",
                "macro_rules! make { () => { pub fn generated() -> u16 { 1 } } } make!();\n",
            ),
        ]);
        assert!(changed_invocation.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("transform-boundary:macro-invocation")
                        && reason.contains("declarative-implementation-digest:sha256:")
                })
        }));

        let unlocked_changed_invocation = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } make!();\n",
                "macro_rules! make { () => { pub fn generated() -> u16 { 1 } } } make!();\n",
            ),
        ]);
        assert!(unlocked_changed_invocation.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("declarative-implementation-digest:unresolved:")
                })
        }));

        let workspace = "[workspace]\nmembers=['api']\nexclude=['macros']\nresolver='2'\n";
        let api_manifest = "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n";
        let macros_manifest = "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n";
        let unresolved_proc_macro_invocation = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", lock, lock),
            ("api/Cargo.toml", api_manifest, api_manifest),
            (
                "api/src/lib.rs",
                "use macros::make; make!();\n",
                "use macros::make; make!();\n",
            ),
            ("macros/Cargo.toml", macros_manifest, macros_manifest),
            (
                "macros/.cargo/config.toml",
                "paths = ['../vendor']\n",
                "paths = ['../vendor']\n",
            ),
            (
                "macros/src/lib.rs",
                "#[proc_macro] pub fn make(_: proc_macro::TokenStream) -> proc_macro::TokenStream { \"pub fn generated() -> u8 { 1 }\".parse().unwrap() }\n",
                "#[proc_macro] pub fn make(_: proc_macro::TokenStream) -> proc_macro::TokenStream { \"pub fn generated() -> u16 { 1 }\".parse().unwrap() }\n",
            ),
        ]);
        assert!(
            unresolved_proc_macro_invocation
                .unknown
                .iter()
                .any(|finding| {
                    finding.identity.name == "MacroGeneratedItems"
                        && finding
                            .unknown_reason
                            .as_deref()
                            .is_some_and(|reason| {
                                reason.contains(
                                    "declarative-implementation-digest:unresolved:reachable transformer substrate",
                                )
                            })
                }),
            "an unresolved reachable proc-macro closure must never be neutralized by the opaque-return digest: {:?}",
            unresolved_proc_macro_invocation.findings()
        );
    }

    #[test]
    fn repository_backed_native_only_macro_boundaries_are_never_falsely_clean() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n";
        let lock = "version = 4\n";

        let declarative_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {} } } export!();\n",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u16) {} } } export!();\n",
            ),
        ]);
        assert!(declarative_changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("transform-boundary:macro-invocation")
                        && reason.contains("declarative-implementation-digest:sha256:")
                })
        }));

        let associated_declarative_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {} } } struct Api; impl Api { export!(); }\n",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u16) {} } } struct Api; impl Api { export!(); }\n",
            ),
        ]);
        assert!(
            associated_declarative_changed
                .unknown
                .iter()
                .any(|finding| {
                    finding.identity.name == "MacroGeneratedItems"
                        && finding.unknown_reason.as_deref().is_some_and(|reason| {
                            reason.contains("native-export-associated-macro")
                                && reason.contains("transform-boundary:macro-invocation")
                                && reason.contains("declarative-implementation-digest:sha256:")
                        })
                })
        );

        let rust_only_manifest = "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib']\n";
        let rust_only_private_changed = repository_delta(&[
            ("Cargo.toml", rust_only_manifest, rust_only_manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! helper { () => { fn private(value: u8) {} } } struct Hidden; impl Hidden { helper!(); }\n",
                "macro_rules! helper { () => { fn private(value: u16) {} } } struct Hidden; impl Hidden { helper!(); }\n",
            ),
        ]);
        assert!(
            rust_only_private_changed.findings().is_empty(),
            "an rlib-only associated macro on a private owner is not a native-export fallback: {:?}",
            rust_only_private_changed.findings()
        );

        let mixed_manifest = "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib','cdylib']\n";
        let mixed_private_changed = repository_delta(&[
            ("Cargo.toml", mixed_manifest, mixed_manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {} } } struct Hidden; impl Hidden { export!(); }\n",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u16) {} } } struct Hidden; impl Hidden { export!(); }\n",
            ),
        ]);
        assert!(mixed_private_changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("native-export-associated-macro")
                        && reason.contains("declarative-implementation-digest:sha256:")
                })
        }));

        let included_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "include!(\"native.rs\");\n",
                "include!(\"native.rs\");\n",
            ),
            (
                "src/native.rs",
                "#[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {}\n",
                "#[unsafe(no_mangle)] pub extern \"C\" fn native(value: u16) {}\n",
            ),
        ]);
        assert!(included_changed.unknown.iter().any(|finding| {
            finding.identity.name == "IncludeMacro"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("include-kind:include")
                        && reason.contains("included-digest:")
                        && !reason.contains("included-digest:unresolved")
                })
        }));

        let associated_include_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "struct Api; impl Api { include!(\"native.rs\"); }\n",
                "struct Api; impl Api { include!(\"native.rs\"); }\n",
            ),
            (
                "src/native.rs",
                "#[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {}\n",
                "#[unsafe(no_mangle)] pub extern \"C\" fn native(value: u16) {}\n",
            ),
        ]);
        assert!(associated_include_changed.unknown.iter().any(|finding| {
            finding.identity.name == "IncludeMacro"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("native-export-associated-macro")
                        && reason.contains("include-kind:include")
                        && reason.contains("included-digest:")
                        && !reason.contains("included-digest:unresolved")
                })
        }));

        let assembly_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "core::arch::global_asm!(\".global native_v1\");\n",
                "core::arch::global_asm!(\".global native_v2\");\n",
            ),
        ]);
        assert!(assembly_changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("global_asm")
                        && reason.contains("transform-boundary:macro-invocation")
                })
        }));

        let unchanged_boundaries = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native() {} } } export!(); core::arch::global_asm!(\".global assembly_native\");\n",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native() {} } } export!(); core::arch::global_asm!(\".global assembly_native\");\n",
            ),
        ]);
        assert!(
            unchanged_boundaries.findings().is_empty(),
            "unchanged proven native macro boundaries must neutralize: {:?}",
            unchanged_boundaries.findings()
        );

        let unchanged_associated_boundary = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native() {} } } struct Api; impl Api { export!(); }\n",
                "macro_rules! export { () => { #[unsafe(no_mangle)] pub extern \"C\" fn native() {} } } struct Api; impl Api { export!(); }\n",
            ),
        ]);
        assert!(
            unchanged_associated_boundary.findings().is_empty(),
            "an unchanged proven associated native macro boundary must neutralize: {:?}",
            unchanged_associated_boundary.findings()
        );

        let unused_declarative_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "macro_rules! unused { () => { fn helper(value: u8) {} } }\n",
                "macro_rules! unused { () => { fn helper(value: u16) {} } }\n",
            ),
        ]);
        assert!(
            unused_declarative_changed.findings().is_empty(),
            "an unused declarative macro is not itself a native export boundary: {:?}",
            unused_declarative_changed.findings()
        );
    }

    #[test]
    fn custom_default_helper_stays_out_of_the_confirmed_enum_contract() {
        let workspace = "[workspace]\nmembers=['api','macros']\nresolver='2'\n";
        let api_manifest = "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n";
        let macro_manifest = "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n";
        let macro_source = "#[proc_macro_derive(Default, attributes(default))] pub fn derive(_: proc_macro::TokenStream) -> proc_macro::TokenStream { proc_macro::TokenStream::new() }\n";
        let delta = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("api/Cargo.toml", api_manifest, api_manifest),
            (
                "api/src/lib.rs",
                "use macros::Default; #[derive(Default)] pub enum Choice { #[default] A, B }\n",
                "use macros::Default; #[derive(Default)] pub enum Choice { A, #[default] B }\n",
            ),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", macro_source, macro_source),
        ]);
        assert!(
            delta.changed.is_empty(),
            "custom helper tokens must not manufacture a confirmed enum change: {:?}",
            delta.findings()
        );
        assert!(
            delta
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );
    }

    #[test]
    fn imported_custom_derive_can_shadow_a_builtin_name() {
        let temp = tempfile::tempdir().expect("custom derive fixture tempdir");
        let macro_source = temp.path().join("macros.rs");
        fs::write(
            &macro_source,
            "extern crate proc_macro;\nuse proc_macro::TokenStream;\n#[proc_macro_derive(Debug)]\npub fn debug(_: TokenStream) -> TokenStream { \"impl Api { pub fn custom_marker() {} }\".parse().unwrap() }\n",
        )
        .expect("write custom derive fixture");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let macro_output = Command::new(&rustc)
            .args([
                "--edition=2021",
                "--crate-name=macros",
                "--crate-type=proc-macro",
            ])
            .arg(&macro_source)
            .arg("--out-dir")
            .arg(temp.path())
            .output()
            .expect("compile custom derive fixture");
        assert!(
            macro_output.status.success(),
            "custom derive fixture must compile: {}",
            String::from_utf8_lossy(&macro_output.stderr)
        );
        let macro_artifact = fs::read_dir(temp.path())
            .expect("read custom derive output")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("libmacros."))
            })
            .expect("find custom derive dynamic library");
        let api_source = temp.path().join("api.rs");
        fs::write(
            &api_source,
            "use macros::Debug;\n#[derive(Debug)] pub struct Api;\npub fn proves_custom_resolution() { Api::custom_marker(); }\n",
        )
        .expect("write custom derive consumer");
        let api_output = Command::new(rustc)
            .args(["--edition=2021", "--crate-type=lib"])
            .arg(&api_source)
            .arg("--extern")
            .arg(format!("macros={}", macro_artifact.display()))
            .arg("-o")
            .arg(temp.path().join("libapi.rlib"))
            .output()
            .expect("compile custom derive consumer");
        assert!(
            api_output.status.success(),
            "an explicit imported derive shadows the builtin name: {}",
            String::from_utf8_lossy(&api_output.stderr)
        );
    }

    #[test]
    fn repository_backed_transformed_associated_native_export_binds_macro_implementation() {
        let workspace = "[workspace]\nmembers=['api','macros']\nresolver='2'\n";
        let api_manifest = "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n[dependencies]\nmacros={path='../macros'}\n";
        let macro_manifest = "[package]\nname='macros'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n";
        let api_source = "pub struct Api; impl Api { #[macros::erase] #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {} }\n";
        let before_macro = "#[proc_macro_attribute] pub fn erase(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n";
        let after_macro = "#[proc_macro_attribute] pub fn erase(_: proc_macro::TokenStream, _: proc_macro::TokenStream) -> proc_macro::TokenStream { proc_macro::TokenStream::new() }\n";

        let changed = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", api_source, api_source),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", before_macro, after_macro),
        ]);
        assert!(changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("native-export-associated-function:native")
                        && reason.contains("transform:transform-boundary:attribute")
                        && reason.contains("input:# [macros :: erase]")
                        && reason.contains("associated-owner-contract:impl Api")
                        && reason.contains("macro-implementation-digest:sha256:")
                })
        }));

        let mixed_api_manifest = "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib','cdylib']\n[dependencies]\nmacros={path='../macros'}\n";
        let mixed_private_source = "struct Hidden; impl Hidden { #[macros::erase] #[unsafe(no_mangle)] pub extern \"C\" fn native(value: u8) {} }\n";
        let mixed_private_changed = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("api/Cargo.toml", mixed_api_manifest, mixed_api_manifest),
            ("api/src/lib.rs", mixed_private_source, mixed_private_source),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", before_macro, after_macro),
        ]);
        assert!(mixed_private_changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("native-export-associated-function:native")
                        && reason.contains("associated-owner-contract:impl Hidden")
                        && reason.contains("macro-implementation-digest:sha256:")
                })
        }));

        let generated_export_source =
            "struct Hidden; impl Hidden { #[macros::expose] pub extern \"C\" fn generated(value: u8) {} }\n";
        let generated_export_before = "#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { let mut output: proc_macro::TokenStream = \"#[unsafe(no_mangle)]\".parse().unwrap(); output.extend(input); output }\n";
        let generated_export_after = "#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n";
        for generated_manifest in [api_manifest, mixed_api_manifest] {
            let generated_export_changed = repository_delta(&[
                ("Cargo.toml", workspace, workspace),
                ("Cargo.lock", "version = 4\n", "version = 4\n"),
                ("api/Cargo.toml", generated_manifest, generated_manifest),
                (
                    "api/src/lib.rs",
                    generated_export_source,
                    generated_export_source,
                ),
                ("macros/Cargo.toml", macro_manifest, macro_manifest),
                (
                    "macros/src/lib.rs",
                    generated_export_before,
                    generated_export_after,
                ),
            ]);
            assert!(generated_export_changed.unknown.iter().any(|finding| {
                finding.identity.name == "MacroGeneratedItems"
                    && finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains("native-export-associated-function:generated")
                            && reason.contains("macro-generated-native-export-potential")
                            && reason.contains("input:# [macros :: expose]")
                            && reason.contains("associated-owner-contract:impl Hidden")
                            && reason.contains("macro-implementation-digest:sha256:")
                    })
            }));
        }

        let generated_export_unchanged = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("api/Cargo.toml", api_manifest, api_manifest),
            (
                "api/src/lib.rs",
                generated_export_source,
                generated_export_source,
            ),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            (
                "macros/src/lib.rs",
                generated_export_before,
                generated_export_before,
            ),
        ]);
        assert!(
            generated_export_unchanged.findings().is_empty(),
            "an unchanged associated native export-generation boundary must neutralize: {:?}",
            generated_export_unchanged.findings()
        );

        let external_manifest = "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n[dependencies]\nmacros='1'\n";
        let external_lock = "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='registry+https://index.crates.io/'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n";
        let body_changed = repository_delta(&[
            ("Cargo.toml", external_manifest, external_manifest),
            ("Cargo.lock", external_lock, external_lock),
            (
                "src/lib.rs",
                "pub struct Api; impl Api { #[macros::erase] #[unsafe(no_mangle)] pub extern \"C\" fn native() { let value = 1; drop(value); } }\n",
                "pub struct Api; impl Api { #[macros::erase] #[unsafe(no_mangle)] pub extern \"C\" fn native() { let value = 2; drop(value); } }\n",
            ),
        ]);
        assert!(body_changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("native-export-associated-function:native")
                        && reason.contains("input:# [macros :: erase]")
                        && reason.contains("macro-implementation-digest:sha256:")
                })
        }));

        let untransformed_body_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
            ),
            (
                "src/lib.rs",
                "pub struct Api; impl Api { #[unsafe(no_mangle)] pub extern \"C\" fn native() { let value = 1; drop(value); } }\n",
                "pub struct Api; impl Api { #[unsafe(no_mangle)] pub extern \"C\" fn native() { let value = 2; drop(value); } }\n",
            ),
        ]);
        assert!(
            untransformed_body_changed.findings().is_empty(),
            "an untransformed native export body remains outside the API contract: {:?}",
            untransformed_body_changed.findings()
        );

        let unchanged = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", api_source, api_source),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", before_macro, before_macro),
        ]);
        assert!(
            unchanged.findings().is_empty(),
            "unchanged associated native transform proof must neutralize: {:?}",
            unchanged.findings()
        );
    }

    #[test]
    fn repository_backed_transform_proof_binds_proc_macro_implementation_closure() {
        let workspace = "[workspace]\nmembers=['api','macros','helper']\nresolver='2'\n";
        let api_manifest = "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n";
        let macro_manifest = "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n[dependencies]\nhelper={path='../helper'}\n";
        let helper_manifest =
            "[package]\nname='helper'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let lock = "version = 4\n";
        let api = "#[macros::expose] pub struct Api;\n";
        let macro_source = "#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { helper::expand(input) }\n";

        let changed = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", lock, lock),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", api, api),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", macro_source, macro_source),
            ("helper/Cargo.toml", helper_manifest, helper_manifest),
            (
                "helper/src/lib.rs",
                "pub fn expand(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n",
                "pub fn expand(_: proc_macro::TokenStream) -> proc_macro::TokenStream { \"impl Api { pub fn generated() {} }\".parse().unwrap() }\n",
            ),
        ]);
        assert!(changed.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("macro-implementation-digest:sha256:"))
        }));
        assert!(
            changed.changed.is_empty(),
            "an implementation-substrate change is uncertainty, not a confirmed API change: {:?}",
            changed.findings()
        );

        let unchanged = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", lock, lock),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", api, api),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", macro_source, macro_source),
            ("helper/Cargo.toml", helper_manifest, helper_manifest),
            (
                "helper/src/lib.rs",
                "pub fn expand(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n",
                "pub fn expand(input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n",
            ),
        ]);
        assert!(
            unchanged.findings().is_empty(),
            "identical transformer input and implementation closure must neutralize: {:?}",
            unchanged.findings()
        );

        let external_root = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api','macros']\nresolver='2'\n",
                "[workspace]\nmembers=['api','macros']\nresolver='2'\n",
            ),
            ("Cargo.lock", lock, lock),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", api, api),
            (
                "macros/Cargo.toml",
                "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='../macro_impl.rs'\nproc-macro=true\n",
                "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='../macro_impl.rs'\nproc-macro=true\n",
            ),
            (
                "macro_impl.rs",
                "#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n",
                "#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, _: proc_macro::TokenStream) -> proc_macro::TokenStream { \"impl Api { pub fn generated() {} }\".parse().unwrap() }\n",
            ),
        ]);
        assert!(external_root.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("macro-implementation-digest:sha256:"))
        }));
    }

    #[test]
    fn repository_backed_transform_proof_requires_the_product_lock_and_resolved_sources() {
        let api = "#[derive(serde::Serialize)] pub struct Api;\n";
        let without_effective_lock = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
            ),
            ("src/lib.rs", api, api),
            (
                "fixtures/demo/Cargo.toml",
                "[package]\nname='demo'\nversion='0.0.0'\n",
                "[package]\nname='demo'\nversion='0.0.0'\n",
            ),
            ("fixtures/demo/Cargo.lock", "version = 4\n", "version = 4\n"),
        ]);
        assert_eq!(without_effective_lock.unknown.len(), 2);
        assert!(without_effective_lock.unknown.iter().all(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("macro-implementation-digest:unresolved:")
                        && reason.contains("effective Cargo.lock")
                })
        }));

        let stale_effective_lock = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
            ),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("src/lib.rs", api, api),
        ]);
        assert_eq!(stale_effective_lock.unknown.len(), 2);
        assert!(stale_effective_lock.unknown.iter().all(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("macro-implementation-digest:unresolved:")
                    && reason.contains("does not contain dependency candidates: serde")
            })
        }));

        let registry_without_checksum = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://index.crates.io/'\n",
                "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://index.crates.io/'\n",
            ),
            ("src/lib.rs", api, api),
        ]);
        assert_eq!(registry_without_checksum.unknown.len(), 2);
        assert!(registry_without_checksum.unknown.iter().all(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("dependency candidates: serde 1"))
        }));

        let git_without_precise_commit = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={git='https://example.invalid/macros'}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={git='https://example.invalid/macros'}\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='git+https://example.invalid/macros'\n",
                "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='git+https://example.invalid/macros#'\n",
            ),
            (
                "src/lib.rs",
                "#[derive(macros::Generate)] pub struct Api;\n",
                "#[derive(macros::Generate)] pub struct Api;\n",
            ),
        ]);
        assert_eq!(git_without_precise_commit.unknown.len(), 2);

        let git_short_precise_commit = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={git='https://example.invalid/macros'}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={git='https://example.invalid/macros'}\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='git+https://example.invalid/macros#deadbee'\n",
                "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='git+https://example.invalid/macros#deadbee'\n",
            ),
            (
                "src/lib.rs",
                "#[derive(macros::Generate)] pub struct Api;\n",
                "#[derive(macros::Generate)] pub struct Api;\n",
            ),
        ]);
        assert_eq!(git_short_precise_commit.unknown.len(), 2);

        let same_name_local_package_does_not_cover_external = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api','serde']\nresolver='2'\n",
                "[workspace]\nmembers=['api','serde']\nresolver='2'\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='serde'\nversion='0.0.0'\n",
                "version = 4\n[[package]]\nname='serde'\nversion='0.0.0'\n",
            ),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_remote={package='serde',version='1',features=['derive']}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_remote={package='serde',version='1',features=['derive']}\n",
            ),
            (
                "api/src/lib.rs",
                "#[derive(serde_remote::Serialize)] pub struct Api;\n",
                "#[derive(serde_remote::Serialize)] pub struct Api;\n",
            ),
            (
                "serde/Cargo.toml",
                "[package]\nname='serde'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='serde'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("serde/src/lib.rs", "", ""),
        ]);
        assert_eq!(
            same_name_local_package_does_not_cover_external
                .unknown
                .len(),
            2
        );
        assert!(
            same_name_local_package_does_not_cover_external
                .unknown
                .iter()
                .all(|finding| {
                    finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains("macro-implementation-digest:unresolved:")
                            && reason.contains("dependency candidates: serde 1")
                    })
                })
        );

        let reachable_member_config = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api']\nresolver='2'\n",
                "[workspace]\nmembers=['api']\nresolver='2'\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://index.crates.io/'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n",
                "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://index.crates.io/'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n",
            ),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
            ),
            (
                "api/.cargo/config.toml",
                "[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory='../vendor-a'\n",
                "[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory='../vendor-b'\n",
            ),
            (
                "api/src/lib.rs",
                "#[derive(serde::Serialize)] pub struct Api;\n",
                "#[derive(serde::Serialize)] pub struct Api;\n",
            ),
        ]);
        assert_eq!(reachable_member_config.unknown.len(), 2);
        assert!(reachable_member_config.unknown.iter().all(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("api/.cargo/config.toml") && reason.contains("source replacement")
            })
        }));

        let wrong_external_source = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={git='https://example.invalid/expected',rev='abc'}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={git='https://example.invalid/expected',rev='abc'}\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
                "version = 4\n[[package]]\nname='macros'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
            ),
            (
                "src/lib.rs",
                "#[derive(macros::Generate)] pub struct Api;\n",
                "#[derive(macros::Generate)] pub struct Api;\n",
            ),
        ]);
        assert_eq!(wrong_external_source.unknown.len(), 2);
        assert!(wrong_external_source.unknown.iter().all(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("macro-implementation-digest:unresolved:")
                    && reason.contains("dependency candidates: macros")
            })
        }));

        let workspace_dependency_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api']\nresolver='2'\n[workspace.dependencies]\nserde={version='1',features=['derive']}\n",
                "[workspace]\nmembers=['api']\nresolver='2'\n[workspace.dependencies]\nserde={version='1',features=['derive','rc']}\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n",
                "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n",
            ),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={workspace=true}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={workspace=true}\n",
            ),
            ("api/src/lib.rs", api, api),
        ]);
        assert_eq!(workspace_dependency_changed.unknown.len(), 2);
        assert!(workspace_dependency_changed.unknown.iter().all(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("macro-implementation-digest:sha256:"))
        }));

        let external_lock = "version = 4\n[[package]]\nname='serde'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='0000000000000000000000000000000000000000000000000000000000000000'\n";
        let config_environment_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
            ),
            ("Cargo.lock", external_lock, external_lock),
            (
                ".cargo/config.toml",
                "[env]\nPRVIEW_MACRO_MODE='base'\n",
                "[env]\nPRVIEW_MACRO_MODE='target'\n",
            ),
            ("src/lib.rs", api, api),
        ]);
        let config_proofs = config_environment_changed
            .unknown
            .iter()
            .filter_map(|finding| finding.unknown_reason.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            config_proofs.len(),
            2,
            "effective Cargo config bytes must distinguish the two external macro substrates: {:?}",
            config_environment_changed.findings()
        );
        assert!(
            config_proofs
                .iter()
                .all(|reason| reason.contains("macro-implementation-digest:sha256:"))
        );

        let reachable_manifest_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api','helper']\nresolver='2'\n",
                "[workspace]\nmembers=['api','helper']\nresolver='2'\n",
            ),
            ("Cargo.lock", external_lock, external_lock),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nhelper={path='../helper'}\n",
                "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nhelper={path='../helper'}\n",
            ),
            (
                "api/src/lib.rs",
                "#[derive(helper::Serialize)] pub struct Api;\n",
                "#[derive(helper::Serialize)] pub struct Api;\n",
            ),
            (
                "helper/Cargo.toml",
                "[package]\nname='helper'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive']}\n",
                "[package]\nname='helper'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde={version='1',features=['derive','rc']}\n",
            ),
            (
                "helper/src/lib.rs",
                "pub use serde::Serialize;\n",
                "pub use serde::Serialize;\n",
            ),
        ]);
        let reachable_proofs = reachable_manifest_changed
            .unknown
            .iter()
            .filter_map(|finding| finding.unknown_reason.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            reachable_proofs.len(),
            2,
            "a reachable manifest change must distinguish the two external macro substrates: {:?}",
            reachable_manifest_changed.findings()
        );
        assert!(
            reachable_proofs
                .iter()
                .all(|reason| reason.contains("macro-implementation-digest:sha256:"))
        );

        let workspace = "[workspace]\nmembers=['api','macros']\nresolver='2'\n[patch.crates-io]\nmacro-helper={path='../../outside-helper'}\n";
        let api_manifest = "[package]\nname='api'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nmacros={path='../macros'}\n";
        let macro_manifest = "[package]\nname='macros'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\nproc-macro=true\n";
        let macro_source = "#[proc_macro_attribute] pub fn expose(_: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream { input }\n";
        let attributed = "#[macros::expose] pub struct Api;\n";
        let manifest_patch = repository_delta(&[
            ("Cargo.toml", workspace, workspace),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", attributed, attributed),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", macro_source, macro_source),
        ]);
        assert_eq!(manifest_patch.unknown.len(), 2);
        assert!(manifest_patch.unknown.iter().all(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("macro-implementation-digest:unresolved:")
                    && reason.contains("patch/replace")
            })
        }));

        let workspace_without_patch = "[workspace]\nmembers=['api','macros']\nresolver='2'\n";
        let cargo_source_replacement = repository_delta(&[
            (
                "Cargo.toml",
                workspace_without_patch,
                workspace_without_patch,
            ),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                ".cargo/config.toml",
                "[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory='vendor'\n",
                "[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory='vendor'\n",
            ),
            ("api/Cargo.toml", api_manifest, api_manifest),
            ("api/src/lib.rs", attributed, attributed),
            ("macros/Cargo.toml", macro_manifest, macro_manifest),
            ("macros/src/lib.rs", macro_source, macro_source),
        ]);
        assert_eq!(cargo_source_replacement.unknown.len(), 2);
        assert!(cargo_source_replacement.unknown.iter().all(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("macro-implementation-digest:unresolved:")
                    && reason.contains("source replacement")
            })
        }));
    }

    #[test]
    fn repository_backed_private_transformers_stay_typed_unknown() {
        let ordinary = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "#[custom] struct Hidden(u8);\n",
                "#[custom] struct Hidden(u16);\n",
            ),
        ]);
        assert!(ordinary.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("struct Hidden"))
        }));

        let inherent = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Owner; impl Owner { #[custom] fn hidden(&self, _: u8) {} }\n",
                "pub struct Owner; impl Owner { #[custom] fn hidden(&self, _: u16) {} }\n",
            ),
        ]);
        assert!(inherent.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("fn hidden"))
        }));

        let private_owner = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "struct Hidden; impl Hidden { #[custom] fn helper(&self, _: u8) {} }\n",
                "struct Hidden; impl Hidden { #[custom] fn helper(&self, _: u16) {} }\n",
            ),
        ]);
        assert!(
            private_owner.findings().is_empty(),
            "an associated transformer on a private owner cannot expose caller-visible API: {:?}",
            private_owner.findings()
        );

        let reexported_owner = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod inner { pub struct Owner; impl Owner { #[custom] fn hidden(&self, _: u8) {} } } pub use inner::Owner as PublicOwner;\n",
                "mod inner { pub struct Owner; impl Owner { #[custom] fn hidden(&self, _: u16) {} } } pub use inner::Owner as PublicOwner;\n",
            ),
        ]);
        assert!(reexported_owner.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("inherent-owner:Owner"))
        }));
    }

    #[test]
    fn repository_backed_associated_transforms_follow_aliases_macros_and_cfg_regions() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let alias_chain = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                "src/lib.rs",
                "mod hidden { pub struct Owner; impl Owner { pub async fn api() {} } } type Mid = hidden::Owner; pub type Public = Mid;\n",
                "mod hidden { pub struct Owner; impl Owner { pub async fn api() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } } } type Mid = hidden::Owner; pub type Public = Mid;\n",
            ),
        ]);
        assert!(alias_chain.unknown.iter().any(|finding| {
            finding.identity.name == "UnresolvedInherentOwner"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("public-type-alias:Public"))
        }));

        let unchanged_declarative = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                "src/lib.rs",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } pub struct Owner; impl Owner { make!(); }\n",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } pub struct Owner; impl Owner { make!(); }\n",
            ),
        ]);
        assert!(
            unchanged_declarative.findings().is_empty(),
            "an unchanged local macro_rules boundary has a revision-backed declarative proof: {:?}",
            unchanged_declarative.findings()
        );

        let changed_declarative = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                "src/lib.rs",
                "macro_rules! make { () => { pub fn generated() -> u8 { 1 } } } pub struct Owner; impl Owner { make!(); }\n",
                "macro_rules! make { () => { pub fn generated() -> u16 { 1 } } } pub struct Owner; impl Owner { make!(); }\n",
            ),
        ]);
        assert!(
            changed_declarative
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "MacroGeneratedItems")
        );

        let unrelated_confirmed_change = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "pub struct Owner; impl Owner { #[custom] fn seed() {} } pub fn other(_: u8) {}\n",
                "pub struct Owner; impl Owner { #[custom] fn seed() {} } pub fn other(_: u16) {}\n",
            ),
        ]);
        assert!(unrelated_confirmed_change.changed.iter().any(|finding| {
            finding.identity.name == "other" && finding.confidence == ApiDeltaConfidence::Confirmed
        }));

        let cfg_disjoint = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "#[cfg(unix)] struct Owner; #[cfg(windows)] pub use Owner as Public; #[cfg(unix)] impl Owner { #[custom] fn seed() {} }\n",
                "#[cfg(unix)] struct Owner; #[cfg(windows)] pub use Owner as Public; #[cfg(unix)] impl Owner { #[custom] fn seed() {} }\n",
            ),
        ]);
        assert!(
            cfg_disjoint.findings().is_empty(),
            "a cfg-disjoint owner exposure cannot inherit an impossible transform region: {:?}",
            cfg_disjoint.findings()
        );
    }

    #[test]
    fn repository_backed_trait_macro_boundaries_follow_reexports_and_impls() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let reexported_trait = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                "src/lib.rs",
                "macro_rules! make { () => { fn generated() -> u8; } } mod inner { pub trait T { make!(); } } pub use inner::T as PublicT;\n",
                "macro_rules! make { () => { fn generated() -> u16; } } mod inner { pub trait T { make!(); } } pub use inner::T as PublicT;\n",
            ),
        ]);
        assert!(reexported_trait.unknown.iter().any(|finding| {
            finding.identity.name == "MacroGeneratedItems"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("trait-owner:T"))
        }));

        let reexported_trait_attribute_input = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "mod inner { pub trait T { #[custom] fn value(_: u8); } } pub use inner::T as PublicT;\n",
                "mod inner { pub trait T { #[custom] fn value(_: u16); } } pub use inner::T as PublicT;\n",
            ),
        ]);
        assert!(
            !reexported_trait_attribute_input
                .changed
                .iter()
                .any(|finding| finding.identity.name == "PublicT"),
            "a replacement attribute keeps the reexported trait change typed unknown: {:?}",
            reexported_trait_attribute_input.findings()
        );
        assert!(
            reexported_trait_attribute_input
                .unknown
                .iter()
                .any(|finding| {
                    finding.identity.name == "PublicT"
                        || finding.identity.name == "MacroGeneratedItems"
                })
        );

        let trait_impl = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", "version = 4\n", "version = 4\n"),
            (
                "src/lib.rs",
                "pub trait T { type Assoc; } pub struct Owner; macro_rules! fill { () => { type Assoc = u8; } } impl T for Owner { fill!(); }\n",
                "pub trait T { type Assoc; } pub struct Owner; macro_rules! fill { () => { type Assoc = u16; } } impl T for Owner { fill!(); }\n",
            ),
        ]);
        assert!(
            trait_impl
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "TraitImplResolution")
        );

        let unlocked_trait_impl = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            (
                "src/lib.rs",
                "pub trait T { type Assoc; } pub struct Owner; macro_rules! fill { () => { type Assoc = u8; } } impl T for Owner { fill!(); }\n",
                "pub trait T { type Assoc; } pub struct Owner; macro_rules! fill { () => { type Assoc = u16; } } impl T for Owner { fill!(); }\n",
            ),
        ]);
        assert!(unlocked_trait_impl.unknown.iter().any(|finding| {
            finding.identity.name == "TraitImplResolution"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("declarative-implementation-digest:unresolved:")
                })
        }));
    }

    #[test]
    fn repository_backed_opaque_returns_preserve_body_dependent_auto_trait_uncertainty() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let lock = "version = 4\n";
        let stale_external_lock = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_remote={package='serde',version='1'}\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n[dependencies]\nserde_remote={package='serde',version='1'}\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n[[package]]\nname='serde'\nversion='0.0.0'\n",
                "version = 4\n[[package]]\nname='serde'\nversion='0.0.0'\n",
            ),
            (
                "src/lib.rs",
                "pub async fn api() {}\n",
                "pub async fn api() {}\n",
            ),
        ]);
        assert_eq!(stale_external_lock.unknown.len(), 2);
        assert!(stale_external_lock.unknown.iter().all(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("opaque-implementation-digest:unresolved:")
                        && reason.contains("dependency candidates: serde 1")
                })
        }));

        let async_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn fetch() { std::future::ready(()).await; }\n",
                "pub async fn fetch() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); }\n",
            ),
        ]);
        assert!(async_changed.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("body-digest:sha256:"))
        }));
        assert!(async_changed.changed.is_empty());

        let rpit_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub fn values() -> impl Iterator<Item = u8> { std::iter::once(1) }\n",
                "pub fn values() -> impl Iterator<Item = u8> { let value = std::rc::Rc::new(1); std::iter::once_with(move || *value) }\n",
            ),
        ]);
        assert!(rpit_changed.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("opaque-return:function"))
        }));

        let unchanged = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn fetch() { std::future::ready(()).await; }\n",
                "pub async fn fetch() { std::future::ready(()).await; }\n",
            ),
        ]);
        assert!(unchanged.findings().is_empty());

        let ordinary_body = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub fn calculate() -> u8 { 1 }\n",
                "pub fn calculate() -> u8 { 2 }\n",
            ),
        ]);
        assert!(ordinary_body.findings().is_empty());

        let private_control = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "mod hidden { pub async fn fetch() { std::future::ready(()).await; } }\n",
                "mod hidden { pub async fn fetch() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } }\n",
            ),
        ]);
        assert!(private_control.findings().is_empty());

        let private_helper_changed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "async fn helper() { std::future::ready(()).await; } pub async fn api() { helper().await; }\n",
                "async fn helper() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } pub async fn api() { helper().await; }\n",
            ),
        ]);
        assert!(private_helper_changed.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("opaque-implementation-digest:sha256:"))
        }));

        let nonstandard_include_input = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn api() { include!(\"../body.inc\"); }\n",
                "pub async fn api() { include!(\"../body.inc\"); }\n",
            ),
            (
                "body.inc",
                "{ std::future::ready(()).await; }\n",
                "{ let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); }\n",
            ),
        ]);
        assert!(nonstandard_include_input.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("opaque-implementation-digest:sha256:"))
        }));

        let binder_rename = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn api<T: Default>() { let _: T = T::default(); }\n",
                "pub async fn api<U: Default>() { let _: U = U::default(); }\n",
            ),
        ]);
        assert!(
            binder_rename.findings().is_empty(),
            "generic binder spelling in an opaque body is not semantic: {:?}",
            binder_rename.findings()
        );

        let value_parameter_rename = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn api(value: u8) -> u8 { value }\n",
                "pub async fn api(renamed: u8) -> u8 { renamed }\n",
            ),
        ]);
        assert!(
            value_parameter_rename.findings().is_empty(),
            "value-parameter spelling in an opaque body is not semantic: {:?}",
            value_parameter_rename.findings()
        );

        let local_destructure_and_shadow_rename = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn api(input: (u8, u8)) -> u8 { let (left, right) = input; let left = left + right; left }\n",
                "pub async fn api(values: (u8, u8)) -> u8 { let (first, second) = values; let first = first + second; first }\n",
            ),
        ]);
        assert!(
            local_destructure_and_shadow_rename.findings().is_empty(),
            "destructuring and lexical shadow spelling in an opaque body are not semantic: {:?}",
            local_destructure_and_shadow_rename.findings()
        );

        let forged_value_name = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "static __prview_v0: u8 = 0; pub async fn api() { let x = std::rc::Rc::new(()); drop(x); std::future::ready(()).await; let _ = __prview_v0; }\n",
                "static __prview_v0: u8 = 0; pub async fn api() { let y = std::rc::Rc::new(()); drop(__prview_v0); std::future::ready(()).await; drop(y); }\n",
            ),
        ]);
        assert!(
            forged_value_name
                .unknown
                .iter()
                .any(|finding| { finding.identity.name == "OpaqueReturnAutoTraits" })
        );

        let forged_type_name = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "struct __PrviewT0_0_0; pub async fn api<T: Default>() { let value = __PrviewT0_0_0; std::future::ready(()).await; drop(value); }\n",
                "struct __PrviewT0_0_0; pub async fn api<T: Default>() { let value = T::default(); std::future::ready(()).await; drop(value); }\n",
            ),
        ]);
        assert!(
            forged_type_name
                .unknown
                .iter()
                .any(|finding| { finding.identity.name == "OpaqueReturnAutoTraits" })
        );

        let shorthand_member_change = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub struct Pair { pub send: u8, pub nonsend: std::rc::Rc<()> } pub async fn api(pair: Pair) { let Pair { send, .. } = pair; std::future::ready(()).await; drop(send); }\n",
                "pub struct Pair { pub send: u8, pub nonsend: std::rc::Rc<()> } pub async fn api(pair: Pair) { let Pair { nonsend, .. } = pair; std::future::ready(()).await; drop(nonsend); }\n",
            ),
        ]);
        assert!(
            shorthand_member_change
                .unknown
                .iter()
                .any(|finding| { finding.identity.name == "OpaqueReturnAutoTraits" })
        );

        let module_path_change = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "mod send { pub async fn work() {} } mod nonsend { pub async fn work() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } } pub async fn api() { let send = (); send::work().await; drop(send); }\n",
                "mod send { pub async fn work() {} } mod nonsend { pub async fn work() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } } pub async fn api() { let nonsend = (); nonsend::work().await; drop(nonsend); }\n",
            ),
        ]);
        assert!(
            module_path_change
                .unknown
                .iter()
                .any(|finding| { finding.identity.name == "OpaqueReturnAutoTraits" })
        );

        let refutable_constants = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "const LEFT: u8 = 1; const RIGHT: u8 = 2; pub async fn api(value: u8) -> u8 { match value { LEFT => 10, _ => 0 } }\n",
                "const LEFT: u8 = 1; const RIGHT: u8 = 2; pub async fn api(value: u8) -> u8 { match value { RIGHT => 10, _ => 0 } }\n",
            ),
        ]);
        assert!(
            refutable_constants
                .unknown
                .iter()
                .any(|finding| { finding.identity.name == "OpaqueReturnAutoTraits" })
        );

        let synthetic_binder_control = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn api(__prview_v0: u8) -> u8 { __prview_v0 }\n",
                "pub async fn api(callback: u8) -> u8 { callback }\n",
            ),
        ]);
        assert!(
            synthetic_binder_control.findings().is_empty(),
            "a user-spelled synthetic-looking binder remains alpha-equivalent: {:?}",
            synthetic_binder_control.findings()
        );
    }

    #[test]
    fn opaque_return_regression_is_downstream_compiler_bound() {
        let compile = |label: &str, source: &str| {
            let temp = tempfile::tempdir().expect("rustc fixture tempdir");
            let input = temp.path().join(format!("{label}.rs"));
            let output = temp.path().join(format!("lib{label}.rlib"));
            fs::write(&input, source).expect("write rustc fixture");
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            Command::new(rustc)
                .args(["--edition=2021", "--crate-type=lib"])
                .arg(&input)
                .arg("-o")
                .arg(output)
                .output()
                .expect("run rustc fixture")
        };
        let base = compile(
            "opaque_send_base",
            "async fn helper() { std::future::ready(()).await; }\n\
             pub async fn api() { helper().await; }\n\
             fn assert_send<T: Send>(_: T) {}\n\
             pub fn downstream() { assert_send(api()); }\n",
        );
        assert!(
            base.status.success(),
            "base contract must compile: {}",
            String::from_utf8_lossy(&base.stderr)
        );
        let target = compile(
            "opaque_send_target",
            "async fn helper() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); }\n\
             pub async fn api() { helper().await; }\n\
             fn assert_send<T: Send>(_: T) {}\n\
             pub fn downstream() { assert_send(api()); }\n",
        );
        assert!(
            !target.status.success(),
            "target must falsify Send after only the private helper changes"
        );
        assert!(String::from_utf8_lossy(&target.stderr).contains("Send"));
    }

    #[test]
    fn repository_backed_opaque_return_proofs_follow_public_origins_without_hiding_signatures() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let lock = "version = 4\n";
        let reexported = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "mod hidden { pub async fn fetch() { std::future::ready(()).await; } } pub use hidden::fetch as public_fetch;\n",
                "mod hidden { pub async fn fetch() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } } pub use hidden::fetch as public_fetch;\n",
            ),
        ]);
        assert!(reexported.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("origin:Value:public_fetch"))
        }));

        let inherent = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub struct Owner; impl Owner { pub async fn fetch() { std::future::ready(()).await; } }\n",
                "pub struct Owner; impl Owner { pub async fn fetch() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } }\n",
            ),
        ]);
        assert!(inherent.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("opaque-return:inherent:fetch"))
        }));

        let signature_change = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub async fn fetch(_: u8) { std::future::ready(()).await; }\n",
                "pub async fn fetch(_: u16) { std::future::ready(()).await; }\n",
            ),
        ]);
        assert!(
            signature_change
                .changed
                .iter()
                .any(|finding| finding.identity.name == "fetch"),
            "item-local opaque uncertainty must not hide a confirmed signature change: {:?}",
            signature_change.findings()
        );

        let added = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            ("src/lib.rs", "", "pub async fn added() {}\n"),
        ]);
        assert!(
            added
                .added
                .iter()
                .any(|finding| finding.identity.name == "added")
        );
        assert!(
            added.unknown.is_empty(),
            "a one-sided compatible addition must not carry redundant opaque uncertainty: {:?}",
            added.findings()
        );

        let removed = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            ("src/lib.rs", "pub async fn removed() {}\n", ""),
        ]);
        assert!(
            removed
                .removed
                .iter()
                .any(|finding| finding.identity.name == "removed")
        );
        assert!(
            removed.unknown.is_empty(),
            "a confirmed removal must not carry redundant opaque uncertainty: {:?}",
            removed.findings()
        );
    }

    #[test]
    fn repository_backed_trait_default_presence_and_opaque_body_are_distinct_contracts() {
        let manifest = "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n";
        let lock = "version = 4\n";
        let removed_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { fn required() {} }\n",
                "pub trait Contract { fn required(); }\n",
            ),
        ]);
        assert!(
            removed_default
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Contract"),
            "removing a trait default makes downstream impls incomplete: {:?}",
            removed_default.findings()
        );

        let added_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { fn optional(); }\n",
                "pub trait Contract { fn optional() {} }\n",
            ),
        ]);
        assert!(
            added_default.findings().is_empty(),
            "adding a trait default is compatible: {:?}",
            added_default.findings()
        );

        let added_async_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { async fn optional(); }\n",
                "pub trait Contract { async fn optional() {} }\n",
            ),
        ]);
        assert!(
            added_async_default.findings().is_empty(),
            "adding an async trait default is compatible and must not leave a redundant opaque proof: {:?}",
            added_async_default.findings()
        );

        let cfg_mixed_default_swap = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { #[cfg(feature = \"a\")] fn f() {} #[cfg(not(feature = \"a\"))] fn f() {} fn g(); }\n",
                "pub trait Contract { #[cfg(feature = \"a\")] fn f(); #[cfg(not(feature = \"a\"))] fn f() {} fn g() {} }\n",
            ),
        ]);
        assert!(
            cfg_mixed_default_swap
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Contract"),
            "cfg-qualified default removal must not be hidden by a sibling default addition: {:?}",
            cfg_mixed_default_swap.findings()
        );

        let associated_const_added_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { const LIMIT: u8; }\n",
                "pub trait Contract { const LIMIT: u8 = 1; }\n",
            ),
        ]);
        assert!(
            associated_const_added_default.findings().is_empty(),
            "adding an associated-const default is compatible: {:?}",
            associated_const_added_default.findings()
        );

        for (before, after, label) in [
            (
                "pub trait Contract { const LIMIT: u8 = 1; }\n",
                "pub trait Contract { const LIMIT: u8; }\n",
                "associated-const default removal",
            ),
            (
                "pub trait Contract { const LIMIT: u8 = 1; }\n",
                "pub trait Contract { const LIMIT: u8 = 2; }\n",
                "associated-const default value change",
            ),
            (
                "pub trait Contract { #[cfg(feature = \"a\")] const LIMIT: u8; }\n",
                "pub trait Contract { #[cfg(feature = \"b\")] const LIMIT: u8 = 1; }\n",
                "associated-const cfg change",
            ),
        ] {
            let delta = repository_delta(&[
                ("Cargo.toml", manifest, manifest),
                ("Cargo.lock", lock, lock),
                ("src/lib.rs", before, after),
            ]);
            assert!(
                delta
                    .changed
                    .iter()
                    .any(|finding| finding.identity.name == "Contract"),
                "{label} must remain breaking: {:?}",
                delta.findings()
            );
        }

        let added_async_default_through_alias = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub mod inner { pub trait Contract { async fn optional(); } } pub use inner::Contract as Alias;\n",
                "pub mod inner { pub trait Contract { async fn optional() {} } } pub use inner::Contract as Alias;\n",
            ),
        ]);
        assert!(
            added_async_default_through_alias.findings().is_empty(),
            "a public trait alias must preserve the compatible default addition: {:?}",
            added_async_default_through_alias.findings()
        );

        let added_default_with_changed_sibling = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { async fn optional(); async fn fetch() { std::future::ready(()).await; } }\n",
                "pub trait Contract { async fn optional() {} async fn fetch() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } }\n",
            ),
        ]);
        assert!(
            added_default_with_changed_sibling
                .unknown
                .iter()
                .any(|finding| {
                    finding.identity.name == "OpaqueReturnAutoTraits"
                        && finding.unknown_reason.as_deref().is_some_and(|reason| {
                            reason.contains("opaque-return:trait-default:fetch")
                        })
                })
        );
        assert!(
            !added_default_with_changed_sibling
                .unknown
                .iter()
                .any(
                    |finding| finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains("opaque-return:trait-default:optional")
                    })
                ),
            "only the newly added optional default proof is redundant: {:?}",
            added_default_with_changed_sibling.findings()
        );

        let opaque_default = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { async fn fetch() { std::future::ready(()).await; } }\n",
                "pub trait Contract { async fn fetch() { let value = std::rc::Rc::new(()); std::future::ready(()).await; drop(value); } }\n",
            ),
        ]);
        assert!(opaque_default.unknown.iter().any(|finding| {
            finding.identity.name == "OpaqueReturnAutoTraits"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("opaque-return:trait-default:fetch"))
        }));
        assert!(opaque_default.changed.is_empty());

        let opaque_trait_impl = repository_delta(&[
            ("Cargo.toml", manifest, manifest),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub trait Contract { fn values(&self) -> impl Iterator<Item = u8>; } pub struct Owner; impl Contract for Owner { fn values(&self) -> impl Iterator<Item = u8> { std::iter::once(1) } }\n",
                "pub trait Contract { fn values(&self) -> impl Iterator<Item = u8>; } pub struct Owner; impl Contract for Owner { fn values(&self) -> impl Iterator<Item = u8> { let value = std::rc::Rc::new(1); std::iter::once_with(move || *value) } }\n",
            ),
        ]);
        assert!(opaque_trait_impl.unknown.iter().any(|finding| {
            finding.identity.name == "TraitImplResolution"
                && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("opaque-return:trait-impl:values"))
        }));
        assert!(opaque_trait_impl.changed.is_empty());
    }

    #[test]
    fn repository_backed_library_target_edition_and_cfg_authority_are_observable() {
        let lock = "version = 4\n";
        let edition_macro = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2021'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "#[macro_export] macro_rules! value { ($e:expr) => { $e } }\n",
                "#[macro_export] macro_rules! value { ($e:expr) => { $e } }\n",
            ),
        ]);
        assert!(edition_macro.changed.iter().any(|finding| {
            finding.identity.name == "value"
                && finding
                    .before
                    .as_ref()
                    .is_some_and(|side| side.contract.contains("definition-edition:2021"))
                && finding
                    .after
                    .as_ref()
                    .is_some_and(|side| side.contract.contains("definition-edition:2024"))
        }));

        let edition_without_macro = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2021'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            ("src/lib.rs", "pub fn stable() {}\n", "pub fn stable() {}\n"),
        ]);
        assert!(
            edition_without_macro.findings().is_empty(),
            "edition is item-local to exported macro semantics: {:?}",
            edition_without_macro.findings()
        );

        let inherited_edition = repository_delta(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers=['api']\n[workspace.package]\nedition='2021'\n",
                "[workspace]\nmembers=['api']\n[workspace.package]\nedition='2024'\n",
            ),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition.workspace=true\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='api'\nversion='0.0.0'\nedition.workspace=true\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "api/src/lib.rs",
                "#[macro_export] macro_rules! value { ($e:expr) => { $e } }\n",
                "#[macro_export] macro_rules! value { ($e:expr) => { $e } }\n",
            ),
        ]);
        assert!(
            inherited_edition
                .changed
                .iter()
                .any(|finding| finding.identity.name == "value"),
            "workspace-inherited edition must bind exported macro semantics: {:?}",
            inherited_edition.findings()
        );

        let target_edition_override = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2021'\n[lib]\npath='src/lib.rs'\nedition='2021'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2021'\n[lib]\npath='src/lib.rs'\nedition='2024'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "#[macro_export] macro_rules! value { ($e:expr) => { $e } }\n",
                "#[macro_export] macro_rules! value { ($e:expr) => { $e } }\n",
            ),
        ]);
        assert!(
            target_edition_override.changed.iter().any(|finding| {
                finding.identity.name == "value"
                    && finding
                        .before
                        .as_ref()
                        .is_some_and(|side| side.contract.contains("definition-edition:2021"))
                    && finding
                        .after
                        .as_ref()
                        .is_some_and(|side| side.contract.contains("definition-edition:2024"))
            }),
            "the library target edition overrides package.edition for exported macro semantics: {:?}",
            target_edition_override.findings()
        );

        let native_only = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "pub fn internal(value: u8) {}\n",
                "pub fn internal(value: u16) {}\n",
            ),
        ]);
        assert!(
            native_only.findings().is_empty(),
            "native-only libraries do not expose ordinary Rust dependency API: {:?}",
            native_only.findings()
        );

        let custom_cfg_changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "build.rs",
                "fn main() { println!(\"cargo::rustc-cfg=public_api\"); }\n",
                "fn main() {}\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(custom_cfg_changed.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("custom-cfg:")
                    && reason.contains("public_api")
                    && reason.contains("cfg-authority-digest:sha256:")
            })
        }));

        let custom_cfg_unchanged = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "build.rs",
                "fn main() { println!(\"cargo::rustc-cfg=public_api\"); }\n",
                "fn main() { println!(\"cargo::rustc-cfg=public_api\"); }\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(
            custom_cfg_unchanged.findings().is_empty(),
            "an unchanged complete cfg authority proof should neutralize: {:?}",
            custom_cfg_unchanged.findings()
        );

        let custom_cfg_build_disabled = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild=false\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild=false\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "build.rs",
                "fn main() { println!(\"cargo::rustc-cfg=public_api\"); }\n",
                "fn main() {}\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(custom_cfg_build_disabled.unknown.iter().all(|finding| {
            !finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("cfg-authority-digest:sha256:"))
        }));
        assert!(custom_cfg_build_disabled.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("cfg-authority-digest:unresolved:no-revision-backed-authority")
            })
        }));

        for (config_path, config, label) in [
            (
                ".cargo/config.toml",
                "[net]\noffline=true\n",
                "an unrelated root Cargo config",
            ),
            (
                "fixtures/.cargo/config.toml",
                "[build]\nrustflags=['--cfg','public_api']\n",
                "a nested fixture Cargo config",
            ),
            (
                ".cargo/config.toml",
                "[unrelated]\nrustflags=['--cfg','public_api']\n",
                "a lookalike key outside Cargo's config schema",
            ),
            (
                ".cargo/config.toml",
                "[env]\nRUSTFLAGS='--cfg public_api'\n",
                "Cargo child-process environment does not feed Cargo's rustflags input",
            ),
            (
                ".cargo/config.toml",
                "[target.x86_64-unknown-linux-gnu.native]\nrustc-cfg=['public_api']\n",
                "a links override without matching package.links",
            ),
        ] {
            let delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("Cargo.lock", lock, lock),
                (config_path, config, config),
                (
                    "src/lib.rs",
                    "#[cfg(public_api)] pub fn api() {}\n",
                    "#[cfg(public_api)] pub fn api() {}\n",
                ),
            ]);
            assert!(
                delta.unknown.iter().any(|finding| {
                    finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains(
                            "cfg-authority-digest:unresolved:no-revision-backed-authority",
                        )
                    })
                }),
                "{label} must not become complete cfg authority: {:?}",
                delta.findings()
            );
        }

        let root_cfg_authority = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                ".cargo/config.toml",
                "[build]\nrustflags=['--cfg','public_api']\n",
                "[build]\nrustflags=['--cfg','public_api']\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(
            root_cfg_authority.findings().is_empty(),
            "an unchanged effective root rustflags authority can neutralize: {:?}",
            root_cfg_authority.findings()
        );

        let target_cfg_rustflags = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                ".cargo/config.toml",
                "[target.'cfg(unix)']\nrustflags=['--cfg','public_api']\n",
                "[target.'cfg(unix)']\nrustflags=['--cfg','public_api']\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(
            target_cfg_rustflags.findings().is_empty(),
            "unchanged target-specific rustflags are revision-backed cfg authority: {:?}",
            target_cfg_rustflags.findings()
        );

        let extensionless_config_precedence = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                ".cargo/config",
                "[net]\noffline=true\n",
                "[net]\noffline=true\n",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags=['--cfg','public_api']\n",
                "[build]\nrustflags=['--cfg','public_api']\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(
            extensionless_config_precedence
                .unknown
                .iter()
                .any(|finding| {
                    finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains(
                            "cfg-authority-digest:unresolved:no-revision-backed-authority",
                        )
                    })
                }),
            "Cargo must ignore config.toml when extensionless config exists beside it: {:?}",
            extensionless_config_precedence.findings()
        );

        let missing_declared_build = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='missing.rs'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\nbuild='missing.rs'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(missing_declared_build.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("cfg-authority-digest:unresolved:build-script:")
            })
        }));

        let private_helper = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "#[cfg(private_helper)] fn helper() {} pub fn api() {}\n",
                "#[cfg(private_helper)] fn helper() {} pub fn api() {}\n",
            ),
        ]);
        assert!(
            private_helper.findings().is_empty(),
            "a non-exported private helper must not create permanent cfg uncertainty: {:?}",
            private_helper.findings()
        );

        let target_abi = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("Cargo.lock", lock, lock),
            (
                "src/lib.rs",
                "#[cfg(target_abi = \"eabihf\")] pub fn api() {}\n",
                "#[cfg(target_abi = \"eabihf\")] pub fn api() {}\n",
            ),
        ]);
        assert!(
            target_abi.findings().is_empty(),
            "rustc-provided target_abi is not custom cfg authority: {:?}",
            target_abi.findings()
        );
    }

    #[test]
    fn repository_backed_cargo_edge_contracts_do_not_false_clean() {
        let build_true = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nbuild=true\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nbuild=true\n[lib]\npath='src/lib.rs'\n",
            ),
            ("build.rs", "fn main() {}\n", "fn main() {}\n"),
            ("src/lib.rs", "pub fn removed(value: u8) {}\n", ""),
        ]);
        assert!(
            build_true
                .removed
                .iter()
                .any(|finding| finding.identity.name == "removed"),
            "Cargo-valid build=true must not hide the library API: {:?}",
            build_true.findings()
        );
        assert!(build_true.unknown.iter().all(|finding| {
            !finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("package.build=true"))
        }));

        let build_true_cfg_authority = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nbuild=true\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nbuild=true\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "build.rs",
                "fn main() { println!(\"cargo::rustc-cfg=public_api\"); }\n",
                "fn main() {}\n",
            ),
            (
                "src/lib.rs",
                "#[cfg(public_api)] pub fn api() {}\n",
                "#[cfg(public_api)] pub fn api() {}\n",
            ),
        ]);
        assert!(build_true_cfg_authority.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("custom-cfg:")
                    && reason.contains("public_api")
                    && reason.contains("cfg-authority-digest:sha256:")
            })
        }));

        for (manifest, label) in [
            (
                "[package]\nname='type'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "inferred keyword crate name",
            ),
            (
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\nname='type'\npath='src/lib.rs'\n",
                "explicit keyword library name",
            ),
        ] {
            let keyword = repository_delta(&[
                ("Cargo.toml", manifest, manifest),
                (
                    "src/lib.rs",
                    "pub fn api(value: u8) {}\n",
                    "pub fn api(value: u16) {}\n",
                ),
            ]);
            assert!(
                keyword.changed.iter().any(|finding| {
                    finding.identity.crate_name == "type" && finding.identity.name == "api"
                }),
                "{label} must retain real API facts: {:?}",
                keyword.findings()
            );
            assert!(keyword.unknown.iter().all(|finding| {
                !finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("ManifestParse"))
            }));
        }

        let associated_native_exports = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
            ),
            (
                "src/lib.rs",
                "pub struct Api; pub trait Exported { extern \"C\" fn call(value: u8); } impl Api { #[unsafe(no_mangle)] pub extern \"C\" fn exported(value: u8) {} } impl Exported for Api { #[unsafe(export_name=\"call_v1\")] extern \"C\" fn call(value: u8) {} }\n",
                "pub struct Api; pub trait Exported { extern \"C\" fn call(value: u16); } impl Api { #[unsafe(no_mangle)] pub extern \"C\" fn exported(value: u16) {} } impl Exported for Api { #[unsafe(export_name=\"call_v2\")] extern \"C\" fn call(value: u16) {} }\n",
            ),
        ]);
        assert!(associated_native_exports.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("Api::exported") && reason.contains("binary-export")
            })
        }));
        assert!(associated_native_exports.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("<Api as Exported>::call") && reason.contains("binary-export")
            })
        }));

        let mixed_private_owner = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib', 'cdylib']\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib', 'cdylib']\n",
            ),
            (
                "src/lib.rs",
                "struct Hidden; impl Hidden { #[unsafe(no_mangle)] pub extern \"C\" fn exported(value: u8) {} }\n",
                "struct Hidden; impl Hidden { #[unsafe(no_mangle)] pub extern \"C\" fn exported(value: u16) {} }\n",
            ),
        ]);
        assert!(mixed_private_owner.unknown.iter().any(|finding| {
            finding.unknown_reason.as_deref().is_some_and(|reason| {
                reason.contains("Hidden::exported") && reason.contains("binary-export")
            })
        }));

        let mixed_private_module_direct_export = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib', 'cdylib']\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['rlib', 'cdylib']\n",
            ),
            (
                "src/lib.rs",
                "mod hidden { #[unsafe(export_name=\"shared\")] pub extern \"C\" fn exported(value: u8) {} }\n",
                "mod hidden { #[unsafe(export_name=\"shared\")] pub extern \"C\" fn exported(value: u16) {} }\n",
            ),
        ]);
        assert!(
            mixed_private_module_direct_export
                .unknown
                .iter()
                .any(|finding| {
                    finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains("public-owner:exported") && reason.contains("binary-export")
                    })
                })
        );

        let alpha_renamed_owner = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\ncrate-type=['cdylib']\n",
            ),
            (
                "src/lib.rs",
                "pub struct Holder<T>(T); impl<T> Holder<T> { #[unsafe(export_name=\"shared\")] pub extern \"C\" fn exported() {} }\n",
                "pub struct Holder<U>(U); impl<U> Holder<U> { #[unsafe(export_name=\"shared\")] pub extern \"C\" fn exported() {} }\n",
            ),
        ]);
        assert!(
            alpha_renamed_owner.findings().is_empty(),
            "pure impl binder renames must not change native export evidence: {:?}",
            alpha_renamed_owner.findings()
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_backed_symlink_library_root_is_non_neutralizable_unknown() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@test.com").unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.0.0'\n",
        )
        .unwrap();
        fs::write(tmp.path().join("shared.rs"), "pub fn api(value: u8) {}\n").unwrap();
        std::os::unix::fs::symlink("../shared.rs", tmp.path().join("src/lib.rs")).unwrap();

        let mut index = git.index().unwrap();
        index.add_path(Path::new("Cargo.toml")).unwrap();
        index.add_path(Path::new("shared.rs")).unwrap();
        index.add_path(Path::new("src/lib.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let base = git
            .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
            .unwrap();
        drop(tree);

        fs::write(tmp.path().join("shared.rs"), "pub fn api(value: u16) {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("shared.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let parent = git.find_commit(base).unwrap();
        let target = git
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "target",
                &tree,
                &[&parent],
            )
            .unwrap();
        drop(tree);
        drop(parent);
        drop(git);

        let repo = Repository::open(tmp.path()).unwrap();
        let delta = compare_rust_api_revisions(
            &repo,
            &[make_diff_with_ids(
                base.to_string(),
                target.to_string(),
                Vec::new(),
            )],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            delta.unknown.len(),
            2,
            "the same unresolved symlink proof must stay visible on both sides: {:?}",
            delta.findings()
        );
        assert!(delta.unknown.iter().all(|finding| {
            finding
                .unknown_reason
                .as_deref()
                .is_some_and(|reason| reason.contains(NON_NEUTRALIZABLE_SYMLINK_ROOT))
        }));
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
                && finding.unknown_reason.as_deref().is_some_and(|reason| {
                    reason.contains("resolved-observable-impls:")
                        && reason.contains("resolved-trait:")
                        && reason.contains("Marker")
                        && reason.contains("resolved-owner:")
                        && reason.contains("Value")
                })
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
    fn repository_backed_trait_impl_alias_retargets_change_canonical_evidence() {
        for (base, target, reason) in [
            (
                "pub trait A {} pub trait B {} pub struct Value; \
                 use A as TraitAlias; impl TraitAlias for Value {}\n",
                "pub trait A {} pub trait B {} pub struct Value; \
                 use B as TraitAlias; impl TraitAlias for Value {}\n",
                "trait alias retarget",
            ),
            (
                "pub trait Marker {} pub struct X; pub struct Y; \
                 use X as Owner; impl Marker for Owner {}\n",
                "pub trait Marker {} pub struct X; pub struct Y; \
                 use Y as Owner; impl Marker for Owner {}\n",
                "owner alias retarget",
            ),
            (
                "pub trait A {} pub trait B {} pub struct Value; \
                 #[cfg(unix)] use A as TraitAlias; \
                 #[cfg(windows)] use B as TraitAlias; \
                 impl TraitAlias for Value {}\n",
                "pub trait A {} pub trait B {} pub struct Value; \
                 #[cfg(unix)] use B as TraitAlias; \
                 #[cfg(windows)] use A as TraitAlias; \
                 impl TraitAlias for Value {}\n",
                "cfg-selected trait alias swap",
            ),
        ] {
            let delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", base, target),
            ]);
            assert!(
                delta
                    .unknown
                    .iter()
                    .any(|finding| finding.identity.name == "TraitImplResolution"),
                "{reason} must alter the resolved impl proof: {:?}",
                delta.findings()
            );
        }
    }

    #[test]
    fn repository_backed_trait_impl_alias_spelling_is_semantically_stable() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker {} mod model { pub struct Hidden; } \
                 use model::Hidden as Alias; pub use model::Hidden as PublicHidden; \
                 impl Marker for Alias {}\n",
                "pub trait Marker {} mod model { pub struct Hidden; } \
                 use model::Hidden as Alias; pub use model::Hidden as PublicHidden; \
                 impl Marker for model::Hidden {}\n",
            ),
        ]);
        assert!(
            delta.findings().is_empty(),
            "alias and canonical owner spellings describe the same observable impl: {:?}",
            delta.findings()
        );
    }

    #[test]
    fn repository_backed_trait_impl_wrapped_owner_spelling_is_semantically_stable() {
        for (alias_owner, canonical_owner) in [
            ("&Alias", "&model::Hidden"),
            ("*const Alias", "*const model::Hidden"),
            ("[Alias]", "[model::Hidden]"),
            ("[Alias; 1]", "[model::Hidden; 1]"),
        ] {
            let base = format!(
                "pub trait Marker {{}} mod model {{ pub struct Hidden; }} \
                 use model::Hidden as Alias; pub use model::Hidden as PublicHidden; \
                 impl Marker for {alias_owner} {{}}\n"
            );
            let target = format!(
                "pub trait Marker {{}} mod model {{ pub struct Hidden; }} \
                 use model::Hidden as Alias; pub use model::Hidden as PublicHidden; \
                 impl Marker for {canonical_owner} {{}}\n"
            );
            let delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", &base, &target),
            ]);
            assert!(
                delta.findings().is_empty(),
                "wrapped owner alias and canonical spelling must match ({alias_owner}): {:?}",
                delta.findings()
            );
        }
    }

    #[test]
    fn repository_backed_trait_impl_item_order_is_semantically_stable() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker { type Out; const N: usize; fn value() -> u8; } \
                 pub struct Value; impl Marker for Value { \
                 type Out = u8; const N: usize = 1; fn value() -> u8 { 1 } }\n",
                "pub trait Marker { type Out; const N: usize; fn value() -> u8; } \
                 pub struct Value; impl Marker for Value { \
                 fn value() -> u8 { 1 } const N: usize = 1; type Out = u8; }\n",
            ),
        ]);
        assert!(
            delta.findings().is_empty(),
            "reordering ordinary associated impl items must not change evidence: {:?}",
            delta.findings()
        );
    }

    #[test]
    fn repository_backed_trait_impl_relative_type_move_stays_fail_closed() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Marker { type Out; } pub struct Value; \
                 pub mod a { pub struct Local; impl crate::Marker for crate::Value { type Out = Local; } } \
                 pub mod b { pub struct Local; }\n",
                "pub trait Marker { type Out; } pub struct Value; \
                 pub mod a { pub struct Local; } \
                 pub mod b { pub struct Local; impl crate::Marker for crate::Value { type Out = Local; } }\n",
            ),
        ]);
        assert!(
            delta
                .unknown
                .iter()
                .any(|finding| finding.identity.name == "TraitImplResolution"),
            "declaring scope must remain in the proof until relative names are compiler-resolved: {:?}",
            delta.findings()
        );
    }

    #[test]
    fn repository_backed_std_display_impl_is_typed_unknown() {
        let added = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Value;\n",
                "pub struct Value;\nimpl std::fmt::Display for Value {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, \"v\") }\n}\n",
            ),
        ]);
        assert!(
            added.unknown.iter().any(|finding| {
                finding.identity.name == "TraitImplResolution"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("Display"))
            }),
            "adding Display for a public type must be TraitImplResolution, got {:?}",
            added.findings()
        );
    }

    #[test]
    fn repository_backed_public_local_trait_impl_for_external_owner_is_typed_unknown() {
        let removed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Extension {}\nimpl Extension for std::string::String {}\n",
                "pub trait Extension {}\n",
            ),
        ]);
        assert!(
            removed.unknown.iter().any(|finding| {
                finding.identity.name == "TraitImplResolution"
                    && finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains("Extension") && reason.contains("String")
                    })
            }),
            "a public local trait impl for an external owner must not disappear: {:?}",
            removed.findings()
        );

        let private_control = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "trait Internal {}\nimpl Internal for std::string::String {}\n",
                "trait Internal {}\n",
            ),
        ]);
        assert!(
            private_control.findings().is_empty(),
            "a private local trait impl does not expose API even when its owner is external"
        );
    }

    #[test]
    fn repository_backed_public_trait_impl_for_reference_owner_is_typed_unknown() {
        let removed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Extension {}\npub struct Public;\nimpl Extension for &Public {}\n",
                "pub trait Extension {}\npub struct Public;\n",
            ),
        ]);
        assert!(
            removed.unknown.iter().any(|finding| {
                finding.identity.name == "TraitImplResolution"
                    && finding.unknown_reason.as_deref().is_some_and(|reason| {
                        reason.contains("Extension") && reason.contains("Public")
                    })
            }),
            "a public trait impl for a non-path owner must not disappear: {:?}",
            removed.findings()
        );

        let private_control = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "trait Internal {}\npub struct Public;\nimpl Internal for &Public {}\n",
                "trait Internal {}\npub struct Public;\n",
            ),
        ]);
        assert!(private_control.findings().is_empty());

        let private_owner_control = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub trait Extension {}\nstruct Private;\nimpl Extension for &Private {}\n",
                "pub trait Extension {}\nstruct Private;\n",
            ),
        ]);
        assert!(
            private_owner_control.findings().is_empty(),
            "a public trait impl for a private referenced owner is not public API"
        );
    }

    #[test]
    fn repository_backed_generic_bound_order_is_semantic_noop() {
        let reordered = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub fn call<T: Send + Sync, U>() where U: Clone + Copy, T: 'static {}\n",
                "pub fn call<T: Sync + Send, U>() where T: 'static, U: Copy + Clone {}\n",
            ),
        ]);
        assert!(
            reordered.findings().is_empty(),
            "reordering equivalent generic bounds and where predicates is not an API change: {:?}",
            reordered.findings()
        );

        let associated_reordered = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Public;\nimpl Public { pub fn call<T: Send + Sync, U>() where U: Clone + Copy, T: 'static {} }\n",
                "pub struct Public;\nimpl Public { pub fn call<T: Sync + Send, U>() where T: 'static, U: Copy + Clone {} }\n",
            ),
        ]);
        assert!(
            associated_reordered.findings().is_empty(),
            "associated-item contracts must also ignore bound ordering: {:?}",
            associated_reordered.findings()
        );

        let changed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub fn call<T: Send + Sync>() {}\n",
                "pub fn call<T: Send + Unpin>() {}\n",
            ),
        ]);
        assert!(
            changed
                .changed
                .iter()
                .any(|finding| finding.identity.name == "call"),
            "changing one bound must remain observable"
        );
    }

    #[test]
    fn repository_backed_unqualified_imported_trait_impl_is_typed_unknown() {
        let removed = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "use serde::Serialize;\npub struct Value;\nimpl Serialize for Value {}\n",
                "use serde::Serialize;\npub struct Value;\n",
            ),
        ]);
        assert!(
            removed.unknown.iter().any(|finding| {
                finding.identity.name == "TraitImplResolution"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("Serialize"))
            }),
            "an unqualified imported trait impl must not disappear: {:?}",
            removed.findings()
        );

        let local_private_control = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "trait Internal {}\npub struct Value;\nimpl Internal for Value {}\n",
                "trait Internal {}\npub struct Value;\n",
            ),
        ]);
        assert!(
            local_private_control.findings().is_empty(),
            "a proven local private trait does not degrade public API"
        );
    }

    #[test]
    fn repository_backed_private_field_auto_trait_change_is_parent_changed() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub struct Holder { inner: u8 }\n",
                "pub struct Holder { inner: std::rc::Rc<()> }\n",
            ),
        ]);
        assert!(
            delta
                .changed
                .iter()
                .any(|finding| finding.identity.name == "Holder"),
            "replacing a private Send field with Rc must change the parent contract, got {:?}",
            delta.findings()
        );
    }

    #[test]
    fn repository_backed_transitive_private_type_change_is_typed_unknown() {
        for (base, target, public_name, reason) in [
            (
                "struct Hidden(u8); pub struct Holder { inner: Hidden }\n",
                "struct Hidden(std::rc::Rc<()>); pub struct Holder { inner: Hidden }\n",
                "Holder",
                "a private local type can change the public parent's auto traits",
            ),
            (
                "struct Leaf(u8); struct Hidden(Leaf); #[repr(C)] pub struct Holder { inner: Hidden }\n",
                "struct Leaf(u16); struct Hidden(Leaf); #[repr(C)] pub struct Holder { inner: Hidden }\n",
                "Holder",
                "a transitive private local type can change repr(C) layout",
            ),
            (
                "mod model { pub struct Hidden(u8); } use model::Hidden; pub enum Public { Value(Hidden) }\n",
                "mod model { pub struct Hidden(std::rc::Rc<()>); } use model::Hidden; pub enum Public { Value(Hidden) }\n",
                "Public",
                "a private import and private module can hide an enum payload's auto-trait change",
            ),
            (
                "struct Hidden; unsafe impl Send for Hidden {} pub fn make() -> Hidden { Hidden }\n",
                "struct Hidden; pub fn make() -> Hidden { Hidden }\n",
                "make",
                "a local trait impl can change a private return type's compiler-derived semantics",
            ),
            (
                "mod model { pub struct Hidden; } use model::Hidden as Alias; unsafe impl Send for Alias {} pub fn make() -> model::Hidden { model::Hidden }\n",
                "mod model { pub struct Hidden; } use model::Hidden as Alias; pub fn make() -> model::Hidden { model::Hidden }\n",
                "make",
                "trait impl evidence indexed under a private alias must reach the canonical owner",
            ),
            (
                "mod model { pub struct Hidden; } use model as alias; unsafe impl Send for alias::Hidden {} pub fn make() -> model::Hidden { model::Hidden }\n",
                "mod model { pub struct Hidden; } use model as alias; pub fn make() -> model::Hidden { model::Hidden }\n",
                "make",
                "trait impl evidence indexed through a private module alias must reach the canonical owner",
            ),
            (
                "mod model { pub struct Hidden; } mod helper { struct Leaf(u8); trait Local { type Out; } impl Local for crate::model::Hidden { type Out = Leaf; } } use model::Hidden; pub fn make() -> Hidden { Hidden }\n",
                "mod model { pub struct Hidden; } mod helper { struct Leaf(std::rc::Rc<()>); trait Local { type Out; } impl Local for crate::model::Hidden { type Out = Leaf; } } use model::Hidden; pub fn make() -> Hidden { Hidden }\n",
                "make",
                "private types used only by a cross-module local impl must resolve from the impl declaration module",
            ),
            (
                "mod model { pub struct Hidden(u8); } mod helper { pub use crate::model::Hidden as Alias; } pub struct Holder { inner: helper::Alias }\n",
                "mod model { pub struct Hidden(std::rc::Rc<()>); } mod helper { pub use crate::model::Hidden as Alias; } pub struct Holder { inner: helper::Alias }\n",
                "Holder",
                "a public use inside an unreachable module is still a private type alias",
            ),
            (
                "mod model { pub struct Hidden(u8); } use model as alias; pub struct Holder { inner: alias::Hidden }\n",
                "mod model { pub struct Hidden(std::rc::Rc<()>); } use model as alias; pub struct Holder { inner: alias::Hidden }\n",
                "Holder",
                "a private module-prefix alias must resolve the dependent type",
            ),
            (
                "extern crate self as alias; struct Hidden(u8); pub struct Api(pub alias::Hidden);\n",
                "extern crate self as alias; struct Hidden(std::rc::Rc<()>); pub struct Api(pub alias::Hidden);\n",
                "Api",
                "an extern-crate self alias must resolve the private root type in Git-tree revisions",
            ),
            (
                "#[cfg(unix)] pub struct Hidden(u8); #[cfg(windows)] struct Hidden(u8); #[cfg(windows)] pub fn make() -> Hidden { Hidden(0) }\n",
                "#[cfg(unix)] pub struct Hidden(u8); #[cfg(windows)] struct Hidden(std::rc::Rc<()>); #[cfg(windows)] pub fn make() -> Hidden { Hidden(std::rc::Rc::new(())) }\n",
                "make",
                "a cfg-disjoint public origin must not hide a private dependent declaration",
            ),
            (
                "#[cfg(feature = \"public\")] pub struct Hidden(u8); #[cfg(not(feature = \"public\"))] struct Hidden(u8); #[cfg(not(feature = \"public\"))] pub fn make() -> Hidden { Hidden(0) }\n",
                "#[cfg(feature = \"public\")] pub struct Hidden(u8); #[cfg(not(feature = \"public\"))] struct Hidden(std::rc::Rc<()>); #[cfg(not(feature = \"public\"))] pub fn make() -> Hidden { Hidden(std::rc::Rc::new(())) }\n",
                "make",
                "a direct cfg atom and its negation are disjoint private/public origins",
            ),
            (
                "struct Hidden; type Alias = Hidden; unsafe impl Send for Alias {} pub fn make() -> Hidden { Hidden }\n",
                "struct Hidden; type Alias = Hidden; pub fn make() -> Hidden { Hidden }\n",
                "make",
                "trait impl evidence reached through a private type alias must fingerprint the nominal owner",
            ),
        ] {
            let delta = repository_delta(&[
                (
                    "Cargo.toml",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                    "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", base, target),
            ]);
            assert!(
                delta.unknown.iter().any(|finding| {
                    finding.identity.name == "PrivateTypeDependency"
                        && finding
                            .unknown_reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains(public_name))
                }),
                "{reason} must remain a typed unknown until compiler-backed semantics exist; got {:?}",
                delta.findings()
            );
            assert!(
                !delta
                    .changed
                    .iter()
                    .any(|finding| finding.identity.name == public_name),
                "private compiler-derived semantics must not be misclassified as confirmed"
            );
        }

        let unrelated_change = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "struct Hidden(u8); pub struct Holder { inner: Hidden } pub fn f(value: u8) {}\n",
                "struct Hidden(u8); pub struct Holder { inner: Hidden } pub fn f(value: u16) {}\n",
            ),
        ]);
        assert!(
            unrelated_change
                .changed
                .iter()
                .any(|finding| finding.identity.name == "f"),
            "an unchanged private dependency must not contaminate unrelated known API changes: {:?}",
            unrelated_change.findings()
        );
        assert!(unrelated_change.unknown.is_empty());

        let cfg_disjoint_impl = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "struct Hidden; #[cfg(unix)] pub fn make() -> Hidden { Hidden } #[cfg(windows)] unsafe impl Send for Hidden {}\n",
                "struct Hidden; #[cfg(unix)] pub fn make() -> Hidden { Hidden }\n",
            ),
        ]);
        assert!(
            cfg_disjoint_impl.findings().is_empty(),
            "a cfg-disjoint private impl must not contaminate another target family: {:?}",
            cfg_disjoint_impl.findings()
        );
    }

    #[test]
    fn repository_backed_private_alias_cfg_target_swap_stays_typed_unknown() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod a { pub struct Item; } mod b { pub struct Item; } \
                 #[cfg(unix)] use a::Item as Hidden; \
                 #[cfg(windows)] use b::Item as Hidden; \
                 pub fn make() -> Hidden { todo!() }\n",
                "mod a { pub struct Item; } mod b { pub struct Item; } \
                 #[cfg(unix)] use b::Item as Hidden; \
                 #[cfg(windows)] use a::Item as Hidden; \
                 pub fn make() -> Hidden { todo!() }\n",
            ),
        ]);
        let private_dependency_rows = delta
            .unknown
            .iter()
            .filter(|finding| {
                finding.identity.name == "PrivateTypeDependency"
                    && finding
                        .unknown_reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("make"))
            })
            .count();
        assert_eq!(
            private_dependency_rows,
            2,
            "base and target cfg mappings must remain unmatched typed proofs: {:?}",
            delta.findings()
        );
        assert!(
            !delta
                .changed
                .iter()
                .any(|finding| finding.identity.name == "make"),
            "source-only private semantics must not be promoted to a confirmed change"
        );
    }

    #[test]
    fn repository_backed_private_impl_alias_spelling_is_semantically_stable() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod model { pub struct Hidden; } use model::Hidden as Alias; \
                 trait Local {} impl Local for Alias {} \
                 pub fn make() -> model::Hidden { model::Hidden }\n",
                "mod model { pub struct Hidden; } use model::Hidden as Alias; \
                 trait Local {} impl Local for model::Hidden {} \
                 pub fn make() -> model::Hidden { model::Hidden }\n",
            ),
        ]);
        assert!(
            delta.findings().is_empty(),
            "impl owner spelling must not change private dependency evidence: {:?}",
            delta.findings()
        );
    }

    #[test]
    fn repository_backed_exhausted_alias_graphs_keep_distinct_evidence() {
        let delta = repository_delta(&[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
                "[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "mod a { pub mod b { pub struct Item; } pub mod c { pub struct Item; } } \
                 use a::b as a; trait Local {} impl Local for a::Item {}\n",
                "mod a { pub mod b { pub struct Item; } pub mod c { pub struct Item; } } \
                 use a::c as a; trait Local {} impl Local for a::Item {}\n",
            ),
        ]);
        assert!(
            delta.unknown.iter().any(|finding| {
                matches!(
                    finding.identity.name.as_str(),
                    "PrivateTypeDependency" | "TraitImplResolution"
                ) && finding
                    .unknown_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("finite graph bound"))
            }),
            "different finite-budget failures must not neutralize: {:?}",
            delta.findings()
        );
    }

    #[test]
    fn identical_exhausted_alias_proofs_never_match() {
        let base = snapshot_rust_api(&MemorySource::source("", "base"));
        let target = snapshot_rust_api(&MemorySource::source("", "target"));
        let make_unknown = |provenance: RevisionProvenance, exhausted: bool| RustApiUnknown {
            kind: RustApiUnknownKind::PrivateTypeDependency,
            crate_name: Some("fixture".to_owned()),
            module_path: vec![],
            source_path: "src/lib.rs".to_owned(),
            cfg_guard: vec![],
            evidence: "same proof; literal: alias resolution exceeded its finite graph bound"
                .to_owned(),
            resolution_exhausted: exhausted,
            provenance,
        };
        let left = make_unknown(base.provenance.clone(), true);
        let right = make_unknown(target.provenance.clone(), true);
        assert!(
            !unknown_proofs_match(&base, &left, &target, &right),
            "an exhausted partial proof never establishes semantic equality"
        );

        let left_literal_only = make_unknown(base.provenance.clone(), false);
        let right_literal_only = make_unknown(target.provenance.clone(), false);
        assert!(
            unknown_proofs_match(&base, &left_literal_only, &target, &right_literal_only),
            "evidence text alone must not masquerade as structural exhaustion"
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
