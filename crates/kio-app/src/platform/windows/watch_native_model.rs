//! Separate Windows data hints from attribute/security hints.
use notify::EventKind;
use notify::event::{MetadataKind, ModifyKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Channel {
    Data,
    Authority,
}
pub(super) fn modified_kind(
    channel: Channel,
    ordinary_directory: bool,
    control_path: bool,
) -> Option<EventKind> {
    match channel {
        Channel::Data if ordinary_directory && !control_path => None,
        Channel::Data => Some(EventKind::Modify(ModifyKind::Any)),
        Channel::Authority => Some(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))),
    }
}
pub(super) fn parse_records(bytes: &[u8]) -> Result<Vec<(u32, String)>, ()> {
    if bytes.is_empty() {
        return Err(());
    }
    let mut records = Vec::new();
    let mut offset = 0usize;
    loop {
        let remaining = bytes.get(offset..).ok_or(())?;
        if remaining.len() < 12 {
            return Err(());
        }
        let word = |at| u32::from_le_bytes(remaining[at..at + 4].try_into().unwrap());
        let next = word(0) as usize;
        let action = word(4);
        let length = word(8) as usize;
        if !(1..=5).contains(&action) || length == 0 || !length.is_multiple_of(2) {
            return Err(());
        }
        let end = 12usize.checked_add(length).ok_or(())?;
        let raw = remaining.get(12..end).ok_or(())?;
        let utf16: Vec<_> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let name = String::from_utf16(&utf16).map_err(|_| ())?;
        if name.contains(['\0', ':', '/'])
            || name.starts_with('\\')
            || name
                .split('\\')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(());
        }
        records.push((action, name));
        if next == 0 {
            // The final record may include its DWORD alignment padding,
            // but cannot silently hide another truncated/garbage record.
            if remaining.len() > (end.checked_add(3).ok_or(())? & !3) {
                return Err(());
            }
            return Ok(records);
        }
        if !next.is_multiple_of(4) || next < end || next > remaining.len().saturating_sub(12) {
            return Err(());
        }
        offset = offset.checked_add(next).ok_or(())?;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn record(action: u32, name: &str) -> Vec<u8> {
        let name: Vec<_> = name.encode_utf16().collect();
        let mut bytes = Vec::new();
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(action.to_le_bytes());
        bytes.extend((name.len() as u32 * 2).to_le_bytes());
        for c in name {
            bytes.extend(c.to_le_bytes());
        }
        bytes
    }
    #[test]
    fn directory_data_modified_is_suppressed_but_authority_is_kept() {
        assert_eq!(modified_kind(Channel::Data, true, false), None);
        assert_eq!(
            modified_kind(Channel::Authority, true, false),
            Some(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)))
        );
        assert!(modified_kind(Channel::Data, false, false).is_some());
    }
    #[test]
    fn directory_valued_controls_keep_data_modified_hints() {
        for control in [".kioignore", ".kio/config.toml", ".kio/management.json"] {
            assert!(crate::watch::is_control_path(std::path::Path::new(control)));
            assert!(
                modified_kind(Channel::Data, true, true).is_some(),
                "{control}"
            );
        }
    }

    #[test]
    fn parser_preserves_actions_and_rejects_unsafe_or_malformed_names() {
        for action in 1..=5 {
            assert_eq!(
                parse_records(&record(action, "child\\file.md")),
                Ok(vec![(action, "child\\file.md".into())])
            );
        }
        for name in [
            "",
            "..\\x",
            "x\\..",
            "\\absolute",
            "C:\\absolute",
            "x\0y",
            "x\\.\\y",
        ] {
            assert!(parse_records(&record(3, name)).is_err(), "{name:?}");
        }
        assert!(parse_records(&[]).is_err());
        let mut truncated = record(3, "x");
        truncated.pop();
        assert!(parse_records(&truncated).is_err());
        let mut bad_offset = record(3, "x");
        bad_offset[..4].copy_from_slice(&13u32.to_le_bytes());
        assert!(parse_records(&bad_offset).is_err());
        assert!(parse_records(&record(99, "x")).is_err());
        let mut trailing = record(3, "x");
        trailing.extend([0; 8]);
        assert!(parse_records(&trailing).is_err());
        let mut invalid_utf16 = record(3, "x");
        invalid_utf16[12..14].copy_from_slice(&0xd800u16.to_le_bytes());
        assert!(parse_records(&invalid_utf16).is_err());
        let mut first = record(4, "old");
        first.resize(20, 0);
        first[..4].copy_from_slice(&20u32.to_le_bytes());
        first.extend(record(5, "new"));
        assert_eq!(
            parse_records(&first),
            Ok(vec![(4, "old".into()), (5, "new".into())])
        );
    }
}
