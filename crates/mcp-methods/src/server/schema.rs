//! Tool schemas that stay inside JSON Schema 2020-12.
//!
//! schemars labels Rust integer and float types with `format` values such as
//! `uint`, `uint64`, `int64` and `double`. None is in the 2020-12 format
//! vocabulary: Claude Code logs `unknown format "uint" ignored` for each one,
//! and a client validating with Ajv in strict mode rejects the schema.
//!
//! [`McpServer`](crate::server::McpServer) applies [`standardize`] to every
//! tool it lists, so tools registered through the framework need nothing.
//! A binary that builds its own schemas with schemars (an output schema, a
//! hand-assembled `Tool`) can run the same cleanup by passing
//! [`StandardFormats`] as a transform:
//!
//! ```
//! use schemars::{generate::SchemaSettings, JsonSchema};
//! use mcp_methods::server::schema::StandardFormats;
//!
//! #[derive(JsonSchema)]
//! struct Args { limit: u64 }
//!
//! let generator = SchemaSettings::draft2020_12()
//!     .with_transform(StandardFormats)
//!     .into_generator();
//! let schema = generator.into_root_schema_for::<Args>();
//! assert_eq!(schema.as_value()["properties"]["limit"].get("format"), None);
//! assert_eq!(schema.as_value()["properties"]["limit"]["minimum"], 0);
//! ```

use schemars::transform::Transform;
use schemars::Schema;
use serde_json::{Map, Value};

/// The `format` values of the JSON Schema 2020-12 format vocabulary.
const STANDARD_FORMATS: &[&str] = &[
    "date-time",
    "date",
    "time",
    "duration",
    "email",
    "idn-email",
    "hostname",
    "idn-hostname",
    "ipv4",
    "ipv6",
    "uri",
    "uri-reference",
    "iri",
    "iri-reference",
    "uuid",
    "uri-template",
    "json-pointer",
    "relative-json-pointer",
    "regex",
];

/// A schemars [`Transform`] that drops non-standard `format` values.
///
/// `type: integer` stays, and an unsigned type (`uint`, `uint8`, ...) keeps
/// or gains `minimum: 0`, so the constraint the format implied survives.
/// Add it with `SchemaSettings::with_transform(StandardFormats)`; the cleanup
/// recurses into every subschema.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardFormats;

impl Transform for StandardFormats {
    fn transform(&mut self, schema: &mut Schema) {
        if let Some(map) = schema.as_object_mut() {
            standardize_object(map);
        }
    }
}

/// Rewrite `schema` (any JSON Schema value, in place) so every `format` is
/// a standard one. See [`StandardFormats`].
pub fn standardize(schema: &mut Value) {
    match schema {
        Value::Object(map) => standardize_object(map),
        Value::Array(items) => items.iter_mut().for_each(standardize),
        _ => {}
    }
}

fn standardize_object(map: &mut Map<String, Value>) {
    let unsupported = match map.get("format") {
        Some(Value::String(format)) if !STANDARD_FORMATS.contains(&format.as_str()) => {
            Some(format.starts_with("uint"))
        }
        _ => None,
    };
    if let Some(unsigned) = unsupported {
        map.remove("format");
        if unsigned {
            map.entry("minimum").or_insert(Value::from(0));
        }
    }
    // A property *named* "format" is an object value here, never a string,
    // so recursing into it cannot mistake it for the keyword. Instance data
    // is not a subschema: a `"format"` key inside it is a value.
    for (keyword, value) in map.iter_mut() {
        if !matches!(keyword.as_str(), "default" | "examples" | "const" | "enum") {
            standardize(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_non_standard_formats_and_keeps_the_rest() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer", "format": "uint64", "minimum": 0},
                "m": {"type": ["integer", "null"], "format": "uint"},
                "s": {"type": "integer", "format": "int64"},
                "f": {"type": "number", "format": "double"},
                "when": {"type": "string", "format": "date-time"},
                "format": {"type": "string", "format": "uuid"},
                "list": {"type": "array", "items": {"type": "integer", "format": "uint8", "minimum": 0}}
            },
            "$defs": {"d": {"type": "integer", "format": "uint32"}}
        });
        standardize(&mut schema);
        let p = &schema["properties"];
        assert_eq!(p["n"], json!({"type": "integer", "minimum": 0}));
        assert_eq!(p["m"], json!({"type": ["integer", "null"], "minimum": 0}));
        assert_eq!(p["s"], json!({"type": "integer"}));
        assert_eq!(p["f"], json!({"type": "number"}));
        assert_eq!(p["when"]["format"], "date-time");
        assert_eq!(p["format"], json!({"type": "string", "format": "uuid"}));
        assert_eq!(p["list"]["items"], json!({"type": "integer", "minimum": 0}));
        assert_eq!(
            schema["$defs"]["d"],
            json!({"type": "integer", "minimum": 0})
        );
    }

    #[test]
    fn instance_data_keeps_its_format_keys() {
        let mut schema = json!({
            "type": "object",
            "default": {"format": "csv"},
            "examples": [{"format": "uint8"}],
            "properties": {
                "out": {
                    "const": {"format": "csv"},
                    "enum": [{"format": "uint64"}],
                    "type": "integer",
                    "format": "uint64"
                }
            }
        });
        standardize(&mut schema);
        assert_eq!(schema["default"], json!({"format": "csv"}));
        assert_eq!(schema["examples"], json!([{"format": "uint8"}]));
        let out = &schema["properties"]["out"];
        assert_eq!(out["const"], json!({"format": "csv"}));
        assert_eq!(out["enum"], json!([{"format": "uint64"}]));
        assert!(out.get("format").is_none());
        assert_eq!(out["minimum"], 0);
    }
}
