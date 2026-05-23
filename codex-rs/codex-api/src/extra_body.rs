//! Unified `extra_body` merge semantics shared by every wire API (Responses, Chat
//! Completions, Anthropic), so the same catalog `extra_body` behaves identically on
//! every wire:
//!
//! - Objects deep-merge recursively, at every nesting level.
//! - `null` removes the key when the body already carries it, and is a no-op when the
//!   key is absent, at every nesting level — including inside objects `extra_body`
//!   newly inserts. A merged body never gains a JSON `null` object value from
//!   `extra_body`, because several providers reject explicit nulls with a 400 (nulls
//!   inside arrays are literal data and are kept). This is how a model opts out
//!   of a default this client projects — a non-reasoning model dropping the projected
//!   `reasoning_effort`, for example — without a code change. Only a JSON
//!   configuration can express the opt-out: a model catalog, or a remote thread
//!   config's `extra_body_json`. `config.toml` cannot, because TOML has no null.
//! - Arrays replace the existing array wholesale rather than appending, so the merged
//!   value is exactly what the catalog declares.
//! - Scalars replace.
//!
//! Callers merge the provider-level layer first and the model-level layer second, so
//! a model can narrow, override, or remove what its provider declares. Wire-specific
//! guards stay with their wire: chat object-merges `stream_options` and forces
//! `include_usage` back on, and Anthropic rejects any `stream` other than `true`.

use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;

/// Merges one `extra_body` layer over a request body with the shared semantics.
/// A non-object body is left untouched.
pub(crate) fn merge_extra_body(body: &mut Value, extra_body: &HashMap<String, Value>) {
    let Some(body) = body.as_object_mut() else {
        return;
    };
    for (key, value) in extra_body {
        merge_extra_body_entry(body, key, value);
    }
}

/// Merges a single `extra_body` entry, so a wire can special-case individual keys
/// (chat's `stream_options`) while sharing the semantics for everything else.
pub(crate) fn merge_extra_body_entry(body: &mut Map<String, Value>, key: &str, value: &Value) {
    if value.is_null() {
        body.remove(key);
        return;
    }
    match (body.get_mut(key), value) {
        (Some(Value::Object(existing)), Value::Object(extra)) => {
            for (key, value) in extra {
                merge_extra_body_entry(existing, key, value);
            }
        }
        _ => {
            body.insert(key.to_string(), merged_new_value(value));
        }
    }
}

