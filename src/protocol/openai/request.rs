//! OpenAI `/v1/chat/completions` 请求解析：JSON → IR（`openai::parse_request`）。
//!
//! 对应 design.md「Converter 与 IR 编解码」一节，实现需求 2 第 4、5 条、
//! 需求 15 第 1、8 条、需求 16 第 1、6、15 条的校验与转换。

use serde_json::{Map, Value, json};

use crate::http::errors::{ValidationError, ValidationReason};
use crate::ir::{
    ClientProtocol, Extra, IrBlock, IrMessage, IrRole, IrRequest, IrTool, IrToolChoice, MaxTokens,
    MaxTokensField, Sampling, ToolResultContent,
};
use crate::protocol::{ConvertError, PrintOutcome};
use crate::registry::RouteTarget;

/// 请求顶层已识别字段集合（需求 15 第 1 条：未出现在该集合中的字段进入 `extra`）。
const RECOGNIZED_TOP_LEVEL_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "temperature",
    "top_p",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "stream_options",
];

fn err(field: impl Into<String>, reason: ValidationReason) -> ValidationError {
    ValidationError { field: field.into(), reason }
}

fn missing(field: impl Into<String>) -> ValidationError {
    err(field, ValidationReason::Missing)
}

fn invalid_type(field: impl Into<String>) -> ValidationError {
    err(field, ValidationReason::InvalidType)
}

/// 将 OpenAI `/v1/chat/completions` 请求体解析为 IR。
///
/// 校验顺序遵循需求 2 第 4、5 条：先检查合法 JSON / 顶层对象 / `model` /
/// `messages` 是否存在（否则 400，错误信息指明字段名），再检查各字段的类型与取值。
pub fn parse_request(body: &[u8]) -> Result<IrRequest, ValidationError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| err("body", ValidationReason::InvalidJson))?;
    let obj = value
        .as_object()
        .ok_or_else(|| err("body", ValidationReason::NotObject))?;

    let model = parse_model(obj)?;
    let messages_raw = obj
        .get("messages")
        .ok_or_else(|| missing("messages"))?
        .as_array()
        .ok_or_else(|| invalid_type("messages"))?;
    if messages_raw.is_empty() {
        return Err(err("messages", ValidationReason::Empty));
    }

    let stream = match obj.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid_type("stream")),
    };

    let (system, messages) = parse_messages(messages_raw)?;
    let sampling = Sampling {
        temperature: parse_number_field(obj, "temperature")?,
        top_p: parse_number_field(obj, "top_p")?,
        max_tokens: parse_max_tokens(obj)?,
        stop: parse_stop(obj.get("stop"))?,
    };
    let tools = parse_tools(obj.get("tools"))?;
    let tool_choice = match obj.get("tool_choice") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_tool_choice(v)?),
    };
    let parallel_tool_calls = match obj.get("parallel_tool_calls") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(invalid_type("parallel_tool_calls")),
    };
    let include_usage = obj
        .get("stream_options")
        .and_then(Value::as_object)
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut extra_fields = Map::new();
    for (k, v) in obj.iter() {
        if !RECOGNIZED_TOP_LEVEL_FIELDS.contains(&k.as_str()) {
            extra_fields.insert(k.clone(), v.clone());
        }
    }

    Ok(IrRequest {
        model,
        system,
        messages,
        sampling,
        tools,
        tool_choice,
        parallel_tool_calls,
        stream,
        include_usage,
        extra: Extra { origin: ClientProtocol::OpenAi, fields: extra_fields },
    })
}

fn parse_model(obj: &Map<String, Value>) -> Result<String, ValidationError> {
    let v = obj.get("model").ok_or_else(|| missing("model"))?;
    match v.as_str() {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(invalid_type("model")),
    }
}

fn parse_number_field(
    obj: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<serde_json::Number>, ValidationError> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.clone())),
        Some(_) => Err(invalid_type(field)),
    }
}

