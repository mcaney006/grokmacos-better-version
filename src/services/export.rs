//! Chat exporters.
//!
//! Three formats are supported:
//! * `Markdown` — `# Title\n\n**User**: …\n**Assistant**: …\n`
//! * `Json` — pretty-printed array of all messages with their full metadata.
//! * `Obsidian` — Markdown with YAML front-matter that Obsidian indexes nicely.

use crate::models::{Chat, Message, Role};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
    Obsidian,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Markdown | Format::Obsidian => "md",
            Format::Json => "json",
        }
    }
}

pub fn export(chat: &Chat, messages: &[Message], format: Format) -> String {
    match format {
        Format::Markdown => markdown(chat, messages, false),
        Format::Obsidian => markdown(chat, messages, true),
        Format::Json => json(chat, messages),
    }
}

fn markdown(chat: &Chat, messages: &[Message], obsidian: bool) -> String {
    let mut out = String::new();
    if obsidian {
        out.push_str("---\n");
        out.push_str(&format!("title: {}\n", yaml_safe(&chat.title)));
        out.push_str(&format!("created: {}\n", chat.created_at.to_rfc3339()));
        out.push_str(&format!("updated: {}\n", chat.updated_at.to_rfc3339()));
        out.push_str(&format!("provider: {}\n", chat.provider));
        out.push_str(&format!("model: {}\n", chat.model));
        out.push_str("tags: [grok-insane]\n");
        out.push_str("---\n\n");
    }
    out.push_str(&format!("# {}\n\n", chat.title));
    if let Some(sp) = chat.system_prompt.as_ref() {
        if !sp.trim().is_empty() {
            out.push_str("> **system**\n>\n");
            for line in sp.lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }
    }
    for m in messages {
        let label = match m.role {
            Role::User => "🧑 **You**",
            Role::Assistant => "🤖 **Assistant**",
            Role::System => "ℹ️ **System**",
            Role::Tool => "🛠 **Tool**",
        };
        out.push_str(&format!(
            "## {} · `{}`\n\n",
            label,
            m.created_at.format("%Y-%m-%d %H:%M:%S")
        ));
        out.push_str(m.content.trim_end());
        out.push_str("\n\n");
    }
    out
}

#[derive(Serialize)]
struct JsonChat<'a> {
    chat: &'a Chat,
    messages: &'a [Message],
}

fn json(chat: &Chat, messages: &[Message]) -> String {
    serde_json::to_string_pretty(&JsonChat { chat, messages })
        .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn yaml_safe(s: &str) -> String {
    // YAML 1.2 plain scalar: avoid leading punctuation, quote if it would
    // confuse the parser.
    let needs_quote = s.is_empty()
        || s.starts_with(|c: char| c.is_ascii_punctuation())
        || s.contains(['"', '\\', ':', '#', '\n', '\r']);
    if needs_quote {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Chat;
    use uuid::Uuid;

    fn fixture() -> (Chat, Vec<Message>) {
        let mut chat = Chat::new("xai", "grok-beta");
        chat.title = "Test: with colon".into();
        let chat_id = chat.id;
        let mut user = Message::new(chat_id, Role::User, "hi there");
        user.id = Uuid::nil();
        let mut asst = Message::new(chat_id, Role::Assistant, "hello!\n\n```rs\nfn x(){}\n```");
        asst.id = Uuid::nil();
        (chat, vec![user, asst])
    }

    #[test]
    fn markdown_export_includes_messages_and_title() {
        let (chat, msgs) = fixture();
        let md = export(&chat, &msgs, Format::Markdown);
        assert!(md.contains("# Test: with colon"));
        assert!(md.contains("🧑 **You**"));
        assert!(md.contains("hi there"));
        assert!(md.contains("hello!"));
        assert!(md.contains("```rs"));
    }

    #[test]
    fn obsidian_export_has_front_matter_and_quotes_title_with_colon() {
        let (chat, msgs) = fixture();
        let md = export(&chat, &msgs, Format::Obsidian);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("title: \"Test: with colon\""));
        assert!(md.contains("tags: [grok-insane]"));
    }

    #[test]
    fn json_export_round_trips() {
        let (chat, msgs) = fixture();
        let body = export(&chat, &msgs, Format::Json);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);
        assert_eq!(v["chat"]["title"], "Test: with colon");
    }
}
