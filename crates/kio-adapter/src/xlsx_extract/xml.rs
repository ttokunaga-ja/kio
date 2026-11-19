use std::{
    borrow::Cow,
    io::{self, BufRead, Read},
};

use quick_xml::{
    Reader,
    events::{BytesRef, BytesStart, Event},
    name::NamespaceResolver,
};

use super::{Budget, invalid, xml_error};
use crate::Result;
use crate::ooxml_package::xml::namespace_bytes;

pub(super) const SPREADSHEET_NS: &[u8] =
    b"http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(super) const DOCUMENT_REL_NS: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(super) const MAX_XML_DEPTH: usize = 256;
pub(super) const MAX_XML_ATTRIBUTES: usize = 1024;

pub(super) struct Xml<'a> {
    reader: Reader<TokenInput<'a>>,
    namespaces: NamespaceResolver,
    pending_pop: bool,
    active: Vec<usize>,
    scope_bytes: usize,
    scope_peak: usize,
    token_peak: usize,
    root: Option<&'static [u8]>,
    depth: usize,
    started: bool,
    declaration: bool,
}

impl<'a> Xml<'a> {
    pub fn new(input: &'a mut dyn BufRead, root: Option<&'static [u8]>) -> Self {
        let mut reader = Reader::from_reader(TokenInput {
            input,
            left: usize::MAX,
            exhausted: false,
        });
        reader.config_mut().check_comments = true;
        let mut namespaces = NamespaceResolver::default();
        namespaces.set_max_declarations_per_element(MAX_XML_ATTRIBUTES);
        Self {
            reader,
            namespaces,
            root,
            depth: 0,
            started: false,
            declaration: false,
            pending_pop: false,
            active: Vec::new(),
            scope_bytes: 0,
            scope_peak: 0,
            token_peak: 0,
        }
    }

    /// Depth is zero for both the opening and closing events of the root.
    pub fn next<'b>(
        &mut self,
        buffer: &'b mut Vec<u8>,
        budget: &mut Budget,
    ) -> Result<(usize, Event<'b>)> {
        if self.pending_pop {
            self.namespaces.pop();
            self.scope_bytes -= self.active.pop().expect("open XML scope");
            self.pending_pop = false;
        }
        buffer.clear();
        let cap = budget
            .available_memory()
            .saturating_sub(XML_FIXED_SCRATCH + self.scope_peak + buffer.capacity() * 2)
            / 3;
        let input = self.reader.get_mut();
        input.left = cap;
        input.exhausted = false;
        let event = match self.reader.read_event_into(buffer) {
            Ok(event) => event,
            Err(error) => {
                if self.reader.get_ref().exhausted {
                    return Err(budget.memory_exhausted());
                }
                return Err(xml_error(error));
            }
        };
        self.token_peak = self.token_peak.max(cap - self.reader.get_ref().left);
        let mut depth = self.depth;
        let empty = matches!(event, Event::Empty(_));
        budget.set_xml_scratch(XML_FIXED_SCRATCH + self.scope_peak + 3 * self.token_peak)?;
        match &event {
            Event::Start(element) | Event::Empty(element) => {
                if depth >= MAX_XML_DEPTH {
                    return Err(invalid("XML nesting exceeds 256 elements"));
                }
                validate_name(element.name().as_ref())?;
                let mut scope = 64 + 3 * element.name().as_ref().len();
                for (index, attr) in element.attributes().with_checks(false).enumerate() {
                    if index >= MAX_XML_ATTRIBUTES {
                        return Err(invalid("XML attribute count exceeds 1024"));
                    }
                    budget.visit()?;
                    let attr = attr.map_err(xml_error)?;
                    validate_name(attr.key.as_ref())?;
                    if attr.key.as_namespace_binding().is_some() {
                        scope += 128 + 3 * (attr.value.len() + attr.key.as_ref().len());
                    }
                }
                self.scope_peak = self.scope_peak.max(self.scope_bytes + scope);
                budget
                    .set_xml_scratch(XML_FIXED_SCRATCH + self.scope_peak + 3 * self.token_peak)?;
                self.namespaces.push(element).map_err(xml_error)?;
                self.active.push(scope);
                self.scope_bytes += scope;
                self.pending_pop = empty;
                let (namespace, local) = self.namespaces.resolve_element(element.name());
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
                for attr in element.attributes().with_checks(false) {
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
                        let (ns, local) = self.namespaces.resolve_attribute(attr.key);
                        (namespace_bytes(ns)?, local.into_inner())
                    };
                    budget.expand(key.0.len())?;
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
                self.pending_pop = true;
            }
            Event::Text(text) => {
                check_chars(&text.decode().map_err(xml_error)?)?;
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
            Event::CData(text) => check_chars(&text.decode().map_err(xml_error)?)?,
            Event::Comment(text) => check_chars(&text.decode().map_err(xml_error)?)?,
            Event::PI(text) => {
                validate_name(text.target())?;
                check_chars(std::str::from_utf8(text.content()).map_err(xml_error)?)?;
            }
            Event::GeneralRef(reference) => {
                budget.visit()?;
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

    pub fn validate(input: &'a mut dyn BufRead, budget: &mut Budget) -> Result<()> {
        let mut xml = Self::new(input, None);
        let mut buffer = Vec::new();
        while !matches!(xml.next(&mut buffer, budget)?.1, Event::Eof) {}
        Ok(())
    }

    pub fn sheet_element(&self, name: quick_xml::name::QName<'_>, expected: &[u8]) -> bool {
        let (namespace, local) = self.namespaces.resolve_element(name);
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
            let (ns, local) = self.namespaces.resolve_attribute(attr.key);
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

// Reserve room for the per-tag expanded-attribute index and scope stack.
const XML_FIXED_SCRATCH: usize = 128 * 1024;

struct TokenInput<'a> {
    input: &'a mut dyn BufRead,
    left: usize,
    exhausted: bool,
}

impl Read for TokenInput<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let input = self.fill_buf()?;
        let count = input.len().min(output.len());
        output[..count].copy_from_slice(&input[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for TokenInput<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let bytes = self.input.fill_buf()?;
        if self.left == 0 && !bytes.is_empty() {
            self.exhausted = true;
            return Err(io::Error::other("XLSX XML token exceeds the memory budget"));
        }
        Ok(&bytes[..bytes.len().min(self.left)])
    }

    fn consume(&mut self, count: usize) {
        self.left -= count;
        self.input.consume(count);
    }
}

// XML 1.0 Fifth Edition NameStartChar and NameChar, with the namespace
// separator handled independently for qualified names.
fn validate_name(name: &[u8]) -> Result<()> {
    fn start(ch: char) -> bool {
        matches!(ch, 'A'..='Z' | 'a'..='z' | '_' | '\u{C0}'..='\u{D6}'
            | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}'
            | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
            | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}'
            | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
    }

    let name = std::str::from_utf8(name).map_err(xml_error)?;
    let component = |value: &str| {
        let mut chars = value.chars();
        chars.next().is_some_and(start) && chars.all(|ch| start(ch) ||
            matches!(ch, '0'..='9' | '-' | '.' | '\u{B7}' | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}'))
    };
    if name.split_once(':').map_or_else(
        || component(name),
        |(prefix, local)| component(prefix) && component(local),
    ) {
        Ok(())
    } else {
        Err(invalid("XML name is invalid"))
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
