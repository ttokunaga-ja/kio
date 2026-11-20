use quick_xml::{Reader, events::Event, name::QName};
use std::{
    io,
    path::{Component, PathBuf},
};

/// Read the files2 dictionary extracted by plutil from a verified resource seal.
pub(crate) fn nested_code_paths(xml: &str) -> io::Result<Vec<PathBuf>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid macOS code resource seal",
        )
    };
    let mut reader = Reader::from_str(xml);
    if !matches!(next_event(&mut reader)?, Event::Start(tag) if tag.name() == QName(b"plist")) {
        return Err(invalid());
    }
    match next_event(&mut reader)? {
        Event::Start(tag) if tag.name() == QName(b"dict") => {}
        Event::Empty(tag) if tag.name() == QName(b"dict") => return Ok(Vec::new()),
        _ => return Err(invalid()),
    }
    let mut paths = Vec::new();
    loop {
        let key = match next_event(&mut reader)? {
            Event::Start(tag) if tag.name() == QName(b"key") => read_key(&mut reader)?,
            Event::End(tag) if tag.name() == QName(b"dict") => break,
            _ => return Err(invalid()),
        };
        match next_event(&mut reader)? {
            Event::Start(tag) if tag.name() == QName(b"dict") => {
                let mut required = false;
                loop {
                    match next_event(&mut reader)? {
                        Event::Start(tag) if tag.name() == QName(b"key") => {
                            let property = read_key(&mut reader)?;
                            let value = next_event(&mut reader)?;
                            if property == "requirement" {
                                if !matches!(&value, Event::Start(tag) if tag.name() == QName(b"string"))
                                {
                                    return Err(invalid());
                                }
                                required = true;
                            }
                            skip_value(&mut reader, value)?;
                        }
                        Event::End(tag) if tag.name() == QName(b"dict") => break,
                        _ => return Err(invalid()),
                    }
                }
                if required {
                    let path = PathBuf::from(key);
                    if path.as_os_str().is_empty()
                        || path
                            .components()
                            .any(|part| !matches!(part, Component::Normal(_)))
                    {
                        return Err(invalid());
                    }
                    paths.push(path);
                }
            }
            value => skip_value(&mut reader, value)?,
        }
    }
    if !matches!(next_event(&mut reader)?, Event::End(tag) if tag.name() == QName(b"plist")) {
        return Err(invalid());
    }
    Ok(paths)
}

fn next_event<'a>(reader: &mut Reader<&'a [u8]>) -> io::Result<Event<'a>> {
    loop {
        match reader.read_event().map_err(io::Error::other)? {
            Event::Text(text) if text.iter().all(u8::is_ascii_whitespace) => continue,
            Event::Decl(_) | Event::DocType(_) | Event::Comment(_) => continue,
            item => return Ok(item),
        }
    }
}

fn read_key(reader: &mut Reader<&[u8]>) -> io::Result<String> {
    let raw = reader.read_text(QName(b"key")).map_err(io::Error::other)?;
    let text = raw.xml10_content().map_err(io::Error::other)?;
    quick_xml::escape::unescape(&text)
        .map(|key| key.into_owned())
        .map_err(io::Error::other)
}

fn skip_value(reader: &mut Reader<&[u8]>, value: Event<'_>) -> io::Result<()> {
    match value {
        Event::Start(tag) => {
            reader.read_to_end(tag.name()).map_err(io::Error::other)?;
            Ok(())
        }
        Event::Empty(_) => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid macOS code resource seal value",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_direct_nested_requirements_and_rejects_escaping_paths() {
        let seal = r#"<plist version="1.0"><dict>
            <key>Frameworks/Helper &amp; Tools.app</key>
            <dict><key>cdhash</key><data>AAAA</data><key>requirement</key><string>identifier "helper"</string></dict>
            <key>Resources/data</key><dict><key>hash2</key><data>AAAA</data></dict>
            <key>Resources/link</key><dict><key>symlink</key><string>elsewhere</string></dict>
        </dict></plist>"#;
        assert_eq!(
            nested_code_paths(seal).unwrap(),
            [PathBuf::from("Frameworks/Helper & Tools.app")]
        );
        assert!(nested_code_paths(&seal.replace("Frameworks/Helper", "../Helper")).is_err());
        assert!(
            nested_code_paths(&seal.replace("Frameworks/Helper", "Frameworks/../Helper")).is_err()
        );
    }
}
