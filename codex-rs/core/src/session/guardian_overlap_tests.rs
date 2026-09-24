use super::proven_suffix_overlap;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn user(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(call_id: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "shell".into(),
        namespace: None,
        arguments: arguments.into(),
        encrypted_function_args: None,
        call_id: call_id.into(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call_output(call_id: &str, output: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.into()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(output.into()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn unproven_repeated_continue_is_not_overlap() {
    let checkpoint = vec![user("continue")];
    let suffix = vec![user("continue")];
    assert_eq!(proven_suffix_overlap(&checkpoint, &suffix), 0);
}

#[test]
fn call_and_output_with_the_same_call_id_are_not_the_same_item() {
    let checkpoint = vec![function_call_output("fc_1", "out")];
    let suffix = vec![function_call("fc_1", "{}")];
    assert_eq!(proven_suffix_overlap(&checkpoint, &suffix), 0);
}

#[test]
fn anchored_window_proves_id_less_user_plus_matching_call() {
    let checkpoint = vec![
        user("older"),
        user("read the file"),
        function_call("fc_1", "{\"path\":\"src/main.rs\"}"),
        function_call_output("fc_1", "fn main() {}"),
    ];
    let suffix = vec![
        user("read the file"),
        function_call("fc_1", "{\"path\":\"src/main.rs\"}"),
        function_call_output("fc_1", "fn main() {}"),
        user("also redact the changelog"),
    ];
    assert_eq!(proven_suffix_overlap(&checkpoint, &suffix), 3);
}

#[test]
fn matching_call_id_is_enough_when_other_fields_differ() {
    let mut reconstructed = function_call("fc_1", "{}");
    if let ResponseItem::FunctionCall { namespace, .. } = &mut reconstructed {
        *namespace = None;
    }
    let mut live = function_call("fc_1", "{}");
    if let ResponseItem::FunctionCall { namespace, .. } = &mut live {
        *namespace = Some("mcp".into());
    }
    assert_eq!(proven_suffix_overlap(&[live], &[reconstructed]), 1);
}

#[test]
fn new_call_after_overlapping_prefix_is_not_swallowed() {
    let checkpoint = vec![function_call("fc_old", "{}")];
    let suffix = vec![function_call("fc_old", "{}"), function_call("fc_new", "{}")];
    assert_eq!(proven_suffix_overlap(&checkpoint, &suffix), 1);
}
