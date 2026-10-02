//! JSON document text as `MySQL` prints it.

/// Renders a JSON value the way `MySQL` prints JSON columns: `", "` between
/// members, `": "` after object keys, and object keys ordered by length then
/// bytes (the binary-JSON normalization order).
///
/// A JSON column's stored text is what a client reads back and what
/// `CAST(... AS CHAR)`, `LENGTH` and comparisons see, so every path that
/// stores a document renders it through here.
#[must_use]
pub fn mysql_json_text(value: &serde_json::Value) -> String {
    fn write(value: &serde_json::Value, output: &mut String) {
        match value {
            serde_json::Value::Array(items) => {
                output.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    write(item, output);
                }
                output.push(']');
            }
            serde_json::Value::Object(members) => {
                let mut keys: Vec<&String> = members.keys().collect();
                keys.sort_by(|left, right| {
                    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                });
                output.push('{');
                for (index, key) in keys.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    output.push_str(&serde_json::Value::String((*key).clone()).to_string());
                    output.push_str(": ");
                    write(&members[*key], output);
                }
                output.push('}');
            }
            other => output.push_str(&other.to_string()),
        }
    }
    let mut output = String::new();
    write(value, &mut output);
    output
}

/// Renders JSON document text as `MySQL` prints it, keeping each number as
/// it is written.
///
/// A document a source server prints spells a DECIMAL member with its
/// scale, as in `{"d": 1.50}`, and a parsed number would come back as `1.5`. So a
/// number without an exponent keeps its exact spelling here; one with an
/// exponent is a double, and is printed as a double, the way
/// [`mysql_json_text`] prints it. Members, separators and key order are as
/// [`mysql_json_text`] has them, and a repeated key keeps its last value.
///
/// # Errors
///
/// Returns a description of the first thing that is not JSON.
pub fn mysql_json_document_text(text: &str) -> Result<String, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        position: 0,
    };
    let document = parser.value(0)?;
    parser.whitespace();
    if parser.position != parser.bytes.len() {
        return Err(format!("trailing characters at offset {}", parser.position));
    }
    let mut output = String::with_capacity(text.len());
    document.write(&mut output);
    Ok(output)
}

/// A parsed document whose numbers keep the text they were written as.
enum Node {
    /// `null`, `true`, `false` or a number, printed as is.
    Literal(String),
    String(String),
    Array(Vec<Node>),
    Object(Vec<(String, Node)>),
}

impl Node {
    fn write(&self, output: &mut String) {
        match self {
            Self::Literal(text) => output.push_str(text),
            Self::String(text) => {
                output.push_str(&serde_json::Value::String(text.clone()).to_string());
            }
            Self::Array(items) => {
                output.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    item.write(output);
                }
                output.push(']');
            }
            Self::Object(members) => {
                output.push('{');
                for (index, (key, value)) in members.iter().enumerate() {
                    if index > 0 {
                        output.push_str(", ");
                    }
                    output.push_str(&serde_json::Value::String(key.clone()).to_string());
                    output.push_str(": ");
                    value.write(output);
                }
                output.push('}');
            }
        }
    }
}