fn parse_max_tokens(obj: &Map<String, Value>) -> Result<Option<MaxTokens>, ValidationError> {
    if let Some(v) = obj.get("max_completion_tokens") {
        if !v.is_null() {
            let value = v.as_u64().ok_or_else(|| invalid_type("max_completion_tokens"))?;
            return Ok(Some(MaxTokens { value, source: MaxTokensField::MaxCompletionTokens }));
        }
    }
    if let Some(v) = obj.get("max_tokens") {
        if !v.is_null() {
            let value = v.as_u64().ok_or_else(|| invalid_type("max_tokens"))?;
            return Ok(Some(MaxTokens { value, source: MaxTokensField::MaxTokens }));
        }
    }
    Ok(None)
}

fn parse_stop(v: Option<&Value>) -> Result<Option<Vec<String>>, ValidationError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                return Ok(None);
            }
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(invalid_type("stop")),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(invalid_type("stop")),
    }
}

fn parse_tools(v: Option<&Value>) -> Result<Vec<IrTool>, ValidationError> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(arr)) => arr.iter().map(parse_tool).collect(),
        Some(_) => Err(invalid_type("tools")),
    }
}

fn parse_tool(v: &Value) -> Result<IrTool, ValidationError> {
    let obj = v.as_object().ok_or_else(|| invalid_type("tools[]"))?;
    let function = obj
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("tools[].function"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid_type("tools[].function.name"))?
        .to_string();
    let description = function.get("description").and_then(Value::as_str).map(String::from);
    let schema = function.get("parameters").cloned().unwrap_or(Value::Object(Map::new()));
    Ok(IrTool { name, description, schema })
}

fn parse_tool_choice(v: &Value) -> Result<IrToolChoice, ValidationError> {
    match v {
        Value::String(s) if s == "auto" => Ok(IrToolChoice::Auto),
        Value::String(s) if s == "required" => Ok(IrToolChoice::Required),
        Value::String(s) if s == "none" => Ok(IrToolChoice::None),
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => {
            let name = o
                .get("function")
                .and_then(Value::as_object)
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            match name {
                Some(n) => Ok(IrToolChoice::Tool { name: n.to_string() }),
                None => Err(err("tool_choice", ValidationReason::Unsupported("invalid tool_choice"))),
            }
        }
        _ => Err(err("tool_choice", ValidationReason::Unsupported("invalid tool_choice"))),
    }
}

/// 从 `messages` 数组中提取 system/developer 文本（以 `"\n\n"` 拼接）与其余消息。
fn parse_messages(raw: &[Value]) -> Result<(Option<String>, Vec<IrMessage>), ValidationError> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<IrMessage> = Vec::new();

    for (idx, m) in raw.iter().enumerate() {
        let mobj = m
            .as_object()
            .ok_or_else(|| invalid_type(format!("messages[{idx}]")))?;
        let role = mobj
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_type(format!("messages[{idx}].role")))?;

        match role {
            "system" | "developer" => {
                let text = extract_concat_text(mobj.get("content"), idx)?;
                system_parts.push(text);
            }
            "tool" => {
                let tool_call_id = mobj
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| missing(format!("messages[{idx}].tool_call_id")))?
                    .to_string();
                let content = extract_tool_result_content(mobj.get("content"), idx)?;
                messages.push(IrMessage {
                    role: IrRole::Tool,
                    content: vec![IrBlock::ToolResult { tool_call_id, content }],
                });
            }
            "user" => {
                let blocks = extract_content_blocks(mobj.get("content"), idx)?;
                messages.push(IrMessage { role: IrRole::User, content: blocks });
            }
            "assistant" => {
                let mut blocks = extract_content_blocks(mobj.get("content"), idx)?;
                if let Some(tool_calls) = mobj.get("tool_calls") {
                    let arr = tool_calls
                        .as_array()
                        .ok_or_else(|| invalid_type(format!("messages[{idx}].tool_calls")))?;
                    for tc in arr {
                        blocks.push(parse_tool_call(tc, idx)?);
                    }
                }
                messages.push(IrMessage { role: IrRole::Assistant, content: blocks });
            }
            _other => {
                return Err(err(
                    format!("messages[{idx}].role"),
                    ValidationReason::Unsupported("unknown role"),
                ));
            }
        }
    }

    let system = if system_parts.is_empty() { None } else { Some(system_parts.join("\n\n")) };
    Ok((system, messages))
}

/// 拒绝多模态内容块的类型集合（需求 15 第 8 条）。
const MULTIMODAL_TYPES: &[&str] = &["image_url", "input_audio", "file"];

