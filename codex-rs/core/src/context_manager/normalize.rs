use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use std::collections::HashMap;
use std::collections::HashSet;
use uuid::Uuid;

use crate::context::ContextualUserFragment;
use crate::context::UnsupportedMedia;
use crate::util::error_or_panic;
use tracing::info;

// Changing this value would change model-visible IDs and invalidate prompt caches.
const SYNTHETIC_OUTPUT_ID_NAMESPACE: Uuid = Uuid::from_u128(0x90d38d3e_6a5b_4d52_bfe2_2f1e634bfac4);

pub(crate) fn ensure_call_outputs_present(items: &mut Vec<ResponseItemEnvelope>) {
    let mut function_output_ids = HashSet::new();
    let mut tool_search_output_ids = HashSet::new();
    let mut custom_tool_output_ids = HashSet::new();
    for envelope in items.iter() {
        match &envelope.item {
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => {
                function_output_ids.insert(call_id.as_str());
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => {
                tool_search_output_ids.insert(call_id.as_str());
            }
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                custom_tool_output_ids.insert(call_id.as_str());
            }
            _ => {}
        }
    }

    // Collect synthetic outputs to insert immediately after their calls.
    // Store the insertion position (index of call) alongside the item so
    // we can insert in reverse order and avoid index shifting.
    let mut missing_outputs_to_insert: Vec<(usize, ResponseItemEnvelope)> = Vec::new();

    for (idx, envelope) in items.iter().enumerate() {
        match &envelope.item {
            ResponseItem::FunctionCall { id, call_id, .. }
                if !function_output_ids.contains(call_id.as_str()) =>
            {
                info!("Function call output is missing for call id: {call_id}");
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: Some(call_id.clone()),
                        name: None,
                        namespace: None,
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    }),
                ));
            }
            ResponseItem::ToolSearchCall {
                id,
                call_id: Some(call_id),
                ..
            } if !tool_search_output_ids.contains(call_id.as_str()) => {
                info!("Tool search output is missing for call id: {call_id}");
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItemEnvelope::new(ResponseItem::ToolSearchOutput {
                        id: synthetic_output_id("tso", id.as_deref()),
                        call_id: Some(call_id.clone()),
                        status: "completed".to_string(),
                        execution: "client".to_string(),
                        tools: Vec::new(),
                        internal_chat_message_metadata_passthrough: None,
                    }),
                ));
            }
            ResponseItem::CustomToolCall { id, call_id, .. }
                if !custom_tool_output_ids.contains(call_id.as_str()) =>
            {
                // Preserved-prefix forks mark copied calls as inherited. A call still
                // running in the parent has no output in the child's snapshot.
                if !envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.inherited_user_message)
                {
                    error_or_panic(format!(
                        "Custom tool call output is missing for call id: {call_id}"
                    ));
                }
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
                        id: synthetic_output_id("ctco", id.as_deref()),
                        call_id: call_id.clone(),
                        name: None,
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    }),
                ));
            }
            // LocalShellCall is represented in upstream streams by a FunctionCallOutput
            ResponseItem::LocalShellCall {
                id,
                call_id: Some(call_id),
                ..
            } if !function_output_ids.contains(call_id.as_str()) => {
                // Preserved-prefix forks mark copied calls as inherited. A call still
                // running in the parent has no output in the child's snapshot.
                if !envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.inherited_user_message)
                {
                    error_or_panic(format!(
                        "Local shell call output is missing for call id: {call_id}"
                    ));
                }
                missing_outputs_to_insert.push((
                    idx,
                    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                        id: synthetic_output_id("fco", id.as_deref()),
                        call_id: Some(call_id.clone()),
                        name: None,
                        namespace: None,
                        output: FunctionCallOutputPayload::from_text("aborted".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    }),
                ));
            }
            _ => {}
        }
    }
    drop((
        function_output_ids,
        tool_search_output_ids,
        custom_tool_output_ids,
    ));

    // Insert synthetic outputs in reverse index order to avoid re-indexing.
    for (idx, output_item) in missing_outputs_to_insert.into_iter().rev() {
        items.insert(idx + 1, output_item);
    }
}

