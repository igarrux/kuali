//! Recovers JSON from LLM responses.
//!
//! Schema-aware providers return clean JSON, while CLIs may wrap it in Markdown
//! or explanatory prose. This extracts the object without trusting model format.

/// Returns the first complete JSON object found in text.
pub fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            // A quote preceded by a slash does not close the string, but `\\"`
            // does; toggle the flag rather than merely setting it.
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Converts empty or blank strings into `None`. Kuali schemas use `""` for
/// optional fields because several structured-output modes reject nullable types.
pub fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_object_passes_through() {
        assert_eq!(extract_json_object(r#"{"a":1}"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn unwraps_a_markdown_fenced_block() {
        let text = "Aquí tienes:\n```json\n{\"a\": 1}\n```\n¡Espero que sirva!";
        assert_eq!(extract_json_object(text), Some("{\"a\": 1}"));
    }

    #[test]
    fn handles_nested_objects() {
        let text = r#"blah {"a": {"b": {"c": 1}}, "d": 2} trailing"#;
        assert_eq!(
            extract_json_object(text),
            Some(r#"{"a": {"b": {"c": 1}}, "d": 2}"#)
        );
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_the_scanner() {
        let text = r#"{"text": "usa {llaves} aquí", "n": 1}"#;
        assert_eq!(extract_json_object(text), Some(text));
    }

    #[test]
    fn escaped_quotes_inside_strings_are_survived() {
        let text = r#"{"text": "dijo \"hola\" y se fue"}"#;
        assert_eq!(extract_json_object(text), Some(text));
    }

    #[test]
    fn a_trailing_backslash_before_the_closing_quote_does_not_swallow_the_object() {
        // `"c:\\"` ends the string: an escaped slash does not escape the quote.
        let text = r#"{"path": "c:\\", "ok": true}"#;
        assert_eq!(extract_json_object(text), Some(text));
    }

    #[test]
    fn truncated_json_yields_nothing_rather_than_garbage() {
        assert_eq!(extract_json_object(r#"{"a": {"b": 1}"#), None);
    }

    #[test]
    fn text_without_any_object_yields_nothing() {
        assert_eq!(extract_json_object("no hay nada aquí"), None);
    }

    #[test]
    fn blank_and_null_become_none() {
        assert_eq!(non_empty("  "), None);
        assert_eq!(non_empty("null"), None);
        assert_eq!(non_empty(" Ana "), Some("Ana".to_string()));
    }
}
