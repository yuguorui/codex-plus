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

/// Summary of malformed tool pairs removed while reconstructing a persisted thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolPairRepairReport {
    pub(crate) dropped_calls: Vec<String>,
    pub(crate) dropped_outputs: Vec<String>,
}

impl ToolPairRepairReport {
    pub(crate) fn is_empty(&self) -> bool {
        self.dropped_calls.is_empty() && self.dropped_outputs.is_empty()
    }

    pub(crate) fn warning_message(&self) -> String {
        let mut parts = Vec::new();
        if !self.dropped_calls.is_empty() {
            parts.push(format!("{} tool call(s)", self.dropped_calls.len()));
        }
        if !self.dropped_outputs.is_empty() {
            parts.push(format!("{} tool output(s)", self.dropped_outputs.len()));
        }
        let ids = self
            .dropped_calls
            .iter()
            .chain(self.dropped_outputs.iter())
            .filter(|id| id.as_str() != "<missing-id>")
            .take(8)
            .cloned()
            .collect::<Vec<_>>();
        let suffix = if ids.is_empty() {
            String::new()
        } else {
            format!(" Affected call ids: {}.", ids.join(", "))
        };
        format!(
            "Repaired malformed tool history while loading this thread: dropped {}. \
             These tool calls/results are not visible to the model.{suffix}",
            parts.join(" and ")
        )
    }
}

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

/// Repair histories where an output is not adjacent to its call batch.
///
/// Sessions written before the tool-pair write barrier existed (and races it could not cover)
/// can contain user context, reasoning, another tool call, or a sibling output between a call
/// and its recorded output. Chat and Anthropic tool protocols require every result to follow
/// its call message immediately, so normalization relocates already-recorded outputs back to
/// their call's block instead of dropping or duplicating them:
/// - the first output of a contiguous call batch seals the assistant message; any other output
///   for that batch moves into the contiguous output block after the first output. This covers
///   partial parallel batches where one sibling output arrives before the others,
/// - a call batch keeps its original call order, and outputs keep their original relative order,
/// - already-adjacent call/output blocks and the context after them are untouched, so unchanged
///   prompt prefixes keep their cache hit,
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

    let mut move_target: HashMap<usize, usize> = HashMap::new();

    // A call batch is sealed by its first output: the converters close the assistant message
    // there. Every other output for that batch must be in the contiguous output block that
    // starts at the first output. Move late outputs into that block. The wire has already lost
    // the original adjacency, so there is no item-type heuristic that can reconstruct it
    // without moving something; this is the minimal, deterministic repair.
    for start in 0..items.len() {
        if tool_call_id(&items[start].item).is_none() {
            continue;
        }
        let end = batch_end[start];
        let batch_call_indices: HashSet<usize> = (start..=end)
            .filter(|index| tool_call_id(&items[*index].item).is_some())
            .collect();
        let output_indices: Vec<usize> = (0..items.len())
            .filter(|index| {
                output_positions[*index]
                    .is_some_and(|call_index| batch_call_indices.contains(&call_index))
            })
            .collect();
        let Some(&first_output) = output_indices.first() else {
            continue;
        };
        // Assistant text and reasoning after the calls stay in the same assistant message.
        let mut assistant_block_end = end;
        while assistant_block_end + 1 < items.len()
            && is_assistant_coalescing_item(&items[assistant_block_end + 1].item)
        {
            assistant_block_end += 1;
        }
        let mut contiguous_end = first_output;
        while contiguous_end + 1 < items.len()
            && output_positions[contiguous_end + 1]
                .is_some_and(|call_index| batch_call_indices.contains(&call_index))
        {
            contiguous_end += 1;
        }
        if first_output > assistant_block_end + 1 {
            // User/developer context, compaction, or another batch split the assistant message
            // from its output. Move the whole output block ahead of the splitter.
            for index in output_indices {
                move_target.insert(index, assistant_block_end);
            }
        } else {
            // Keep the first output block in place and move only late outputs into it.
            for index in output_indices {
                if index > contiguous_end {
                    move_target.insert(index, contiguous_end);
                }
            }
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
        let Some(insert_after) = move_target.get(&index) else {
            continue;
        };
        movable.push((index, *insert_after));
    }
    if movable.is_empty() && dropped_positions.is_empty() {
        return;
    }

    // Group every moved output after the contiguous output block it belongs to, keeping the
    // original relative order of the moved outputs.
    let mut moved_positions: HashSet<usize> = movable.iter().map(|(index, _)| *index).collect();
    let mut outputs_after_block: HashMap<usize, Vec<ResponseItemEnvelope>> = HashMap::new();
    for (index, insert_after) in &movable {
        outputs_after_block
            .entry(*insert_after)
            .or_default()
            .push(items[*index].clone());
    }

    let original = std::mem::take(items);
    let mut rebuilt: Vec<ResponseItemEnvelope> = Vec::with_capacity(original.len());
    for (index, envelope) in original.into_iter().enumerate() {
        if moved_positions.remove(&index) || dropped_positions.remove(&index) {
            continue;
        }
        rebuilt.push(envelope);
        if let Some(outputs) = outputs_after_block.remove(&index) {
            rebuilt.extend(outputs);
        }
    }
    // Defensive: an insertion point that never matched keeps its outputs at the end rather than
    // dropping them; orphan removal and output synthesis still run afterwards.
    for (_, outputs) in outputs_after_block {
        rebuilt.extend(outputs);
    }
    *items = rebuilt;
}

