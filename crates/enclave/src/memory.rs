//! `read_memory`: how a model reads the person's Memory Palace, one file at a time.
//!
//! The app sends a question with a compact index of the palace — id, title, description
//! per file, nothing from inside a file — and the enclave offers the model one tool,
//! `read_memory(file_id)`. When the model calls it, the answer goes back to the app as it
//! is (`toolCalls`), the app reads the file from its own copy, leaves out what must never
//! be sent, anonymises the rest, and asks again with the call and its result appended to
//! the messages. Nothing of this is kept here: every round is an ordinary, stateless chat
//! request, held and billed like any other.
//!
//! The neutral shape in `messages`, mapped by each provider (`openai::to_input`,
//! `gemini::to_gemini`):
//! - `{role:"assistant", content, toolCalls:[{id, name, args:{file_id}, sig?}]}` — `sig` is
//!   the provider's own opaque token for the call (Gemini's thought signature), handed
//!   back unchanged;
//! - `{role:"tool", toolCallId, name, content}`.

use crate::policy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const TOOL: &str = "read_memory";

const DESCRIPTION: &str = "Read one file of the user's Memory Palace — facts the user chose to keep about themselves. \
Call it only when a file's title or description suggests it holds something you need to answer well. \
Returns the file's facts, one per line.";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryIndex {
    #[serde(default)]
    pub index: Vec<IndexEntry>,
}

fn clip(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

/// The parameters' JSON schema, the same for every provider.
pub fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": { "file_id": { "type": "string", "description": "The id of the file, as the index gives it." } },
        "required": ["file_id"],
    })
}

pub fn openai_tool() -> Value {
    json!({ "type": "function", "name": TOOL, "description": DESCRIPTION, "parameters": parameters(), "strict": false })
}

pub fn gemini_tool() -> Value {
    json!({ "functionDeclarations": [{ "name": TOOL, "description": DESCRIPTION, "parameters": parameters() }] })
}

/// `read_memory` results since the last question the person asked.
pub fn reads_this_turn(messages: &Value) -> usize {
    let msgs = messages.as_array().map(Vec::as_slice).unwrap_or(&[]);
    msgs.iter().rev().take_while(|m| m.get("role").and_then(|r| r.as_str()) != Some("user")).filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool")).count()
}

/// Is what the app sent about the palace within bounds? The index and every tool result.
pub fn check(index: Option<&MemoryIndex>, messages: &Value) -> Result<(), String> {
    if let Some(ix) = index {
        if ix.index.len() > policy::MEMORY_INDEX_ENTRIES {
            return Err(format!("the memory index may list {} files at most", policy::MEMORY_INDEX_ENTRIES));
        }
    }
    for m in messages.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
            let n = m.get("content").and_then(|c| c.as_str()).map(str::len).unwrap_or(0);
            if n > policy::MEMORY_RESULT_BYTES {
                return Err("a memory file sent to the model is too long".into());
            }
        }
    }
    Ok(())
}

/// Has this question had all the reads it may have? The tool stays declared (the
/// history holds calls to it, which a provider would refuse otherwise) but may not be
/// called: the model answers with what it has.
pub fn exhausted(messages: &Value) -> bool {
    reads_this_turn(messages) >= policy::MEMORY_READS_PER_TURN
}

/// What the model is told when the tool is offered: what the palace is, how to use it, and
/// the index. None when there is no index.
pub fn instruction(index: Option<&MemoryIndex>, messages: &Value) -> Option<String> {
    let ix = index?;
    if ix.index.is_empty() {
        return None;
    }
    let mut s = String::from(
        "The user keeps a Memory Palace: facts about themselves, end-to-end encrypted on their own devices. \
You see only its index below. If a file looks relevant to the user's message, call read_memory with its id before you answer; \
otherwise answer directly. Read only what you need. Never mention file ids or the tool. \
Placeholders in square brackets such as [EMPLOYER_1] or [PERSON_1] stand for details the user anonymised: use them exactly as written and never guess what they stand for.\n\nMemory Palace index:\n",
    );
    for e in ix.index.iter().take(policy::MEMORY_INDEX_ENTRIES) {
        let id = clip(&e.id, 40);
        let title = clip(&e.title, policy::MEMORY_TITLE_CHARS);
        let desc = clip(&e.description, policy::MEMORY_DESCRIPTION_CHARS);
        if desc.is_empty() {
            s.push_str(&format!("- {id}: {title}\n"));
        } else {
            s.push_str(&format!("- {id}: {title} — {desc}\n"));
        }
    }
    if exhausted(messages) {
        s.push_str("\nYou have read all the memory you may read for this message. Answer now with what you have.\n");
    }
    Some(s)
}

/// The neutral form of one call, as it goes back to the app.
pub fn call(id: &str, file_id: &str, sig: Option<&str>) -> Value {
    let mut c = json!({ "id": id, "name": TOOL, "args": { "file_id": file_id } });
    if let Some(s) = sig {
        c["sig"] = json!(s);
    }
    c
}

