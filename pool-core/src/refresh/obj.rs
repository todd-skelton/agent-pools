//! Object helpers for state held as JSON, with JavaScript's property semantics: assigning an
//! existing key keeps its place, a new key goes last, and `delete` keeps the others in order.

use crate::js;
use serde_json::{Map, Value};

/// The value as an object, made one (empty) if it is not.
pub fn obj(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

/// `v[k] = x`.
pub fn put(v: &mut Value, k: &str, x: Value) {
    obj(v).insert(k.to_string(), x);
}

/// `delete v[k]`.
pub fn del(v: &mut Value, k: &str) {
    if let Value::Object(m) = v {
        m.shift_remove(k);
    }
}

/// `v[k]` when `v` is an object.
pub fn at<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    js::get(Some(v), k)
}

/// `a === b` for values read from JSON (`None` is undefined): numbers by value (`NaN` is
/// never equal), anything else by type and content. Objects and arrays never come up here as
/// operands except by identity, which a copy cannot keep, so they compare by content.
pub fn strict_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// `{ ...base, k1: v1, ... }`: `None` stands for an `undefined` value, which
/// `JSON.stringify` drops, so the key is removed.
pub fn spread(base: &Value, extra: Vec<(&str, Option<Value>)>) -> Value {
    let mut out = match base {
        Value::Object(m) => Value::Object(m.clone()),
        _ => Value::Object(Map::new()),
    };
    for (k, v) in extra {
        match v {
            Some(v) => put(&mut out, k, v),
            None => del(&mut out, k),
        }
    }
    out
}

/// `Number.isInteger(v)`.
pub fn is_integer(v: Option<&Value>) -> bool {
    js::finite(v).is_some_and(|f| f.fract() == 0.0)
}

/// A number as JavaScript keeps it in JSON: integral values as integers, so a copy written
/// out reads `63`, not `63.0`.
pub fn num(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        if f == 0.0 {
            return Value::from(0);
        }
        return Value::from(f as i64);
    }
    js::number_value(f)
}

/// Every number in `v` as [`num`] writes it.
pub fn jsify(v: Value) -> Value {
    match v {
        Value::Number(n) => n.as_f64().map_or(Value::Number(n), num),
        Value::Array(a) => Value::Array(a.into_iter().map(jsify).collect()),
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, v)| (k, jsify(v))).collect()),
        v => v,
    }
}

/// A string value, or `None`.
pub fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    at(v, k).and_then(Value::as_str)
}