/// 提取一条消息 `content` 中的文本块，拒绝多模态内容（需求 15 第 8 条）。
fn extract_content_blocks(content: Option<&Value>, idx: usize) -> Result<Vec<IrBlock>, ValidationError> {
    match content {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => Ok(vec![IrBlock::Text(s.clone())]),
        Some(Value::Array(parts)) => {
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                let part_obj = part
                    .as_object()
                    .ok_or_else(|| invalid_type(format!("messages[{idx}].content[]")))?;
                let ty = part_obj.get("type").and_then(Value::as_str).unwrap_or("");
                if MULTIMODAL_TYPES.contains(&ty) {
                    return Err(err(
                        format!("messages[{idx}].content[].type"),
                        ValidationReason::Unsupported(multimodal_label(ty)),
                    ));
                }
                if ty == "text" {
                    let text = part_obj
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid_type(format!("messages[{idx}].content[].text")))?;
                    out.push(IrBlock::Text(text.to_string()));
                } else {
                    return Err(err(
                        format!("messages[{idx}].content[].type"),
                        ValidationReason::Unsupported("unrecognized content block type"),
                    ));
                }
            }
            Ok(out)
        }
        Some(_) => Err(invalid_type(format!("messages[{idx}].content"))),
    }
}

fn multimodal_label(ty: &str) -> &'static str {
    match ty {
        "image_url" => "image_url content is not supported",
        "input_audio" => "input_audio content is not supported",
        "file" => "file content is not supported",
        _ => "multimodal content is not supported",
    }
}

/// 提取 system/developer 消息内容并拼接为单个字符串（需求 16 第 1 条的消息内拼接规则）。
fn extract_concat_text(content: Option<&Value>, idx: usize) -> Result<String, ValidationError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut texts = Vec::new();
            for part in parts {
                let part_obj = part
                    .as_object()
                    .ok_or_else(|| invalid_type(format!("messages[{idx}].content[]")))?;
                if part_obj.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = part_obj.get("text").and_then(Value::as_str) {
                        texts.push(text.to_string());
                    }
                }
            }
            Ok(texts.join("\n\n"))
        }
        Some(_) => Err(invalid_type(format!("messages[{idx}].content"))),
    }
}

/// 提取 `role: tool` 消息的内容（字符串原样保留，数组按顺序保留全部文本部分）。
fn extract_tool_result_content(
    content: Option<&Value>,
    idx: usize,
) -> Result<ToolResultContent, ValidationError> {
    match content {
        None | Some(Value::Null) => Ok(ToolResultContent::Text(String::new())),
        Some(Value::String(s)) => Ok(ToolResultContent::Text(s.clone())),
        Some(Value::Array(parts)) => {
            let mut texts = Vec::new();
            for part in parts {
                if let Some(part_obj) = part.as_object() {
                    if part_obj.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = part_obj.get("text").and_then(Value::as_str) {
                            texts.push(text.to_string());
                        }
                        continue;
                    }
                }
                return Err(invalid_type(format!("messages[{idx}].content[]")));
            }
            Ok(ToolResultContent::Parts(texts))
        }
        Some(_) => Err(invalid_type(format!("messages[{idx}].content"))),
    }
}

/// 解析一个 `tool_calls[]` 元素为 `IrBlock::ToolCall`（需求 16 第 6 条）。
fn parse_tool_call(tc: &Value, msg_idx: usize) -> Result<IrBlock, ValidationError> {
    let obj = tc
        .as_object()
        .ok_or_else(|| invalid_type(format!("messages[{msg_idx}].tool_calls[]")))?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing(format!("messages[{msg_idx}].tool_calls[].id")))?
        .to_string();
    let function = obj
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| missing(format!("tool_call:{id}")))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| missing(format!("tool_call:{id}")))?
        .to_string();
    let arguments = parse_tool_arguments(function.get("arguments"), &id)?;
    Ok(IrBlock::ToolCall { id, name, arguments })
}

