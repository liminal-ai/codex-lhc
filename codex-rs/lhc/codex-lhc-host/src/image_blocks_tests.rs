use super::*;
use pretty_assertions::assert_eq;

#[test]
fn missing_blob_and_missing_url_remain_text_placeholders() {
    let blocks = vec![
        json!({"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":{"$blob":"sha256:absent", "bytes":3}}}),
        json!({"type":"image", "source":{"type":"url"}}),
    ];
    let blocks = blocks
        .into_iter()
        .map(|v| v.as_object().expect("block").clone())
        .collect::<Vec<_>>();
    let restored = restore_user(&blocks);
    assert_eq!(restored.len(), 2);
    for item in restored {
        let ContentItem::InputText { text } = item else {
            panic!("missing image must not become an image URL")
        };
        assert!(text.contains("image"));
        assert!(text.len() < 100);
    }
}

#[test]
fn image_details_and_text_order_round_trip() {
    for detail in [
        None,
        Some(ImageDetail::Auto),
        Some(ImageDetail::Low),
        Some(ImageDetail::High),
        Some(ImageDetail::Original),
    ] {
        let items = vec![
            ContentItem::InputText {
                text: "before".into(),
            },
            ContentItem::InputImage {
                image_url: "data:image/png;base64,YWJj".into(),
                detail,
            },
            ContentItem::InputText {
                text: "after".into(),
            },
        ];
        let blocks = user_blocks(&items).expect("blocks");
        let blocks = serde_json::from_value::<Vec<Map<String, Value>>>(blocks).expect("maps");
        assert_eq!(restore_user(&blocks), items);
    }
}
