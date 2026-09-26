//! Complete, namespace-aware validation of the parts that identify an Office
//! package. This establishes input type before conversion; it is not an OOXML
//! schema validator and does not replace renderer confinement.

use std::collections::{HashMap, HashSet};

use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};

use super::{Result, contract};

const CONTENT_TYPES_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/content-types";
const RELATIONSHIPS_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
const OFFICE_DOCUMENT_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
const MAX_XML_DEPTH: usize = 256;
const MAX_XML_ATTRIBUTES: usize = 1_024;

/// The visitor receives only unqualified attributes: namespace declarations
/// and namespaced lookalikes must never stand in for OPC authority attributes.
fn walk(
    xml: &[u8],
    root_local: &[u8],
    root_namespace: &[u8],
    mut visit: impl FnMut(usize, &[u8], &[u8], &HashMap<String, String>) -> Result<()>,
) -> Result<()> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().check_comments = true;
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut roots = 0usize;
    let mut declaration = false;
    loop {
        let event = reader.read_event_into(&mut buffer).map_err(xml_error)?;
        let empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                if depth >= MAX_XML_DEPTH {
                    return Err(contract("Office", "XML nesting exceeds its bound"));
                }
                let (resolved, local) = reader.resolver().resolve_element(element.name());
                let namespace = namespace_bytes(resolved)?;
                if depth == 0 {
                    roots += 1;
                    if roots != 1 || local.as_ref() != root_local || namespace != root_namespace {
                        return Err(contract(
                            "Office",
                            "XML has an unexpected root or namespace",
                        ));
                    }
                }
                let mut expanded = HashSet::new();
                let mut attributes = HashMap::new();
                for (index, attribute) in element.attributes().enumerate() {
                    if index >= MAX_XML_ATTRIBUTES {
                        return Err(contract("Office", "XML attribute count exceeds its bound"));
                    }
                    let attribute = attribute.map_err(xml_error)?;
                    let value = attribute
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(xml_error)?;
                    if attribute.key.as_ref() == b"xmlns"
                        || attribute.key.as_ref().starts_with(b"xmlns:")
                    {
                        continue;
                    }
                    let (resolved, local) = reader.resolver().resolve_attribute(attribute.key);
                    let namespace = namespace_bytes(resolved)?;
                    if !expanded.insert((namespace.to_vec(), local.as_ref().to_vec())) {
                        return Err(contract("Office", "XML has duplicate expanded attributes"));
                    }
                    if namespace.is_empty() {
                        let key = std::str::from_utf8(local.as_ref()).map_err(xml_error)?;
                        attributes.insert(key.to_owned(), value.into_owned());
                    }
                }
                visit(depth, namespace, local.as_ref(), &attributes)?;
                if !empty {
                    depth += 1;
                }
            }
            Event::End(_) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| contract("Office", "XML has an unmatched end element"))?;
            }
            Event::Text(text) => {
                let text = text.decode().map_err(xml_error)?;
                if depth == 0 && !text.bytes().all(xml_space) {
                    return Err(contract("Office", "XML has text outside its root"));
                }
            }
            Event::CData(text) => {
                text.decode().map_err(xml_error)?;
                if depth == 0 {
                    return Err(contract("Office", "XML has CDATA outside its root"));
                }
            }
            Event::GeneralRef(reference) => {
                let numeric = reference.resolve_char_ref().map_err(xml_error)?;
                let reference_bytes: &[u8] = &reference;
                if depth == 0
                    || (numeric.is_none()
                        && !matches!(reference_bytes, b"amp" | b"lt" | b"gt" | b"apos" | b"quot"))
                {
                    return Err(contract(
                        "Office",
                        "XML has an unsupported entity reference",
                    ));
                }
            }
            Event::DocType(_) => {
                return Err(contract(
                    "Office",
                    "XML document type declarations are forbidden",
                ));
            }
            Event::Decl(value) => {
                if declaration || roots != 0 {
                    return Err(contract("Office", "XML declaration is misplaced"));
                }
                value.version().map_err(xml_error)?;
                if let Some(encoding) = value.encoding() {
                    let encoding = encoding.map_err(xml_error)?;
                    if !encoding.eq_ignore_ascii_case(b"UTF-8") {
                        return Err(contract("Office", "package XML must use UTF-8"));
                    }
                }
                declaration = true;
            }
            Event::Eof => {
                if depth != 0 || roots != 1 {
                    return Err(contract("Office", "XML is incomplete"));
                }
                return Ok(());
            }
            Event::Comment(_) | Event::PI(_) => {}
        }
        buffer.clear();
    }
}

fn xml_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn namespace_bytes(resolved: ResolveResult<'_>) -> Result<&[u8]> {
    match resolved {
        ResolveResult::Bound(namespace) => Ok(namespace.into_inner()),
        ResolveResult::Unbound => Ok(b""),
        ResolveResult::Unknown(_) => Err(contract("Office", "XML uses an undeclared namespace")),
    }
}

fn xml_error(error: impl std::fmt::Display) -> super::AdapterError {
    contract("Office", format!("package XML is invalid: {error}"))
}