/// 解析 `function.arguments`：空串视为 `{}`；非法 JSON 或非对象结果返回带 tool_call id 的错误
/// （需求 16 第 6 条）。
fn parse_tool_arguments(v: Option<&Value>, tool_call_id: &str) -> Result<Value, ValidationError> {
    let raw = match v {
        None | Some(Value::Null) => return Ok(Value::Object(Map::new())),
        Some(Value::String(s)) => s,
        Some(_) => {
            return Err(err(
                format!("tool_call:{tool_call_id}"),
                ValidationReason::Unsupported("arguments must be a JSON string"),
            ));
        }
    };
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(parsed) if parsed.is_object() => Ok(parsed),
        _ => Err(err(
            format!("tool_call:{tool_call_id}"),
            ValidationReason::Unsupported("arguments must be a JSON object"),
        )),
    }
}

/// 顶层字段名，由本函数生成，extra 中的同名字段永远不覆盖（需求 15 第 4 条）。
const GATEWAY_GENERATED_FIELDS: &[&str] = &["model", "stream", "stream_options"];

/// 将 IR 转换为 OpenAI `/v1/chat/completions` 请求体（design.md「IR → OpenAI 请求」，需求 17）。
///
/// `target.upstream_model` 决定 `model` 字段；`target.default_max_tokens` 在本函数中未使用
/// （只有 Anthropic 目标需要它作为 `max_tokens` 的兜底值，见 `anthropic::print_request`）。
pub fn print_request(ir: &IrRequest, target: &RouteTarget) -> Result<PrintOutcome, ConvertError> {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(target.upstream_model.clone()));

    let mut messages = Vec::new();
    if let Some(system) = &ir.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &ir.messages {
        print_message(&mut messages, message);
    }
    body.insert("messages".to_string(), Value::Array(messages));

    if let Some(temperature) = &ir.sampling.temperature {
        body.insert("temperature".to_string(), Value::Number(temperature.clone()));
    }
    if let Some(top_p) = &ir.sampling.top_p {
        body.insert("top_p".to_string(), Value::Number(top_p.clone()));
    }
    if let Some(stop) = &ir.sampling.stop {
        if !stop.is_empty() {
            let arr = stop.iter().cloned().map(Value::String).collect();
            body.insert("stop".to_string(), Value::Array(arr));
        }
    }
    if let Some(max_tokens) = &ir.sampling.max_tokens {
        let field = match max_tokens.source {
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
            MaxTokensField::MaxTokens | MaxTokensField::AnthropicMaxTokens => "max_tokens",
        };
        body.insert(field.to_string(), json!(max_tokens.value));
    }

    if !ir.tools.is_empty() {
        let tools = ir.tools.iter().map(print_tool).collect();
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(tool_choice) = &ir.tool_choice {
        body.insert("tool_choice".to_string(), print_tool_choice(tool_choice));
    }
    if let Some(parallel) = ir.parallel_tool_calls {
        body.insert("parallel_tool_calls".to_string(), Value::Bool(parallel));
    }

    body.insert("stream".to_string(), Value::Bool(ir.stream));
    if ir.stream {
        body.insert("stream_options".to_string(), json!({ "include_usage": true }));
    }

    let dropped_extra = if ir.extra.origin == ClientProtocol::OpenAi {
        for (key, value) in ir.extra.fields.iter() {
            if GATEWAY_GENERATED_FIELDS.contains(&key.as_str()) {
                continue;
            }
            if !body.contains_key(key) {
                body.insert(key.clone(), value.clone());
            }
        }
        Vec::new()
    } else {
        ir.extra.fields.keys().cloned().collect()
    };

    Ok(PrintOutcome { body: Value::Object(body), dropped_extra })
}

/// 把一条 IR 消息追加为 0 到多条 OpenAI 消息（design.md「IR → OpenAI 请求」第 2–4 条）。
fn print_message(out: &mut Vec<Value>, message: &IrMessage) {
    match message.role {
        IrRole::User => print_user_message(out, &message.content),
        IrRole::Tool => print_tool_messages(out, &message.content),
        IrRole::Assistant => out.push(print_assistant_message(&message.content)),
    }
}

