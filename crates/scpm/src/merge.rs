//! Deep merge of YAML values, used for presets (`extends:`) and variant overrides
//! (design §4.4, "Resolution order").
//!
//! Rules: maps merge key by key, scalars override, lists append (no deduplication: option
//! lists such as `[-release, '17']` must keep repeated tokens). A value tagged `!replace`
//! replaces the inherited value. At the top level of a module, the matrix-defining keys
//! `scala` and `platforms` always replace.

use serde_yaml_ng::Value;
use serde_yaml_ng::value::{Tag, TaggedValue};

const REPLACE_TAG: &str = "replace";

/// Top-level module keys whose values replace rather than merge.
const MATRIX_KEYS: [&str; 2] = ["scala", "platforms"];

/// Merges a module-shaped `overlay` on top of `base` in place.
pub fn merge_module(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                // Only a plain null inherits; `!replace null` clears (`is_null` looks through tags).
                if matches!(value, Value::Null) {
                    continue;
                }
                let is_matrix = key.as_str().is_some_and(|k| MATRIX_KEYS.contains(&k));
                match base_map.get_mut(&key) {
                    Some(existing) if !is_matrix => merge(existing, value),
                    _ => {
                        base_map.insert(key, strip_replace_tags(value));
                    }
                }
            }
        }
        (base, overlay) => merge(base, overlay),
    }
}

