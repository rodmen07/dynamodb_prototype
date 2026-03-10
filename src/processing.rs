use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn remove_nulls(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                if let Some(mut vv) = map.remove(&k) {
                    remove_nulls(&mut vv);
                    match &vv {
                        Value::Null => {}
                        _ => {
                            map.insert(k, vv);
                        }
                    }
                }
            }
        }
        Value::Array(arr) => {
            arr.retain(|x| !x.is_null());
            for x in arr.iter_mut() {
                remove_nulls(x);
            }
        }
        _ => {}
    }
}

pub fn apply_defaults(obj: &mut Map<String, Value>) {
    if !obj.contains_key("event_type") {
        obj.insert("event_type".to_string(), Value::String("unknown".to_string()));
    }
    if !obj.contains_key("created_at") {
        let now = chrono::Utc::now().to_rfc3339();
        obj.insert("created_at".to_string(), Value::String(now));
    }
    if !obj.contains_key("when") {
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            obj.insert("when".to_string(), Value::Number(serde_json::Number::from(now.as_secs())));
        }
    }
}

pub fn normalize_amount(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_nulls_and_defaults() {
        let mut v: Value = serde_json::from_str(r#"{"a":null, "b": {"c": null}, "d": "x"}"#).unwrap();
        remove_nulls(&mut v);
        if let Value::Object(map) = v {
            assert!(!map.contains_key("a"));
            assert!(map.contains_key("d"));
        } else {
            panic!("expected object");
        }
    }

    #[test]
    fn test_normalize_amount() {
        assert_eq!(normalize_amount(&Value::Number(serde_json::Number::from(5u64))), Some(5.0));
        assert_eq!(normalize_amount(&Value::String("3.14".to_string())), Some(3.14));
    }
}