/// User 消息：先按序输出其中每个 `ToolResult` 块对应的 `role: tool` 消息，
/// 再把全部 `Text` 块按序输出为一条 `role: user` 消息（无 `Text` 则不输出）。
fn print_user_message(out: &mut Vec<Value>, content: &[IrBlock]) {
    let mut texts = Vec::new();
    for block in content {
        match block {
            IrBlock::Text(text) => texts.push(text.clone()),
            IrBlock::ToolResult { tool_call_id, content } => {
                out.push(print_tool_result_message(tool_call_id, content));
            }
            IrBlock::ToolCall { .. } => {}
        }
    }
    if !texts.is_empty() {
        let content = if texts.len() == 1 {
            Value::String(texts[0].clone())
        } else {
            Value::Array(texts.iter().map(|t| json!({ "type": "text", "text": t })).collect())
        };
        out.push(json!({ "role": "user", "content": content }));
    }
}

/// Tool 消息（OpenAI 来源）：原样输出为 `{role: tool, tool_call_id, content}`。
fn print_tool_messages(out: &mut Vec<Value>, content: &[IrBlock]) {
    for block in content {
        if let IrBlock::ToolResult { tool_call_id, content } = block {
            out.push(print_tool_result_message(tool_call_id, content));
        }
    }
}

fn print_tool_result_message(tool_call_id: &str, content: &ToolResultContent) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": print_tool_result_content(content),
    })
}

fn print_tool_result_content(content: &ToolResultContent) -> Value {
    match content {
        ToolResultContent::Text(s) => Value::String(s.clone()),
        ToolResultContent::Parts(parts) => {
            Value::Array(parts.iter().map(|p| json!({ "type": "text", "text": p })).collect())
        }
    }
}

/// Assistant 消息：全部 `Text` 块按序直接拼接为 `content`（无 `Text` 时为 `null`），
/// 全部 `ToolCall` 块按序转换为 `tool_calls`（需求 17 第 3 条）。
fn print_assistant_message(content: &[IrBlock]) -> Value {
    let mut text = String::new();
    let mut has_text = false;
    let mut tool_calls = Vec::new();
    for block in content {
        match block {
            IrBlock::Text(t) => {
                has_text = true;
                text.push_str(t);
            }
            IrBlock::ToolCall { id, name, arguments } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": stringify_arguments(arguments) },
                }));
            }
            IrBlock::ToolResult { .. } => {}
        }
    }
    let mut obj = Map::new();
    obj.insert("role".to_string(), Value::String("assistant".to_string()));
    obj.insert("content".to_string(), if has_text { Value::String(text) } else { Value::Null });
    if !tool_calls.is_empty() {
        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    Value::Object(obj)
}

/// `arguments` 为空对象 / `null` 时输出 `"{}"`（需求 18 第 6 条、需求 38 第 4 条）。
fn stringify_arguments(arguments: &Value) -> String {
    let is_empty = match arguments {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    };
    if is_empty { "{}".to_string() } else { serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string()) }
}

