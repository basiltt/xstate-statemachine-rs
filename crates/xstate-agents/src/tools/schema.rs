//! A structural checker for the JSON-schema subset tool arguments and structured output use.
//!
//! Python derives a strict pydantic model from each tool's signature; Rust tools carry a JSON
//! schema instead and [`validate`] enforces it with pydantic's strict semantics: no
//! coercion (`"1"` is not an integer, `1.0` is not an integer, `true` is not a number),
//! `required` keys, unknown keys refused under `additionalProperties: false`, and `default`
//! values filled in. Error messages use pydantic's wording so denials read the same.
//!
//! Supported keywords: `type` (string or list), `properties`, `required`,
//! `additionalProperties` (bool or schema), `items`, `enum`, `const`, `minimum`, `maximum`,
//! `exclusiveMinimum`, `exclusiveMaximum`, `minLength`, `maxLength`, `minItems`, `maxItems`,
//! `anyOf` / `oneOf`, local `$ref` (`#/$defs/Name`, `#/definitions/Name`) and `default`.
//! Anything else is ignored.

use serde_json::{Map, Value};

use crate::pyfmt::{py_dumps, py_repr_str};

/// Nesting deeper than this is refused (`nested too deeply`).
pub const MAX_DEPTH: usize = 64;

/// One validation failure: where, and pydantic's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    /// Path from the root: object keys and array indices (as text).
    pub loc: Vec<String>,
    /// Whether each `loc` segment is an array index.
    pub index: Vec<bool>,
    /// Message, e.g. `Input should be a valid integer`.
    pub msg: String,
}

impl SchemaError {
    /// `a.b: msg` (or just `msg` at the root).
    #[must_use]
    pub fn render(&self) -> String {
        if self.loc.is_empty() {
            self.msg.clone()
        } else {
            format!("{}: {}", self.loc.join("."), self.msg)
        }
    }
}

/// Render errors the way the tool denial message does: `a: x; b: y`.
#[must_use]
pub fn render_errors(errors: &[SchemaError]) -> String {
    errors.iter().map(SchemaError::render).collect::<Vec<_>>().join("; ")
}

/// `(keyword, words of the message, check)` for one numeric bound.
type BoundRule = (&'static str, &'static str, fn(f64, f64) -> bool);

struct Walker<'a> {
    root: &'a Value,
    errors: Vec<SchemaError>,
    loc: Vec<(String, bool)>,
}

/// Validate `value` against `schema`; on success return it with `default`s filled in, in the
/// schema's property order.
///
/// # Errors
/// Every failure found, in field order.
pub fn validate(schema: &Value, value: &Value) -> Result<Value, Vec<SchemaError>> {
    let mut w = Walker { root: schema, errors: Vec::new(), loc: Vec::new() };
    let out = w.check(schema, value, 0);
    if w.errors.is_empty() { Ok(out) } else { Err(w.errors) }
}

fn type_ok(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    }
}

fn type_msg(ty: &str) -> String {
    match ty {
        "object" => "Input should be a valid dictionary".into(),
        "array" => "Input should be a valid list".into(),
        "null" => "Input should be None".into(),
        other => format!("Input should be a valid {other}"),
    }
}

fn literal_list(items: &[Value]) -> String {
    let parts: Vec<String> =
        items.iter().map(|v| v.as_str().map_or_else(|| py_dumps(v), py_repr_str)).collect();
    match parts.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => parts.join(""),
    }
}

