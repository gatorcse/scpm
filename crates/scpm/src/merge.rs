//! Deep merge of YAML values, used for presets (`extends:`) and variant overrides
//! (design §4.4, "Resolution order").
//!
//! Rules: maps merge key by key, scalars override, lists append (no deduplication: option
//! lists such as `[-release, '17']` must keep repeated tokens). A value tagged `!replace`
//! replaces the inherited value. At the top level of a module, the matrix-defining keys
//! `scala` and `platforms` always replace.

use serde_yaml_ng::Value;
use serde_yaml_ng::value::TaggedValue;

const REPLACE_TAG: &str = "replace";

/// Top-level module keys whose values replace rather than merge.
const MATRIX_KEYS: [&str; 2] = ["scala", "platforms"];

/// Merges a module-shaped `overlay` on top of `base` in place.
pub fn merge_module(base: &mut Value, overlay: Value) {
    let (Value::Mapping(base_map), Value::Mapping(overlay_map)) = (&mut *base, &overlay) else {
        merge(base, overlay);
        return;
    };
    for (key, value) in overlay_map.clone() {
        let is_matrix = key.as_str().is_some_and(|k| MATRIX_KEYS.contains(&k));
        match base_map.get_mut(&key) {
            Some(existing) if !is_matrix => merge(existing, value),
            _ => {
                base_map.insert(key, strip_replace_tags(value));
            }
        }
    }
}

/// Merges `overlay` on top of `base` in place.
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (base, Value::Tagged(tagged)) if tagged.tag == REPLACE_TAG => {
            *base = strip_replace_tags(tagged.value);
        }
        (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base_map.insert(key, strip_replace_tags(value));
                    }
                }
            }
        }
        (Value::Sequence(base_seq), Value::Sequence(overlay_seq)) => {
            base_seq.extend(overlay_seq.into_iter().map(strip_replace_tags));
        }
        (base, overlay) => *base = strip_replace_tags(overlay),
    }
}

/// Removes `!replace` tags so they don't leak into deserialization when there was nothing
/// to replace.
pub fn strip_replace_tags(value: Value) -> Value {
    match value {
        Value::Tagged(tagged) if tagged.tag == REPLACE_TAG => strip_replace_tags(tagged.value),
        Value::Tagged(tagged) => {
            Value::Tagged(Box::new(TaggedValue { tag: tagged.tag, value: strip_replace_tags(tagged.value) }))
        }
        Value::Mapping(map) => Value::Mapping(map.into_iter().map(|(k, v)| (k, strip_replace_tags(v))).collect()),
        Value::Sequence(seq) => Value::Sequence(seq.into_iter().map(strip_replace_tags).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn maps_merge_lists_append_scalars_override() {
        let mut base = yaml("{a: 1, opts: [-x, -y], deps: {c: '1'}}");
        merge(&mut base, yaml("{a: 2, opts: [-y, -z], deps: {d: '2'}}"));
        assert_eq!(base, yaml("{a: 2, opts: [-x, -y, -y, -z], deps: {c: '1', d: '2'}}"));
    }

    #[test]
    fn repeated_option_tokens_survive() {
        let mut base = yaml("{opts: [-release, '17']}");
        merge(&mut base, yaml("{opts: [-java-output-version, '17']}"));
        assert_eq!(base, yaml("{opts: [-release, '17', -java-output-version, '17']}"));
    }

    #[test]
    fn replace_tag_replaces() {
        let mut base = yaml("{opts: [-x, -Werror]}");
        merge(&mut base, yaml("{opts: !replace [-x]}"));
        assert_eq!(base, yaml("{opts: [-x]}"));
    }

    #[test]
    fn replace_tag_without_base_is_stripped() {
        let mut base = yaml("{}");
        merge(&mut base, yaml("{opts: !replace [-x]}"));
        assert_eq!(base, yaml("{opts: [-x]}"));
    }

    #[test]
    fn matrix_keys_replace_at_top_level() {
        let mut base = yaml("{scala: ['3.9.0', '2.13.18'], platforms: [jvm, js], opts: [-a]}");
        merge_module(&mut base, yaml("{scala: '3.9.0', platforms: [jvm], opts: [-b]}"));
        assert_eq!(base, yaml("{scala: '3.9.0', platforms: [jvm], opts: [-a, -b]}"));
    }
}
