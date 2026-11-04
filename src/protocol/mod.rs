pub mod anthropic;
pub mod openai;

/// 打印非流式响应时使用的上下文（design.md「非流式响应转换」）。
///
/// `id`、`created` 由 Gateway 在收到请求时生成/记录，`public_model` 取自请求中的
/// Public_Model 原值；跨协议或同协议均统一由 Gateway 生成 id（需求 2 第 3 条、
/// 需求 3 第 3 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseCtx {
    pub id: String,
    pub created: i64,
    pub public_model: String,
}

/// 上游非流式响应解析失败：2xx 但响应体不是合法 JSON、缺少协议必需字段，
/// 或工具调用参数非法（需求 18 第 8、9 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDecodeError(pub String);

impl UpstreamDecodeError {
    pub fn new(message: impl Into<String>) -> Self {
        UpstreamDecodeError(message.into())
    }
}

impl std::fmt::Display for UpstreamDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UpstreamDecodeError {}

/// `print_request` 的成功返回值：生成的上游请求体，连同因 extra 来源协议与目标协议
/// 不同而被丢弃的顶层字段名（需求 15 第 3 条，调用方据此写 debug 日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintOutcome {
    pub body: serde_json::Value,
    pub dropped_extra: Vec<String>,
}

/// `print_request` 在转换期发现的错误：Bytes_Sent 之前即可确定，不计入重试
/// （design.md「Proxy_Engine」「尝试循环」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// Anthropic 目标缺少可用的 `max_tokens`（需求 16 第 2、3 条）。
    MaxTokensMissing,
    /// 工具调用的 `arguments`/`input` 无法序列化为 JSON 对象。
    InvalidToolArguments { tool_call_id: String },
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::MaxTokensMissing => write!(f, "max_tokens is required"),
            ConvertError::InvalidToolArguments { tool_call_id } => {
                write!(f, "invalid function.arguments in tool_call {tool_call_id}")
            }
        }
    }
}

impl std::error::Error for ConvertError {}
