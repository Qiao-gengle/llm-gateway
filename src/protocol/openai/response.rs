//! OpenAI 非流式响应的解析（上游 → IR）与打印（IR → Client），design.md
//! 「非流式响应转换」一节，需求 2.1、2.3、18.1、18.2、18.3、18.6、18.9、38.4。

use serde_json::{Map, Value, json};

use crate::ir::{IrBlock, IrResponse, IrUsage, StopReason};
use crate::protocol::{ResponseCtx, UpstreamDecodeError};

/// 将上游 OpenAI `chat.completion` 响应体解析为 IR。
///
/// 缺少非空 `choices` 数组时返回 `UpstreamDecodeError`（需求 18 第 9 条）。
/// `message.content` 为长度 ≥ 1 的字符串时产生一个 `Text` 块，否则不产生；
/// `message.tool_calls` 按原顺序转为 `ToolCall` 块，`function.arguments`
/// 缺失/`null`/空串视为 `{}`，非法 JSON 或非对象 JSON 返回错误（需求 18 第 8 条）。
pub fn parse_response(body: &[u8]) -> Result<IrResponse, UpstreamDecodeError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| UpstreamDecodeError::new("response body is not valid JSON"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| UpstreamDecodeError::new("response body must be a JSON object"))?;

    let choice_obj = obj
        .get("choices")
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
        .and_then(|c| c.first())
        .and_then(Value::as_object)
        .ok_or_else(|| UpstreamDecodeError::new("response is missing a non-empty 'choices' array"))?;

    let id = obj.get("id").and_then(Value::as_str).map(str::to_owned);

    let message = choice_obj.get("message").and_then(Value::as_object);
    let mut content = Vec::new();

    if let Some(text) = message.and_then(|m| m.get("content")).and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(IrBlock::Text(text.to_string()));
        }
    }

    if let Some(tool_calls) = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array) {
        for tc in tool_calls {
            let tc_obj = tc.as_object();
            let tc_id = tc_obj
                .and_then(|o| o.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let function = tc_obj.and_then(|o| o.get("function")).and_then(Value::as_object);
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = parse_arguments(function.and_then(|f| f.get("arguments")), &tc_id)?;
            content.push(IrBlock::ToolCall { id: tc_id, name, arguments });
        }
    }

    let finish_reason = choice_obj.get("finish_reason").and_then(Value::as_str);
    let stop = parse_stop_reason(finish_reason);

    let usage_obj = obj.get("usage").and_then(Value::as_object);
    let usage = parse_usage(usage_obj);

    Ok(IrResponse { id, content, stop, usage })
}

/// `function.arguments` → 解析后的 JSON 对象。缺失/`null`/空串为 `{}`；
/// 非法 JSON 或解析结果非对象返回错误，错误消息中带上该工具调用的 id。
fn parse_arguments(arguments: Option<&Value>, tool_call_id: &str) -> Result<Value, UpstreamDecodeError> {
    match arguments {
        None | Some(Value::Null) => Ok(Value::Object(Map::new())),
        Some(Value::String(s)) if s.is_empty() => Ok(Value::Object(Map::new())),
        Some(Value::String(s)) => {
            let parsed: Value = serde_json::from_str(s).map_err(|_| {
                UpstreamDecodeError::new(format!(
                    "tool_call '{tool_call_id}' has invalid function.arguments"
                ))
            })?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Err(UpstreamDecodeError::new(format!(
                    "tool_call '{tool_call_id}' has invalid function.arguments"
                )))
            }
        }
        Some(_) => Err(UpstreamDecodeError::new(format!(
            "tool_call '{tool_call_id}' has invalid function.arguments"
        ))),
    }
}

/// OpenAI 上游 `finish_reason` → IR `StopReason`（design.md 停止原因映射表）。
/// 缺失、`null` 或未知值一律映射为 `EndTurn`（需求 18 第 2 条）。
fn parse_stop_reason(finish_reason: Option<&str>) -> StopReason {
    match finish_reason {
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some("tool_calls") => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    }
}

/// IR `StopReason` → OpenAI `finish_reason`（同一映射表，输出方向）。
fn stop_reason_to_finish_reason(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
    }
}