/// The file id a neutral call asks for.
pub fn file_id_of(call: &Value) -> String {
    call.pointer("/args/file_id").and_then(|f| f.as_str()).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> MemoryIndex {
        MemoryIndex { index: vec![IndexEntry { id: "f_1".into(), title: "Work".into(), description: "Job, role, team size".into() }] }
    }

    #[test]
    fn the_tool_is_offered_until_the_question_has_had_its_reads() {
        let mut msgs = vec![json!({ "role": "user", "content": "what should I tell my team?" })];
        assert!(instruction(Some(&index()), &json!(msgs)).unwrap().contains("- f_1: Work — Job, role, team size"));
        assert!(instruction(None, &json!(msgs)).is_none());
        assert!(instruction(Some(&MemoryIndex::default()), &json!(msgs)).is_none());
        for i in 0..policy::MEMORY_READS_PER_TURN {
            msgs.push(json!({ "role": "assistant", "content": "", "toolCalls": [call(&format!("c{i}"), "f_1", None)] }));
            msgs.push(json!({ "role": "tool", "toolCallId": format!("c{i}"), "name": TOOL, "content": "Works at [EMPLOYER_1]" }));
        }
        assert!(exhausted(&json!(msgs)), "three reads, then answer");
        assert!(instruction(Some(&index()), &json!(msgs)).unwrap().contains("Answer now"));
        // A new question opens the tool again.
        msgs.push(json!({ "role": "assistant", "content": "answer" }));
        msgs.push(json!({ "role": "user", "content": "and now?" }));
        assert!(!exhausted(&json!(msgs)));
        assert!(!instruction(Some(&index()), &json!(msgs)).unwrap().contains("Answer now"));
    }

    #[test]
    fn oversized_results_and_indexes_are_refused() {
        let long = "x".repeat(policy::MEMORY_RESULT_BYTES + 1);
        assert!(check(None, &json!([{ "role": "tool", "content": long }])).is_err());
        let many = MemoryIndex { index: (0..policy::MEMORY_INDEX_ENTRIES + 1).map(|i| IndexEntry { id: format!("f_{i}"), ..Default::default() }).collect() };
        assert!(check(Some(&many), &json!([])).is_err());
        assert!(check(Some(&index()), &json!([{ "role": "tool", "content": "ok" }])).is_ok());
    }

    fn round() -> Value {
        json!([
            { "role": "user", "content": "what should I tell my team?" },
            { "role": "assistant", "content": "", "toolCalls": [call("c_1", "f_1", Some("SIG"))] },
            { "role": "tool", "toolCallId": "c_1", "name": TOOL, "content": "Leads a team of [NUMBER_1]" },
        ])
    }

    #[test]
    fn openai_gets_a_function_call_and_its_output() {
        let input = crate::openai::to_input(&round());
        let items = input.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "c_1");
        assert_eq!(serde_json::from_str::<Value>(items[1]["arguments"].as_str().unwrap()).unwrap()["file_id"], "f_1");
        assert_eq!(items[2]["type"], "function_call_output");
        assert_eq!(items[2]["output"], "Leads a team of [NUMBER_1]");
        let answer = json!({ "output": [{ "type": "function_call", "call_id": "call_9", "name": TOOL, "arguments": "{\"file_id\":\"f_2\"}" }] });
        let calls = crate::openai::tool_calls(&answer);
        assert_eq!((calls[0]["id"].as_str(), file_id_of(&calls[0]).as_str()), (Some("call_9"), "f_2"));
    }

    #[test]
    fn gemini_gets_the_call_with_its_signature_and_one_turn_of_results() {
        let mut msgs = round();
        msgs.as_array_mut().unwrap()[1]["toolCalls"].as_array_mut().unwrap().push(call("gcall_1", "f_2", None));
        msgs.as_array_mut().unwrap().push(json!({ "role": "tool", "toolCallId": "gcall_1", "name": TOOL, "content": "second" }));
        let body = crate::gemini::to_gemini(&msgs, 100, 0, false);
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "{body}");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "SIG");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["args"]["file_id"], "f_1");
        assert!(contents[1]["parts"][1]["functionCall"].get("id").is_none(), "a made-up id is not sent back");
        let responses = contents[2]["parts"].as_array().unwrap();
        assert_eq!(responses.len(), 2, "both results in one turn");
        assert_eq!(responses[0]["functionResponse"]["response"]["content"], "Leads a team of [NUMBER_1]");
        let parts = json!([{ "functionCall": { "name": TOOL, "args": { "file_id": "f_3" } }, "thoughtSignature": "S2" }]);
        let calls = crate::gemini::tool_calls(parts.as_array().unwrap());
        assert_eq!((file_id_of(&calls[0]).as_str(), calls[0]["sig"].as_str()), ("f_3", Some("S2")));
        assert!(calls[0]["id"].as_str().unwrap().starts_with("gcall_"));
    }
}