impl Walker<'_> {
    fn fail(&mut self, msg: impl Into<String>) {
        let (loc, index) = self.loc.iter().cloned().unzip();
        self.errors.push(SchemaError { loc, index, msg: msg.into() });
    }

    fn resolve<'s>(&'s self, schema: &'s Value) -> &'s Value {
        let Some(r) = schema.get("$ref").and_then(Value::as_str) else { return schema };
        let path = r.strip_prefix("#/").unwrap_or("");
        let mut node = self.root;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            match node.get(part) {
                Some(next) => node = next,
                None => return schema,
            }
        }
        node
    }

    fn check(&mut self, schema: &Value, value: &Value, depth: usize) -> Value {
        if depth > MAX_DEPTH {
            self.fail("nested too deeply");
            return value.clone();
        }
        let schema = self.resolve(schema).clone();
        if let Some(branches) =
            schema.get("anyOf").or_else(|| schema.get("oneOf")).and_then(Value::as_array)
        {
            return self.check_any(branches, value, depth);
        }
        if !self.check_type(&schema, value) {
            return value.clone();
        }
        if !self.check_literals(&schema, value) {
            return value.clone();
        }
        match value {
            Value::Object(map) => self.check_object(&schema, map, depth),
            Value::Array(items) => self.check_array(&schema, items, depth),
            Value::Number(_) => {
                self.check_bounds(&schema, value);
                value.clone()
            }
            Value::String(s) => {
                self.check_length(&schema, s);
                value.clone()
            }
            _ => value.clone(),
        }
    }

    fn check_any(&mut self, branches: &[Value], value: &Value, depth: usize) -> Value {
        let mut first_errors = None;
        for branch in branches {
            let mut sub = Walker { root: self.root, errors: Vec::new(), loc: self.loc.clone() };
            let out = sub.check(branch, value, depth + 1);
            if sub.errors.is_empty() {
                return out;
            }
            first_errors.get_or_insert(sub.errors);
        }
        self.errors.extend(first_errors.unwrap_or_default());
        value.clone()
    }

    fn check_type(&mut self, schema: &Value, value: &Value) -> bool {
        let types: Vec<&str> = match schema.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => return true,
        };
        if types.iter().any(|t| type_ok(t, value)) {
            return true;
        }
        self.fail(types.first().map_or_else(String::new, |t| type_msg(t)));
        false
    }

    fn check_literals(&mut self, schema: &Value, value: &Value) -> bool {
        if let Some(c) = schema.get("const")
            && c != value
        {
            self.fail(format!("Input should be {}", literal_list(std::slice::from_ref(c))));
            return false;
        }
        if let Some(options) = schema.get("enum").and_then(Value::as_array)
            && !options.contains(value)
        {
            self.fail(format!("Input should be {}", literal_list(options)));
            return false;
        }
        true
    }

    fn check_bounds(&mut self, schema: &Value, value: &Value) {
        let Some(v) = value.as_f64() else { return };
        let rules: [BoundRule; 4] = [
            ("exclusiveMinimum", "greater than", |v, b| v > b),
            ("minimum", "greater than or equal to", |v, b| v >= b),
            ("exclusiveMaximum", "less than", |v, b| v < b),
            ("maximum", "less than or equal to", |v, b| v <= b),
        ];
        for (key, words, ok) in rules {
            if let Some(bound) = schema.get(key)
                && let Some(b) = bound.as_f64()
                && !ok(v, b)
            {
                self.fail(format!("Input should be {words} {}", py_dumps(bound)));
                return;
            }
        }
    }

    fn check_length(&mut self, schema: &Value, s: &str) {
        let n = s.chars().count() as u64;
        let plural = |k: u64| if k == 1 { "character" } else { "characters" };
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
            && n < min
        {
            self.fail(format!("String should have at least {min} {}", plural(min)));
        } else if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
            && n > max
        {
            self.fail(format!("String should have at most {max} {}", plural(max)));
        }
    }

    fn check_array(&mut self, schema: &Value, items: &[Value], depth: usize) -> Value {
        let n = items.len() as u64;
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
            && n < min
        {
            self.fail(format!(
                "List should have at least {min} item{} after validation, not {n}",
                if min == 1 { "" } else { "s" }
            ));
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
            && n > max
        {
            self.fail(format!(
                "List should have at most {max} item{} after validation, not {n}",
                if max == 1 { "" } else { "s" }
            ));
        }
        let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) else {
            return Value::Array(items.to_vec());
        };
        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            self.loc.push((i.to_string(), true));
            out.push(self.check(item_schema, item, depth + 1));
            self.loc.pop();
        }
        Value::Array(out)
    }

    fn check_object(&mut self, schema: &Value, map: &Map<String, Value>, depth: usize) -> Value {
        let empty = Map::new();
        let props = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut out = Map::new();
        for (name, prop) in props {
            self.loc.push((name.clone(), false));
            if let Some(v) = map.get(name) {
                out.insert(name.clone(), self.check(prop, v, depth + 1));
            } else if required.contains(&name.as_str()) {
                self.fail("Field required");
            } else if let Some(default) = self.resolve(prop).get("default") {
                out.insert(name.clone(), default.clone());
            }
            self.loc.pop();
        }
        for name in required.iter().filter(|r| !props.contains_key(**r) && !map.contains_key(**r)) {
            self.loc.push(((*name).to_owned(), false));
            self.fail("Field required");
            self.loc.pop();
        }
        let extra = schema.get("additionalProperties");
        for (k, v) in map.iter().filter(|(k, _)| !props.contains_key(*k)) {
            self.loc.push((k.clone(), false));
            match extra {
                Some(Value::Bool(false)) => self.fail("Extra inputs are not permitted"),
                Some(s @ Value::Object(_)) => {
                    let checked = self.check(s, v, depth + 1);
                    out.insert(k.clone(), checked);
                }
                _ => {
                    out.insert(k.clone(), v.clone());
                }
            }
            self.loc.pop();
        }
        Value::Object(out)
    }
}

