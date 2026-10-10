//! A small JSON Schema checker for tool arguments, covering the subset the tool schemas use
//! (`type`, `enum`, `required`, `properties`, `additionalProperties: false`, `items`, `minimum`,
//! `maximum`, `minItems`, `maxItems`). The schema a tool advertises is the schema it enforces, so a
//! typo'd argument is an error that names it instead of a silently ignored key.

use serde_json::Value;

/// Why `value` does not satisfy `schema`, or `Ok(())`. `path` names the value in messages.
pub fn validate(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(t) = schema.get("type") {
        let allowed: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !allowed.is_empty() && !allowed.iter().any(|t| type_matches(value, t)) {
            return Err(format!("{path} must be {}, got {}", allowed.join(" or "), type_name(value)));
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        let list: Vec<String> = options.iter().map(|o| o.as_str().map_or_else(|| o.to_string(), str::to_string)).collect();
        return Err(format!("{path} must be one of: {}", list.join(", ")));
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("{path} must be at least {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("{path} must be at most {max}"));
        }
    }
    if let Some(obj) = value.as_object() {
        let props = schema.get("properties").and_then(Value::as_object);
        if let Some(req) = schema.get("required").and_then(Value::as_array) {
            for k in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(k) {
                    return Err(format!("{path} is missing required argument \"{k}\""));
                }
            }
        }
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            let known: Vec<&str> = props.map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
            if let Some(k) = obj.keys().find(|k| !known.contains(&k.as_str())) {
                let mut known = known;
                known.sort_unstable();
                return Err(format!("unknown argument \"{k}\" for {path}; expected: {}", known.join(", ")));
            }
        }
        if let Some(props) = props {
            for (k, v) in obj {
                if let Some(s) = props.get(k) {
                    validate(v, s, &format!("{path}.{k}"))?;
                }
            }
        }
    }
    if let Some(arr) = value.as_array() {
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
            && (arr.len() as u64) < min
        {
            return Err(format!("{path} needs at least {min} item(s)"));
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
            && (arr.len() as u64) > max
        {
            return Err(format!("{path} takes at most {max} item(s)"));
        }
        if let Some(items) = schema.get("items") {
            for (i, v) in arr.iter().enumerate() {
                validate(v, items, &format!("{path}[{i}]"))?;
            }
        }
    }
    Ok(())
}

fn type_matches(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0 && f.is_finite()),
        "number" => v.is_number(),
        _ => true,
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type":"object","properties":{"id":{"type":"string"},"n":{"type":"integer","minimum":1,"maximum":5},
            "mode":{"enum":["a","b"]},"xs":{"type":"array","items":{"type":"integer"},"maxItems":2}},
            "required":["id"],"additionalProperties":false})
    }

    #[test]
    fn accepts_valid_and_names_every_kind_of_mistake() {
        let s = schema();
        assert!(validate(&json!({"id":"x","n":3,"mode":"a","xs":[1,2]}), &s, "args").is_ok());
        let cases = [
            (json!({}), "missing required argument \"id\""),
            (json!({"id":"x","idd":1}), "unknown argument \"idd\" for args; expected: id, mode, n, xs"),
            (json!({"id":1}), "args.id must be string"),
            (json!({"id":"x","n":9}), "at most 5"),
            (json!({"id":"x","n":1.5}), "must be integer"),
            (json!({"id":"x","mode":"z"}), "must be one of: a, b"),
            (json!({"id":"x","xs":[1,2,3]}), "at most 2"),
            (json!({"id":"x","xs":["q"]}), "args.xs[0] must be integer"),
            (json!([]), "must be object"),
        ];
        for (v, want) in cases {
            let e = validate(&v, &s, "args").err().unwrap_or_default();
            assert!(e.contains(want), "{v}: {e}");
        }
    }
}
