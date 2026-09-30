use axum::Json;
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Value, json};

use super::{InterceptorProtocol, LOCAL_ROUTE_HEADER, interceptor_error_response};
use crate::{
    anthropic_usage, chat_completion_to_anthropic_message, chat_completion_to_response,
    chat_usage_to_responses_usage, sse_event, text_from_value, unix_timestamp,
};

pub(crate) fn render_local_completion(
    protocol: InterceptorProtocol,
    mut completion: Value,
    client_model: Option<String>,
    streaming: bool,
    request_id: &str,
) -> Response {
    match protocol {
        InterceptorProtocol::ChatCompletions => {
            if let Some(model) = client_model.as_ref() {
                completion["model"] = Value::String(model.clone());
            }
            if streaming {
                chat_completion_sse(&completion, request_id)
            } else {
                routed_json_response(completion, request_id)
            }
        }
        InterceptorProtocol::Responses => {
            if streaming {
                responses_sse(&completion, client_model, request_id)
            } else {
                let mut response = chat_completion_to_response(completion.clone(), client_model);
                if let Some(usage) = completion.get("usage") {
                    response["usage"] = chat_usage_to_responses_usage(usage);
                }
                routed_json_response(response, request_id)
            }
        }
        InterceptorProtocol::AnthropicMessages => {
            if streaming {
                anthropic_sse(&completion, client_model, request_id)
            } else {
                routed_json_response(
                    chat_completion_to_anthropic_message(completion, client_model),
                    request_id,
                )
            }
        }
    }
}

pub(crate) fn render_rejection(
    protocol: InterceptorProtocol,
    reason: &str,
    client_model: Option<String>,
    streaming: bool,
    request_id: &str,
) -> Response {
    let message = format!(
        "No action was taken. llmproxy blocked this request before external routing because it may contain {reason}. Redact the sensitive data and retry."
    );
    let completion = json!({
        "id": "chatcmpl_rejected",
        "object": "chat.completion",
        "created": unix_timestamp(),
        "model": "llmproxy-reject",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": message },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        }
    });
    let mut response =
        render_local_completion(protocol, completion, client_model, streaming, request_id);
    response.headers_mut().insert(
        HeaderName::from_static(LOCAL_ROUTE_HEADER),
        HeaderValue::from_static("reject"),
    );
    response
}

fn routed_json_response(value: Value, request_id: &str) -> Response {
    let mut response = Json(value).into_response();
    add_route_headers(&mut response, request_id);
    response
}

fn chat_completion_sse(completion: &Value, request_id: &str) -> Response {
    let id = completion
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl_local");
    let created = completion
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(unix_timestamp);
    let model = completion
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("local");
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));
    let content = message
        .and_then(|message| message.get("content"))
        .map(text_from_value)
        .unwrap_or_default();
    let mut tool_calls = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (index, tool_call) in tool_calls.iter_mut().enumerate() {
        if let Some(object) = tool_call.as_object_mut() {
            object.insert("index".to_string(), json!(index));
        }
    }
    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or(if tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        });

    let base_chunk = |delta: Value, finish_reason: Value| {
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "logprobs": Value::Null,
                "finish_reason": finish_reason
            }]
        })
    };
    let mut body = Vec::new();
    push_chat_data(
        &mut body,
        base_chunk(json!({ "role": "assistant", "content": "" }), Value::Null),
    );
    if !content.is_empty() {
        push_chat_data(
            &mut body,
            base_chunk(json!({ "content": content }), Value::Null),
        );
    }
    if !tool_calls.is_empty() {
        push_chat_data(
            &mut body,
            base_chunk(json!({ "tool_calls": tool_calls }), Value::Null),
        );
    }
    push_chat_data(
        &mut body,
        base_chunk(json!({}), Value::String(finish_reason.to_string())),
    );
    if let Some(usage) = completion.get("usage") {
        push_chat_data(
            &mut body,
            json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [],
                "usage": usage
            }),
        );
    }
    body.extend_from_slice(b"data: [DONE]\n\n");
    routed_sse_response(body, request_id)
}

