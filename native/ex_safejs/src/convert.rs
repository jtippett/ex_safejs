use crate::atoms;
use rustler::{Encoder, Env, Term};

const MAX_DEPTH: u32 = 64;

// ── JS → Erlang ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum JsValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<JsValue>),
    Object(Vec<(String, JsValue)>),
}

impl Encoder for JsValue {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        match self {
            JsValue::Null => rustler::types::atom::nil().encode(env),
            JsValue::Bool(b) => b.encode(env),
            JsValue::Int(n) => n.encode(env),
            JsValue::Float(f) => f.encode(env),
            JsValue::String(s) => s.encode(env),
            JsValue::Array(arr) => arr.encode(env),
            JsValue::Object(pairs) => {
                let keys: Vec<Term> = pairs.iter().map(|(k, _)| k.encode(env)).collect();
                let vals: Vec<Term> = pairs.iter().map(|(_, v)| v.encode(env)).collect();
                Term::map_from_arrays(env, &keys, &vals)
                    .unwrap_or_else(|_| rustler::types::atom::nil().encode(env))
            }
        }
    }
}

pub enum JsResult {
    Ok(JsValue),
    Err(String),
}

impl From<Result<JsValue, String>> for JsResult {
    fn from(result: Result<JsValue, String>) -> Self {
        match result {
            Ok(val) => JsResult::Ok(val),
            Err(err) => JsResult::Err(err),
        }
    }
}

impl Encoder for JsResult {
    fn encode<'a>(&self, env: Env<'a>) -> Term<'a> {
        match self {
            JsResult::Ok(val) => (atoms::ok(), val).encode(env),
            JsResult::Err(msg) => (atoms::error(), msg.as_str()).encode(env),
        }
    }
}

pub fn js_to_term<'js>(
    ctx: &rquickjs::Ctx<'js>,
    val: rquickjs::Value<'js>,
) -> Result<JsValue, String> {
    js_to_term_depth(ctx, val, 0)
}

fn js_to_term_depth<'js>(
    ctx: &rquickjs::Ctx<'js>,
    val: rquickjs::Value<'js>,
    depth: u32,
) -> Result<JsValue, String> {
    if depth > MAX_DEPTH {
        return Err("Maximum nesting depth exceeded".to_string());
    }

    if val.is_undefined() || val.is_null() {
        return Ok(JsValue::Null);
    }

    if let Some(b) = val.as_bool() {
        return Ok(JsValue::Bool(b));
    }

    if let Some(n) = val.as_int() {
        return Ok(JsValue::Int(n as i64));
    }

    if let Some(f) = val.as_float() {
        if !f.is_finite() {
            return Ok(JsValue::Null);
        }
        if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 {
            return Ok(JsValue::Int(f as i64));
        }
        return Ok(JsValue::Float(f));
    }

    if let Some(s) = val.as_string() {
        return Ok(JsValue::String(
            s.to_string()
                .map_err(|e| format!("String conversion error: {e}"))?,
        ));
    }

    if val.is_function() {
        return Ok(JsValue::Null);
    }

    if val.is_array() {
        if let Some(obj) = val.as_object() {
            let length: i32 = obj.get("length").unwrap_or(0);
            let length = length.max(0) as u32;
            let mut result = Vec::with_capacity((length as usize).min(1024));
            for i in 0..length {
                let item: rquickjs::Value = obj
                    .get(i)
                    .unwrap_or(rquickjs::Value::new_undefined(ctx.clone()));
                result.push(js_to_term_depth(ctx, item, depth + 1)?);
            }
            return Ok(JsValue::Array(result));
        }
    }

    if val.is_object() {
        if let Some(obj) = val.as_object() {
            let mut pairs = Vec::new();
            for key in obj.keys::<String>().flatten() {
                let v: rquickjs::Value = obj
                    .get(&*key)
                    .unwrap_or(rquickjs::Value::new_undefined(ctx.clone()));
                if !v.is_function() {
                    pairs.push((key, js_to_term_depth(ctx, v, depth + 1)?));
                }
            }
            return Ok(JsValue::Object(pairs));
        }
    }

    Ok(JsValue::Null)
}

// ── Erlang → JS ─────────────────────────────────────────────────────────────

/// Represents a callback result sent from Elixir back to JS.
/// We use an enum to distinguish ok/error at the Rust level,
/// then convert to JS value or throw.
pub enum CallbackResult {
    Ok(TermValue),
    Err(String),
}

/// Intermediate representation of an Erlang term for conversion to JS.
/// Decoded from Erlang terms on the NIF side, then converted to rquickjs
/// values on the worker thread.
#[derive(Debug, Clone)]
pub enum TermValue {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Atom(String),
    List(Vec<TermValue>),
    Map(Vec<(TermValue, TermValue)>),
}

/// Decode a rustler Term into a TermValue.
/// Called on the NIF thread (has access to Env).
pub fn term_to_intermediate<'a>(term: Term<'a>) -> Result<TermValue, String> {
    term_to_intermediate_depth(term, 0)
}

