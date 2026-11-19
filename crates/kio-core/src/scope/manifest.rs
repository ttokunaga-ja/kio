//! Streamed mutable manifest metadata. HEAD remains the source for live rows.
use super::*;
use crate::disk_records::{DiskRecords, RecordSorter, RecordStageRoot, RecordWriter};
use std::io::{Cursor, Read};

fn present_optional<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_optional"
    )]
    pub(super) schema_version: Option<serde_json::Number>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_optional"
    )]
    pub(super) updated_at: Option<String>,
    pub(super) files: DiskRecords<ManifestRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManifestRow {
    pub(super) path: String,
    pub(super) raw_hash: String,
    pub(super) status: String,
}
impl ManifestRow {
    pub(super) fn validate(&self) -> Result<()> {
        if self.path.is_empty() || self.path.contains('/') {
            return Err(KioError::path(
                "manifest file path must be a direct child file name",
                self.path.clone(),
            ));
        }
        if !is_hash(&self.raw_hash) {
            return Err(KioError::schema("manifest raw_hash must be a hash"));
        }
        if !matches!(
            self.status.as_str(),
            "new" | "modified" | "deleted" | "unchanged"
        ) {
            return Err(KioError::schema("manifest status has invalid value"));
        }
        Ok(())
    }
}
impl Manifest {
    pub(super) fn validate(&self) -> Result<()> {
        // Validate only the constant-sized header through the existing schema.
        let mut header = json!({"files": []});
        if let Some(version) = &self.schema_version {
            header["schema_version"] = Value::Number(version.clone());
        }
        if let Some(updated) = &self.updated_at {
            header["updated_at"] = Value::String(updated.clone());
        }
        validate_json_schema(SchemaKind::Manifest, &header)?;
        for row in &self.files {
            row?.validate()?;
        }
        Ok(())
    }
    pub(super) fn read(root: &RecordStageRoot, reader: impl Read) -> Result<Self> {
        let manifest: Self = crate::disk_records::decode_bounded_json(
            root,
            std::io::BufReader::with_capacity(64 * 1024, reader),
        )?;
        manifest.validate()?;
        Ok(manifest)
    }
}

#[derive(Serialize, Deserialize)]
struct PreviousRow {
    path: String,
    raw_hash: String,
    priority: u8,
    ordinal: u64,
}
fn next_unique<I: Iterator<Item = Result<PreviousRow>>>(
    iter: &mut I,
    last: &mut Option<String>,
) -> Result<Option<PreviousRow>> {
    for row in iter {
        let row = row?;
        if last.as_deref() == Some(row.path.as_str()) {
            continue;
        }
        *last = Some(row.path.clone());
        return Ok(Some(row));
    }
    Ok(None)
}
pub(super) fn build_manifest(
    root: &RecordStageRoot,
    tree: &TreeObject,
    prior_tree: Option<&TreeObject>,
    deleted: DiskRecords<ManifestRow>,
    updated_at: String,
) -> Result<Manifest> {
    tree.validate()?;
    if let Some(prior) = prior_tree {
        prior.validate()?;
    }
    let mut sorter = RecordSorter::new(root, |left: &PreviousRow, right: &PreviousRow| {
        left.path
            .as_bytes()
            .cmp(right.path.as_bytes())
            .then_with(|| left.priority.cmp(&right.priority))
            .then_with(|| right.ordinal.cmp(&left.ordinal))
    });
    if let Some(prior) = prior_tree {
        for entry in &prior.entries {
            let entry = entry?;
            sorter.push(PreviousRow {
                path: entry.path,
                raw_hash: entry.raw_hash,
                priority: 0,
                ordinal: 0,
            })?;
        }
    }
    for (ordinal, row) in deleted.iter().enumerate() {
        let row = row?;
        sorter.push(PreviousRow {
            path: row.path,
            raw_hash: row.raw_hash,
            priority: 1,
            ordinal: ordinal as u64,
        })?;
    }
    let previous = sorter.finish()?;
    let mut previous = previous.iter();
    let mut last = None;
    let mut prior = next_unique(&mut previous, &mut last)?;
    let mut rows = RecordWriter::new(root)?;
    for entry in &tree.entries {
        let entry = entry?;
        while prior
            .as_ref()
            .is_some_and(|old| old.path.as_bytes() < entry.path.as_bytes())
        {
            let old = prior.take().expect("prior row");
            rows.push(&ManifestRow {
                path: old.path,
                raw_hash: old.raw_hash,
                status: "deleted".into(),
            })?;
            prior = next_unique(&mut previous, &mut last)?;
        }
        let status = match &prior {
            Some(old) if old.path == entry.path && old.raw_hash == entry.raw_hash => "unchanged",
            Some(old) if old.path == entry.path => "modified",
            _ => "new",
        };
        let matches_prior = prior.as_ref().is_some_and(|old| old.path == entry.path);
        rows.push(&ManifestRow {
            path: entry.path,
            raw_hash: entry.raw_hash,
            status: status.into(),
        })?;
        if matches_prior {
            prior = next_unique(&mut previous, &mut last)?;
        }
    }
    while let Some(old) = prior {
        rows.push(&ManifestRow {
            path: old.path,
            raw_hash: old.raw_hash,
            status: "deleted".into(),
        })?;
        prior = next_unique(&mut previous, &mut last)?;
    }
    let manifest = Manifest {
        schema_version: Some(1.into()),
        updated_at: Some(updated_at),
        files: rows.finish()?,
    };
    manifest.validate()?;
    Ok(manifest)
}

