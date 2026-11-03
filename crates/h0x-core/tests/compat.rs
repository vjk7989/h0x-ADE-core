//! The API only grows. `schema/released.openrpc.json` is the API as last
//! released; the code's must accept every request that one accepted and
//! give every answer field that one gave. Refreshed at each release.

use serde_json::{Value, json};
use std::collections::BTreeSet;

fn resolve<'a>(doc: &'a Value, s: &'a Value) -> &'a Value {
    match s["$ref"].as_str().and_then(|r| r.strip_prefix("#/components/schemas/")) {
        Some(name) => resolve(doc, &doc["components"]["schemas"][name]),
        None => s,
    }
}

/// Its kinds of value (`string`, `null`, ...) and the object schemas among them.
fn shape<'a>(doc: &'a Value, s: &'a Value) -> (BTreeSet<String>, Vec<&'a Value>) {
    let s = resolve(doc, s);
    let mut kinds = BTreeSet::new();
    let mut objects = Vec::new();
    match &s["type"] {
        Value::String(t) => drop(kinds.insert(t.clone())),
        Value::Array(ts) => kinds.extend(ts.iter().filter_map(|t| t.as_str().map(String::from))),
        _ => {}
    }
    if s.get("properties").is_some() || s.get("items").is_some() || s.get("enum").is_some() {
        objects.push(s);
    }
    for member in s["anyOf"].as_array().into_iter().chain(s["oneOf"].as_array()).flatten() {
        let (k, o) = shape(doc, member);
        kinds.extend(k);
        objects.extend(o);
    }
    (kinds, objects)
}

fn names(v: &Value) -> BTreeSet<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()
}

fn props(s: &Value) -> Vec<(&String, &Value)> {
    s["properties"].as_object().into_iter().flatten().collect()
}

/// `input`: the new side must accept all the old did. Else (output) the new
/// side must give all the old gave, nothing the old couldn't.
fn compare(old: (&Value, &Value), new: (&Value, &Value), input: bool, at: &str, depth: u8, out: &mut Vec<String>) {
    if depth > 8 {
        return;
    }
    let (ok, oo) = shape(old.0, old.1);
    let (nk, no) = shape(new.0, new.1);
    let (wider, narrower) = if input { (&nk, &ok) } else { (&ok, &nk) };
    if !wider.is_empty() && !narrower.is_subset(wider) {
        out.push(format!("{at}: type {ok:?} became {nk:?}"));
    }
    // An answer that promised a shape keeps promising it (untyped is wider).
    if !input && ((!ok.is_empty() && nk.is_empty()) || (!oo.is_empty() && no.is_empty())) {
        out.push(format!("{at}: lost its type"));
    }
    let (Some(o), Some(n)) = (oo.first(), no.first()) else { return };
    let (oe, ne) = (names(&o["enum"]), names(&n["enum"]));
    if !oe.is_empty() && (if input { !oe.is_subset(&ne) } else { !ne.is_subset(&oe) }) {
        out.push(format!("{at}: values {oe:?} became {ne:?}"));
    }
    if o.get("items").is_some() {
        compare((old.0, &o["items"]), (new.0, &n["items"]), input, &format!("{at}[]"), depth + 1, out);
    }
    let (oreq, nreq) = (names(&o["required"]), names(&n["required"]));
    for (k, os) in props(o) {
        let path = format!("{at}.{k}");
        match n["properties"].get(k) {
            None => out.push(format!("{path}: removed")),
            Some(ns) => compare((old.0, os), (new.0, ns), input, &path, depth + 1, out),
        }
        if !input && oreq.contains(k) && !nreq.contains(k) {
            out.push(format!("{path}: may be missing now"));
        }
    }
    if input {
        for k in nreq.difference(&oreq) {
            out.push(format!("{at}.{k}: newly required"));
        }
    }
}

