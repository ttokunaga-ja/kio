//! Typed ownership authority for local image CAS objects.
//!
//! Markdown and provider metadata describe a document; neither can authorize
//! a local image-object read.  The immutable normalized-unit object records
//! the decoded image hashes its adapter actually persisted.  This module
//! derives the current policy and secret-send consequences from those pinned
//! objects and their complete retained owner aliases.

use std::collections::{BTreeMap, BTreeSet};

use kio_core::cas::is_hash;
use kio_core::scope::Repository;
use kio_core::{KioError, Result};
use kio_pipeline::policy::CurrentPolicyEvaluator;
use kio_pipeline::scan::classify_secret;

use crate::historical_reindex::{RetainedNormalizedInstance, pinned_done_units};
use crate::object_persistence::pipeline_to_kio;

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
    // A global owned hash permits access, but cannot turn another unit's
    // Markdown link into an authenticated image citation.
    unit_images: BTreeMap<(String, String, u64, String, String), BTreeSet<String>>,
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

    pub(crate) fn unit_owns_image(
        &self,
        raw_hash: &str,
        profile_hash: &str,
        generation: u64,
        manifest_hash: &str,
        unit_key: &str,
        image_hash: &str,
    ) -> bool {
        self.unit_images
            .get(&(
                raw_hash.to_owned(),
                profile_hash.to_owned(),
                generation,
                manifest_hash.to_owned(),
                unit_key.to_owned(),
            ))
            .is_some_and(|images| images.contains(image_hash))
    }

    pub(crate) fn allowed_hashes(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter_map(|(hash, entry)| entry.policy_allowed.then_some(hash.as_str()))
    }

    #[cfg(test)]
    pub(crate) fn test_map(entries: BTreeMap<String, ImageAuthorityEntry>) -> Self {
        Self {
            entries,
            unit_images: BTreeMap::new(),
        }
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
            if policy_allowed && !unit.owned_image_hashes.is_empty() {
                authority
                    .unit_images
                    .entry((
                        instance.raw_hash.clone(),
                        instance.normalize.tool_profile_hash.clone(),
                        instance.normalize.r#gen,
                        instance.normalize.manifest_hash.clone(),
                        unit.unit_key.clone(),
                    ))
                    .or_default()
                    .extend(unit.owned_image_hashes.iter().cloned());
            }
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
        let repo = match crate::management::initialize_explicit_root(
            root.path(),
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap()
        {
            crate::management::ExplicitRoot::Created(repo) => repo,
            crate::management::ExplicitRoot::Existing(_) => panic!("fresh root must be created"),
        };
        let policy = crate::search::scope::current_embedding_policy(&repo).unwrap();
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
    fn image_citation_requires_exact_owning_unit_and_manifest() {
        use kio_core::cas::hash_bytes;
        use std::collections::{BTreeMap, BTreeSet};
        let source = hash_bytes(b"scan.png");
        let image = hash_bytes(b"verified image pixels");
        let profile = hash_bytes(b"normalizer profile");
        let manifest = hash_bytes(b"pinned owner manifest");
        let authority = ImageAuthorityMap {
            entries: BTreeMap::from([(
                image.clone(),
                ImageAuthorityEntry {
                    policy_allowed: true,
                    requires_secret_approval: false,
                },
            )]),
            unit_images: BTreeMap::from([(
                (
                    source.clone(),
                    profile.clone(),
                    1,
                    manifest.clone(),
                    "image:1".to_owned(),
                ),
                BTreeSet::from([image.clone()]),
            )]),
        };
        assert!(
            authority.permits(&image),
            "the retained source owns these pixels"
        );
        assert!(authority.unit_owns_image(&source, &profile, 1, &manifest, "image:1", &image));
        assert!(
            !authority.unit_owns_image(
                &hash_bytes(b"public-citation.md"),
                &profile,
                1,
                &manifest,
                "image:1",
                &image
            ),
            "a public Markdown link does not acquire another unit's owned image"
        );
        assert!(
            !authority.unit_owns_image(
                &source,
                &profile,
                1,
                &hash_bytes(b"other manifest"),
                "image:1",
                &image
            ),
            "a same-generation retry cannot borrow ownership from another manifest"
        );
        assert!(
            !authority.unit_owns_image(&source, &profile, 1, &manifest, "image:2", &image),
            "another unit in one manifest cannot supply this image citation"
        );
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
        let authority = ImageAuthorityMap {
            entries,
            ..ImageAuthorityMap::default()
        };
        assert!(
            authority
                .permits("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(authority.requires_secret_approval(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
    }
}