/// Remove malformed tool calls/outputs from a replayed history.
///
/// Replayed history is repaired conservatively: a call/output that cannot form a valid
/// adjacent pair is dropped instead of being moved or synthesized. The returned report is
/// surfaced to the UI so the user knows the model did not see those calls/results.
pub(crate) fn repair_replayed_tool_pairs(
    items: &mut Vec<ResponseItemEnvelope>,
) -> ToolPairRepairReport {
    let mut report = ToolPairRepairReport::default();
    let mut keep = vec![true; items.len()];
    let mut valid_call_indices: HashSet<usize> = HashSet::new();
    let mut valid_outputs: HashSet<usize> = HashSet::new();

    // Find each assistant tool-call block and the contiguous output block that follows it.
    let mut index = 0;
    while index < items.len() {
        if !is_tool_call_item(&items[index].item) {
            index += 1;
            continue;
        }
        let mut block_call_indices = Vec::new();
        let mut cursor = index;
        while cursor < items.len() {
            if is_tool_call_item(&items[cursor].item) {
                block_call_indices.push(cursor);
                cursor += 1;
            } else if matches!(&items[cursor].item, ResponseItem::Message { role, .. } if role == "assistant")
                || matches!(&items[cursor].item, ResponseItem::Reasoning { .. })
            {
                cursor += 1;
            } else {
                break;
            }
        }
        let mut block_output_indices = Vec::new();
        while cursor < items.len() {
            let Some(call_id) = output_call_id(&items[cursor].item) else {
                break;
            };
            if !block_call_indices
                .iter()
                .any(|call_index| tool_call_id(&items[*call_index].item) == Some(call_id))
            {
                break;
            }
            block_output_indices.push(cursor);
            cursor += 1;
        }
        let mut seen_outputs = HashSet::new();
        let mut block_valid_calls = HashSet::new();
        for call_index in block_call_indices {
            let Some(call_id) = tool_call_id(&items[call_index].item) else {
                keep[call_index] = false;
                report.dropped_calls.push("<missing-id>".to_string());
                continue;
            };
            if call_id.is_empty() {
                keep[call_index] = false;
                report.dropped_calls.push("<empty-id>".to_string());
                continue;
            }
            let has_valid_output = block_output_indices
                .iter()
                .any(|output_index| output_call_id(&items[*output_index].item) == Some(call_id));
            if has_valid_output {
                block_valid_calls.insert(call_id.to_string());
                valid_call_indices.insert(call_index);
            } else {
                keep[call_index] = false;
                report.dropped_calls.push(call_id.to_string());
            }
        }
        for output_index in block_output_indices {
            let Some(call_id) = output_call_id(&items[output_index].item) else {
                continue;
            };
            if !block_valid_calls.contains(call_id) {
                keep[output_index] = false;
                report.dropped_outputs.push(call_id.to_string());
            } else if !seen_outputs.insert(call_id.to_string()) {
                keep[output_index] = false;
                report.dropped_outputs.push(call_id.to_string());
            } else {
                valid_outputs.insert(output_index);
            }
        }
        index = cursor.max(index + 1);
    }

    // Drop outputs that are not the first output in a valid block, including late outputs and
    // outputs whose call was dropped.
    for (item_index, envelope) in items.iter().enumerate() {
        if !is_tool_output_item(&envelope.item) || is_standalone_tool_output(&envelope.item) {
            continue;
        }
        if valid_outputs.contains(&item_index) {
            continue;
        }
        keep[item_index] = false;
        if let Some(call_id) = output_call_id(&envelope.item) {
            report.dropped_outputs.push(call_id.to_string());
        } else {
            report.dropped_outputs.push("<missing-id>".to_string());
        }
    }

    // Drop every call that was not paired with an output in its block.
    for (item_index, envelope) in items.iter().enumerate() {
        if !is_tool_call_item(&envelope.item) {
            continue;
        }
        if valid_call_indices.contains(&item_index) {
            continue;
        }
        let call_id = tool_call_id(&envelope.item);
        keep[item_index] = false;
        report.dropped_calls.push(
            call_id
                .filter(|id| !id.is_empty())
                .unwrap_or("<missing-id>")
                .to_string(),
        );
    }

    report.dropped_calls.sort();
    report.dropped_calls.dedup();
    report.dropped_outputs.sort();
    report.dropped_outputs.dedup();
    let mut index = 0;
    items.retain(|_| {
        let retain = keep[index];
        index += 1;
        retain
    });
    report
}

fn is_assistant_coalescing_item(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { role, .. } if role == "assistant")
        || matches!(item, ResponseItem::Reasoning { .. })
}

fn is_tool_call_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::FunctionCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::LocalShellCall { .. }
    )
}

fn is_tool_output_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
    )
}

fn is_standalone_tool_output(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::FunctionCallOutput { call_id: None, .. })
        || matches!(
            item,
            ResponseItem::ToolSearchOutput { execution, .. } if execution == "server"
        )
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
                                normalized_content_items
                                    .push(UnsupportedMedia::IMAGE.render().into());
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
                            *content_item = UnsupportedMedia::AUDIO.render().into();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
