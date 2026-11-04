//! 协议无关的内部表示（IR）。
//!
//! OpenAI 与 Anthropic 的请求/响应都先转换为这里定义的类型，再由目标协议的
//! Pretty_Printer 输出。详见 design.md “IR 类型”一节。
//!
//! IR 等价比较（用于往返性质测试，需求 15 第 5、6 条）：数值按数值大小比较
//! （`1` 与 `1.0` 等价），JSON 对象的字段顺序不参与比较；因此含
//! `serde_json::Value` / `serde_json::Number` 字段的类型都手写了 `PartialEq`，
//! 其余类型可以直接 `derive`。

use serde_json::{Map, Number, Value};

/// 客户端使用的协议。上游协议取值相同，用类型别名区分语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientProtocol {
    OpenAi,
    Anthropic,
}

/// 上游协议与客户端协议取值相同。
pub type UpstreamProtocol = ClientProtocol;

/// 两个 JSON 数字是否数值相等（忽略整数/浮点的内部表示差异）。
pub fn ir_number_eq(a: &Number, b: &Number) -> bool {
    if a == b {
        return true;
    }
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn ir_opt_number_eq(a: &Option<Number>, b: &Option<Number>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => ir_number_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

/// 两个 JSON 值是否满足 IR 等价：数值按大小比较，对象字段顺序不参与比较。
pub fn ir_value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => ir_number_eq(a, b),
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| ir_value_eq(a, b))
        }
        (Value::Object(a), Value::Object(b)) => ir_map_eq(a, b),
        _ => false,
    }
}

/// 两个 JSON 对象是否满足 IR 等价（字段顺序不参与比较）。
pub fn ir_map_eq(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(k, v)| b.get(k).is_some_and(|v2| ir_value_eq(v, v2)))
}

/// 一次 Chat Completion / Messages 请求的内部表示。
#[derive(Debug, Clone, PartialEq)]
pub struct IrRequest {
    /// Public_Model（或往返测试中直接等于 upstream_model）。
    pub model: String,
    /// 已拼接完成的 system 提示文本。
    pub system: Option<String>,
    pub messages: Vec<IrMessage>,
    pub sampling: Sampling,
    pub tools: Vec<IrTool>,
    pub tool_choice: Option<IrToolChoice>,
    /// OpenAI 语义：`true` 表示允许并行工具调用。Anthropic 的
    /// `disable_parallel_tool_use` 取反后存入此字段。
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
    /// OpenAI `stream_options.include_usage`。
    pub include_usage: bool,
    pub extra: Extra,
}

/// 采样相关参数。
#[derive(Debug, Clone, Default)]
pub struct Sampling {
    pub temperature: Option<Number>,
    pub top_p: Option<Number>,
    pub max_tokens: Option<MaxTokens>,
    pub stop: Option<Vec<String>>,
}

impl PartialEq for Sampling {
    fn eq(&self, other: &Self) -> bool {
        ir_opt_number_eq(&self.temperature, &other.temperature)
            && ir_opt_number_eq(&self.top_p, &other.top_p)
            && self.max_tokens == other.max_tokens
            && self.stop == other.stop
    }
}

/// `max_tokens`，连同其来源字段一起记录，保证 OpenAI 往返时
/// `max_completion_tokens` 与 `max_tokens` 不互换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxTokens {
    pub value: u64,
    pub source: MaxTokensField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    MaxCompletionTokens,
    MaxTokens,
    AnthropicMaxTokens,
}

/// IR 中的消息角色。不含 System：system 文本统一放在 `IrRequest.system`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IrRole {
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IrMessage {
    pub role: IrRole,
    pub content: Vec<IrBlock>,
}

/// 消息内容块。
#[derive(Debug, Clone)]
pub enum IrBlock {
    Text(String),
    /// 已解析的工具调用，`arguments` 为解析后的 JSON 对象。
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: ToolResultContent,
    },
}

impl PartialEq for IrBlock {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (IrBlock::Text(a), IrBlock::Text(b)) => a == b,
            (
                IrBlock::ToolCall {
                    id,
                    name,
                    arguments,
                },
                IrBlock::ToolCall {
                    id: id2,
                    name: name2,
                    arguments: arguments2,
                },
            ) => id == id2 && name == name2 && ir_value_eq(arguments, arguments2),
            (
                IrBlock::ToolResult {
                    tool_call_id,
                    content,
                },
                IrBlock::ToolResult {
                    tool_call_id: id2,
                    content: content2,
                },
            ) => tool_call_id == id2 && content == content2,
            _ => false,
        }
    }
}

/// 工具结果消息的内容：字符串或多个文本片段（保留 OpenAI 数组形式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResultContent {
    Text(String),
    Parts(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct IrTool {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema 原值。
    pub schema: Value,
}

impl PartialEq for IrTool {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && ir_value_eq(&self.schema, &other.schema)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrToolChoice {
    Auto,
    Required,
    None,
    Tool { name: String },
}

/// 网关未识别的顶层字段，连同来源协议一起保存，用于同协议透传
/// （需求 15 第 1、2、3 条）。
#[derive(Debug, Clone)]
pub struct Extra {
    pub origin: ClientProtocol,
    /// 需要 serde_json 开启 "preserve_order" feature。
    pub fields: Map<String, Value>,
}

impl Default for Extra {
    fn default() -> Self {
        Extra {
            origin: ClientProtocol::OpenAi,
            fields: Map::new(),
        }
    }
}

impl PartialEq for Extra {
    fn eq(&self, other: &Self) -> bool {
        self.origin == other.origin && ir_map_eq(&self.fields, &other.fields)
    }
}

/// 一次非流式响应的内部表示。
#[derive(Debug, Clone, PartialEq)]
pub struct IrResponse {
    pub id: Option<String>,
    /// 只含 `Text` 与 `ToolCall`。
    pub content: Vec<IrBlock>,
    pub stop: StopReason,
    pub usage: IrUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    StopSequence,
    MaxTokens,
    ToolUse,
}

/// 用量统计，字段均为累积值。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IrUsage {
    /// 不含缓存部分。
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// 上游是否报告过 usage（需求 30 第 5 条）。
    pub reported: bool,
}

impl IrUsage {
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }
}

/// 流式响应的逐事件表示，上游解码器产出、客户端编码器消费。
#[derive(Debug, Clone, PartialEq)]
pub enum IrStreamEvent {
    /// Anthropic `message_start` 携带的 usage 放在这里。
    Start {
        upstream_id: Option<String>,
        usage: Option<IrUsage>,
    },
    TextDelta(String),
    /// `key`：上游侧工具调用标识（OpenAI 的 `index` 或 Anthropic 的 block index）。
    ToolCallStart {
        key: u32,
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        key: u32,
        fragment: String,
    },
    /// 覆盖式更新。
    Usage(IrUsage),
    Stop(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamEnd {
    pub stop: StopReason,
    pub usage: IrUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamErrorInfo {
    pub kind: StreamFaultKind,
    pub upstream_type: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFaultKind {
    Disconnected,
    PrematureEnd,
    Unparseable,
    Timeout,
    UpstreamError,
}
