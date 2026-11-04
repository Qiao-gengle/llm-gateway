//! Property 14: OpenAI 请求往返
//!
//! **Validates: Requirements 15.5**
//!
//! 对于通过 Gateway 请求校验且不含多模态内容的 OpenAI 请求 r：
//! `openai::parse_request(openai::print_request(openai::parse_request(r), target).body) ≡ openai::parse_request(r)`，
//! 其中 `target.upstream_model == ir.model`。等价使用 `IrRequest` 的 `PartialEq`
//! （design.md「IR 等价比较」：数值按大小比较，对象字段顺序不参与比较）。

use llm_gateway::protocol::openai::request::{parse_request, print_request};
use llm_gateway::registry::RouteTarget;
use proptest::prelude::*;
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn cases() -> u32 {
    let env = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    env.max(256)
}

/// 简单的有界递归 JSON 生成器，仅使用 JSON 数字（整数）、布尔、字符串、`null`、
/// 数组与对象，供工具 Schema 与 extra 字段的值使用。
fn arb_json(depth: u32) -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-1000i64..1000i64).prop_map(|n| json!(n)),
        "[a-zA-Z0-9 ]{0,8}".prop_map(Value::String),
    ];
    if depth == 0 {
        leaf.boxed()
    } else {
        let next = depth - 1;
        prop_oneof![
            3 => leaf,
            1 => prop::collection::vec(arb_json(next), 0..3).prop_map(Value::Array),
            1 => prop::collection::vec(("[a-z]{1,6}", arb_json(next)), 0..3).prop_map(|entries| {
                let mut m = Map::new();
                for (k, v) in entries {
                    m.insert(k, v);
                }
                Value::Object(m)
            }),
        ]
        .boxed()
    }
}

fn arb_json_object(depth: u32) -> impl Strategy<Value = Value> {
    prop::collection::vec(("[a-z]{1,6}", arb_json(depth)), 0..3).prop_map(|entries| {
        let mut m = Map::new();
        for (k, v) in entries {
            m.insert(k, v);
        }
        Value::Object(m)
    })
}

fn arb_ident() -> impl Strategy<Value = String> {
    "[a-zA-Z_][a-zA-Z0-9_]{0,8}"
}

fn arb_nonempty_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .,!?]{1,20}"
}

fn arb_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .,!?]{0,20}"
}

/// user 消息 `content`：保证至少包含一个文本块，避免 `print_request`
/// 把没有任何文本块的 user 消息整条丢弃（这会使往返性质必然不成立）。
fn arb_user_content() -> impl Strategy<Value = Value> {
    prop_oneof![
        arb_nonempty_text().prop_map(Value::String),
        prop::collection::vec(arb_text(), 1..=3)
            .prop_map(|parts| Value::Array(parts.into_iter().map(|t| json!({"type": "text", "text": t})).collect())),
    ]
}

/// assistant 消息文本内容：只用单个字符串，避免 `print_request` 把多个独立文本块
/// 合并为一个字符串时丢失块边界（`extra_content[]` 的数组形式在该路径上不是逐块保留的）。
fn arb_assistant_text() -> impl Strategy<Value = Value> {
    prop_oneof![Just(Value::Null), arb_text().prop_map(Value::String)]
}

fn arb_tool_result_content() -> impl Strategy<Value = Value> {
    prop_oneof![
        arb_text().prop_map(Value::String),
        prop::collection::vec(arb_text(), 0..=3)
            .prop_map(|parts| Value::Array(parts.into_iter().map(|t| json!({"type": "text", "text": t})).collect())),
    ]
}

fn arb_tool_call() -> impl Strategy<Value = Value> {
    (arb_ident(), arb_ident(), arb_json_object(1)).prop_map(|(id, name, args)| {
        json!({
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": serde_json::to_string(&args).unwrap() },
        })
    })
}

#[derive(Debug, Clone)]
enum MsgSpec {
    System(String),
    Developer(String),
    Tool { tool_call_id: String, content: Value },
    User { content: Value },
    Assistant { content: Value, tool_calls: Vec<Value> },
}

fn arb_message() -> impl Strategy<Value = MsgSpec> {
    prop_oneof![
        2 => arb_text().prop_map(MsgSpec::System),
        1 => arb_text().prop_map(MsgSpec::Developer),
        2 => (arb_ident(), arb_tool_result_content())
            .prop_map(|(id, content)| MsgSpec::Tool { tool_call_id: id, content }),
        4 => arb_user_content().prop_map(|content| MsgSpec::User { content }),
        4 => (arb_assistant_text(), prop::collection::vec(arb_tool_call(), 0..=2))
            .prop_map(|(content, tool_calls)| MsgSpec::Assistant { content, tool_calls }),
    ]
}

fn msg_spec_to_json(spec: &MsgSpec) -> Value {
    match spec {
        MsgSpec::System(text) => json!({ "role": "system", "content": text }),
        MsgSpec::Developer(text) => json!({ "role": "developer", "content": text }),
        MsgSpec::Tool { tool_call_id, content } => {
            json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content })
        }
        MsgSpec::User { content } => json!({ "role": "user", "content": content }),
        MsgSpec::Assistant { content, tool_calls } => {
            let mut obj = Map::new();
            obj.insert("role".to_string(), Value::String("assistant".to_string()));
            obj.insert("content".to_string(), content.clone());
            if !tool_calls.is_empty() {
                obj.insert("tool_calls".to_string(), Value::Array(tool_calls.clone()));
            }
            Value::Object(obj)
        }
    }
}