fn responses_sse(completion: &Value, client_model: Option<String>, request_id: &str) -> Response {
    let mut response = chat_completion_to_response(completion.clone(), client_model);
    if let Some(usage) = completion.get("usage") {
        response["usage"] = chat_usage_to_responses_usage(usage);
    }
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_tool_call = output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
    if !has_tool_call {
        response["end_turn"] = Value::Bool(true);
    }

    let mut created_response = response.clone();
    created_response["status"] = Value::String("in_progress".to_string());
    created_response["output"] = Value::Array(Vec::new());
    created_response["output_text"] = Value::String(String::new());

    let mut body = Vec::new();
    let mut sequence = 0_u64;
    push_responses_event(
        &mut body,
        &mut sequence,
        "response.created",
        json!({
            "type": "response.created",
            "response": created_response
        }),
    );

    for (output_index, item) in output.iter().enumerate() {
        let mut added_item = item.clone();
        added_item["status"] = Value::String("in_progress".to_string());
        if added_item.get("type").and_then(Value::as_str) == Some("function_call") {
            added_item["arguments"] = Value::String(String::new());
        }
        push_responses_event(
            &mut body,
            &mut sequence,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": added_item
            }),
        );

        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .and_then(|parts| parts.first())
                    .and_then(|part| part.get("text"))
                    .map(text_from_value)
                    .unwrap_or_default();
                push_responses_event(
                    &mut body,
                    &mut sequence,
                    "response.content_part.added",
                    json!({
                        "type": "response.content_part.added",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": {
                            "type": "output_text",
                            "text": "",
                            "annotations": []
                        }
                    }),
                );
                if !text.is_empty() {
                    push_responses_event(
                        &mut body,
                        &mut sequence,
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "delta": text
                        }),
                    );
                }
                push_responses_event(
                    &mut body,
                    &mut sequence,
                    "response.output_text.done",
                    json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "text": text
                    }),
                );
                push_responses_event(
                    &mut body,
                    &mut sequence,
                    "response.content_part.done",
                    json!({
                        "type": "response.content_part.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "part": {
                            "type": "output_text",
                            "text": text,
                            "annotations": []
                        }
                    }),
                );
            }
            Some("function_call") => {
                let arguments = item
                    .get("arguments")
                    .map(text_from_value)
                    .unwrap_or_default();
                if !arguments.is_empty() {
                    push_responses_event(
                        &mut body,
                        &mut sequence,
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item.get("id").cloned().unwrap_or(Value::Null),
                            "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                            "output_index": output_index,
                            "delta": arguments
                        }),
                    );
                }
            }
            _ => {}
        }

        push_responses_event(
            &mut body,
            &mut sequence,
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item
            }),
        );
    }

    push_responses_event(
        &mut body,
        &mut sequence,
        "response.completed",
        json!({
            "type": "response.completed",
            "response": response
        }),
    );
    routed_sse_response(body, request_id)
}

fn anthropic_sse(completion: &Value, client_model: Option<String>, request_id: &str) -> Response {
    let message = chat_completion_to_anthropic_message(completion.clone(), client_model);
    let content = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let usage = message.get("usage").cloned().unwrap_or_else(|| {
        json!({
            "input_tokens": 0,
            "output_tokens": 0
        })
    });
    let mut start_message = message.clone();
    start_message["content"] = Value::Array(Vec::new());
    start_message["stop_reason"] = Value::Null;
    start_message["stop_sequence"] = Value::Null;
    start_message["usage"] = json!({
        "input_tokens": usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        "output_tokens": 0
    });

    let mut body = Vec::new();
    push_sse(
        &mut body,
        "message_start",
        json!({
            "type": "message_start",
            "message": start_message
        }),
    );
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block.get("text").map(text_from_value).unwrap_or_default();
                push_sse(
                    &mut body,
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": { "type": "text", "text": "" }
                    }),
                );
                if !text.is_empty() {
                    push_sse(
                        &mut body,
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": { "type": "text_delta", "text": text }
                        }),
                    );
                }
            }
            Some("tool_use") => {
                push_sse(
                    &mut body,
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": block.get("id").cloned().unwrap_or(Value::Null),
                            "name": block.get("name").cloned().unwrap_or(Value::Null),
                            "input": {}
                        }
                    }),
                );
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                push_sse(
                    &mut body,
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": input.to_string()
                        }
                    }),
                );
            }
            _ => continue,
        }
        push_sse(
            &mut body,
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": index
            }),
        );
    }
    push_sse(
        &mut body,
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": message.get("stop_reason").cloned().unwrap_or(Value::Null),
                "stop_sequence": message.get("stop_sequence").cloned().unwrap_or(Value::Null)
            },
            "usage": anthropic_usage(completion.get("usage"))
        }),
    );
    push_sse(&mut body, "message_stop", json!({ "type": "message_stop" }));
    routed_sse_response(body, request_id)
}

fn push_chat_data(body: &mut Vec<u8>, value: Value) {
    body.extend_from_slice(format!("data: {value}\n\n").as_bytes());
}

fn push_responses_event(body: &mut Vec<u8>, sequence: &mut u64, event: &str, mut value: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert("sequence_number".to_string(), json!(*sequence));
    }
    *sequence += 1;
    push_sse(body, event, value);
}

fn push_sse(body: &mut Vec<u8>, event: &str, value: Value) {
    let bytes: Bytes = sse_event(event, value);
    body.extend_from_slice(&bytes);
}

fn routed_sse_response(body: Vec<u8>, request_id: &str) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap_or_else(|error| {
            interceptor_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                request_id,
                error.to_string(),
            )
        });
    add_route_headers(&mut response, request_id);
    response
}

fn add_route_headers(response: &mut Response, request_id: &str) {
    response.headers_mut().insert(
        HeaderName::from_static(LOCAL_ROUTE_HEADER),
        HeaderValue::from_static("local"),
    );
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
}