/// Derives a stable ID for a prompt-only output from its source call's item ID.
///
/// Prompt normalization can run repeatedly without persisting its synthetic
/// outputs, so the namespace and name format must remain stable across retries
/// and resumes to preserve prompt-cache reuse. Returning `None` when the source
/// call has no ID preserves the legacy behavior for older history items.
pub(crate) fn synthetic_output_id(prefix: &str, item_id: Option<&str>) -> Option<ResponseItemId> {
    let source_id = item_id.filter(|id| !id.is_empty())?;
    let name = format!("{prefix}:{source_id}");
    Some(ResponseItemId::with_suffix(
        prefix,
        Uuid::new_v5(&SYNTHETIC_OUTPUT_ID_NAMESPACE, name.as_bytes()),
    ))
}

/// Repair histories where a user-context item landed between a call and its recorded output.
///
/// Sessions written before the tool-pair write barrier existed (and races it could not cover)
/// can contain `FunctionCall(call-1), Message(user), FunctionCallOutput(call-1)`. Chat and
/// Anthropic tool protocols require a result to follow its call immediately, so normalization
/// relocates already-recorded outputs back to their call's block instead of dropping or
/// duplicating them:
/// - an output moves directly after its call block when its call id matches the block,
/// - a call batch keeps its original call order, and outputs keep their original relative order,
/// - already-adjacent pairs and the context after them are untouched, so unchanged prompt
///   prefixes keep their cache hit,
/// - duplicate outputs for one call id collapse to the first occurrence,
/// - calls without any output and outputs without any call are left to
///   `ensure_call_outputs_present` / `remove_orphan_outputs`.
///
/// Runs before `ensure_call_outputs_present` so a call whose only output was relocated is not
/// given a second synthetic result.
pub(crate) fn reorder_delayed_tool_outputs_after_calls(items: &mut Vec<ResponseItemEnvelope>) {
    let mut call_index_by_id: HashMap<String, usize> = HashMap::new();
    for (index, envelope) in items.iter().enumerate() {
        if let Some(call_id) = tool_call_id(&envelope.item) {
            call_index_by_id.entry(call_id.to_string()).or_insert(index);
        }
    }
    if call_index_by_id.is_empty() {
        return;
    }

    // For each output, the index of the call it matches; `None` when the call is missing
    // (orphan removal handles those).
    let mut output_positions: Vec<Option<usize>> = vec![None; items.len()];
    for (index, envelope) in items.iter().enumerate() {
        let Some(call_id) = output_call_id(&envelope.item) else {
            continue;
        };
        if let Some(call_index) = call_index_by_id.get(call_id).copied() {
            output_positions[index] = Some(call_index);
        }
    }

    // The end of the contiguous call run that starts at each call index. A parallel batch is
    // a run of adjacent calls, and every result of the batch belongs after its last call so
    // the wire keeps call A, call B, output A, output B rather than interleaving results.
    let mut batch_end: Vec<usize> = vec![0; items.len()];
    for index in 0..items.len() {
        if tool_call_id(&items[index].item).is_none() {
            continue;
        }
        let mut end = index;
        while end + 1 < items.len() && tool_call_id(&items[end + 1].item).is_some() {
            end += 1;
        }
        batch_end[index] = end;
    }

    // An output must move when user-visible context sits between its call and the output.
    // Assistant text and reasoning stay where they are: the wire converters coalesce them into
    // the assistant message that carries the tool_use, so they do not split the pair.
    let mut move_target: HashMap<usize, usize> = HashMap::new();
    for (index, target) in output_positions.iter().enumerate() {
        let Some(call_index) = *target else {
            continue;
        };
        if index <= call_index {
            continue;
        }
        let leaked_context =
            (call_index + 1..index).any(|slot| splits_tool_pair(&items[slot].item));
        if leaked_context {
            move_target.insert(index, call_index);
        }
    }

    // A duplicate output is dropped whether or not its call survives (orphan removal keeps
    // only one copy, and `ensure_call_outputs_present` counts ids by presence), so track the
    // first kept occurrence for every output.
    let mut dropped_positions: HashSet<usize> = HashSet::new();
    let mut first_seen_outputs: HashMap<String, usize> = HashMap::new();
    let mut movable: Vec<(usize, usize)> = Vec::new();
    for (index, envelope) in items.iter().enumerate() {
        let Some(output_id) = output_call_id(&envelope.item) else {
            continue;
        };
        let output_id = output_id.to_string();
        if first_seen_outputs.insert(output_id, index).is_some() {
            dropped_positions.insert(index);
            continue;
        }
        let Some(call_index) = output_positions[index] else {
            continue;
        };
        if move_target.get(&index) == Some(&call_index) {
            movable.push((index, call_index));
        }
    }
    if movable.is_empty() && dropped_positions.is_empty() {
        return;
    }

    // Group every moved output after its batch's last call, keeping original output order.
    let mut moved_positions: HashSet<usize> = movable.iter().map(|(index, _)| *index).collect();
    let mut outputs_after_call: HashMap<usize, Vec<ResponseItemEnvelope>> = HashMap::new();
    for (index, call_index) in &movable {
        let end = batch_end
            .get(*call_index)
            .copied()
            .filter(|end| *end > *call_index)
            .unwrap_or(*call_index);
        outputs_after_call
            .entry(end)
            .or_default()
            .push(items[*index].clone());
    }

    let original = std::mem::take(items);
    let mut rebuilt: Vec<ResponseItemEnvelope> = Vec::with_capacity(original.len());
    for (index, envelope) in original.into_iter().enumerate() {
        if moved_positions.remove(&index) || dropped_positions.remove(&index) {
            continue;
        }
        let is_call = tool_call_id(&envelope.item).is_some();
        rebuilt.push(envelope);
        if is_call && let Some(outputs) = outputs_after_call.remove(&index) {
            rebuilt.extend(outputs);
        }
    }
    // Defensive: a call block that never matched keeps its outputs at the end rather than
    // dropping them; orphan removal and output synthesis still run afterwards.
    for (_, outputs) in outputs_after_call {
        rebuilt.extend(outputs);
    }
    *items = rebuilt;
}