fn breaks(old: &Value, new: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let methods = |doc: &Value| doc["methods"].as_array().cloned().unwrap_or_default();
    let new_methods = methods(new);
    for om in methods(old) {
        let name = om["name"].as_str().unwrap_or_default().to_string();
        let Some(nm) = new_methods.iter().find(|m| m["name"] == name.as_str()) else {
            out.push(format!("{name}: removed"));
            continue;
        };
        let params = |m: &Value| m["params"].as_array().cloned().unwrap_or_default();
        let (op, np) = (params(&om), params(nm));
        for p in &op {
            let at = format!("{name}({})", p["name"].as_str().unwrap_or_default());
            match np.iter().find(|q| q["name"] == p["name"]) {
                None => out.push(format!("{at}: removed")),
                Some(q) => {
                    if q["required"] == true && p["required"] != true {
                        out.push(format!("{at}: newly required"));
                    }
                    compare((old, &p["schema"]), (new, &q["schema"]), true, &at, 0, &mut out);
                }
            }
        }
        for q in np.iter().filter(|q| q["required"] == true && !op.iter().any(|p| p["name"] == q["name"])) {
            out.push(format!("{name}({}): new and required", q["name"].as_str().unwrap_or_default()));
        }
        compare(
            (old, &om["result"]["schema"]),
            (new, &nm["result"]["schema"]),
            false,
            &format!("{name} ->"),
            0,
            &mut out,
        );
    }
    out
}

#[test]
fn the_api_only_grows_since_the_last_release() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema");
    let released: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("released.openrpc.json")).unwrap()).unwrap();
    let now = herdr_core::api::openrpc();
    let broken = breaks(&released, &now);
    assert!(broken.is_empty(), "these changes break clients of the released API:\n{}", broken.join("\n"));
}

#[test]
fn the_rules_catch_what_breaks_and_allow_what_grows() {
    let doc = |params: Value, result: Value| json!({"methods": [{"name": "m", "params": params, "result": {"schema": result}}], "components": {"schemas": {}}});
    let obj = |props: Value, required: Value| json!({"type": "object", "properties": props, "required": required});
    let base = doc(
        json!([{"name": "t", "required": true, "schema": {"type": "string"}}, {"name": "o", "required": false, "schema": {"type": "string", "enum": ["a", "b"]}}]),
        obj(json!({"x": {"type": "string"}, "y": {"type": ["integer", "null"]}}), json!(["x"])),
    );
    assert_eq!(breaks(&base, &base), Vec::<String>::new());
    let grown = doc(
        json!([{"name": "t", "required": true, "schema": {"type": "string"}}, {"name": "o", "required": false, "schema": {"type": "string", "enum": ["a", "b", "c"]}}, {"name": "n", "required": false, "schema": {"type": "string"}}]),
        obj(json!({"x": {"type": "string"}, "y": {"type": "integer"}, "z": {"type": "string"}}), json!(["x"])),
    );
    assert_eq!(breaks(&base, &grown), Vec::<String>::new(), "added inputs, values and fields are fine");
    let broken = doc(
        json!([{"name": "t", "required": true, "schema": {"type": "integer"}}, {"name": "o", "required": true, "schema": {"type": "string", "enum": ["a"]}}, {"name": "n", "required": true, "schema": {"type": "string"}}]),
        obj(json!({"y": {"type": ["integer", "string", "null"]}}), json!([])),
    );
    let found = breaks(&base, &broken).join("\n");
    for expected in [
        "m(t): type",
        "m(o): newly required",
        "m(o): values",
        "m(n): new and required",
        "m ->.x: removed",
        "m ->.y: type",
    ] {
        assert!(found.contains(expected), "missed {expected:?} in:\n{found}");
    }
    let untyped = doc(json!([]), obj(json!({"x": true, "y": {"type": ["integer", "null"]}}), json!(["x"])));
    assert!(breaks(&base, &untyped).join("\n").contains("m ->.x: lost its type"));
    let gone = json!({"methods": [], "components": {"schemas": {}}});
    assert_eq!(breaks(&base, &gone), ["m: removed"]);
}
