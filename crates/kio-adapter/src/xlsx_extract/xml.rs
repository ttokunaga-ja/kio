use std::borrow::Cow;

use quick_xml::{
    events::{BytesRef, BytesStart, Event},
    reader::NsReader,
};

use super::{invalid, xml_error};
use crate::Result;
use crate::ooxml_package::xml::namespace_bytes;

pub(super) const SPREADSHEET_NS: &[u8] =
    b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const DOCUMENT_REL_NS: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const MAX_XML_DEPTH: usize = 256;
pub(super) const MAX_XML_ATTRIBUTES: usize = 1024;

pub(super) struct Xml<'a> {
    reader: NsReader<&'a [u8]>,
    root: Option<&'static [u8]>,
    depth: usize,
    started: bool,
    declaration: bool,
}

impl<'a> Xml<'a> {
    pub fn new(bytes: &'a [u8], root: Option<&'static [u8]>) -> Result<Self> {
        let text = std::str::from_utf8(bytes).map_err(xml_error)?;
        check_chars(text)?;
        let mut reader = NsReader::from_reader(bytes);
        reader.config_mut().check_comments = true;
        reader
            .resolver_mut()
            .set_max_declarations_per_element(MAX_XML_ATTRIBUTES);
        Ok(Self {
            reader,
            root,
            depth: 0,
            started: false,
            declaration: false,
        })
    }

    /// Depth is zero for both the opening and closing events of the root.
    pub fn next(&mut self) -> Result<(usize, Event<'a>)> {
        let event = self.reader.read_event().map_err(xml_error)?;
        let mut depth = self.depth;
        match &event {
            Event::Start(element) | Event::Empty(element) => {
                if depth >= MAX_XML_DEPTH {
                    return Err(invalid("XML nesting exceeds 256 elements"));
                }
                let (namespace, local) = self.reader.resolver().resolve_element(element.name());
                let namespace = namespace_bytes(namespace)?;
                if depth == 0 {
                    if self.started
                        || self.root.is_some_and(|root| {
                            local.as_ref() != root || namespace != SPREADSHEET_NS
                        })
                    {
                        return Err(invalid("XML has an unexpected root or namespace"));
                    }
                    self.started = true;
                }
                let mut expanded = std::collections::HashSet::new();
                for (index, attr) in element.attributes().with_checks(false).enumerate() {
                    if index >= MAX_XML_ATTRIBUTES {
                        return Err(invalid("XML attribute count exceeds 1024"));
                    }
                    let attr = attr.map_err(xml_error)?;
                    attr.decoded_and_normalized_value(
                        quick_xml::XmlVersion::Implicit1_0,
                        self.reader.decoder(),
                    )
                    .map_err(xml_error)
                    .and_then(|value| check_chars(&value))?;
                    let key = if attr.key.as_namespace_binding().is_some() {
                        (
                            b"http://www.w3.org/2000/xmlns/".as_slice(),
                            attr.key.into_inner(),
                        )
                    } else {
                        let (ns, local) = self.reader.resolver().resolve_attribute(attr.key);
                        (namespace_bytes(ns)?, local.into_inner())
                    };
                    if !expanded.insert(key) {
                        return Err(invalid("XML repeats an expanded attribute"));
                    }
                }
                self.depth += usize::from(matches!(event, Event::Start(_)));
            }
            Event::End(_) => {
                self.depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| invalid("XML has an unmatched end element"))?;
                depth = self.depth;
            }
            Event::Text(text) => {
                if depth == 0
                    && !text
                        .iter()
                        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    return Err(invalid("XML has text outside its root"));
                }
            }
            Event::CData(_) | Event::GeneralRef(_) if depth == 0 => {
                return Err(invalid("XML has content outside its root"));
            }
            Event::GeneralRef(reference) => {
                check_chars(&reference_text(reference)?)?;
            }
            Event::DocType(_) => return Err(invalid("XML DTD declarations are forbidden")),
            Event::Decl(decl) => {
                if self.started || self.declaration {
                    return Err(invalid("XML declaration is misplaced"));
                }
                decl.version().map_err(xml_error)?;
                if let Some(encoding) = decl.encoding()
                    && !encoding.map_err(xml_error)?.eq_ignore_ascii_case(b"UTF-8")
                {
                    return Err(invalid("XML must use UTF-8"));
                }
                self.declaration = true;
            }
            Event::Eof if depth != 0 || !self.started => return Err(invalid("XML is incomplete")),
            _ => {}
        }
        Ok((depth, event))
    }

    pub fn validate(bytes: &'a [u8]) -> Result<()> {
        let mut xml = Self::new(bytes, None)?;
        while !matches!(xml.next()?.1, Event::Eof) {}
        Ok(())
    }

    pub fn sheet_element(&self, name: quick_xml::name::QName<'_>, expected: &[u8]) -> bool {
        let (namespace, local) = self.reader.resolver().resolve_element(name);
        local.as_ref() == expected
            && namespace_bytes(namespace).is_ok_and(|ns| ns == SPREADSHEET_NS)
    }

    pub fn attribute(
        &self,
        element: &BytesStart<'_>,
        namespace: &[u8],
        key: &[u8],
    ) -> Result<Option<String>> {
        for attr in element.attributes().with_checks(false) {
            let attr = attr.map_err(xml_error)?;
            let (ns, local) = self.reader.resolver().resolve_attribute(attr.key);
            if local.as_ref() == key && namespace_bytes(ns)? == namespace {
                // The initial structural pass for this event checked normalization and entities.
                let raw = std::str::from_utf8(attr.value.as_ref()).map_err(xml_error)?;
                return Ok(Some(
                    quick_xml::escape::unescape(raw)
                        .map_err(xml_error)?
                        .into_owned(),
                ));
            }
        }
        Ok(None)
    }
}

fn check_chars(value: &str) -> Result<()> {
    if value.chars().any(|ch| matches!(ch, '\0'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}' | '\u{FFFE}' | '\u{FFFF}')) {
        return Err(invalid("XML contains an invalid character"));
    }
    Ok(())
}

pub(super) fn reference_text(reference: &BytesRef<'_>) -> Result<Cow<'static, str>> {
    if let Some(character) = reference.resolve_char_ref().map_err(xml_error)? {
        return Ok(Cow::Owned(character.to_string()));
    }
    let name = reference.decode().map_err(xml_error)?;
    quick_xml::escape::resolve_predefined_entity(&name)
        .map(Cow::Borrowed)
        .ok_or_else(|| invalid("XML contains an unsupported entity reference"))
}
