//! The formatting sid does on its own, for JSON and XML, when neither a formatter nor a
//! language server is there to do it: a server reached over SSH rarely has either.
//!
//! Both work on the text as written rather than on a parsed model of it, so numbers,
//! escapes and key order come out exactly as they went in, and so do XML comments; only
//! the whitespace between the pieces changes. JSON is the standard's JSON, without
//! comments. Text that does not read as a whole document is refused and left alone.

/// A document formatted, and what in it was left as written.
#[derive(Debug, PartialEq)]
pub struct Formatted {
    pub text: String,
    /// The elements kept exactly as they were, and why, so a no-op is explained; empty
    /// when the whole document was reflowed.
    pub kept: Vec<Kept>,
}

/// Why an element was kept as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kept {
    /// The element carries `xml:space="preserve"`.
    XmlSpace,
    /// The element holds text, or spaces, that are its content.
    CharacterData,
}

impl Formatted {
    /// What was kept, for the status bar: `3 elements kept as written: character data`;
    /// none when nothing was.
    pub fn kept_message(&self) -> Option<String> {
        if self.kept.is_empty() {
            return None;
        }
        let mut reasons: Vec<&str> = Vec::new();
        for kept in &self.kept {
            let reason = match kept {
                Kept::XmlSpace => "xml:space=\"preserve\"",
                Kept::CharacterData => "character data",
            };
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
        let count = self.kept.len();
        let elements = if count == 1 { "element" } else { "elements" };
        Some(format!(
            "{count} {elements} kept as written: {}",
            reasons.join(" and ")
        ))
    }
}

/// Formats `text` as the language `language` names, indenting by `indent` and ending lines
/// with `newline`; `None` when sid has no formatter of its own for that language.
pub fn format(
    language: &str,
    text: &str,
    indent: &str,
    newline: &str,
) -> Option<Result<Formatted, String>> {
    match language {
        "json" => Some(json(text, indent, newline).map(|text| Formatted {
            text,
            kept: Vec::new(),
        })),
        "xml" => Some(xml(text, indent, newline)),
        _ => None,
    }
}

fn json(text: &str, indent: &str, newline: &str) -> Result<String, String> {
    // Checked without building a value: nothing is allocated, and a number too big for
    // an f64 is still JSON.
    if let Err(err) = serde_json::from_str::<serde::de::IgnoredAny>(text) {
        return Err(format!("Not valid JSON, left as it was: {err}"));
    }

    let mut out = String::with_capacity(text.len() * 2);
    let mut depth = 0usize;
    let mut chars = text.chars().peekable();
    let line_break = |out: &mut String, depth: usize| {
        out.push_str(newline);
        for _ in 0..depth {
            out.push_str(indent);
        }
    };
    while let Some(char) = chars.next() {
        match char {
            '"' => {
                out.push('"');
                while let Some(char) = chars.next() {
                    out.push(char);
                    match char {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '{' | '[' => {
                let close = if char == '{' { '}' } else { ']' };
                while chars.peek().is_some_and(|next| next.is_whitespace()) {
                    chars.next();
                }
                out.push(char);
                if chars.peek() == Some(&close) {
                    out.push(close);
                    chars.next();
                } else {
                    depth += 1;
                    line_break(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                line_break(&mut out, depth);
                out.push(char);
            }
            ',' => {
                out.push(',');
                line_break(&mut out, depth);
            }
            ':' => out.push_str(": "),
            char if char.is_whitespace() => {}
            char => out.push(char),
        }
    }
    out.push_str(newline);
    Ok(out)
}

/// One piece of an XML document as it was written.
#[derive(Debug, PartialEq)]
enum Piece<'a> {
    Open {
        name: &'a str,
        text: &'a str,
    },
    Close {
        name: &'a str,
        text: &'a str,
    },
    /// A self-closing element, a comment, a declaration, CDATA, a DOCTYPE.
    Whole(&'a str),
    Text(&'a str),
}

impl Piece<'_> {
    fn text(&self) -> &str {
        match self {
            Piece::Open { text, .. } | Piece::Close { text, .. } => text,
            Piece::Whole(text) | Piece::Text(text) => text,
        }
    }

    /// Whitespace between tags: the indentation the formatter makes, not content.
    fn is_blank(&self) -> bool {
        matches!(self, Piece::Text(text) if text.trim().is_empty())
    }

    fn is_cdata(&self) -> bool {
        matches!(self, Piece::Whole(text) if text.starts_with("<![CDATA["))
    }
}

fn xml(text: &str, indent: &str, newline: &str) -> Result<Formatted, String> {
    let pieces = xml_pieces(text)?;

    struct Element<'a> {
        name: &'a str,
        start: usize,
        /// Carries `xml:space="preserve"`.
        space: bool,
        /// Holds text that is not whitespace: a text leaf, or mixed content.
        text: bool,
        /// Holds whitespace with no line break in it: spaces, not indentation.
        spaces: bool,
        /// Holds child elements, comments or declarations.
        markup: bool,
    }
    let mut open: Vec<Element<'_>> = Vec::new();
    let mut preserved = vec![None; pieces.len()];
    for (index, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Open { name, text } => {
                if let Some(parent) = open.last_mut() {
                    parent.markup = true;
                }
                open.push(Element {
                    name,
                    start: index,
                    space: preserves_xml_space(text),
                    text: false,
                    spaces: false,
                    markup: false,
                });
            }
            Piece::Close { name, .. } => match open.pop() {
                Some(element) if element.name == *name => {
                    // Text is content wherever it is. Spaces with no line break are
                    // content in a leaf, and indentation where there are child elements
                    // to indent: a one-line `<root> <a/> <b/> </root>` is reflowed.
                    let kept = if element.space {
                        Some(Kept::XmlSpace)
                    } else if element.text || (element.spaces && !element.markup) {
                        Some(Kept::CharacterData)
                    } else {
                        None
                    };
                    if let Some(kept) = kept {
                        preserved[element.start] = Some((index, kept));
                    }
                }
                Some(element) => {
                    return Err(format!(
                        "Not valid XML, left as it was: </{name}> closes <{}>",
                        element.name
                    ))
                }
                None => {
                    return Err(format!(
                        "Not valid XML, left as it was: </{name}> closes nothing"
                    ))
                }
            },
            Piece::Text(text) => match open.last_mut() {
                Some(element) => {
                    if !text.trim().is_empty() {
                        element.text = true;
                    } else if !text.contains(['\n', '\r']) {
                        element.spaces = true;
                    }
                }
                // Reflowing would drop it: nothing outside the root is kept.
                None if !text.trim().is_empty() => {
                    return Err(
                        "Not valid XML, left as it was: text outside the root element".to_string(),
                    );
                }
                None => {}
            },
            // CDATA is character data: it is content like text, not markup.
            Piece::Whole(_) if piece.is_cdata() => {}
            Piece::Whole(_) => {
                if let Some(element) = open.last_mut() {
                    element.markup = true;
                }
            }
        }
    }
    if let Some(element) = open.pop() {
        return Err(format!(
            "Not valid XML, left as it was: <{}> is never closed",
            element.name
        ));
    }

    let mut out = String::with_capacity(text.len() * 2);
    let mut kept = Vec::new();
    let mut depth = 0usize;
    let mut index = 0;
    // Every line break inside a piece - a preserved subtree, a comment, an attribute list
    // over several lines - is the document's line ending, so a CRLF file formatted with
    // `\n` does not come out mixed.
    let line = |out: &mut String, depth: usize, content: &str| {
        if !out.is_empty() {
            out.push_str(newline);
        }
        for _ in 0..depth {
            out.push_str(indent);
        }
        let content = content.replace("\r\n", "\n").replace('\r', "\n");
        out.push_str(&content.replace('\n', newline));
    };
    while index < pieces.len() {
        if let Some((end, why)) = preserved[index] {
            // Keep the entire subtree verbatim, including inherited xml:space and
            // whitespace around nested inline tags, comments and CDATA.
            let raw: String = pieces[index..=end].iter().map(Piece::text).collect();
            line(&mut out, depth, &raw);
            kept.push(why);
            index = end + 1;
            continue;
        }
        match &pieces[index] {
            Piece::Open { name, text } => {
                // An element holding nothing, or one CDATA on one line, stays on one line:
                // `<name></name>`, `<name><![CDATA[..]]></name>`. Whitespace with a line
                // break between its tags is indentation, and goes.
                let mut next = index + 1;
                while pieces.get(next).is_some_and(Piece::is_blank) {
                    next += 1;
                }
                let mut inner = "";
                if let Some(cdata) = pieces.get(next).filter(|piece| piece.is_cdata()) {
                    let cdata = cdata.text();
                    if !cdata.contains(['\n', '\r']) {
                        inner = cdata;
                        next += 1;
                        while pieces.get(next).is_some_and(Piece::is_blank) {
                            next += 1;
                        }
                    }
                }
                match pieces.get(next) {
                    Some(Piece::Close {
                        name: closing,
                        text: close,
                    }) if closing == name => {
                        line(&mut out, depth, &format!("{text}{inner}{close}"));
                        index = next + 1;
                        continue;
                    }
                    _ => {}
                }
                line(&mut out, depth, text);
                depth += 1;
            }
            Piece::Close { text, .. } => {
                depth = depth.saturating_sub(1);
                line(&mut out, depth, text);
            }
            Piece::Whole(text) => line(&mut out, depth, text),
            Piece::Text(_) => {} // Indentation outside a preserved subtree.
        }
        index += 1;
    }
    out.push_str(newline);
    Ok(Formatted { text: out, kept })
}

/// Reads the attribute rather than matching words inside another attribute's value.
fn preserves_xml_space(tag: &str) -> bool {
    let Some(start) = tag.find(char::is_whitespace) else {
        return false;
    };
    let mut attributes = &tag[start..];
    while let Some((name, value)) = attributes.trim_start().split_once('=') {
        let value = value.trim_start();
        let Some(quote @ ('\'' | '"')) = value.chars().next() else {
            break;
        };
        let Some(end) = value[1..].find(quote).map(|end| end + 1) else {
            break;
        };
        if name.trim() == "xml:space" && &value[1..end] == "preserve" {
            return true;
        }
        attributes = &value[end + 1..];
    }
    false
}

/// Cuts a document into its pieces, keeping character data exactly as written.
fn xml_pieces(text: &str) -> Result<Vec<Piece<'_>>, String> {
    let unterminated = |what: &str| format!("Not valid XML, left as it was: unterminated {what}");
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let Some(start) = rest.find('<') else {
            if !rest.is_empty() {
                pieces.push(Piece::Text(rest));
            }
            break;
        };
        if start > 0 {
            pieces.push(Piece::Text(&rest[..start]));
        }
        rest = &rest[start..];
        let special = [
            ("<!--", "-->", "comment"),
            ("<![CDATA[", "]]>", "CDATA section"),
            ("<?", "?>", "declaration"),
        ];
        if let Some((_, end, what)) = special.iter().find(|(open, ..)| rest.starts_with(open)) {
            let length = rest.find(end).ok_or_else(|| unterminated(what))? + end.len();
            pieces.push(Piece::Whole(&rest[..length]));
            rest = &rest[length..];
            continue;
        }
        // A tag ends at the first `>` outside quotes. A DOCTYPE may hold an internal
        // subset in brackets, with whole declarations - tags of their own - inside it.
        let mut quote = None;
        let mut brackets = 0usize;
        let mut end = None;
        for (at, char) in rest.char_indices().skip(1) {
            match (quote, char) {
                (Some(open), char) if char == open => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(char),
                (None, '[') => brackets += 1,
                (None, ']') => brackets = brackets.saturating_sub(1),
                (None, '>') if brackets == 0 => {
                    end = Some(at + 1);
                    break;
                }
                (None, '<') if brackets == 0 => return Err(unterminated("tag")),
                _ => {}
            }
        }
        let end = end.ok_or_else(|| unterminated("tag"))?;
        let tag = &rest[..end];
        rest = &rest[end..];
        if tag.starts_with("<!") || tag.ends_with("/>") {
            pieces.push(Piece::Whole(tag));
        } else if let Some(body) = tag.strip_prefix("</") {
            let name = body.trim_end_matches('>').trim();
            pieces.push(Piece::Close { name, text: tag });
        } else {
            let name = tag[1..tag.len() - 1]
                .split(|char: char| char.is_whitespace())
                .next()
                .unwrap_or_default();
            if name.is_empty() {
                return Err("Not valid XML, left as it was: a tag with no name".to_string());
            }
            pieces.push(Piece::Open { name, text: tag });
        }
    }
    Ok(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml_text(text: &str) -> Result<String, String> {
        xml(text, "  ", "\n").map(|formatted| formatted.text)
    }

    #[test]
    fn json_is_indented_keeping_order_numbers_and_escapes() {
        let text = r#"{"b":1.50,"a":[1, 2,{}],"s":"x\"y, {z}: ","e":[],
            "n":{"deep":  null}}"#;
        let formatted = json(text, "  ", "\n").unwrap();
        assert_eq!(
            formatted,
            "{\n  \"b\": 1.50,\n  \"a\": [\n    1,\n    2,\n    {}\n  ],\n  \"s\": \"x\\\"y, {z}: \",\n  \"e\": [],\n  \"n\": {\n    \"deep\": null\n  }\n}\n"
        );
        assert_eq!(json(&formatted, "  ", "\n").unwrap(), formatted);
    }

    #[test]
    fn invalid_json_is_refused() {
        assert!(json("{\"a\": 1,}", "  ", "\n").is_err());
        assert!(json("{\"a\": 1}\n{\"b\": 2}\n", "  ", "\n").is_err());
        assert!(json("// note\n{}", "  ", "\n").is_err());
    }

    #[test]
    fn json_numbers_beyond_f64_are_still_json() {
        assert_eq!(json("[1e400]", "  ", "\n").unwrap(), "[\n  1e400\n]\n");
    }

    #[test]
    fn xml_is_indented_keeping_text_elements_on_one_line() {
        let text = "<?xml version=\"1.0\"?><root a=\"x > y\"><!-- note --><name>  Golf </name>\
            <empty/><list><item>1</item><item></item></list><raw><![CDATA[<raw>]]></raw></root>";
        let formatted = xml_text(text).unwrap();
        assert_eq!(
            formatted,
            "<?xml version=\"1.0\"?>\n<root a=\"x > y\">\n  <!-- note -->\n  <name>  Golf </name>\n  <empty/>\n  <list>\n    <item>1</item>\n    <item></item>\n  </list>\n  <raw><![CDATA[<raw>]]></raw>\n</root>\n"
        );
        assert_eq!(xml_text(&formatted).unwrap(), formatted);
    }

    #[test]
    fn unbalanced_xml_is_refused() {
        assert!(xml_text("<a><b></a>").is_err());
        assert!(xml_text("<a>").is_err());
        assert!(xml_text("<a><!-- open </a>").is_err());
    }

    #[test]
    fn text_outside_the_root_element_is_refused_rather_than_dropped() {
        let err = xml_text("<a/>\nhello\n<b/>").unwrap_err();
        assert!(err.contains("text outside the root element"), "{err}");
        assert!(xml_text("<a></a>\ntail").is_err());
        assert!(xml_text("head <a/>").is_err());
        // Line breaks and indentation around the root are not text.
        assert_eq!(xml_text("\n  <a/>\n\n").unwrap(), "<a/>\n");
    }

    #[test]
    fn a_doctype_with_an_internal_subset_is_one_piece() {
        let text = "<!DOCTYPE x [<!ENTITY a \"b\"><!ELEMENT x (#PCDATA)>]><x>&a;</x>";
        assert_eq!(
            xml_text(text).unwrap(),
            "<!DOCTYPE x [<!ENTITY a \"b\"><!ELEMENT x (#PCDATA)>]>\n<x>&a;</x>\n"
        );
        // A bracket inside a quoted value does not open a subset.
        assert_eq!(
            xml_text("<!DOCTYPE x \"[\"><x/>").unwrap(),
            "<!DOCTYPE x \"[\">\n<x/>\n"
        );
        assert!(xml_text("<!DOCTYPE x [<!ENTITY a \"b\">").is_err());
    }

    #[test]
    fn xml_preserves_text_and_space_sensitive_subtrees() {
        for text in [
            "<root xml:space=\"preserve\">  keep these spaces  </root>",
            "<root xml:space = 'preserve'><a/> \n <b/></root>",
            "<root><name>  Golf </name></root>",
            "<root><space> \t </space></root>",
            "<root><raw> <![CDATA[ x ]]> </raw></root>",
            "<p>Hello <b>world</b> !</p>",
            "<p><b>Hello</b> <i>world</i>!</p>",
            "<p><![CDATA[  text  ]]> and <b>more</b></p>",
        ] {
            let formatted = xml(text, "  ", "\n").unwrap();
            // Compare the literal character data, including whitespace-only leaves.
            let expected = if text.starts_with("<root><") {
                text.replacen("<root>", "<root>\n  ", 1)
                    .replace("</root>", "\n</root>\n")
            } else {
                format!("{text}\n")
            };
            assert_eq!(formatted.text, expected);
            assert_eq!(formatted.kept.len(), 1, "{text}");
            assert_eq!(xml_text(&formatted.text).unwrap(), formatted.text);
        }
    }

    #[test]
    fn what_was_kept_is_named() {
        let formatted = xml(
            "<root><a xml:space=\"preserve\"> x </a><b>text</b><c> y </c><d/></root>",
            "  ",
            "\n",
        )
        .unwrap();
        assert_eq!(
            formatted.kept_message().unwrap(),
            "3 elements kept as written: xml:space=\"preserve\" and character data"
        );
        let one = xml("<root><b>text</b></root>", "  ", "\n").unwrap();
        assert_eq!(
            one.kept_message().unwrap(),
            "1 element kept as written: character data"
        );
        assert_eq!(xml("<root><a/></root>", "  ", "\n").unwrap().kept, vec![]);
        assert_eq!(
            format("json", "{}", "  ", "\n")
                .unwrap()
                .unwrap()
                .kept_message(),
            None
        );
    }

    #[test]
    fn whitespace_with_a_line_break_in_a_leaf_is_indentation() {
        assert_eq!(
            xml_text("<root>\n  <a>\n    </a>\n  <b>\n</b></root>").unwrap(),
            "<root>\n  <a></a>\n  <b></b>\n</root>\n"
        );
    }

    #[test]
    fn spaces_between_child_elements_are_reflowed() {
        assert_eq!(
            xml_text("<root> <a/> <b>1</b> </root>").unwrap(),
            "<root>\n  <a/>\n  <b>1</b>\n</root>\n"
        );
        // A leaf's spaces are its content, and text among the children makes them so.
        assert_eq!(xml_text("<root> </root>").unwrap(), "<root> </root>\n");
        assert_eq!(
            xml_text("<root> <a/> x </root>").unwrap(),
            "<root> <a/> x </root>\n"
        );
    }

    #[test]
    fn cdata_alone_does_not_keep_the_parent_as_written() {
        // A CDATA on a line of its own among elements: indented with them.
        assert_eq!(
            xml_text("<root>\n<![CDATA[x]]>\n<a/>\n<b/>\n</root>").unwrap(),
            "<root>\n  <![CDATA[x]]>\n  <a/>\n  <b/>\n</root>\n"
        );
        // A leaf holding one CDATA sits on one line, or around it when it spans lines.
        assert_eq!(
            xml_text("<root>\n  <raw>\n    <![CDATA[x]]>\n  </raw>\n</root>").unwrap(),
            "<root>\n  <raw><![CDATA[x]]></raw>\n</root>\n"
        );
        assert_eq!(
            xml_text("<root><script><![CDATA[\n  code\n]]></script></root>").unwrap(),
            "<root>\n  <script>\n    <![CDATA[\n  code\n]]>\n  </script>\n</root>\n"
        );
    }

    #[test]
    fn line_breaks_inside_kept_pieces_follow_the_chosen_newline() {
        let text = "<root>\r\n<p>Hello\r\n<b>world</b></p>\r\n<!-- a\r\nnote -->\r\n<a\r\n  b=\"1\"/>\r\n</root>\r\n";
        assert_eq!(
            xml_text(text).unwrap(),
            "<root>\n  <p>Hello\n<b>world</b></p>\n  <!-- a\nnote -->\n  <a\n  b=\"1\"/>\n</root>\n"
        );
        assert_eq!(
            xml("<p>a\n<b/></p>", "  ", "\r\n").unwrap().text,
            "<p>a\r\n<b/></p>\r\n"
        );
    }
}
