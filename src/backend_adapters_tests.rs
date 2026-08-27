use serde_json::json;

use super::{
    AnthropicStreamTranslator, CLAUDE_CODE_IDENTITY, anthropic_to_chat_response,
    chat_to_anthropic_request, chat_to_codex_responses_request, codex_sse_to_chat_completion,
    ensure_claude_code_identity, sanitize_copilot_chat_request,
    sanitize_cross_provider_chat_request,
};

#[test]
fn chat_to_anthropic_preserves_tools_results_and_images() {
    let request = chat_to_anthropic_request(&json!({
        "model": "claude-test",
        "messages": [
            { "role": "system", "content": "be precise" },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "inspect" },
                    { "type": "image_url", "image_url": { "url": "https://example.invalid/a.png" } }
                ]
            },
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
                }]
            },
            { "role": "tool", "tool_call_id": "call_1", "content": "ok" }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "read",
                "description": "read a file",
                "parameters": { "type": "object" }
            }
        }]
    }));

    assert_eq!(request["system"][0]["text"], "be precise");
    assert_eq!(
        request["messages"][0]["content"][1]["source"]["type"],
        "url"
    );
    assert_eq!(request["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(request["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(request["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(request["max_tokens"], 4096);
}

#[test]
fn cross_provider_sanitizer_removes_signed_history_artifacts() {
    let mut request = json!({
        "messages": [{
            "role": "assistant",
            "reasoning_content": "private",
            "content": [
                { "type": "thinking", "thinking": "private", "signature": "signed", "encrypted_content": "ciphertext" },
                { "type": "text", "text": "visible" }
            ]
        }]
    });
    sanitize_cross_provider_chat_request(&mut request);

    assert!(request["messages"][0].get("reasoning_content").is_none());
    assert_eq!(
        request["messages"][0]["content"].as_array().unwrap().len(),
        1
    );
    assert_eq!(request["messages"][0]["content"][0]["text"], "visible");
    assert!(!request.to_string().contains("signed"));
    assert!(!request.to_string().contains("ciphertext"));
}

#[test]
fn anthropic_response_becomes_chat_with_usage_and_tools() {
    let response = anthropic_to_chat_response(
        json!({
            "id": "msg_1",
            "model": "claude-test",
            "content": [
                { "type": "thinking", "thinking": "private", "signature": "signed" },
                { "type": "text", "text": "done" },
                { "type": "tool_use", "id": "call_2", "name": "write", "input": { "path": "b" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 7, "output_tokens": 3 }
        }),
        Some("anthropic/claude-test"),
        42,
    );

    assert_eq!(response["model"], "anthropic/claude-test");
    assert_eq!(response["choices"][0]["message"]["content"], "done");
    assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(response["usage"]["total_tokens"], 10);
    assert_eq!(
        response["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "write"
    );
    assert!(!response.to_string().contains("signed"));
}

#[test]
fn copilot_sanitizer_drops_signed_reasoning_and_normalizes_text_parts() {
    let mut request = json!({
        "reasoning": { "effort": "high" },
        "messages": [{
            "role": "user",
            "content": [
                { "type": "input_text", "text": "hello", "cache_control": { "type": "ephemeral" } },
                { "type": "thinking", "thinking": "private", "signature": "signed" },
                { "type": "tool_result", "content": { "ok": true } },
                { "type": "image_url", "image_url": { "url": "https://example.invalid/a.png" } }
            ]
        }]
    });
    sanitize_copilot_chat_request(&mut request);

    assert!(request.get("reasoning").is_none());
    assert_eq!(request["messages"][0]["content"][0]["type"], "text");
    assert_eq!(
        request["messages"][0]["content"].as_array().unwrap().len(),
        3
    );
    assert_eq!(request["messages"][0]["content"][1]["type"], "text");
    assert!(
        request["messages"][0]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("ok")
    );
    assert!(!request.to_string().contains("signed"));
}

#[test]
fn anthropic_stream_translator_emits_chat_deltas() {
    let mut translator = AnthropicStreamTranslator::new(Some("client-model".to_string()), 42);
    let mut chunks = translator.translate(&json!({
        "type": "message_start",
        "message": {
            "id": "msg_1",
            "model": "claude-test",
            "usage": { "input_tokens": 6, "output_tokens": 0 }
        }
    }));
    chunks.extend(translator.translate(&json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": { "type": "tool_use", "id": "call_1", "name": "read", "input": {} }
    })));
    chunks.extend(translator.translate(&json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" }
    })));
    chunks.extend(translator.translate(&json!({
        "type": "message_delta",
        "delta": { "stop_reason": "tool_use" },
        "usage": { "output_tokens": 4 }
    })));

    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":"
    );
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[3]["usage"]["prompt_tokens"], 6);
    assert_eq!(chunks[3]["usage"]["completion_tokens"], 4);
    assert_eq!(chunks[3]["usage"]["total_tokens"], 10);
}