fn arb_tool_choice() -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        1 => Just(None),
        1 => Just(Some(Value::String("auto".to_string()))),
        1 => Just(Some(Value::String("required".to_string()))),
        1 => Just(Some(Value::String("none".to_string()))),
        1 => arb_ident().prop_map(|name| {
            Some(json!({ "type": "function", "function": { "name": name } }))
        }),
    ]
}

fn arb_stop() -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        1 => Just(None),
        2 => arb_nonempty_text().prop_map(|s| Some(Value::String(s))),
        2 => prop::collection::vec(arb_text(), 1..=3)
            .prop_map(|v| Some(Value::Array(v.into_iter().map(Value::String).collect()))),
    ]
}

fn arb_tool_def() -> impl Strategy<Value = Value> {
    (arb_ident(), prop::option::of(arb_text()), arb_json_object(2)).prop_map(
        |(name, description, schema)| {
            let mut function = Map::new();
            function.insert("name".to_string(), Value::String(name));
            if let Some(d) = description {
                function.insert("description".to_string(), Value::String(d));
            }
            function.insert("parameters".to_string(), schema);
            json!({ "type": "function", "function": Value::Object(function) })
        },
    )
}

/// `extra` 顶层字段：使用不与任何已识别字段冲突的 `x_` 前缀键名。
fn arb_extra() -> impl Strategy<Value = Vec<(String, Value)>> {
    prop::collection::vec(
        ("[a-z]{1,6}".prop_map(|s| format!("x_{s}")), arb_json(2)),
        0..3,
    )
    .prop_map(|mut entries| {
        // 去重键名，避免重复键在 Map 构造中互相覆盖导致长度不一致。
        let mut seen = std::collections::HashSet::new();
        entries.retain(|(k, _)| seen.insert(k.clone()));
        entries
    })
}

#[allow(clippy::type_complexity)]
fn arb_openai_request() -> impl Strategy<Value = Value> {
    (
        arb_ident(),
        any::<bool>(),
        prop::option::of(-5.0f64..5.0f64),
        prop::option::of(-5.0f64..5.0f64),
        prop::option::of(1u64..100_000u64),
        prop::option::of(1u64..100_000u64),
        arb_stop(),
        prop::collection::vec(arb_tool_def(), 0..=3),
        arb_tool_choice(),
        prop::option::of(any::<bool>()),
        prop::collection::vec(arb_message(), 1..=6),
        arb_extra(),
    )
        .prop_map(
            |(
                model,
                stream,
                temperature,
                top_p,
                max_tokens,
                max_completion_tokens,
                stop,
                tools,
                tool_choice,
                parallel_tool_calls,
                messages,
                extra,
            )| {
                let mut obj = Map::new();
                obj.insert("model".to_string(), Value::String(model));
                obj.insert(
                    "messages".to_string(),
                    Value::Array(messages.iter().map(msg_spec_to_json).collect()),
                );
                obj.insert("stream".to_string(), Value::Bool(stream));
                // `stream_options.include_usage` 完全由 Gateway 在 print 阶段根据
                // `stream` 推导（design.md 需求 15 第 4 条），因此这里保持与 `stream`
                // 一致，否则输入值在往返后必然被覆盖，不满足往返性质。
                if stream {
                    obj.insert("stream_options".to_string(), json!({ "include_usage": true }));
                }
                if let Some(t) = temperature {
                    obj.insert("temperature".to_string(), json!(t));
                }
                if let Some(t) = top_p {
                    obj.insert("top_p".to_string(), json!(t));
                }
                if let Some(v) = max_completion_tokens {
                    obj.insert("max_completion_tokens".to_string(), json!(v));
                } else if let Some(v) = max_tokens {
                    obj.insert("max_tokens".to_string(), json!(v));
                }
                if let Some(v) = stop {
                    obj.insert("stop".to_string(), v);
                }
                if !tools.is_empty() {
                    obj.insert("tools".to_string(), Value::Array(tools));
                }
                if let Some(v) = tool_choice {
                    obj.insert("tool_choice".to_string(), v);
                }
                if let Some(v) = parallel_tool_calls {
                    obj.insert("parallel_tool_calls".to_string(), Value::Bool(v));
                }
                for (k, v) in extra {
                    obj.insert(k, v);
                }
                Value::Object(obj)
            },
        )
}

fn make_target(upstream_model: &str) -> RouteTarget {
    RouteTarget {
        id: Uuid::new_v4(),
        provider_id: Uuid::new_v4(),
        upstream_model: upstream_model.to_string(),
        priority: 0,
        weight: 1,
        default_max_tokens: None,
        enabled: true,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    // Feature: llm-gateway, Property 14: OpenAI 请求往返
    /// **Validates: Requirements 15.5**
    #[test]
    fn p14_openai_request_round_trip(request in arb_openai_request()) {
        let body = serde_json::to_vec(&request).unwrap();
        let ir1 = parse_request(&body).expect("generator must only produce requests that pass validation");

        let target = make_target(&ir1.model);
        let printed = print_request(&ir1, &target).expect("OpenAI->OpenAI print_request is infallible");

        let printed_body = serde_json::to_vec(&printed.body).unwrap();
        let ir2 = parse_request(&printed_body).expect("printed body must re-parse");

        prop_assert_eq!(ir1, ir2);
    }
}