/// Whether `item` renders as a standalone message on the tool protocols, which requires a
/// recorded output to move ahead of it so the call/result pair stays adjacent.
fn splits_tool_pair(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role != "assistant",
        ResponseItem::AgentMessage { .. } => true,
        _ => false,
    }
}

fn tool_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. }
        | ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => Some(call_id.as_str()),
        _ => None,
    }
}

fn output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    }
}

pub(crate) fn remove_orphan_outputs(items: &mut Vec<ResponseItemEnvelope>) {
    let mut function_call_ids = HashSet::new();
    let mut tool_search_call_ids = HashSet::new();
    let mut custom_tool_call_ids = HashSet::new();
    for envelope in items.iter() {
        match &envelope.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => {
                function_call_ids.insert(call_id.as_str());
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => {
                tool_search_call_ids.insert(call_id.as_str());
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                custom_tool_call_ids.insert(call_id.as_str());
            }
            _ => {}
        }
    }

    let mut orphan_positions = Vec::new();
    for (position, envelope) in items.iter().enumerate() {
        match &envelope.item {
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } if !function_call_ids.contains(call_id.as_str()) => {
                error_or_panic(format!(
                    "Orphan function call output for call id: {call_id}"
                ));
                orphan_positions.push(position);
            }
            ResponseItem::CustomToolCallOutput { call_id, .. }
                if !custom_tool_call_ids.contains(call_id.as_str()) =>
            {
                error_or_panic(format!(
                    "Orphan custom tool call output for call id: {call_id}"
                ));
                orphan_positions.push(position);
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                execution,
                ..
            } if execution != "server" && !tool_search_call_ids.contains(call_id.as_str()) => {
                error_or_panic(format!("Orphan tool search output for call id: {call_id}"));
                orphan_positions.push(position);
            }
            _ => {}
        }
    }

    if !orphan_positions.is_empty() {
        let mut orphan_positions = orphan_positions.into_iter().peekable();
        let mut position = 0;
        items.retain(|_| {
            let retain = orphan_positions.peek() != Some(&position);
            if !retain {
                orphan_positions.next();
            }
            position += 1;
            retain
        });
    }
}

