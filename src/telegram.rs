use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Message {
        chat_id: i64,
        user_id: i64,
        text: String,
    },
    Callback {
        id: String,
        chat_id: i64,
        user_id: i64,
        message_id: i64,
        data: String,
    },
}

/// Inline keyboard: rows of (label, callback_data).
pub type Keyboard = Vec<Vec<(String, String)>>;

pub struct Telegram {
    api_url: String,
    token: String,
    agent: ureq::Agent,
}

impl Telegram {
    pub fn new(api_url: &str, token: &str) -> Telegram {
        Telegram {
            api_url: api_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
        }
    }

    /// Calls a Bot API method. Errors never include the URL, which holds the token.
    fn call(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("{}/bot{}/{method}", self.api_url, self.token);
        let resp: Value = match self.agent.post(&url).send_json(body) {
            Ok(r) => r.into_json()?,
            Err(ureq::Error::Status(code, r)) => {
                let desc = r
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| {
                        v.get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                return Err(anyhow!("telegram {method}: HTTP {code} {desc}"));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(anyhow!("telegram {method}: transport error {:?}", t.kind()));
            }
        };
        if resp.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(resp.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(anyhow!("telegram {method} failed: {resp}"))
        }
    }

    pub fn get_updates(&self, offset: i64, timeout_secs: u32) -> Result<(i64, Vec<Update>)> {
        let result = self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }),
        )?;
        Ok(parse_updates(&result, offset))
    }

    pub fn send(&self, chat_id: i64, html: &str, kb: Option<&Keyboard>) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(kb) = kb {
            body["reply_markup"] = keyboard_json(kb);
        }
        self.call("sendMessage", body).map(|_| ())
    }

    pub fn send_photo(&self, chat_id: i64, photo_url: &str, caption_html: &str) -> Result<()> {
        self.call(
            "sendPhoto",
            json!({
                "chat_id": chat_id,
                "photo": photo_url,
                "caption": caption_html,
                "parse_mode": "HTML",
            }),
        )
        .map(|_| ())
    }

    pub fn edit(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
        kb: Option<&Keyboard>,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(kb) = kb {
            body["reply_markup"] = keyboard_json(kb);
        }
        self.call("editMessageText", body).map(|_| ())
    }

    pub fn answer_callback(&self, id: &str, text: &str) -> Result<()> {
        self.call(
            "answerCallbackQuery",
            json!({"callback_query_id": id, "text": text}),
        )
        .map(|_| ())
    }

    pub fn set_commands(&self) -> Result<()> {
        self.call(
            "setMyCommands",
            json!({"commands": [
                {"command": "list", "description": "Your searches"},
                {"command": "check", "description": "Check all searches now"},
                {"command": "help", "description": "How to use the bot"},
            ]}),
        )
        .map(|_| ())
    }
}

fn keyboard_json(kb: &Keyboard) -> Value {
    let rows: Vec<Value> = kb
        .iter()
        .map(|row| {
            Value::Array(
                row.iter()
                    .map(|(text, data)| json!({"text": text, "callback_data": data}))
                    .collect(),
            )
        })
        .collect();
    json!({ "inline_keyboard": rows })
}

/// Parses a getUpdates result. Returns the next offset and the updates we care about.
pub fn parse_updates(result: &Value, offset: i64) -> (i64, Vec<Update>) {
    let mut next = offset;
    let mut out = Vec::new();
    for item in result.as_array().into_iter().flatten() {
        if let Some(uid) = item.get("update_id").and_then(Value::as_i64) {
            next = next.max(uid + 1);
        }
        if let Some(m) = item.get("message") {
            let chat_id = m.pointer("/chat/id").and_then(Value::as_i64);
            let user_id = m.pointer("/from/id").and_then(Value::as_i64);
            let text = m.get("text").and_then(Value::as_str);
            if let (Some(chat_id), Some(user_id), Some(text)) = (chat_id, user_id, text) {
                out.push(Update::Message {
                    chat_id,
                    user_id,
                    text: text.to_string(),
                });
            }
        } else if let Some(c) = item.get("callback_query") {
            let id = c.get("id").and_then(Value::as_str);
            let user_id = c.pointer("/from/id").and_then(Value::as_i64);
            let chat_id = c.pointer("/message/chat/id").and_then(Value::as_i64);
            let message_id = c.pointer("/message/message_id").and_then(Value::as_i64);
            let data = c.get("data").and_then(Value::as_str);
            if let (Some(id), Some(user_id), Some(chat_id), Some(message_id), Some(data)) =
                (id, user_id, chat_id, message_id, data)
            {
                out.push(Update::Callback {
                    id: id.to_string(),
                    chat_id,
                    user_id,
                    message_id,
                    data: data.to_string(),
                });
            }
        }
    }
    (next, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages_and_callbacks() {
        let v: Value = serde_json::from_str(
            r#"[
              {"update_id": 10, "message": {"text": "/start", "chat": {"id": 5}, "from": {"id": 7}}},
              {"update_id": 11, "message": {"chat": {"id": 5}, "from": {"id": 7}, "photo": []}},
              {"update_id": 12, "edited_message": {"text": "x", "chat": {"id": 5}, "from": {"id": 7}}},
              {"update_id": 12, "callback_query": {"id": "incomplete"}},
              {"update_id": 12, "callback_query": {"id": "q", "from": {"id": 7}, "data": "p:3",
                "message": {"message_id": 99, "chat": {"id": 5}}}}
            ]"#,
        )
        .unwrap();
        let (next, ups) = parse_updates(&v, 0);
        assert_eq!(next, 13);
        assert_eq!(ups.len(), 2);
        assert_eq!(
            ups[0],
            Update::Message {
                chat_id: 5,
                user_id: 7,
                text: "/start".into()
            }
        );
        assert_eq!(
            ups[1],
            Update::Callback {
                id: "q".into(),
                chat_id: 5,
                user_id: 7,
                message_id: 99,
                data: "p:3".into()
            }
        );
    }

    #[test]
    fn empty_keeps_offset() {
        let (next, ups) = parse_updates(&Value::Array(vec![]), 42);
        assert_eq!(next, 42);
        assert!(ups.is_empty());
    }
}