/// Every property name declared anywhere in `schema` (nested objects, items, `$defs`): the
/// only `loc` strings safe to echo back to the model.
#[must_use]
pub fn field_names(schema: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match v {
            Value::Object(map) => {
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    out.extend(props.keys().cloned());
                }
                map.values().for_each(|c| walk(c, out, depth + 1));
            }
            Value::Array(items) => items.iter().for_each(|c| walk(c, out, depth + 1)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(schema, &mut out, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refund_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "order_id": {"type": "string", "minLength": 1},
                "amount_cents": {"type": "integer", "exclusiveMinimum": 0},
                "note": {"type": "string", "default": ""},
                "tags": {"type": "array", "items": {"enum": ["a", "b"]}, "maxItems": 2}
            },
            "required": ["order_id", "amount_cents"],
            "additionalProperties": false
        })
    }

    fn errors(schema: &Value, v: &Value) -> String {
        render_errors(&validate(schema, v).err().unwrap_or_default())
    }

    #[test]
    fn strict_types_and_defaults() {
        let s = refund_schema();
        let ok = validate(&s, &json!({"amount_cents": 5, "order_id": "A"}))
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(ok, json!({"order_id": "A", "amount_cents": 5, "note": ""}));
        assert_eq!(
            errors(&s, &json!({"order_id": "A", "amount_cents": "all"})),
            "amount_cents: Input should be a valid integer"
        );
        assert_eq!(
            errors(&s, &json!({"order_id": "A", "amount_cents": 1.0})),
            "amount_cents: Input should be a valid integer"
        );
        assert_eq!(
            errors(&s, &json!({"amount_cents": 0, "x": 1})),
            "order_id: Field required; amount_cents: Input should be greater than 0; x: Extra inputs are not permitted"
        );
        assert_eq!(
            errors(&s, &json!({"order_id": "", "amount_cents": 1, "tags": ["c"]})),
            "order_id: String should have at least 1 character; tags.0: Input should be 'a' or 'b'"
        );
        assert_eq!(errors(&s, &json!([])), "Input should be a valid dictionary");
    }

    #[test]
    fn any_of_refs_and_depth() {
        let s = json!({
            "$defs": {"Inner": {"type": "object", "properties": {"n": {"type": ["integer", "null"]}}, "required": ["n"]}},
            "type": "object",
            "properties": {"inner": {"$ref": "#/$defs/Inner"}, "opt": {"anyOf": [{"type": "number"}, {"type": "null"}], "default": null}},
            "additionalProperties": {"type": "boolean"}
        });
        assert!(validate(&s, &json!({"inner": {"n": null}, "flag": true})).is_ok());
        assert_eq!(
            errors(&s, &json!({"inner": {"n": "x"}, "opt": "y", "flag": 1})),
            "inner.n: Input should be a valid integer; opt: Input should be a valid number; flag: Input should be a valid boolean"
        );
        let mut deep = json!(1);
        for _ in 0..80 {
            deep = json!([deep]);
        }
        let nested = json!({"type": "array", "items": {"$ref": "#"}});
        assert!(errors(&nested, &deep).contains("nested too deeply"));
        assert_eq!(errors(&json!({"const": "x"}), &json!("y")), "Input should be 'x'");
        assert_eq!(errors(&json!({"type": "null"}), &json!(1)), "Input should be None");
        assert_eq!(
            errors(&json!({"maximum": 2}), &json!(3)),
            "Input should be less than or equal to 2"
        );
        assert_eq!(
            errors(&json!({"maxLength": 1}), &json!("ab")),
            "String should have at most 1 character"
        );
        assert_eq!(
            errors(&json!({"minItems": 2}), &json!([1])),
            "List should have at least 2 items after validation, not 1"
        );
    }

    #[test]
    fn field_names_cover_nested_schemas() {
        let names = field_names(&refund_schema());
        assert!(names.contains(&"order_id".to_owned()) && names.contains(&"tags".to_owned()));
    }
}