pub(crate) fn remove_corresponding_for(items: &mut Vec<ResponseItemEnvelope>, item: &ResponseItem) {
    match item {
        ResponseItem::FunctionCall { call_id, .. } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::FunctionCallOutput {
                        call_id: Some(existing),
                        ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        } => {
            if let Some(pos) = items.iter().position(|envelope| {
                matches!(&envelope.item, ResponseItem::FunctionCall { call_id: existing, .. } if existing == call_id)
            }) {
                items.remove(pos);
            } else if let Some(pos) = items.iter().position(|envelope| {
                matches!(&envelope.item, ResponseItem::LocalShellCall { call_id: Some(existing), .. } if existing == call_id)
            }) {
                items.remove(pos);
            }
        }
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::ToolSearchOutput {
                        call_id: Some(existing),
                        ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(
                items,
                |i| {
                    matches!(
                        i,
                        ResponseItem::ToolSearchCall {
                            call_id: Some(existing),
                            ..
                        } if existing == call_id
                    )
                },
            );
        }
        ResponseItem::CustomToolCall { call_id, .. } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::CustomToolCallOutput {
                        call_id: existing, ..
                    } if existing == call_id
                )
            });
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            remove_first_matching(
                items,
                |i| matches!(i, ResponseItem::CustomToolCall { call_id: existing, .. } if existing == call_id),
            );
        }
        ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => {
            remove_first_matching(items, |i| {
                matches!(
                    i,
                    ResponseItem::FunctionCallOutput {
                        call_id: Some(existing),
                        ..
                    } if existing == call_id
                )
            });
        }
        _ => {}
    }
}

fn remove_first_matching<F>(items: &mut Vec<ResponseItemEnvelope>, predicate: F)
where
    F: Fn(&ResponseItem) -> bool,
{
    if let Some(pos) = items.iter().position(|envelope| predicate(&envelope.item)) {
        items.remove(pos);
    }
}

/// Strip image content from messages and tool outputs when the model does not support images.
/// When `input_modalities` contains `InputModality::Image`, no stripping is performed.
pub(crate) fn strip_images_when_unsupported(
    input_modalities: &[InputModality],
    items: &mut [ResponseItemEnvelope],
) {
    let supports_images = input_modalities.contains(&InputModality::Image);
    if supports_images {
        return;
    }

    for envelope in items.iter_mut() {
        match &mut envelope.item {
            ResponseItem::Message { .. } => {
                let Some(mut content) = to_annotated_content(&mut envelope.item) else {
                    continue;
                };
                for content_item in &mut content {
                    if matches!(content_item.content(), ContentItem::InputImage { .. }) {
                        *content_item = UnsupportedMedia::IMAGE.render_fragment().into_parts().1;
                    }
                }
                let _ = set_annotated_content(&mut envelope.item, content);
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content_items) = output.content_items_mut() {
                    let mut normalized_content_items = Vec::with_capacity(content_items.len());
                    for content_item in content_items.iter() {
                        match content_item {
                            FunctionCallOutputContentItem::InputImage { .. } => {
                                normalized_content_items.push(
                                    FunctionCallOutputContentItem::InputText {
                                        text: UnsupportedMedia::IMAGE.render(),
                                    },
                                );
                            }
                            _ => normalized_content_items.push(content_item.clone()),
                        }
                    }
                    *content_items = normalized_content_items;
                }
            }
            ResponseItem::ImageGenerationCall { result, .. } => {
                result.clear();
            }
            _ => {}
        }
    }
}

/// Strip audio content from messages and tool outputs when the model does not support audio.
/// When `input_modalities` contains `InputModality::Audio`, no stripping is performed.
pub(crate) fn strip_audio_when_unsupported(
    input_modalities: &[InputModality],
    items: &mut [ResponseItemEnvelope],
) {
    if input_modalities.contains(&InputModality::Audio) {
        return;
    }

    for envelope in items.iter_mut() {
        match &mut envelope.item {
            ResponseItem::Message { .. } => {
                let Some(mut content) = to_annotated_content(&mut envelope.item) else {
                    continue;
                };
                for content_item in &mut content {
                    if matches!(content_item.content(), ContentItem::InputAudio { .. }) {
                        *content_item = UnsupportedMedia::AUDIO.render_fragment().into_parts().1;
                    }
                }
                let _ = set_annotated_content(&mut envelope.item, content);
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content_items) = output.content_items_mut() {
                    for content_item in content_items.iter_mut() {
                        if matches!(
                            content_item,
                            FunctionCallOutputContentItem::InputAudio { .. }
                        ) {
                            *content_item = FunctionCallOutputContentItem::InputText {
                                text: UnsupportedMedia::AUDIO.render(),
                            };
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
