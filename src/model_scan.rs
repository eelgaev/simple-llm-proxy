/// Incremental scanner that pulls the top-level `"model"` key out of a JSON
/// request body as it arrives, so the proxy can pick a backend without
/// buffering the whole body first.
///
/// Only depth-1 keys count: a `"model"` inside `messages[].content` is skipped
/// like any other nested value, so chat text mentioning a model name can't
/// hijack routing.
#[derive(Debug, PartialEq)]
pub enum ScanError {
    /// The body isn't a JSON object, or is malformed before `"model"`.
    Malformed,
    /// A top-level `"model"` exists but isn't a string.
    ModelNotString,
    /// The top-level object closed without a `"model"` key.
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    /// Before the opening `{`.
    Start,
    /// Expecting a key's opening quote, a `,`, or the closing `}`.
    KeyStart,
    /// Inside a key string.
    Key,
    KeyEscape,
    /// Between a key and its `:`.
    Colon,
    /// Expecting the first byte of a value.
    ValueStart,
    /// Inside the string value of the top-level `"model"` key.
    Model,
    ModelEscape,
    /// Skipping a value we don't care about.
    SkipString,
    SkipStringEscape,
    /// Skipping a scalar (number, true, false, null) until its delimiter.
    SkipScalar,
    /// Skipping a nested object or array; `depth` tracks nesting.
    SkipNested,
    SkipNestedString,
    SkipNestedEscape,
}

pub struct ModelScanner {
    state: State,
    key: String,
    model: String,
    /// Nesting level while skipping a `{...}` or `[...]` value.
    depth: usize,
}

impl ModelScanner {
    pub fn new() -> Self {
        Self {
            state: State::Start,
            key: String::new(),
            model: String::new(),
            depth: 0,
        }
    }

    /// Feeds the next chunk of the body.
    ///
    /// Returns `Ok(Some(model))` once the value is complete, `Ok(None)` if more
    /// bytes are needed, and `Err` when the body can't yield one.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Option<String>, ScanError> {
        for &b in chunk {
            match self.state {
                State::Start => match b {
                    b'{' => self.state = State::KeyStart,
                    b if b.is_ascii_whitespace() => {}
                    _ => return Err(ScanError::Malformed),
                },
                State::KeyStart => match b {
                    b'"' => {
                        self.key.clear();
                        self.state = State::Key;
                    }
                    b',' => {}
                    b'}' => return Err(ScanError::Absent),
                    b if b.is_ascii_whitespace() => {}
                    _ => return Err(ScanError::Malformed),
                },
                State::Key => match b {
                    b'"' => self.state = State::Colon,
                    b'\\' => self.state = State::KeyEscape,
                    _ => self.key.push(b as char),
                },
                State::KeyEscape => {
                    self.key.push(b as char);
                    self.state = State::Key;
                }
                State::Colon => match b {
                    b':' => self.state = State::ValueStart,
                    b if b.is_ascii_whitespace() => {}
                    _ => return Err(ScanError::Malformed),
                },
                State::ValueStart => {
                    let wanted = self.key == "model";
                    match b {
                        b if b.is_ascii_whitespace() => {}
                        b'"' if wanted => {
                            self.model.clear();
                            self.state = State::Model;
                        }
                        _ if wanted => return Err(ScanError::ModelNotString),
                        b'"' => self.state = State::SkipString,
                        b'{' | b'[' => {
                            self.depth = 1;
                            self.state = State::SkipNested;
                        }
                        b'}' => return Err(ScanError::Malformed),
                        _ => self.state = State::SkipScalar,
                    }
                }
                // The model value is a JSON string; unescape the handful of
                // escapes a model id could plausibly carry and take the rest
                // literally. Ids are ASCII in practice.
                State::Model => match b {
                    b'"' => return Ok(Some(std::mem::take(&mut self.model))),
                    b'\\' => self.state = State::ModelEscape,
                    _ => self.model.push(b as char),
                },
                State::ModelEscape => {
                    self.model.push(match b {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        other => other as char,
                    });
                    self.state = State::Model;
                }
                State::SkipString => match b {
                    b'"' => self.state = State::KeyStart,
                    b'\\' => self.state = State::SkipStringEscape,
                    _ => {}
                },
                State::SkipStringEscape => self.state = State::SkipString,
                State::SkipScalar => match b {
                    b',' => self.state = State::KeyStart,
                    b'}' => return Err(ScanError::Absent),
                    _ => {}
                },
                State::SkipNested => match b {
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' => {
                        self.depth -= 1;
                        if self.depth == 0 {
                            self.state = State::KeyStart;
                        }
                    }
                    b'"' => self.state = State::SkipNestedString,
                    _ => {}
                },
                State::SkipNestedString => match b {
                    b'"' => self.state = State::SkipNested,
                    b'\\' => self.state = State::SkipNestedEscape,
                    _ => {}
                },
                State::SkipNestedEscape => self.state = State::SkipNestedString,
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(body: &str) -> Result<Option<String>, ScanError> {
        ModelScanner::new().feed(body.as_bytes())
    }

    /// Feeds the body one byte at a time, proving the scanner survives chunk
    /// boundaries landing anywhere.
    fn scan_bytewise(body: &str) -> Result<Option<String>, ScanError> {
        let mut scanner = ModelScanner::new();
        for b in body.as_bytes() {
            if let Some(model) = scanner.feed(&[*b])? {
                return Ok(Some(model));
            }
        }
        Ok(None)
    }

    #[test]
    fn finds_model_first() {
        let body = r#"{"model":"gemma","stream":true}"#;
        assert_eq!(scan(body).unwrap().as_deref(), Some("gemma"));
        assert_eq!(scan_bytewise(body).unwrap().as_deref(), Some("gemma"));
    }

    #[test]
    fn finds_model_after_nested_values() {
        let body = r#"{"messages":[{"role":"user","content":"what model are you"}],
                       "tools":[{"function":{"name":"x"}}],"model":"qwen"}"#;
        assert_eq!(scan(body).unwrap().as_deref(), Some("qwen"));
        assert_eq!(scan_bytewise(body).unwrap().as_deref(), Some("qwen"));
    }

    #[test]
    fn ignores_nested_model_keys() {
        // A "model" inside message content must not be mistaken for routing.
        let body = r#"{"messages":[{"content":"{\"model\":\"evil\"}"},
                       {"model":"also-not-it"}],"model":"real"}"#;
        assert_eq!(scan(body).unwrap().as_deref(), Some("real"));
        assert_eq!(scan_bytewise(body).unwrap().as_deref(), Some("real"));
    }

    #[test]
    fn handles_escapes_and_whitespace() {
        let body = "{ \"model\" : \"vendor\\/model-1\" , \"n\" : 1 }";
        assert_eq!(scan(body).unwrap().as_deref(), Some("vendor/model-1"));
    }

    #[test]
    fn needs_more_when_truncated() {
        assert_eq!(scan(r#"{"messages":[{"role":"#).unwrap(), None);
        assert_eq!(scan(r#"{"model":"gem"#).unwrap(), None);
    }

    #[test]
    fn reports_absent_model() {
        assert_eq!(scan(r#"{"stream":true}"#), Err(ScanError::Absent));
        assert_eq!(scan(r#"{"messages":[]}"#), Err(ScanError::Absent));
    }

    #[test]
    fn rejects_non_string_and_non_object() {
        assert_eq!(scan(r#"{"model":123}"#), Err(ScanError::ModelNotString));
        assert_eq!(scan(r#"{"model":null}"#), Err(ScanError::ModelNotString));
        assert_eq!(scan("[1,2]"), Err(ScanError::Malformed));
    }
}