fn term_to_intermediate_depth<'a>(term: Term<'a>, depth: u32) -> Result<TermValue, String> {
    if depth > MAX_DEPTH {
        return Err("Maximum nesting depth exceeded".to_string());
    }

    // nil
    if term.is_atom() {
        let atom_str: String = term
            .atom_to_string()
            .map_err(|_| "Failed to convert atom".to_string())?;
        return match atom_str.as_str() {
            "nil" => Ok(TermValue::Nil),
            "true" => Ok(TermValue::Bool(true)),
            "false" => Ok(TermValue::Bool(false)),
            _ => Ok(TermValue::Atom(atom_str)),
        };
    }

    // integer
    if let Ok(n) = term.decode::<i64>() {
        return Ok(TermValue::Int(n));
    }

    // float
    if let Ok(f) = term.decode::<f64>() {
        return Ok(TermValue::Float(f));
    }

    // binary/string
    if term.is_binary() {
        let s: String = term
            .decode()
            .map_err(|_| "Failed to decode binary as UTF-8 string".to_string())?;
        return Ok(TermValue::String(s));
    }

    // list
    if term.is_list() {
        let items: Vec<Term> = term
            .decode()
            .map_err(|_| "Failed to decode list".to_string())?;
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            result.push(term_to_intermediate_depth(item, depth + 1)?);
        }
        return Ok(TermValue::List(result));
    }

    // map
    if term.is_map() {
        let iter: rustler::MapIterator = term
            .decode()
            .map_err(|_| "Failed to decode map".to_string())?;
        let mut pairs = Vec::new();
        for (k, v) in iter {
            let key = term_to_intermediate_depth(k, depth + 1)?;
            let val = term_to_intermediate_depth(v, depth + 1)?;
            pairs.push((key, val));
        }
        return Ok(TermValue::Map(pairs));
    }

    Err("Unsupported Elixir term type".to_string())
}

/// Convert a TermValue to a rquickjs Value.
/// Called on the worker thread (has access to rquickjs Ctx).
pub fn intermediate_to_js<'js>(
    ctx: &rquickjs::Ctx<'js>,
    tv: &TermValue,
) -> Result<rquickjs::Value<'js>, String> {
    intermediate_to_js_depth(ctx, tv, 0)
}

fn intermediate_to_js_depth<'js>(
    ctx: &rquickjs::Ctx<'js>,
    tv: &TermValue,
    depth: u32,
) -> Result<rquickjs::Value<'js>, String> {
    if depth > MAX_DEPTH {
        return Err("Maximum nesting depth exceeded".to_string());
    }

    match tv {
        TermValue::Nil => Ok(rquickjs::Value::new_null(ctx.clone())),
        TermValue::Bool(b) => Ok(rquickjs::Value::new_bool(ctx.clone(), *b)),
        TermValue::Int(n) => {
            if *n >= i32::MIN as i64 && *n <= i32::MAX as i64 {
                Ok(rquickjs::Value::new_int(ctx.clone(), *n as i32))
            } else {
                Ok(rquickjs::Value::new_float(ctx.clone(), *n as f64))
            }
        }
        TermValue::Float(f) => Ok(rquickjs::Value::new_float(ctx.clone(), *f)),
        TermValue::String(s) => {
            let js_str = rquickjs::String::from_str(ctx.clone(), s)
                .map_err(|e| format!("Failed to create JS string: {e}"))?;
            Ok(js_str.into())
        }
        TermValue::Atom(s) => {
            let js_str = rquickjs::String::from_str(ctx.clone(), s)
                .map_err(|e| format!("Failed to create JS string: {e}"))?;
            Ok(js_str.into())
        }
        TermValue::List(items) => {
            let array = rquickjs::Array::new(ctx.clone())
                .map_err(|e| format!("Failed to create JS array: {e}"))?;
            for (i, item) in items.iter().enumerate() {
                let val = intermediate_to_js_depth(ctx, item, depth + 1)?;
                array
                    .set(i, val)
                    .map_err(|e| format!("Failed to set array element: {e}"))?;
            }
            Ok(array.into_value())
        }
        TermValue::Map(pairs) => {
            let obj = rquickjs::Object::new(ctx.clone())
                .map_err(|e| format!("Failed to create JS object: {e}"))?;
            for (k, v) in pairs {
                let key_str = match k {
                    TermValue::String(s) => s.clone(),
                    TermValue::Atom(s) => s.clone(),
                    TermValue::Int(n) => n.to_string(),
                    _ => return Err("Map keys must be strings, atoms, or integers".to_string()),
                };
                let val = intermediate_to_js_depth(ctx, v, depth + 1)?;
                obj.set(&*key_str, val)
                    .map_err(|e| format!("Failed to set object property: {e}"))?;
            }
            Ok(obj.into_value())
        }
    }
}