/// Prepares a value that replaces rather than deep-merges, applying the same null
/// removal inside objects so no merge path can introduce a JSON null object value.
fn merged_new_value(value: &Value) -> Value {
    let Value::Object(extra) = value else {
        return value.clone();
    };
    let mut merged = Map::new();
    for (key, value) in extra {
        merge_extra_body_entry(&mut merged, key, value);
    }
    Value::Object(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn layer(value: Value) -> HashMap<String, Value> {
        serde_json::from_value(value).expect("extra_body layers are JSON objects")
    }

    #[test]
    fn deep_merges_objects_recursively() {
        let mut body = json!({
            "reasoning": {"summary": "auto", "nested": {"keep": 1, "drop": 2}},
        });
        merge_extra_body(
            &mut body,
            &layer(json!({
                "reasoning": {"effort": "high", "nested": {"drop": null, "add": 3}},
            })),
        );
        assert_eq!(
            body,
            json!({
                "reasoning": {"summary": "auto", "effort": "high", "nested": {"keep": 1, "add": 3}},
            })
        );
    }

    #[test]
    fn null_removes_existing_keys_and_never_adds_nulls() {
        let mut body = json!({
            "reasoning_effort": "high",
            "reasoning": {"summary": "auto"},
        });
        merge_extra_body(
            &mut body,
            &layer(json!({
                "reasoning_effort": null,
                "reasoning": {"summary": null},
                "absent": null,
                "nested_absent": {"gone": null},
            })),
        );
        assert_eq!(body, json!({"reasoning": {}, "nested_absent": {}}));
    }

    #[test]
    fn replacing_a_scalar_with_an_object_still_strips_nulls() {
        let mut body = json!({"options": 1});
        merge_extra_body(
            &mut body,
            &layer(json!({"options": {"keep": true, "drop": null}})),
        );
        assert_eq!(body, json!({"options": {"keep": true}}));
    }

    #[test]
    fn arrays_are_literal_values_including_nulls() {
        let mut body = json!({"stop": ["a"]});
        merge_extra_body(&mut body, &layer(json!({"stop": [null, "b"]})));
        assert_eq!(body, json!({"stop": [null, "b"]}));
    }

    #[test]
    fn arrays_replace_instead_of_appending() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "shell"}],
            "options": {"stop": ["a"]},
        });
        merge_extra_body(
            &mut body,
            &layer(json!({
                "tools": [{"type": "web_search"}],
                "options": {"stop": ["b", "c"]},
            })),
        );
        assert_eq!(
            body,
            json!({
                "tools": [{"type": "web_search"}],
                "options": {"stop": ["b", "c"]},
            })
        );
    }

    #[test]
    fn scalars_replace() {
        let mut body = json!({"tool_choice": "auto", "temperature": 0.7});
        merge_extra_body(
            &mut body,
            &layer(json!({"tool_choice": "required", "temperature": 0.2})),
        );
        assert_eq!(body, json!({"tool_choice": "required", "temperature": 0.2}));
    }

    #[test]
    fn non_object_body_is_left_untouched() {
        let mut body = json!("not an object");
        merge_extra_body(&mut body, &layer(json!({"key": "value"})));
        assert_eq!(body, json!("not an object"));
    }

    #[test]
    fn a_later_layer_overrides_and_removes_an_earlier_one() {
        let mut body = json!({"model": "m", "reasoning_effort": "high"});
        merge_extra_body(
            &mut body,
            &layer(json!({"thinking_budget": 1024, "temperature": 0.7})),
        );
        merge_extra_body(
            &mut body,
            &layer(json!({"thinking_budget": null, "temperature": 0.2})),
        );
        assert_eq!(
            body,
            json!({"model": "m", "reasoning_effort": "high", "temperature": 0.2})
        );
    }

    /// The same base body and the same `extra_body` must merge identically on all
    /// three wires. The fixture avoids the documented wire-specific guards (chat's
    /// `stream_options` and thinking-budget reconciliation, Anthropic's `stream`
    /// check), which the per-wire tests pin separately. The Responses wire routes
    /// through the shared helper directly.
    #[test]
    fn identical_extra_body_merges_identically_on_every_wire() {
        let base = json!({
            "model": "catalog-model",
            "stream": true,
            "temperature": 0.7,
            "tools": [{"type": "function", "name": "shell"}],
            "reasoning": {"summary": "auto", "effort": "high"},
            "metadata": {"user_id": "u", "session": {"id": "s1", "trace": true}},
            "logprobs": false,
        });
        let extra = layer(json!({
            "temperature": 0.2,
            "tools": [{"type": "web_search"}],
            "reasoning": {"effort": "max", "summary": null},
            "metadata": {"session": {"trace": null, "tag": "t"}, "new": 1},
            "logprobs": null,
            "absent_key": null,
            "enable_thinking": true,
        }));
        let expected = json!({
            "model": "catalog-model",
            "stream": true,
            "temperature": 0.2,
            "tools": [{"type": "web_search"}],
            "reasoning": {"effort": "max"},
            "metadata": {"user_id": "u", "session": {"id": "s1", "tag": "t"}, "new": 1},
            "enable_thinking": true,
        });

        let mut responses_body = base.clone();
        merge_extra_body(&mut responses_body, &extra);
        assert_eq!(responses_body, expected, "responses wire");

        let mut chat_body = base.clone();
        crate::endpoint::chat_body::merge_extra_body(&mut chat_body, &extra)
            .expect("chat merge should succeed");
        assert_eq!(chat_body, expected, "chat wire");

        let mut anthropic_body = base;
        crate::endpoint::anthropic::merge_extra_body(&mut anthropic_body, &extra)
            .expect("anthropic merge should succeed");
        assert_eq!(anthropic_body, expected, "anthropic wire");
    }

    /// A model-level `null` removes a provider-level declaration on every wire, and
    /// the chat path runs through `finalize_chat_body` so the reconciliation sees the
    /// removal too.
    #[test]
    fn model_layer_null_removes_a_provider_declaration_on_every_wire() {
        let base = json!({"model": "catalog-model", "stream": true});
        let provider = layer(json!({"thinking_budget": 1024, "metadata": {"user_id": "u"}}));
        let model = layer(json!({"thinking_budget": null}));
        let expected = json!({
            "model": "catalog-model",
            "stream": true,
            "metadata": {"user_id": "u"},
        });

        let mut responses_body = base.clone();
        merge_extra_body(&mut responses_body, &provider);
        merge_extra_body(&mut responses_body, &model);
        assert_eq!(responses_body, expected, "responses wire");

        let mut chat_body = base.clone();
        crate::endpoint::chat_body::finalize_chat_body(&mut chat_body, &provider, &model)
            .expect("chat finalize should succeed");
        assert_eq!(chat_body, expected, "chat wire");

        let mut anthropic_body = base;
        crate::endpoint::anthropic::merge_extra_body(&mut anthropic_body, &provider)
            .expect("anthropic provider merge should succeed");
        crate::endpoint::anthropic::merge_extra_body(&mut anthropic_body, &model)
            .expect("anthropic model merge should succeed");
        assert_eq!(anthropic_body, expected, "anthropic wire");
    }
}