fn print_tool(tool: &IrTool) -> Value {
    let mut function = Map::new();
    function.insert("name".to_string(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        function.insert("description".to_string(), Value::String(description.clone()));
    }
    function.insert("parameters".to_string(), tool.schema.clone());
    json!({ "type": "function", "function": Value::Object(function) })
}

fn print_tool_choice(tool_choice: &IrToolChoice) -> Value {
    match tool_choice {
        IrToolChoice::Auto => Value::String("auto".to_string()),
        IrToolChoice::Required => Value::String("required".to_string()),
        IrToolChoice::None => Value::String("none".to_string()),
        IrToolChoice::Tool { name } => json!({ "type": "function", "function": { "name": name } }),
    }
}

/// 生成 `/v1/chat/completions` 的完整上游 URL：`base_url` 已以 `/v1` 结尾时只追加
/// `/chat/completions`，否则在追加前补上 `/v1`（design.md「IR → OpenAI 请求」第 10 条）。
pub fn upstream_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else {
        format!("{trimmed}/v1/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Result<IrRequest, ValidationError> {
        parse_request(serde_json::to_vec(&v).unwrap().as_slice())
    }

    #[test]
    fn minimal_valid_request() {
        let ir = parse(json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        assert_eq!(ir.model, "gpt-4o");
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, IrRole::User);
        assert_eq!(ir.messages[0].content, vec![IrBlock::Text("hi".into())]);
        assert!(!ir.stream);
        assert!(ir.system.is_none());
    }

    #[test]
    fn invalid_json_body() {
        let e = parse_request(b"not json").unwrap_err();
        assert_eq!(e.reason, ValidationReason::InvalidJson);
    }

    #[test]
    fn not_an_object() {
        let e = parse_request(b"[1,2,3]").unwrap_err();
        assert_eq!(e.reason, ValidationReason::NotObject);
    }

    #[test]
    fn missing_model() {
        let e = parse(json!({"messages": [{"role": "user", "content": "hi"}]})).unwrap_err();
        assert_eq!(e.field, "model");
        assert_eq!(e.reason, ValidationReason::Missing);
    }

    #[test]
    fn empty_model_is_invalid() {
        let e = parse(json!({"model": "", "messages": [{"role":"user","content":"hi"}]})).unwrap_err();
        assert_eq!(e.field, "model");
    }

    #[test]
    fn missing_messages() {
        let e = parse(json!({"model": "gpt-4o"})).unwrap_err();
        assert_eq!(e.field, "messages");
        assert_eq!(e.reason, ValidationReason::Missing);
    }

    #[test]
    fn empty_messages_array() {
        let e = parse(json!({"model": "gpt-4o", "messages": []})).unwrap_err();
        assert_eq!(e.field, "messages");
        assert_eq!(e.reason, ValidationReason::Empty);
    }

    #[test]
    fn message_without_role() {
        let e = parse(json!({"model": "m", "messages": [{"content": "hi"}]})).unwrap_err();
        assert_eq!(e.field, "messages[0].role");
    }

    #[test]
    fn stream_must_be_bool() {
        let e = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}], "stream": "yes"
        }))
        .unwrap_err();
        assert_eq!(e.field, "stream");
    }

    #[test]
    fn system_and_developer_are_concatenated_in_order() {
        let ir = parse(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys1"},
                {"role": "developer", "content": "dev1"},
                {"role": "user", "content": "hi"}
            ]
        }))
        .unwrap();
        assert_eq!(ir.system, Some("sys1\n\ndev1".to_string()));
        assert_eq!(ir.messages.len(), 1);
    }

    #[test]
    fn system_content_array_joins_text_parts() {
        let ir = parse(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "a"},
                    {"type": "text", "text": "b"}
                ]},
                {"role": "user", "content": "hi"}
            ]
        }))
        .unwrap();
        assert_eq!(ir.system, Some("a\n\nb".to_string()));
    }

    #[test]
    fn tool_message_becomes_tool_result_block() {
        let ir = parse(json!({
            "model": "m",
            "messages": [
                {"role": "tool", "tool_call_id": "call_1", "content": "42"}
            ]
        }))
        .unwrap();
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, IrRole::Tool);
        assert_eq!(
            ir.messages[0].content,
            vec![IrBlock::ToolResult {
                tool_call_id: "call_1".into(),
                content: ToolResultContent::Text("42".into())
            }]
        );
    }

    #[test]
    fn assistant_tool_calls_parsed_with_arguments() {
        let ir = parse(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"sf\"}"}
                }]
            }]
        }))
        .unwrap();
        assert_eq!(
            ir.messages[0].content,
            vec![IrBlock::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments: json!({"city": "sf"})
            }]
        );
    }

    #[test]
    fn empty_tool_arguments_string_becomes_empty_object() {
        let ir = parse(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": ""}
                }]
            }]
        }))
        .unwrap();
        match &ir.messages[0].content[0] {
            IrBlock::ToolCall { arguments, .. } => assert_eq!(arguments, &json!({})),
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn invalid_tool_arguments_reports_tool_call_id() {
        let e = parse(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_42",
                    "type": "function",
                    "function": {"name": "f", "arguments": "[1,2]"}
                }]
            }]
        }))
        .unwrap_err();
        assert_eq!(e.field, "tool_call:call_42");
    }

    #[test]
    fn non_object_tool_arguments_json_rejected() {
        let e = parse(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "not json"}
                }]
            }]
        }))
        .unwrap_err();
        assert_eq!(e.field, "tool_call:call_1");
    }

    #[test]
    fn multimodal_image_content_rejected() {
        let e = parse(json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "http://x"}}
                ]
            }]
        }))
        .unwrap_err();
        assert_eq!(e.field, "messages[0].content[].type");
    }

    #[test]
    fn stop_string_becomes_single_element_vec() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}], "stop": "END"
        }))
        .unwrap();
        assert_eq!(ir.sampling.stop, Some(vec!["END".to_string()]));
    }

    #[test]
    fn stop_empty_array_becomes_none() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}], "stop": []
        }))
        .unwrap();
        assert_eq!(ir.sampling.stop, None);
    }

    #[test]
    fn tool_choice_variants() {
        for (input, expected) in [
            (json!("auto"), IrToolChoice::Auto),
            (json!("required"), IrToolChoice::Required),
            (json!("none"), IrToolChoice::None),
            (
                json!({"type": "function", "function": {"name": "f"}}),
                IrToolChoice::Tool { name: "f".into() },
            ),
        ] {
            let ir = parse(json!({
                "model": "m", "messages": [{"role":"user","content":"hi"}], "tool_choice": input
            }))
            .unwrap();
            assert_eq!(ir.tool_choice, Some(expected));
        }
    }

    #[test]
    fn invalid_tool_choice_rejected() {
        let e = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}], "tool_choice": "bogus"
        }))
        .unwrap_err();
        assert_eq!(e.field, "tool_choice");
    }

    #[test]
    fn max_tokens_priority_prefers_max_completion_tokens() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}],
            "max_tokens": 10, "max_completion_tokens": 20
        }))
        .unwrap();
        assert_eq!(
            ir.sampling.max_tokens,
            Some(MaxTokens { value: 20, source: MaxTokensField::MaxCompletionTokens })
        );
    }

    #[test]
    fn max_tokens_used_when_max_completion_tokens_absent() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}], "max_tokens": 10
        }))
        .unwrap();
        assert_eq!(
            ir.sampling.max_tokens,
            Some(MaxTokens { value: 10, source: MaxTokensField::MaxTokens })
        );
    }

    #[test]
    fn unrecognized_top_level_fields_collected_as_extra() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}],
            "seed": 42, "response_format": {"type": "json_object"}
        }))
        .unwrap();
        assert_eq!(ir.extra.origin, ClientProtocol::OpenAi);
        assert_eq!(ir.extra.fields.get("seed"), Some(&json!(42)));
        assert_eq!(ir.extra.fields.get("response_format"), Some(&json!({"type": "json_object"})));
        assert!(ir.extra.fields.get("model").is_none());
    }

    #[test]
    fn stream_options_include_usage_is_recognized_not_extra() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}],
            "stream_options": {"include_usage": true}
        }))
        .unwrap();
        assert!(ir.include_usage);
        assert!(ir.extra.fields.get("stream_options").is_none());
    }

    #[test]
    fn tools_preserve_schema_verbatim() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        let ir = parse(json!({
            "model": "m", "messages": [{"role":"user","content":"hi"}],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": schema}}]
        }))
        .unwrap();
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "f");
        assert_eq!(ir.tools[0].description, Some("d".to_string()));
        assert_eq!(ir.tools[0].schema, schema);
    }

    fn target(upstream_model: &str) -> RouteTarget {
        RouteTarget {
            id: uuid::Uuid::nil(),
            provider_id: uuid::Uuid::nil(),
            upstream_model: upstream_model.to_string(),
            priority: 0,
            weight: 1,
            default_max_tokens: None,
            enabled: true,
        }
    }

    #[test]
    fn print_sets_model_stream_and_messages() {
        let ir = parse(json!({
            "model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}], "stream": true
        }))
        .unwrap();
        let out = print_request(&ir, &target("gpt-4o-mini")).unwrap();
        assert_eq!(out.body["model"], "gpt-4o-mini");
        assert_eq!(out.body["stream"], true);
        assert_eq!(out.body["stream_options"], json!({"include_usage": true}));
        assert_eq!(out.body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert!(out.dropped_extra.is_empty());
    }

    #[test]
    fn print_omits_stream_options_when_not_streaming() {
        let ir = parse(json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})).unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(out.body["stream"], false);
        assert!(out.body.get("stream_options").is_none());
    }

    #[test]
    fn print_emits_system_message_first() {
        let ir = parse(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "sys1"},
                {"role": "developer", "content": "dev1"},
                {"role": "user", "content": "hi"}
            ]
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(
            out.body["messages"],
            json!([
                {"role": "system", "content": "sys1\n\ndev1"},
                {"role": "user", "content": "hi"}
            ])
        );
    }

    #[test]
    fn print_round_trips_tool_message() {
        let ir = parse(json!({
            "model": "m",
            "messages": [{"role": "tool", "tool_call_id": "call_1", "content": "42"}]
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(
            out.body["messages"],
            json!([{"role": "tool", "tool_call_id": "call_1", "content": "42"}])
        );
        let ir2 = parse(out.body).unwrap();
        assert_eq!(ir, ir2);
    }

    #[test]
    fn print_round_trips_assistant_tool_calls() {
        let ir = parse(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": "thinking",
                "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"sf\"}"}
                }]
            }]
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(
            out.body["messages"][0],
            json!({
                "role": "assistant",
                "content": "thinking",
                "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"sf\"}"}
                }]
            })
        );
        let ir2 = parse(out.body).unwrap();
        assert_eq!(ir, ir2);
    }

    #[test]
    fn print_empty_tool_call_arguments_as_empty_object_string() {
        let ir = parse(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "tool_calls": [{"id": "c", "type": "function", "function": {"name": "f", "arguments": ""}}]
            }]
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(out.body["messages"][0]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(out.body["messages"][0]["content"], Value::Null);
    }

    #[test]
    fn print_outputs_max_tokens_by_source_field() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 10
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(out.body["max_tokens"], 10);
        assert!(out.body.get("max_completion_tokens").is_none());

        let ir = parse(json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}], "max_completion_tokens": 20
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(out.body["max_completion_tokens"], 20);
        assert!(out.body.get("max_tokens").is_none());
    }

    #[test]
    fn print_outputs_stop_tools_and_tool_choice() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}],
            "stop": "END",
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}}],
            "tool_choice": {"type": "function", "function": {"name": "f"}},
            "parallel_tool_calls": false
        }))
        .unwrap();
        let out = print_request(&ir, &target("m")).unwrap();
        assert_eq!(out.body["stop"], json!(["END"]));
        assert_eq!(
            out.body["tools"],
            json!([{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}}])
        );
        assert_eq!(out.body["tool_choice"], json!({"type": "function", "function": {"name": "f"}}));
        assert_eq!(out.body["parallel_tool_calls"], false);
    }

    #[test]
    fn print_writes_same_origin_extra_without_overriding_generated_fields() {
        let ir = parse(json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}],
            "seed": 42, "model_extra_marker": "ignored_name_but_not_model"
        }))
        .unwrap();
        let out = print_request(&ir, &target("upstream-m")).unwrap();
        assert_eq!(out.body["seed"], 42);
        // 顶层 `model` 永远是 Gateway 生成的值，不会被 extra 覆盖（此处 extra 中没有名为
        // `model` 的字段，这里只是确认已识别字段优先）。
        assert_eq!(out.body["model"], "upstream-m");
        assert!(out.dropped_extra.is_empty());
    }

    #[test]
    fn print_drops_extra_from_different_origin() {
        let mut ir = parse(json!({
            "model": "m", "messages": [{"role": "user", "content": "hi"}], "seed": 42
        }))
        .unwrap();
        ir.extra.origin = ClientProtocol::Anthropic;
        let out = print_request(&ir, &target("m")).unwrap();
        assert!(out.body.get("seed").is_none());
        assert_eq!(out.dropped_extra, vec!["seed".to_string()]);
    }

    #[test]
    fn print_ignores_extra_fields_named_like_generated_fields() {
        let mut ir = parse(json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})).unwrap();
        ir.extra.fields.insert("model".to_string(), json!("hijacked"));
        ir.extra.fields.insert("stream".to_string(), json!(true));
        let out = print_request(&ir, &target("real-model")).unwrap();
        assert_eq!(out.body["model"], "real-model");
        assert_eq!(out.body["stream"], false);
    }

    #[test]
    fn upstream_url_appends_without_duplicating_v1() {
        assert_eq!(upstream_url("https://api.example.com/v1"), "https://api.example.com/v1/chat/completions");
        assert_eq!(upstream_url("https://api.example.com/v1/"), "https://api.example.com/v1/chat/completions");
        assert_eq!(upstream_url("https://api.example.com"), "https://api.example.com/v1/chat/completions");
    }
}
