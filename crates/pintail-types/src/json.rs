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

#[cfg(test)]
mod tests {
    use super::mysql_json_text;

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
