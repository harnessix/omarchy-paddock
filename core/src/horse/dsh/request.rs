//! The request that dsh sends for a session log, rebuilt offline.
//!
//! A port of the path from the log to the HTTP body in dsh 0.2.1-rc.5:
//! `Agent.buildRequest` (`packages/core/agent-loop/src/agent.ts`) takes the
//! request config and tools of the last `request/header` and the messages
//! of `Session.deriveMessages()` (`packages/core/session/src/index.ts`,
//! `surface.ts`). `serialize` (`packages/llm/llm-deepseek/src/serialize.ts`)
//! turns them into the Messages API body, and the adapter sends
//! `JSON.stringify` of it. The port covers text, reasoning, tool calls and
//! tool results. It refuses what it does not cover (images, compaction
//! replacements, developer tool changes), so a proof never passes by
//! leaving a part out. The prompt cache of DeepSeek is keyed by the content
//! prefix (system, tools, messages), not by the session id.

use anyhow::{bail, Context as _, Result};
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A JSON value that keeps the order of object keys, as JavaScript does.
#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(serde_json::Number),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<J, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = J;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<J, E> {
                Ok(J::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<J, E> {
                Ok(J::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<J, E> {
                Ok(J::Num(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<J, E> {
                Ok(J::Num(n.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<J, E> {
                serde_json::Number::from_f64(n)
                    .map(J::Num)
                    .ok_or_else(|| E::custom("not a finite number"))
            }
            fn visit_str<E>(self, s: &str) -> Result<J, E> {
                Ok(J::Str(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<J, E> {
                Ok(J::Str(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<J, A::Error> {
                let mut v = Vec::new();
                while let Some(x) = a.next_element()? {
                    v.push(x);
                }
                Ok(J::Arr(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<J, A::Error> {
                let mut v: Vec<(String, J)> = Vec::new();
                while let Some((k, x)) = a.next_entry::<String, J>()? {
                    // JavaScript keeps the first position and the last value.
                    match v.iter_mut().find(|(e, _)| *e == k) {
                        Some(e) => e.1 = x,
                        None => v.push((k, x)),
                    }
                }
                Ok(J::Obj(v))
            }
        }
        d.deserialize_any(V)
    }
}

impl J {
    pub fn parse(raw: &[u8]) -> Result<J> {
        Ok(serde_json::from_slice(raw)?)
    }

    pub fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(v) => v.iter().find(|(e, _)| e == k).map(|(_, x)| x),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    fn arr(&self) -> &[J] {
        match self {
            J::Arr(v) => v,
            _ => &[],
        }
    }

    fn s(k: &str, v: &str) -> (String, J) {
        (k.to_string(), J::Str(v.to_string()))
    }

    /// `JSON.stringify` of the value.
    pub fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(&n.to_string()),
            J::Str(s) => out.push_str(&serde_json::to_string(s).expect("a string encodes")),
            J::Arr(v) => {
                out.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    x.write(out);
                }
                out.push(']');
            }
            J::Obj(v) => {
                out.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(k).expect("a key encodes"));
                    out.push(':');
                    x.write(out);
                }
                out.push('}');
            }
        }
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }
}

const SURFACE: [&str; 5] = [
    "system/message",
    "developer/message",
    "user/message",
    "assistant/message",
    "tool/result",
];

/// The messages of `deriveMessages()` for a log whose surface only grows
/// by `surfaceOp: "append"`.
pub fn derive_messages(rows: &[J]) -> Result<Vec<J>> {
    let mut out = Vec::new();
    for r in rows {
        let ty = r.get("type").and_then(J::str).unwrap_or_default();
        if r.get("sourceEventSeqs").is_some() {
            bail!("{ty}: surface replacements (compaction) are not in this port");
        }
        match r.get("surfaceOp").and_then(J::str) {
            None => continue,
            Some("append") => {}
            Some(op) => bail!("{ty}: surfaceOp {op} is not in this port"),
        }
        if !SURFACE.contains(&ty) {
            bail!("{ty} has a surfaceOp but is no surface event");
        }
        let data = r.get("data").context("a surface event has no data")?;
        let msg = if ty == "user/message" {
            data
        } else {
            data.get("message")
                .context("a surface event has no message")?
        };
        let empty = msg
            .get("content")
            .map(J::arr)
            .unwrap_or_default()
            .is_empty();
        if empty
            && matches!(
                ty,
                "system/message" | "developer/message" | "assistant/message"
            )
        {
            continue;
        }
        out.push(msg.clone());
    }
    Ok(out)
}

/// The config and tools of the last `request/header` in the log.
fn last_header(rows: &[J]) -> Result<&J> {
    rows.iter()
        .rev()
        .find(|r| r.get("type").and_then(J::str) == Some("request/header"))
        .and_then(|r| r.get("data")?.get("header"))
        .context("the log has no request/header")
}

/// Whether the route of the last `request/context` appends system prompt
/// updates in the history (`systemPromptUpdate: "in-history"`).
fn in_history(rows: &[J]) -> bool {
    rows.iter()
        .rev()
        .find(|r| r.get("type").and_then(J::str) == Some("request/context"))
        .and_then(|r| r.get("data")?.get("systemPromptUpdate")?.str())
        == Some("in-history")
}

/// `readReplay`: the replay blocks of an assistant message, or none.
fn replay<'a>(m: &'a J, model: &str) -> Option<&'a [J]> {
    let src = m.get("source")?;
    if src.get("kind")?.str()? != "model" {
        return None;
    }
    let env = src.get("replayState")?;
    let resp = env.get("response")?;
    let ok = resp.get("kind")?.str()? == "deepseek-messages"
        && resp.get("version") == Some(&J::Num(1.into()))
        && resp.get("model")?.str() == src.get("model")?.str();
    let blocks = env.get("blocks")?.arr();
    let content = m.get("content")?.arr();
    if !ok || blocks.len() != content.len() {
        return None;
    }
    for (b, c) in blocks.iter().zip(content) {
        let t = b.get("type")?.str()?;
        if Some(t) != c.get("type")?.str() || !matches!(t, "text" | "reasoning" | "tool-call") {
            return None;
        }
        if b.get("signature")
            .is_some_and(|s| t != "reasoning" || s.str().is_none())
        {
            return None;
        }
    }
    (resp.get("model")?.str()? == model).then_some(blocks)
}

/// User and tool result content: text only in this port.
fn input(blocks: &[J]) -> Result<Vec<J>> {
    let mut out = Vec::new();
    for b in blocks {
        match b.get("type").and_then(J::str) {
            Some("text") => {
                let t = b.get("text").and_then(J::str).unwrap_or_default();
                if !t.is_empty() {
                    out.push(J::Obj(vec![J::s("type", "text"), J::s("text", t)]));
                }
            }
            Some("reasoning" | "tool-call") => {}
            other => bail!("user content {other:?} is not in this port"),
        }
    }
    Ok(out)
}

fn assistant(m: &J, model: &str) -> Result<Vec<J>> {
    let rp = replay(m, model);
    let mut out = Vec::new();
    for (i, b) in m
        .get("content")
        .map(J::arr)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let txt = |k: &str| b.get(k).and_then(J::str).unwrap_or_default().to_string();
        match b.get("type").and_then(J::str) {
            Some("text") if txt("text").is_empty() => {}
            Some("text") => out.push(J::Obj(vec![
                J::s("type", "text"),
                J::s("text", &txt("text")),
            ])),
            Some("reasoning") => {
                let mut o = vec![J::s("type", "thinking"), J::s("thinking", &txt("text"))];
                if let Some(sig) = rp.and_then(|r| r[i].get("signature")) {
                    o.push(("signature".into(), sig.clone()));
                }
                out.push(J::Obj(o));
            }
            Some("tool-call") => {
                let inp = match J::parse(txt("arguments").as_bytes()) {
                    Ok(o @ J::Obj(_)) => o,
                    _ => J::Obj(vec![]),
                };
                out.push(J::Obj(vec![
                    J::s("type", "tool_use"),
                    J::s("id", &txt("id")),
                    J::s("name", &txt("name")),
                    ("input".into(), inp),
                ]));
            }
            other => bail!("assistant content {other:?} is not in this port"),
        }
    }
    Ok(out)
}

fn role(m: &J) -> &str {
    m.get("role").and_then(J::str).unwrap_or_default()
}

/// The body that dsh sends for the next request of a log: the log up to
/// the moment before the request.
pub fn build(rows: &[J]) -> Result<J> {
    let header = last_header(rows)?;
    let config = header
        .get("config")
        .context("request/header has no config")?;
    if let J::Obj(v) = config {
        for (k, _) in v {
            if !matches!(
                k.as_str(),
                "provider" | "model" | "maxTokens" | "reasoningEffort" | "temperature" | "stop"
            ) {
                bail!("config key {k} is not in this port");
            }
        }
    }
    let model = config.get("model").and_then(J::str).context("no model")?;
    let in_hist = in_history(rows);
    let mut messages: Vec<(String, Vec<J>)> = Vec::new();
    let mut history_system: Option<String> = None;
    let mut updates: Vec<(String, Vec<J>)> = Vec::new();
    for m in derive_messages(rows)? {
        let r = role(&m);
        let content = m.get("content").map(J::arr).unwrap_or_default();
        if r == "developer" {
            bail!("developer messages are not in this port");
        }
        if r == "system" {
            let mut text = String::new();
            for b in content {
                if b.get("type").and_then(J::str) != Some("text") {
                    bail!("DeepSeek Messages cannot represent a non-text system message");
                }
                text += b.get("text").and_then(J::str).unwrap_or_default();
            }
            if in_hist && !messages.is_empty() {
                if text.is_empty() {
                    bail!("empty in-history system update");
                }
                let t = J::Obj(vec![J::s("type", "text"), J::s("text", &text)]);
                updates.push(("system".into(), vec![t]));
            } else {
                history_system = Some(text);
            }
            continue;
        }
        let blocks = match r {
            "assistant" => assistant(&m, model)?,
            "tool" => {
                let mut o = vec![
                    J::s("type", "tool_result"),
                    (
                        "tool_use_id".into(),
                        m.get("toolCallId").cloned().unwrap_or(J::Null),
                    ),
                    ("content".into(), J::Arr(input(content)?)),
                ];
                if let Some(e) = m.get("isError") {
                    o.push(("is_error".into(), e.clone()));
                }
                vec![J::Obj(o)]
            }
            _ => input(content)?,
        };
        if matches!(r, "user" | "assistant") && blocks.is_empty() {
            continue;
        }
        if r == "assistant" && !updates.is_empty() {
            if messages.last().map(|m| m.0.as_str()) != Some("user") {
                bail!("system update without a preceding user or tool-result turn");
            }
            messages.append(&mut updates);
        }
        let wire = if r == "tool" { "user" } else { r }.to_string();
        match messages.last_mut() {
            Some(prev) if prev.0 == wire => prev.1.extend(blocks),
            _ => messages.push((wire, blocks)),
        }
    }
    if !updates.is_empty() {
        if messages.last().map(|m| m.0.as_str()) != Some("user") {
            bail!("system update without a preceding user or tool-result turn");
        }
        messages.append(&mut updates);
    }
    order_tool_results(&mut messages)?;
    let effort = config
        .get("reasoningEffort")
        .and_then(J::str)
        .context("the port needs reasoningEffort in the request config")?;
    if !matches!(effort, "off" | "low" | "high" | "max") {
        bail!("DeepSeek Messages does not support reasoning effort {effort}");
    }
    let wire_messages = messages
        .into_iter()
        .map(|(r, c)| J::Obj(vec![J::s("role", &r), ("content".into(), J::Arr(c))]))
        .collect();
    let mut body = vec![
        J::s("model", model),
        ("stream".into(), J::Bool(true)),
        ("messages".into(), J::Arr(wire_messages)),
        (
            "max_tokens".into(),
            config
                .get("maxTokens")
                .cloned()
                .context("the port needs maxTokens")?,
        ),
        (
            "thinking".into(),
            J::Obj(vec![J::s(
                "type",
                if effort == "off" {
                    "disabled"
                } else {
                    "enabled"
                },
            )]),
        ),
    ];
    if effort != "off" {
        body.push(("output_config".into(), J::Obj(vec![J::s("effort", effort)])));
    }
    if let Some(s) = history_system.filter(|s| !s.is_empty()) {
        body.push(J::s("system", &s));
    }
    if let Some(t) = config.get("temperature") {
        body.push(("temperature".into(), t.clone()));
    }
    if let Some(s) = config.get("stop") {
        body.push(("stop_sequences".into(), s.clone()));
    }
    if let Some(tools) = header.get("tools") {
        let mut out = Vec::new();
        for t in tools.arr() {
            let mut o = vec![
                (
                    "name".into(),
                    t.get("name").cloned().context("a tool has no name")?,
                ),
                (
                    "description".into(),
                    t.get("description").cloned().unwrap_or(J::Null),
                ),
                (
                    "input_schema".into(),
                    t.get("parameters").cloned().unwrap_or(J::Null),
                ),
            ];
            if t.get("deferLoading") == Some(&J::Bool(true)) {
                o.push(("defer_loading".into(), J::Bool(true)));
            }
            out.push(J::Obj(o));
        }
        body.push(("tools".into(), J::Arr(out)));
    }
    Ok(J::Obj(body))
}

/// The tool call checks of `serialize`, and tool results first in a user
/// message.
fn order_tool_results(messages: &mut [(String, Vec<J>)]) -> Result<()> {
    let is = |b: &J, t: &str| b.get("type").and_then(J::str) == Some(t);
    let mut pending: Vec<J> = Vec::new();
    for (r, content) in messages.iter_mut() {
        if r == "assistant" {
            pending = content
                .iter()
                .filter(|b| is(b, "tool_use"))
                .filter_map(|b| b.get("id").cloned())
                .collect();
        } else if r == "user" {
            for b in content.iter().filter(|b| is(b, "tool_result")) {
                let id = b.get("tool_use_id").cloned().unwrap_or(J::Null);
                let Some(i) = pending.iter().position(|p| *p == id) else {
                    bail!("DeepSeek Messages tool result has no matching call");
                };
                pending.remove(i);
            }
            if !pending.is_empty() {
                bail!("DeepSeek Messages tool calls need immediate results");
            }
            let (mut first, rest): (Vec<J>, Vec<J>) =
                content.drain(..).partition(|b| is(b, "tool_result"));
            first.extend(rest);
            *content = first;
        }
    }
    if !pending.is_empty() {
        bail!("DeepSeek Messages history ends with unresolved tools");
    }
    Ok(())
}

/// The prompt as the model reads it, in cache order: the system text, the
/// tool list, then each message. Two requests share a cached prefix as far
/// as these pieces are equal.
pub fn prompt_pieces(body: &J) -> Vec<String> {
    let mut out = vec![
        body.get("system").map(J::text).unwrap_or_default(),
        body.get("tools").map(J::text).unwrap_or_default(),
    ];
    for k in ["model", "thinking", "output_config"] {
        out.push(body.get(k).map(J::text).unwrap_or_default());
    }
    out.extend(
        body.get("messages")
            .map(J::arr)
            .unwrap_or_default()
            .iter()
            .map(J::text),
    );
    out
}