/// Nesting deeper than this is refused, as `MySQL` refuses documents nested
/// past 100 levels; it also bounds the recursion.
const MAX_DEPTH: usize = 100;

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.position += 1;
        }
    }

    fn error(&self, what: &str) -> String {
        format!("{what} at offset {}", self.position)
    }

    fn value(&mut self, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("document nested too deeply"));
        }
        self.whitespace();
        match self.bytes.get(self.position) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Node::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => {
                for word in ["null", "true", "false"] {
                    if self.text[self.position..].starts_with(word) {
                        self.position += word.len();
                        return Ok(Node::Literal(word.to_owned()));
                    }
                }
                Err(self.error("unexpected character"))
            }
            None => Err(self.error("unexpected end of document")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Node, String> {
        self.position += 1;
        let mut items = Vec::new();
        self.whitespace();
        if self.bytes.get(self.position) == Some(&b']') {
            self.position += 1;
            return Ok(Node::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.whitespace();
            match self.bytes.get(self.position) {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    return Ok(Node::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Node, String> {
        self.position += 1;
        let mut members: Vec<(String, Node)> = Vec::new();
        self.whitespace();
        if self.bytes.get(self.position) == Some(&b'}') {
            self.position += 1;
            return Ok(Node::Object(members));
        }
        loop {
            self.whitespace();
            if self.bytes.get(self.position) != Some(&b'"') {
                return Err(self.error("expected an object key"));
            }
            let key = self.string()?;
            self.whitespace();
            if self.bytes.get(self.position) != Some(&b':') {
                return Err(self.error("expected ':'"));
            }
            self.position += 1;
            let value = self.value(depth + 1)?;
            match members.iter_mut().find(|(existing, _)| *existing == key) {
                Some(member) => member.1 = value,
                None => members.push((key, value)),
            }
            self.whitespace();
            match self.bytes.get(self.position) {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    members.sort_by(|(left, _), (right, _)| {
                        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                    });
                    return Ok(Node::Object(members));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    /// A string token, decoded. Its extent is found here and its escapes
    /// are decoded by the JSON library, so both agree on what a string is.
    fn string(&mut self) -> Result<String, String> {
        let start = self.position;
        self.position += 1;
        loop {
            match self.bytes.get(self.position) {
                Some(b'"') => break,
                Some(b'\\') => self.position += 2,
                Some(_) => self.position += 1,
                None => return Err(self.error("unterminated string")),
            }
        }
        self.position += 1;
        serde_json::from_str::<String>(&self.text[start..self.position])
            .map_err(|error| format!("invalid string at offset {start}: {error}"))
    }

    /// A number token: kept as written unless it has an exponent.
    fn number(&mut self) -> Result<Node, String> {
        let start = self.position;
        while self
            .bytes
            .get(self.position)
            .is_some_and(|byte| matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.position += 1;
        }
        let token = &self.text[start..self.position];
        let parsed = serde_json::from_str::<serde_json::Number>(token)
            .map_err(|error| format!("invalid number at offset {start}: {error}"))?;
        Ok(Node::Literal(if token.contains(['e', 'E']) {
            parsed.to_string()
        } else {
            token.to_owned()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{mysql_json_document_text, mysql_json_text};

    #[test]
    fn document_text_keeps_number_spellings_and_mysql_layout() {
        for (text, expected) in [
            (r#"{"d": 1.50, "e": 3.000}"#, r#"{"d": 1.50, "e": 3.000}"#),
            (
                r#"{"bb":[1,true,null,"xé\n"],"a":{"ccc":1.25,"d":{}}}"#,
                "{\"a\": {\"d\": {}, \"ccc\": 1.25}, \"bb\": [1, true, null, \"x\u{e9}\\n\"]}",
            ),
            (
                "[-0.001, 123456789012345678901234567890.10]",
                "[-0.001, 123456789012345678901234567890.10]",
            ),
            (
                "[1e2, 1.5E-7, 18446744073709551615]",
                "[100.0, 1.5e-7, 18446744073709551615]",
            ),
            (r#"{"k": 1, "k": 2}"#, r#"{"k": 2}"#),
            ("  \"text\" ", "\"text\""),
            ("[]", "[]"),
        ] {
            assert_eq!(
                mysql_json_document_text(text).as_deref(),
                Ok(expected),
                "{text}"
            );
        }
        for invalid in [
            "",
            "[1,]",
            "{\"a\" 1}",
            "[1] x",
            "01",
            "1.",
            "nul",
            "\"open",
        ] {
            assert!(mysql_json_document_text(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn documents_print_with_mysql_separators_and_key_order() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"bb":[1,true,null,"x"],"a":{"ccc":1.25,"d":{}}}"#)
                .expect("json");
        assert_eq!(
            mysql_json_text(&value),
            r#"{"a": {"d": {}, "ccc": 1.25}, "bb": [1, true, null, "x"]}"#
        );
    }
}