/// OpenAI `usage` → IR `IrUsage`（需求 18 第 4 条）。缺失或为 `null` 的
/// `prompt_tokens`、`prompt_tokens_details.cached_tokens`、`completion_tokens`
/// 按 0 计算；`usage` 对象存在时标记 `reported = true`。
fn parse_usage(usage: Option<&Map<String, Value>>) -> IrUsage {
    let prompt_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(Value::as_object)
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    IrUsage {
        input_tokens: prompt_tokens.saturating_sub(cached_tokens),
        output_tokens: completion_tokens,
        cache_read_tokens: cached_tokens,
        cache_creation_tokens: 0,
        reported: usage.is_some(),
    }
}

/// 把工具调用参数字符串化。空对象、`null`、缺失或空串一律输出 `"{}"`
/// （需求 18 第 6 条、38 第 4 条），否则按 JSON 序列化。
pub fn stringify_arguments(arguments: &Value) -> String {
    match arguments {
        Value::Null => "{}".to_string(),
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// 把 IR 响应打印为 OpenAI `chat.completion` 对象（design.md「非流式响应转换」）。
///
/// `model` 为 `ctx.public_model`，`id` 为 `ctx.id`（Gateway 生成，形如
/// `chatcmpl-{uuid}`），`created` 为 `ctx.created`。
pub fn print_response(ir: &IrResponse, ctx: &ResponseCtx) -> Value {
    let mut has_text = false;
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in &ir.content {
        match block {
            IrBlock::Text(t) => {
                has_text = true;
                text.push_str(t);
            }
            IrBlock::ToolCall { id, name, arguments } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": stringify_arguments(arguments),
                    },
                }));
            }
            IrBlock::ToolResult { .. } => {}
        }
    }

    let mut message = json!({
        "role": "assistant",
        "content": if has_text { Value::String(text) } else { Value::Null },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let usage = &ir.usage;
    let prompt_tokens = usage.total_input();
    let completion_tokens = usage.output_tokens;

    json!({
        "id": ctx.id,
        "object": "chat.completion",
        "created": ctx.created,
        "model": ctx.public_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": stop_reason_to_finish_reason(ir.stop),
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
            "prompt_tokens_details": { "cached_tokens": usage.cache_read_tokens },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ResponseCtx {
        ResponseCtx {
            id: "chatcmpl-test".to_string(),
            created: 1_700_000_000,
            public_model: "gpt-4o".to_string(),
        }
    }

    #[test]
    fn parse_response_missing_choices_errors() {
        let err = parse_response(br#"{"id":"x"}"#).unwrap_err();
        assert!(err.to_string().contains("choices"));

        let err = parse_response(br#"{"choices":[]}"#).unwrap_err();
        assert!(err.to_string().contains("choices"));

        let err = parse_response(b"not json").unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn parse_response_basic_text() {
        let body = br#"{
            "id": "chatcmpl-abc",
            "choices": [{
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }"#;
        let ir = parse_response(body).unwrap();
        assert_eq!(ir.id, Some("chatcmpl-abc".to_string()));
        assert_eq!(ir.content, vec![IrBlock::Text("hello".to_string())]);
        assert_eq!(ir.stop, StopReason::EndTurn);
        assert_eq!(ir.usage.input_tokens, 10);
        assert_eq!(ir.usage.output_tokens, 5);
        assert!(ir.usage.reported);
    }

    #[test]
    fn parse_response_empty_content_produces_no_text_block() {
        let body = br#"{"choices":[{"message":{"content":""}}]}"#;
        let ir = parse_response(body).unwrap();
        assert!(ir.content.is_empty());
    }

    #[test]
    fn parse_response_tool_call_arguments_normalized() {
        for args in ["null", "\"\"", "missing"] {
            let body = if args == "missing" {
                br#"{"choices":[{"message":{"tool_calls":[{"id":"call_1","function":{"name":"f"}}]}}]}"#.to_vec()
            } else {
                format!(
                    r#"{{"choices":[{{"message":{{"tool_calls":[{{"id":"call_1","function":{{"name":"f","arguments":{args}}}}}]}}}}]}}"#
                )
                .into_bytes()
            };
            let ir = parse_response(&body).unwrap();
            assert_eq!(
                ir.content,
                vec![IrBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "f".to_string(),
                    arguments: Value::Object(Map::new()),
                }]
            );
        }
    }

    #[test]
    fn parse_response_invalid_tool_arguments_errors() {
        for args in [r#""not json""#, "\"[1,2]\"", "\"\\\"a string\\\"\""] {
            let body = format!(
                r#"{{"choices":[{{"message":{{"tool_calls":[{{"id":"call_1","function":{{"name":"f","arguments":{args}}}}}]}}}}]}}"#
            );
            let err = parse_response(body.as_bytes()).unwrap_err();
            assert!(err.to_string().contains("call_1"));
        }
    }

    #[test]
    fn parse_response_finish_reason_mapping() {
        let cases = [
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            ("tool_calls", StopReason::ToolUse),
            ("unknown", StopReason::EndTurn),
        ];
        for (reason, expected) in cases {
            let body = format!(r#"{{"choices":[{{"message":{{}},"finish_reason":"{reason}"}}]}}"#);
            let ir = parse_response(body.as_bytes()).unwrap();
            assert_eq!(ir.stop, expected);
        }
        let body = br#"{"choices":[{"message":{}}]}"#;
        assert_eq!(parse_response(body).unwrap().stop, StopReason::EndTurn);
    }

    #[test]
    fn parse_response_usage_missing_defaults_to_zero_unreported() {
        let body = br#"{"choices":[{"message":{}}]}"#;
        let ir = parse_response(body).unwrap();
        assert_eq!(ir.usage, IrUsage::default());
        assert!(!ir.usage.reported);
    }

    #[test]
    fn parse_response_cached_tokens_subtracted_from_input() {
        let body = br#"{
            "choices":[{"message":{}}],
            "usage":{"prompt_tokens": 100, "completion_tokens": 20,
                      "prompt_tokens_details": {"cached_tokens": 30}}
        }"#;
        let ir = parse_response(body).unwrap();
        assert_eq!(ir.usage.input_tokens, 70);
        assert_eq!(ir.usage.cache_read_tokens, 30);
        assert_eq!(ir.usage.cache_creation_tokens, 0);
        assert_eq!(ir.usage.output_tokens, 20);
    }

    #[test]
    fn stringify_arguments_empty_forms_output_braces() {
        assert_eq!(stringify_arguments(&Value::Null), "{}");
        assert_eq!(stringify_arguments(&Value::Object(Map::new())), "{}");
        assert_eq!(stringify_arguments(&json!({"a": 1})), r#"{"a":1}"#);
    }

    #[test]
    fn print_response_basic_fields() {
        let ir = IrResponse {
            id: Some("upstream-id".to_string()),
            content: vec![IrBlock::Text("hi".to_string())],
            stop: StopReason::EndTurn,
            usage: IrUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 2,
                cache_creation_tokens: 0,
                reported: true,
            },
        };
        let out = print_response(&ir, &ctx());
        assert_eq!(out["id"], "chatcmpl-test");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["created"], 1_700_000_000);
        assert_eq!(out["model"], "gpt-4o");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert!(out["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(out["usage"]["prompt_tokens"], 12);
        assert_eq!(out["usage"]["completion_tokens"], 5);
        assert_eq!(out["usage"]["total_tokens"], 17);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    }

    #[test]
    fn print_response_no_text_content_is_null() {
        let ir = IrResponse {
            id: None,
            content: vec![IrBlock::ToolCall {
                id: "call_1".to_string(),
                name: "f".to_string(),
                arguments: Value::Object(Map::new()),
            }],
            stop: StopReason::ToolUse,
            usage: IrUsage::default(),
        };
        let out = print_response(&ir, &ctx());
        assert_eq!(out["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        let tool_calls = out["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "f");
        assert_eq!(tool_calls[0]["function"]["arguments"], "{}");
    }

    #[test]
    fn print_response_stop_reason_mapping() {
        let base = IrResponse {
            id: None,
            content: vec![],
            stop: StopReason::EndTurn,
            usage: IrUsage::default(),
        };
        let cases = [
            (StopReason::EndTurn, "stop"),
            (StopReason::StopSequence, "stop"),
            (StopReason::MaxTokens, "length"),
            (StopReason::ToolUse, "tool_calls"),
        ];
        for (stop, expected) in cases {
            let ir = IrResponse { stop, ..base.clone() };
            let out = print_response(&ir, &ctx());
            assert_eq!(out["choices"][0]["finish_reason"], expected);
        }
    }
}
