//! Codex images in SDK content blocks. Blob extraction remains SDK-owned.

use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

fn image_block(url: &str, detail: Option<ImageDetail>) -> Value {
    let source = url
        .strip_prefix("data:")
        .and_then(|rest| {
            let (meta, data) = rest.split_once(',')?;
            let media_type = meta.strip_suffix(";base64")?;
            (!media_type.is_empty())
                .then(|| json!({"type":"base64", "media_type":media_type, "data":data}))
        })
        .unwrap_or_else(|| json!({"type":"url", "url":url}));
    json!({"type":"image", "source":source, "codexImageDetail":detail})
}

pub(crate) fn user_blocks(items: &[ContentItem]) -> Option<Value> {
    if !items
        .iter()
        .any(|item| matches!(item, ContentItem::InputImage { .. }))
    {
        return None;
    }
    Some(Value::Array(
        items
            .iter()
            .map(|item| match item {
                ContentItem::InputImage { image_url, detail } => image_block(image_url, *detail),
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    json!({"type":"text", "text":text})
                }
                ContentItem::InputAudio { audio_url } => {
                    json!({"type":"text", "text":format!("[audio:{audio_url}]")})
                }
            })
            .collect(),
    ))
}

pub(crate) fn tool_blocks(body: &FunctionCallOutputBody) -> Option<Value> {
    let FunctionCallOutputBody::ContentItems(items) = body else {
        return None;
    };
    if !items
        .iter()
        .any(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
    {
        return None;
    }
    Some(Value::Array(
        items
            .iter()
            .map(|item| match item {
                FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                    image_block(image_url, *detail)
                }
                FunctionCallOutputContentItem::InputText { text } => {
                    json!({"type":"text", "text":text})
                }
                FunctionCallOutputContentItem::InputAudio { audio_url } => {
                    json!({"type":"text", "text":format!("[audio:{audio_url}]")})
                }
                FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                    json!({"type":"text", "text":encrypted_content})
                }
            })
            .collect(),
    ))
}

/// Only the text projection enters text payloads; image bytes belong in blobs.
pub(crate) fn text_projection(blocks: &Value) -> String {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .map(lhc::shared_tech::content_blocks::placeholder_text)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn restore_user(blocks: &[Map<String, Value>]) -> Vec<ContentItem> {
    blocks
        .iter()
        .map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("image") {
                let source = block.get("source");
                let url = match source.and_then(|s| s.get("type")).and_then(Value::as_str) {
                    Some("url") => source
                        .and_then(|s| s.get("url"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    Some("base64") => source.and_then(|s| {
                        Some(format!(
                            "data:{};base64,{}",
                            s.get("media_type")?.as_str()?,
                            s.get("data")?.as_str()?
                        ))
                    }),
                    _ => None,
                };
                if let Some(image_url) = url.filter(|url| !url.is_empty()) {
                    let detail = block
                        .get("codexImageDetail")
                        .and_then(|v| serde_json::from_value(v.clone()).ok());
                    return ContentItem::InputImage { image_url, detail };
                }
            }
            ContentItem::InputText {
                text: lhc::shared_tech::content_blocks::placeholder_text(&Value::Object(
                    block.clone(),
                )),
            }
        })
        .collect()
}

pub(crate) fn restore_tool(blocks: &[Map<String, Value>]) -> FunctionCallOutputBody {
    FunctionCallOutputBody::ContentItems(
        restore_user(blocks)
            .into_iter()
            .map(|item| match item {
                ContentItem::InputImage { image_url, detail } => {
                    FunctionCallOutputContentItem::InputImage { image_url, detail }
                }
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    FunctionCallOutputContentItem::InputText { text }
                }
                ContentItem::InputAudio { audio_url } => {
                    FunctionCallOutputContentItem::InputAudio { audio_url }
                }
            })
            .collect(),
    )
}

#[cfg(test)]
#[path = "image_blocks_tests.rs"]
mod tests;