pub(super) fn validate_content_type(xml: &[u8], main_part: &str, expected: &str) -> Result<()> {
    let main_name = format!("/{main_part}");
    let mut overrides = HashSet::new();
    let mut defaults = HashSet::new();
    let mut matched = false;
    walk(
        xml,
        b"Types",
        CONTENT_TYPES_NS,
        |depth, ns, local, attrs| {
            if depth == 0 {
                return Ok(());
            }
            if depth != 1 || ns != CONTENT_TYPES_NS {
                return Err(contract(
                    "Office",
                    "content types have an unexpected element",
                ));
            }
            match local {
                b"Override" => {
                    let part = required(attrs, "PartName")?;
                    let kind = required(attrs, "ContentType")?;
                    if !overrides.insert(part.to_owned()) {
                        return Err(contract("Office", "content types repeat a part mapping"));
                    }
                    if part == main_name {
                        if kind != expected {
                            return Err(contract("Office", "main part has the wrong content type"));
                        }
                        matched = true;
                    }
                }
                b"Default" => {
                    let extension = required(attrs, "Extension")?;
                    required(attrs, "ContentType")?;
                    if !defaults.insert(extension.to_ascii_lowercase()) {
                        return Err(contract(
                            "Office",
                            "content types repeat an extension mapping",
                        ));
                    }
                }
                _ => {
                    return Err(contract(
                        "Office",
                        "content types have an unexpected element",
                    ));
                }
            }
            Ok(())
        },
    )?;
    if !matched {
        return Err(contract(
            "Office",
            "main part has no required content type mapping",
        ));
    }
    Ok(())
}

pub(super) fn validate_root_relationship(xml: &[u8], main_part: &str) -> Result<()> {
    let mut ids = HashSet::new();
    let mut matched = false;
    walk(
        xml,
        b"Relationships",
        RELATIONSHIPS_NS,
        |depth, ns, local, attrs| {
            if depth == 0 {
                return Ok(());
            }
            if depth != 1 || ns != RELATIONSHIPS_NS || local != b"Relationship" {
                return Err(contract(
                    "Office",
                    "root relationships have an unexpected element",
                ));
            }
            let id = required(attrs, "Id")?;
            let kind = required(attrs, "Type")?;
            let target = required(attrs, "Target")?;
            if !ids.insert(id.to_owned()) {
                return Err(contract("Office", "root relationships repeat an identity"));
            }
            if kind == OFFICE_DOCUMENT_REL {
                if matched
                    || target.strip_prefix('/').unwrap_or(target) != main_part
                    || attrs
                        .get("TargetMode")
                        .is_some_and(|mode| mode != "Internal")
                {
                    return Err(contract(
                        "Office",
                        "main relationship is ambiguous or external",
                    ));
                }
                matched = true;
            }
            Ok(())
        },
    )?;
    if !matched {
        return Err(contract(
            "Office",
            "main part has no internal root relationship",
        ));
    }
    Ok(())
}

fn required<'a>(attributes: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    attributes
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            contract(
                "Office",
                format!("package XML lacks required attribute {key}"),
            )
        })
}

pub(super) fn validate_xml_root(
    xml: &[u8],
    expected_name: &[u8],
    namespace: &str,
    _part: &str,
) -> Result<()> {
    let local = expected_name
        .rsplit(|byte| *byte == b':')
        .next()
        .unwrap_or(expected_name);
    walk(xml, local, namespace.as_bytes(), |_, _, _, _| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORD_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    #[test]
    fn main_xml_resolves_default_and_arbitrary_prefixes() {
        for xml in [
            format!("<document xmlns=\"{WORD_NS}\"><body/></document>"),
            format!("<z:document xmlns:z=\"{WORD_NS}\"><z:body/></z:document>"),
        ] {
            validate_xml_root(xml.as_bytes(), b"w:document", WORD_NS, "word/document.xml").unwrap();
        }
    }

    #[test]
    fn main_xml_rejects_invalid_content_after_valid_root() {
        for tail in [
            "<w:body>",
            "<w:body></bad></w:document>",
            "</w:document><second/>",
            "<!DOCTYPE x></w:document>",
            "&custom;</w:document>",
            "<w:body a=\"1\" a=\"2\"/></w:document>",
            "<bad:body/></w:document>",
        ] {
            let xml = format!("<w:document xmlns:w=\"{WORD_NS}\">{tail}");
            assert!(
                validate_xml_root(xml.as_bytes(), b"w:document", WORD_NS, "main").is_err(),
                "{tail}"
            );
        }
    }

    #[test]
    fn duplicate_or_external_main_relationship_is_rejected() {
        let relationship = format!(
            "<Relationship Id=\"r1\" Type=\"{OFFICE_DOCUMENT_REL}\" Target=\"word/document.xml\"/>"
        );
        let wrap = |body: &str| {
            format!(
                "<Relationships xmlns=\"{}\">{body}</Relationships>",
                std::str::from_utf8(RELATIONSHIPS_NS).unwrap()
            )
        };
        validate_root_relationship(wrap(&relationship).as_bytes(), "word/document.xml").unwrap();
        for body in [
            relationship.repeat(2),
            relationship.replace("/>", " TargetMode=\"External\"/>"),
            relationship.replace("r1", "r2") + &relationship,
        ] {
            assert!(
                validate_root_relationship(wrap(&body).as_bytes(), "word/document.xml").is_err()
            );
        }
    }

    #[test]
    fn content_type_requires_its_namespace_and_unique_mapping_through_eof() {
        let entry = "<Override PartName=\"/word/document.xml\" ContentType=\"wanted\"/>";
        let wrap = |body: &str| {
            format!(
                "<Types xmlns=\"{}\">{body}</Types>",
                std::str::from_utf8(CONTENT_TYPES_NS).unwrap()
            )
        };
        validate_content_type(wrap(entry).as_bytes(), "word/document.xml", "wanted").unwrap();
        for xml in [
            wrap(&entry.repeat(2)),
            wrap(entry).replace("</Types>", "<broken>"),
            format!("<Types>{entry}</Types>"),
            wrap(entry).replace("<Override ", "<Override xmlns=\"wrong\" "),
        ] {
            assert!(validate_content_type(xml.as_bytes(), "word/document.xml", "wanted").is_err());
        }
    }
}