/// Merges `overlay` on top of `base` in place. A null overlay (an empty `key:`) changes nothing.
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (_, Value::Null) => {}
        (base, Value::Tagged(tagged)) if tagged.tag == REPLACE_TAG => {
            *base = strip_replace_tags(tagged.value);
        }
        (Value::Mapping(base_map), Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None if value.is_null() => {}
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

/// Removes a top-level `!replace` tag; the flag says whether there was one.
pub fn untag_replace(value: Value) -> (Value, bool) {
    match value {
        Value::Tagged(tagged) if tagged.tag == REPLACE_TAG => (tagged.value, true),
        other => (other, false),
    }
}

/// Whether `value` contains a `!replace` tag anywhere.
pub fn has_replace_tags(value: &Value) -> bool {
    match value {
        Value::Tagged(tagged) => tagged.tag == REPLACE_TAG || has_replace_tags(&tagged.value),
        Value::Mapping(map) => map.values().any(has_replace_tags),
        Value::Sequence(seq) => seq.iter().any(has_replace_tags),
        _ => false,
    }
}

/// The local tag prefix [`parse_with_visible_tags`] maps the `!!` handle to.
const CORE_TAG_PREFIX: &str = "!scpm-core:";

/// Whether `line` is a `%TAG` directive for the `!!` handle.
fn is_secondary_tag_directive(line: &str) -> bool {
    let mut words = line.split_whitespace();
    line.starts_with('%') && words.next() == Some("%TAG") && words.next() == Some("!!")
}

/// Parses `text` so that every tag stays visible. serde_yaml_ng drops core-schema tags
/// (`!!replace`, `!!str`, …, which libyaml resolves to `tag:yaml.org,2002:…`), so `!!replace`
/// would otherwise read as an untagged value and silently append. A `%TAG` directive maps
/// `!!` to a local prefix instead; a `%TAG !!` directive the file declares itself is replaced
/// by it. Returns `None` if the rewritten text doesn't parse; callers then fall back to the
/// plain parse.
pub fn parse_with_visible_tags(text: &str) -> Option<Value> {
    // Directives start at column 0, where plain scalars can't start with `%`.
    if text.lines().any(is_secondary_tag_directive) {
        let directive = format!("%TAG !! {CORE_TAG_PREFIX}");
        let rewritten: Vec<&str> =
            text.lines().map(|l| if is_secondary_tag_directive(l) { directive.as_str() } else { l }).collect();
        return serde_yaml_ng::from_str(&rewritten.join("\n")).ok();
    }
    let first = text.lines().map(str::trim_start).find(|l| !l.is_empty() && !l.starts_with('#'));
    let starts_document =
        first.is_some_and(|l| l.starts_with('%') || l == "---" || l.starts_with("--- ") || l.starts_with("---\t"));
    let directive = format!("%TAG !! {CORE_TAG_PREFIX}\n");
    let header = if starts_document { directive } else { format!("{directive}---\n") };
    serde_yaml_ng::from_str(&format!("{header}{text}")).ok()
}

/// A tag as the user wrote it.
fn written_tag(tag: &Tag) -> String {
    let shown = tag.to_string();
    match shown.strip_prefix(CORE_TAG_PREFIX) {
        Some(suffix) => format!("!!{suffix}"),
        // serde_yaml_ng displays the non-specific tag `!` as `!!`.
        None if shown == "!!" => "!".to_string(),
        None => shown,
    }
}

/// Calls `found(key_path, tag)` for every tag on a value other than exactly `!replace`, with the
/// tag as written (`!!replace` included, when `value` comes from [`parse_with_visible_tags`]).
/// Tags on mapping keys are always reported: `!replace` has no meaning there.
pub fn unknown_tags(value: &Value, path: &str, found: &mut dyn FnMut(String, String)) {
    match value {
        Value::Tagged(tagged) => {
            let written = written_tag(&tagged.tag);
            if written != "!replace" {
                found(path.to_string(), written);
            }
            unknown_tags(&tagged.value, path, found);
        }
        Value::Mapping(map) => {
            for (k, v) in map {
                let untagged = match k {
                    Value::Tagged(tagged) => &tagged.value,
                    other => other,
                };
                let key = match untagged {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => "?".to_string(),
                };
                let child = if path.is_empty() { key } else { format!("{path}.{key}") };
                if let Value::Tagged(tagged) = k {
                    found(child.clone(), written_tag(&tagged.tag));
                }
                unknown_tags(v, &child, found);
            }
        }
        Value::Sequence(seq) => {
            for (i, v) in seq.iter().enumerate() {
                unknown_tags(v, &format!("{path}[{i}]"), found);
            }
        }
        _ => {}
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

    #[test]
    fn replace_null_clears() {
        let mut base = yaml("{release: '17', opts: [-a]}");
        merge_module(&mut base, yaml("{release: !replace null, opts: !replace ~}"));
        assert_eq!(base, yaml("{release: null, opts: null}"));
    }

    #[test]
    fn null_overlays_change_nothing() {
        let mut base = yaml("{deps: {a: '1'}, opts: [-a]}");
        merge_module(&mut base, yaml("{deps: ~, opts: ~, extra: ~}"));
        assert_eq!(base, yaml("{deps: {a: '1'}, opts: [-a]}"));
        merge(&mut base, yaml("{deps: {b: ~}}"));
        assert_eq!(base, yaml("{deps: {a: '1'}, opts: [-a]}"));
    }

    #[test]
    fn finds_unknown_tags() {
        let mut found = Vec::new();
        unknown_tags(&yaml("{a: !replace [x], b: {c: [y, !foo z]}}"), "", &mut |k, t| found.push((k, t)));
        assert_eq!(found, vec![("b.c[1]".to_string(), "!foo".to_string())]);
        assert!(has_replace_tags(&yaml("{a: {b: !replace x}}")));
        let mut found = Vec::new();
        unknown_tags(&yaml("{env: {!typo FOO: x, !replace BAR: y}}"), "", &mut |k, t| found.push((k, t)));
        assert_eq!(
            found,
            vec![("env.FOO".to_string(), "!typo".to_string()), ("env.BAR".to_string(), "!replace".to_string())]
        );
    }

    fn visible_tags(text: &str) -> Vec<(String, String)> {
        let mut found = Vec::new();
        unknown_tags(&parse_with_visible_tags(text).unwrap(), "", &mut |k, t| found.push((k, t)));
        found
    }

    #[test]
    fn core_schema_tags_are_visible() {
        // The plain parse drops `!!replace` entirely.
        assert_eq!(yaml("a: !!replace [x]"), yaml("a: [x]"));
        let replace = vec![("a".to_string(), "!!replace".to_string())];
        for text in [
            "a: !!replace [x]",
            "---\na: !!replace [x]",
            "# c\n%YAML 1.2\n---\na: !!replace [x]",
            "%TAG !! tag:yaml.org,2002:\n---\na: !!replace [x]",
        ] {
            assert_eq!(visible_tags(text), replace, "{text}");
        }
        assert_eq!(
            visible_tags("a: !replace [x]\nb: !!str x\nc: ! y\n"),
            vec![("b".to_string(), "!!str".to_string()), ("c".to_string(), "!".to_string())]
        );
    }
}
