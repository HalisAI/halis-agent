use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        signature: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: String,
        name: String,
        ok: bool,
        content: String,
    },
}

impl Block {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Block::Text { text } => Some(text),
            _ => None,
        }
    }

    pub fn as_tool_call(&self) -> Option<ToolCall> {
        match self {
            Block::ToolCall {
                id,
                name,
                arguments,
            } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Msg {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            blocks: vec![Block::Text { text: text.into() }],
        }
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            blocks: vec![Block::Text { text: text.into() }],
        }
    }

    pub fn user_tool_results(results: Vec<(ToolCall, ToolOutput)>) -> Self {
        Self {
            role: Role::User,
            blocks: results
                .into_iter()
                .map(|(call, out)| Block::ToolResult {
                    id: call.id,
                    name: call.name,
                    ok: out.ok,
                    content: out.content,
                })
                .collect(),
        }
    }

    pub fn assistant(blocks: Vec<Block>) -> Self {
        Self {
            role: Role::Assistant,
            blocks,
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::assistant(vec![Block::Text { text: text.into() }])
    }

    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.blocks
            .iter()
            .filter_map(|b| b.as_tool_call())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub ok: bool,
    pub content: String,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            ok: true,
            content: content.into(),
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            ok: false,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    pub fn cost(&self, price_in: f64, price_out: f64) -> f64 {
        (self.input as f64 * price_in + self.output as f64 * price_out) / 1_000_000.0
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TurnStats {
    pub turns: u32,
    pub tool_calls: u32,
    pub usage: Usage,
    pub cost: f64,
    pub secs: f64,
}
