//! Fit output allowances without truncating prompts or cloning request trees.
//! Catalog context sizes are exact; prompt counts are conservative estimates.
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ModelLimits {
    pub(crate) context: Option<u64>,
    output: Option<u64>,
}

impl ModelLimits {
    pub(crate) fn from_model(model: &Value) -> Self {
        fn minimum(model: &Value, paths: &[&str]) -> Option<u64> {
            paths
                .iter()
                .filter_map(|path| model.pointer(path)?.as_u64())
                .filter(|limit| *limit > 0)
                .min()
        }
        Self {
            context: minimum(
                model,
                &[
                    "/meta/n_ctx",
                    "/context_window",
                    "/context_length",
                    "/max_model_len",
                    "/max_context_length",
                ],
            ),
            output: minimum(model, &["/max_output_tokens", "/max_completion_tokens"]),
        }
    }
}

const OUTPUT_FIELDS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

pub(crate) struct FittedRequest<'a> {
    body: &'a Value,
    pub(crate) cap: Option<u64>,
}

impl<'a> FittedRequest<'a> {
    pub(crate) fn new(body: &'a Value, limits: ModelLimits) -> Self {
        let requested = OUTPUT_FIELDS
            .iter()
            .filter_map(|key| body.get(key)?.as_u64())
            .filter(|cap| *cap > 0)
            .max();
        let cap = requested.and_then(|requested| {
            if limits.context.is_none() && limits.output.is_none() {
                return None;
            }
            let room = limits.context.map(|context| {
                // Includes messages, tool schemas and other request content.
                // Reserve 20% plus 256 tokens for estimation/template error.
                // An estimate cannot prove a prompt is too long: leave its
                // validation to the upstream tokenizer, with one output token.
                let prompt = super::estimate_tokens(body).tokens;
                let reserved = prompt
                    .saturating_add(prompt.div_ceil(5))
                    .saturating_add(256);
                context.saturating_sub(reserved).max(1)
            });
            let allowed = room.into_iter().chain(limits.output).min()?;
            (requested > allowed).then_some(allowed)
        });
        Self { body, cap }
    }
}

impl Serialize for FittedRequest<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(cap) = self.cap else {
            return self.body.serialize(serializer);
        };
        let Some(object) = self.body.as_object() else {
            return self.body.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(object.len()))?;
        for (key, value) in object {
            if OUTPUT_FIELDS.contains(&key.as_str()) && value.as_u64().is_some_and(|n| n > cap) {
                map.serialize_entry(key, &cap)?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl std::fmt::Display for FittedRequest<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct Writer<'a, 'b>(&'a mut std::fmt::Formatter<'b>);
        impl std::io::Write for Writer<'_, '_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let text =
                    std::str::from_utf8(bytes).map_err(|_| std::io::ErrorKind::InvalidData)?;
                self.0
                    .write_str(text)
                    .map_err(|_| std::io::ErrorKind::WriteZero)?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(Writer(formatter), self).map_err(|_| std::fmt::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_limits_are_positive_and_conservative() {
        let limits = ModelLimits::from_model(
            &json!({"meta":{"n_ctx":32768}, "context_window":65536,"max_output_tokens":8192}),
        );
        assert_eq!(limits.context, Some(32768));
        assert_eq!(limits.output, Some(8192));
        assert_eq!(
            ModelLimits::from_model(&json!({"meta":{"n_ctx":0}})).context,
            None
        );
    }

    #[test]
    fn fitting_preserves_prompt_and_request_and_never_increases_caps() {
        let body = json!({"model":"test","messages":[{"role":"user","content":"hello"}],
            "tools":[{"type":"function","function":{"name":"test","parameters":{"type":"object"}}}],
            "max_tokens":30000,"max_completion_tokens":10,"max_output_tokens":30000});
        let fitted = FittedRequest::new(
            &body,
            ModelLimits {
                context: Some(1024),
                output: None,
            },
        );
        let wire = serde_json::to_value(&fitted).unwrap();
        assert!(wire["max_tokens"].as_u64().unwrap() < 1024);
        assert_eq!(wire["max_output_tokens"], wire["max_tokens"]);
        assert_eq!(wire["max_completion_tokens"], 10);
        assert_eq!(wire["messages"], body["messages"]);
        assert_eq!(wire["tools"], body["tools"]);
        assert_eq!(body["max_tokens"], 30000);
    }

    #[test]
    fn missing_metadata_or_caps_and_small_requests_are_unchanged() {
        for body in [
            json!({"messages":[]}),
            json!({"max_tokens":0}),
            json!({"max_tokens":-1}),
            json!({"max_tokens":10}),
        ] {
            let fitted = FittedRequest::new(
                &body,
                ModelLimits {
                    context: Some(32768),
                    output: None,
                },
            );
            assert_eq!(serde_json::to_value(&fitted).unwrap(), body);
        }
        let body = json!({"max_tokens":30000});
        assert_eq!(
            serde_json::to_value(FittedRequest::new(&body, ModelLimits::default())).unwrap(),
            body
        );
    }

    #[test]
    fn larger_prompts_get_less_output_room() {
        let make = |content: String| json!({"messages":[{"role":"user","content":content}],"max_tokens":30000});
        let small = make("hi".to_owned());
        let large = make("hello world ".repeat(1000));
        let limits = ModelLimits {
            context: Some(32768),
            output: None,
        };
        let small = serde_json::to_value(FittedRequest::new(&small, limits)).unwrap();
        let large = serde_json::to_value(FittedRequest::new(&large, limits)).unwrap();
        assert!(large["max_tokens"].as_u64().unwrap() < small["max_tokens"].as_u64().unwrap());
    }
}
