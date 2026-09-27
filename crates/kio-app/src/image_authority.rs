//! Typed ownership authority for local image CAS objects.
//!
//! Markdown and provider metadata describe a document; neither can authorize
//! a local image-object read.  The immutable normalized-unit object records
//! the decoded image hashes its adapter actually persisted.  This module
//! derives the current policy and secret-send consequences from those pinned
//! objects and their complete retained owner aliases.

use std::collections::BTreeMap;

use kio_core::cas::is_hash;
use kio_core::scope::Repository;
use kio_core::{KioError, Result};
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::scan::classify_secret;

use crate::historical_reindex::{RetainedNormalizedInstance, pinned_done_units};
use crate::pipeline_to_kio;

/// Current authority for one typed, immutable image CAS object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImageAuthorityEntry {
    /// At least one retained owner alias remains in the current policy.
    pub(crate) policy_allowed: bool,
    /// Any retained alias is secret. This intentionally remains true when a
    /// separate public alias is policy-allowed, so that aliases cannot lower
    /// the approval required to disclose shared image bytes.
    pub(crate) requires_secret_approval: bool,
}

/// Image authority derived only from pinned normalized units authenticated by
/// retained tree entries. Keys absent from this map have no authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ImageAuthorityMap {
    entries: BTreeMap<String, ImageAuthorityEntry>,
}

impl ImageAuthorityMap {
    pub(crate) fn permits(&self, hash: &str) -> bool {
        self.entries
            .get(hash)
            .is_some_and(|entry| entry.policy_allowed)
    }

    pub(crate) fn requires_secret_approval(&self, hash: &str) -> bool {
        self.entries
            .get(hash)
            .is_some_and(|entry| entry.requires_secret_approval)
    }

    pub(crate) fn allowed_hashes(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter_map(|(hash, entry)| entry.policy_allowed.then_some(hash.as_str()))
    }

    #[cfg(test)]
    pub(crate) fn test_map(entries: BTreeMap<String, ImageAuthorityEntry>) -> Self {
        Self { entries }
    }
}

/// Build the image authority from the retained source graph. `pinned_done_units`
/// verifies the exact tree-pinned manifest and immutable normalized-unit CAS
/// object before exposing typed ownership, preventing a mutable cache or
/// arbitrary Markdown from adding an owner.
pub(crate) fn retained_image_authority(
    repo: &Repository,
    retained_instances: &[RetainedNormalizedInstance],
    policy: &CurrentPolicyEvaluator,
) -> Result<ImageAuthorityMap> {
    let mut authority = ImageAuthorityMap::default();
    for instance in retained_instances {
        // Evaluate every retained name. A current public name may make bytes
        // available, but cannot erase a historical/current secret alias.
        let mut policy_allowed = false;
        let mut requires_secret_approval = false;
        for path in &instance.policy_paths {
            policy_allowed |= policy.allows_path(path).map_err(pipeline_to_kio)?;
            requires_secret_approval |= classify_secret(path).is_some();
        }

        let units = match pinned_done_units(repo, &instance.raw_hash, &instance.normalize) {
            Ok(units) => units,
            Err(error) => {
                // Unit absence may be wrapped as a pipeline corruption error.
                // The verifier independently proves actual CAS absence and
                // checks every surviving body; error text is not authority.
                if crate::verify_objects::purge_explains_missing_retained_instance(
                    repo,
                    &instance.raw_hash,
                    &instance.normalize,
                )? {
                    // A completed purge can leave an immutable historical
                    // reference after re-ingest retires its marker. Omit that
                    // verified old closure entirely: absent bytes establish
                    // no image ownership, even when some bodies survived.
                    continue;
                }
                return Err(error);
            }
        };
        for unit in units {
            for image_hash in unit.owned_image_hashes {
                if !is_hash(&image_hash) {
                    return Err(KioError::schema(
                        "retained image source unit has a non-canonical owned image hash",
                    ));
                }
                let entry = authority.entries.entry(image_hash).or_default();
                entry.policy_allowed |= policy_allowed;
                entry.requires_secret_approval |= requires_secret_approval;
            }
        }
    }
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use super::{ImageAuthorityEntry, ImageAuthorityMap};

    #[test]
    fn retained_authority_rejects_missing_or_corrupt_unexplained_manifest() {
        use std::collections::BTreeSet;

        use kio_core::cas::{ContentObjectKind, hash_bytes};
        use kio_core::dag::NormalizeRef;

        let root = tempfile::tempdir().unwrap();
        let repo = match crate::initialize_explicit_root(root.path()).unwrap() {
            crate::ExplicitRoot::Created(repo) => repo,
            crate::ExplicitRoot::Existing(_) => panic!("fresh root must be created"),
        };
        let policy = crate::current_embedding_policy(&repo).unwrap();
        let mut instance = crate::historical_reindex::RetainedNormalizedInstance {
            raw_hash: hash_bytes(b"raw"),
            normalize: NormalizeRef {
                tool_profile_hash: hash_bytes(b"profile"),
                r#gen: 0,
                manifest_hash: hash_bytes(b"absent manifest"),
            },
            raw_path: "source.md".to_owned(),
            policy_paths: BTreeSet::from(["source.md".to_owned()]),
            introductions: Vec::new(),
        };
        let missing =
            super::retained_image_authority(&repo, std::slice::from_ref(&instance), &policy)
                .unwrap_err();
        assert_eq!(missing.error_code(), "KIO-E-STORE-NOT-FOUND-001");

        instance.normalize.manifest_hash = repo
            .object_store()
            .write_content_object(ContentObjectKind::Manifest, b"not a manifest")
            .unwrap();
        let corrupt = super::retained_image_authority(&repo, &[instance], &policy).unwrap_err();
        assert_eq!(corrupt.error_code(), "KIO-E-STORE-CORRUPT-001");
    }

    #[test]
    fn public_alias_does_not_remove_secret_requirement() {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            ImageAuthorityEntry {
                policy_allowed: true,
                requires_secret_approval: true,
            },
        );
        let authority = ImageAuthorityMap { entries };
        assert!(
            authority
                .permits("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(authority.requires_secret_approval(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
    }
}