/// Canonical JSON emitted one row at a time; the wire object remains unchanged.
pub(super) struct ManifestJsonReader<'a> {
    manifest: &'a Manifest,
    rows: crate::disk_records::RecordIter<ManifestRow>,
    buffer: Cursor<Vec<u8>>,
    first: bool,
    finished: bool,
}
impl<'a> ManifestJsonReader<'a> {
    pub(super) fn new(manifest: &'a Manifest) -> Result<Self> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            rows: manifest.files.iter(),
            buffer: Cursor::new(b"{\"files\":[".to_vec()),
            first: true,
            finished: false,
        })
    }
}
impl Read for ManifestJsonReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.buffer.read(output)?;
            if count != 0 || self.finished {
                return Ok(count);
            }
            let mut bytes = Vec::new();
            if let Some(row) = self.rows.next() {
                let row = row.map_err(std::io::Error::other)?;
                if !self.first {
                    bytes.push(b',');
                }
                self.first = false;
                serde_jcs::to_writer(&mut bytes, &row).map_err(std::io::Error::other)?;
            } else {
                bytes.push(b']');
                if let Some(version) = &self.manifest.schema_version {
                    bytes.extend_from_slice(b",\"schema_version\":");
                    serde_jcs::to_writer(&mut bytes, version).map_err(std::io::Error::other)?;
                }
                if let Some(updated) = &self.manifest.updated_at {
                    bytes.extend_from_slice(b",\"updated_at\":");
                    serde_jcs::to_writer(&mut bytes, updated).map_err(std::io::Error::other)?;
                }
                bytes.push(b'}');
                self.finished = true;
            }
            self.buffer = Cursor::new(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{TreeBuilder, build_tree};

    fn hash(value: u8) -> String {
        crate::cas::hash_bytes(&[value])
    }
    fn entry(path: &str, value: u8) -> TreeEntry {
        TreeEntry::raw_file(path, hash(value)).unwrap()
    }
    fn deleted(path: &str, value: u8) -> ManifestRow {
        ManifestRow {
            path: path.into(),
            raw_hash: hash(value),
            status: "deleted".into(),
        }
    }

    #[test]
    fn merge_preserves_prior_tree_authority_deleted_rows_and_reappearance() {
        let prior = build_tree(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            vec![entry("a", 1), entry("c", 3)],
        )
        .unwrap();
        let current = build_tree(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            vec![entry("a", 1), entry("b", 5), entry("d", 4)],
        )
        .unwrap();
        let old = DiskRecords::from_records(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            [
                deleted("e", 6),
                deleted("a", 9),
                deleted("b", 2),
                deleted("e", 7),
            ],
        )
        .unwrap();
        let manifest = build_manifest(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            &current,
            Some(&prior),
            old,
            "2026-11-13T00:00:00Z".into(),
        )
        .unwrap();
        let rows = manifest.files.to_vec().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.path.as_str(), row.status.as_str()))
                .collect::<Vec<_>>(),
            [
                ("a", "unchanged"),
                ("b", "modified"),
                ("c", "deleted"),
                ("d", "new"),
                ("e", "deleted")
            ]
        );
        assert_eq!(rows[4].raw_hash, hash(7));
        let mut bytes = Vec::new();
        ManifestJsonReader::new(&manifest)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, serde_jcs::to_vec(&manifest).unwrap());
        assert_eq!(
            Manifest::read(
                &crate::disk_records::RecordStageRoot::temporary().unwrap(),
                bytes.as_slice()
            )
            .unwrap()
            .files
            .len(),
            5
        );
    }

    #[test]
    fn header_and_rows_preserve_strict_schema_with_optional_headers() {
        assert!(
            Manifest::read(
                &crate::disk_records::RecordStageRoot::temporary().unwrap(),
                br#"{"files":[]}"#.as_slice()
            )
            .is_ok()
        );
        assert!(
            Manifest::read(
                &crate::disk_records::RecordStageRoot::temporary().unwrap(),
                br#"{"schema_version":1.0,"files":[]}"#.as_slice()
            )
            .is_ok()
        );
        for value in [
            json!({"files":[], "schema_version": null}),
            json!({"files":[], "updated_at": null}),
            json!({"files":[], "schema_version": 0}),
            json!({"files":[], "updated_at": "yesterday"}),
            json!({"files":[], "unknown": true}),
            json!({"files":[{"path":"a", "raw_hash":hash(1), "status":"unknown"}]}),
            json!({"files":[{"path":"a", "raw_hash":hash(1), "status":"new", "unknown":true}]}),
            json!({"files":[{"path":"a/b", "raw_hash":hash(1), "status":"new"}]}),
        ] {
            assert!(
                Manifest::read(
                    &crate::disk_records::RecordStageRoot::temporary().unwrap(),
                    serde_json::to_vec(&value).unwrap().as_slice()
                )
                .is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn manifests_larger_than_the_config_byte_limit_stream_and_roundtrip() {
        let config = build_tree(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            Vec::new(),
        )
        .unwrap()
        .chunking_config_hash;
        let mut builder = TreeBuilder::new(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            config,
        );
        for index in 0..10_001 {
            builder
                .push(entry(&format!("{index:05}-document.txt"), 1))
                .unwrap();
        }
        let tree = builder.finish().unwrap();
        let manifest = build_manifest(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            &tree,
            None,
            DiskRecords::from_records(
                &crate::disk_records::RecordStageRoot::temporary().unwrap(),
                [],
            )
            .unwrap(),
            "2026-11-13T00:00:00Z".into(),
        )
        .unwrap();
        let bytes = std::io::copy(
            &mut ManifestJsonReader::new(&manifest).unwrap(),
            &mut std::io::sink(),
        )
        .unwrap();
        assert!(bytes > MAX_BOUND_CONFIG_BYTES);
        let reread = Manifest::read(
            &crate::disk_records::RecordStageRoot::temporary().unwrap(),
            ManifestJsonReader::new(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(reread.files.len(), 10_001);
    }
}