#[test]
fn claude_code_identity_is_prepended_and_idempotent() {
    let mut request = json!({
        "system": [{ "type": "text", "text": "be precise" }],
        "messages": [],
    });
    ensure_claude_code_identity(&mut request);
    assert_eq!(request["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(request["system"][1]["text"], "be precise");
    // Idempotent: a second pass does not duplicate the identity block.
    ensure_claude_code_identity(&mut request);
    assert_eq!(request["system"].as_array().unwrap().len(), 2);

    // String system prompts and missing system both gain the identity.
    let mut from_string = json!({ "system": "custom", "messages": [] });
    ensure_claude_code_identity(&mut from_string);
    assert_eq!(from_string["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(from_string["system"][1]["text"], "custom");

    let mut from_none = json!({ "messages": [] });
    ensure_claude_code_identity(&mut from_none);
    assert_eq!(from_none["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(from_none["system"].as_array().unwrap().len(), 1);
}

#[test]
fn chat_to_codex_responses_shapes_request() {
    let request = chat_to_codex_responses_request(&json!({
        "model": "gpt-5.6-sol",
        "messages": [
            { "role": "system", "content": "be a backend expert" },
            { "role": "user", "content": "fix the server" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "shell", "arguments": "{\"cmd\":\"ls\"}" }
                }]
            },
            { "role": "tool", "tool_call_id": "call_1", "content": "file.txt" }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "shell",
                "description": "run a shell command",
                "parameters": { "type": "object", "properties": {} }
            }
        }]
    }));

    assert_eq!(request["model"], "gpt-5.6-sol");
    assert_eq!(request["instructions"], "be a backend expert");
    assert_eq!(request["stream"], true);
    assert_eq!(request["store"], false);
    assert_eq!(request["include"][0], "reasoning.encrypted_content");
    // Flat Responses tool shape (name at top level, not nested under function).
    assert_eq!(request["tools"][0]["type"], "function");
    assert_eq!(request["tools"][0]["name"], "shell");
    let input = request["input"].as_array().unwrap();
    assert!(input.iter().any(|item| item["type"] == "function_call"
        && item["call_id"] == "call_1"
        && item["name"] == "shell"));
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["call_id"] == "call_1"
                && item["output"] == "file.txt")
    );
}

#[test]
fn codex_sse_folds_into_chat_completion() {
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n",
        "data: [DONE]\n\n",
    );
    let completion = codex_sse_to_chat_completion(sse, "gpt-5.6-sol");
    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["model"], "gpt-5.6-sol");
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        "Hello world"
    );
    assert_eq!(completion["choices"][0]["finish_reason"], "stop");
    assert_eq!(completion["usage"]["prompt_tokens"], 11);
    assert_eq!(completion["usage"]["completion_tokens"], 3);
    assert_eq!(completion["usage"]["total_tokens"], 14);
}

#[test]
fn codex_sse_folds_function_call() {
    let sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        "data: [DONE]\n\n",
    );
    let completion = codex_sse_to_chat_completion(sse, "gpt-5.6-sol");
    assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");
    let call = &completion["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_9");
    assert_eq!(call["function"]["name"], "shell");
    assert_eq!(call["function"]["arguments"], "{\"cmd\":\"pwd\"}");
    assert!(completion["choices"][0]["message"]["content"].is_null());
}

#[test]
fn anthropic_cache_tokens_reach_the_chat_wire_inside_prompt_tokens() {
    let response = anthropic_to_chat_response(
        json!({
            "id": "msg_1",
            "model": "claude-test",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 40,
                "cache_read_input_tokens": 700,
                "cache_creation_input_tokens": 200
            }
        }),
        None,
        7,
    );
    let usage = &response["usage"];
    // Anthropic excludes both cache fields from input_tokens; the chat wire
    // convention includes them.
    assert_eq!(usage["prompt_tokens"], 1000);
    assert_eq!(usage["completion_tokens"], 40);
    assert_eq!(usage["total_tokens"], 1040);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 700);
    assert_eq!(usage["prompt_tokens_details"]["cache_creation_tokens"], 200);
}

#[test]
fn anthropic_stream_carries_cache_tokens_into_the_final_usage_chunk() {
    let mut translator = AnthropicStreamTranslator::new(Some("client-model".to_string()), 42);
    translator.translate(&json!({
        "type": "message_start",
        "message": {
            "id": "msg_2",
            "model": "claude-test",
            "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 90,
                "cache_creation_input_tokens": 5
            }
        }
    }));
    let chunks = translator.translate(&json!({
        "type": "message_delta",
        "delta": { "stop_reason": "end_turn" },
        "usage": { "output_tokens": 12 }
    }));
    let usage = chunks
        .iter()
        .find_map(|chunk| chunk.get("usage"))
        .expect("final chunk carries usage");
    assert_eq!(usage["prompt_tokens"], 105);
    assert_eq!(usage["completion_tokens"], 12);
    assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 90);
    assert_eq!(usage["prompt_tokens_details"]["cache_creation_tokens"], 5);
}
