use anyhow::{bail, Result};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Part {
    Text(String),
    Field(String),
}

/// Message text with `{field}` placeholders; `{{` and `}}` produce literal braces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Template {
    parts: Vec<Part>,
}

impl Template {
    pub fn parse(source: &str, allowed: &[&str]) -> Result<Self> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut characters = source.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '{' if characters.peek() == Some(&'{') => {
                    characters.next();
                    text.push('{');
                }
                '}' if characters.peek() == Some(&'}') => {
                    characters.next();
                    text.push('}');
                }
                '{' => {
                    let mut name = String::new();
                    loop {
                        match characters.next() {
                            Some('}') => break,
                            Some(next) if next.is_ascii_lowercase() || next == '_' => {
                                name.push(next)
                            }
                            _ => bail!("unterminated or invalid placeholder in {source:?}"),
                        }
                    }
                    if !allowed.contains(&name.as_str()) {
                        bail!(
                            "unknown placeholder {{{name}}}; allowed: {}",
                            allowed
                                .iter()
                                .map(|field| format!("{{{field}}}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                    }
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(Part::Field(name));
                }
                '}' => bail!("unmatched '}}' in {source:?}; use '}}}}' for a literal brace"),
                other => text.push(other),
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { parts })
    }

    /// True when the template contains the `{name}` placeholder.
    pub fn uses(&self, name: &str) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, Part::Field(field) if field == name))
    }

    /// True when the template has at least one placeholder.
    pub fn has_fields(&self) -> bool {
        self.parts.iter().any(|part| matches!(part, Part::Field(_)))
    }

    /// True when a placeholder has no value in `fields`.
    pub fn misses_any(&self, fields: &BTreeMap<&'static str, String>) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, Part::Field(name) if !fields.contains_key(name.as_str())))
    }

    pub fn render(&self, fields: &BTreeMap<&'static str, String>) -> String {
        self.render_with(fields, |value| value.to_string())
    }

    /// Renders with every placeholder value passed through `encode` (literal text is kept).
    pub fn render_with(
        &self,
        fields: &BTreeMap<&'static str, String>,
        encode: impl Fn(&str) -> String,
    ) -> String {
        self.parts
            .iter()
            .map(|part| match part {
                Part::Text(text) => text.clone(),
                Part::Field(name) => encode(fields.get(name.as_str()).map_or("", String::as_str)),
            })
            .collect()
    }
}
