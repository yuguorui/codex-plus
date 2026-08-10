use super::*;

impl WorkflowDelegate {
    pub(super) async fn invoke_agent(
        &self,
        mut input: AgentToolInput,
        tool_cancellation: CancellationToken,
    ) -> Result<JsonValue, String> {
        if let Some(label) = &mut input.options.label {
            *label = sanitize_progress_text(label);
        }
        if let Some(phase) = &mut input.options.phase {
            *phase = sanitize_progress_text(phase);
        }
        if let Some(phase_title) = &mut input.phase_title {
            *phase_title = sanitize_progress_text(phase_title);
        }
        let inputs_sha256 =
            workflow_agent_inputs_sha256(input.options.inputs.as_ref()).map_err(agent_error)?;
        if let Some(label) = input.options.label.as_deref() {
            ensure_progress_text_bound("workflow agent label", label).map_err(agent_error)?;
        }
        if let Some(phase) = input.options.phase.as_deref() {
            ensure_progress_text_bound("workflow phase title", phase).map_err(agent_error)?;
        }
        if let Some(phase_title) = input.phase_title.as_deref() {
            ensure_progress_text_bound("workflow phase title", phase_title).map_err(agent_error)?;
        }
        if input
            .options
            .stall_ms
            .is_some_and(|stall_ms| stall_ms > MAX_WORKFLOW_AGENT_STALL_MS)
        {
            return Err(agent_error(
                "choose stallMs within the supported workflow agent timeout range",
            ));
        }
        let journal_key = {
            let mut state = self
                .invocation_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.agent_count = state.agent_count.saturating_add(1);
            input.index = match state.invocation_indices.get(&input.invocation_id) {
                Some(index) => *index,
                None => {
                    let requested = input.index;
                    let index = if state
                        .invocation_indices
                        .values()
                        .any(|index| *index == requested)
                    {
                        let index = state.next_index;
                        state.next_index = state.next_index.saturating_add(1);
                        index
                    } else {
                        state.next_index = state.next_index.max(requested.saturating_add(1));
                        requested
                    };
                    state
                        .invocation_indices
                        .insert(input.invocation_id.clone(), index);
                    index
                }
            };

            workflow_cache_key(
                &self.cache_root,
                &input.invocation_id,
                &input.prompt,
                &input.options,
                input.result_mode,
                inputs_sha256.as_deref(),
            )
        };
        let queued_at = unix_seconds();
        let label = input
            .options
            .label
            .clone()
            .unwrap_or_else(|| format!("agent-{}", input.index + 1));
        let prompt_preview = truncate_utf8(&input.prompt, PROMPT_PREVIEW_BYTES);
        let cached = {
            let _replay_guard = self.replay_gate.lock().await;
            let replay_allowed = {
                let state = self
                    .invocation_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state
                    .rerun_from
                    .is_none_or(|rerun_from| input.index < rerun_from)
                    && !state.replay_diverged
            };
            let journal = self.config.journal.as_ref();
            let cached = match journal {
                Some(journal) if replay_allowed => {
                    journal.replay(&journal_key).await.map_err(|error| {
                        format!("workflow journal replay failed for agent {label}: {error}")
                    })?
                }
                Some(_) | None => None,
            };
            let cached_hit = cached.is_some();
            let cached_replayable = cached
                .as_ref()
                .is_some_and(|cached| cached_outcome_is_replayable(&cached.outcome));
            if replay_allowed
                && !cached_hit
                && let Some(journal) = journal
                && !journal.started(&journal_key).await.map_err(|error| {
                    format!("workflow journal replay failed for agent {label}: {error}")
                })?
            {
                self.mark_replay_diverged();
            }
            if replay_allowed && cached_hit && !cached_replayable {
                // A journaled failure or explicit skip is not a reusable result.
                // Once resume reaches one, later calls must re-execute even if
                // their own keys still match the journal.
                self.mark_replay_diverged();
            }
            cached
        };
        if let Some(cached) = cached.filter(|cached| cached_outcome_is_replayable(&cached.outcome))
        {
            let failure = match &cached.outcome {
                WorkflowAgentOutcome::Success => None,
                WorkflowAgentOutcome::Failure { kind, message } => Some((*kind, message.clone())),
            };
            let progress_state = if failure.is_some() {
                WorkflowAgentState::Error
            } else {
                WorkflowAgentState::Done
            };
            if let Some((kind, message)) = &failure
                && *kind != WorkflowAgentFailureKind::Skipped
            {
                self.failures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!("{label}: {message}"));
            }
            self.emit_agent(
                &input,
                &label,
                progress_state,
                AgentEventDetails {
                    queued_at,
                    started_at: Some(queued_at),
                    agent_id: cached.result.agent_id.clone(),
                    model: cached.result.model.clone(),
                    fallback_model: cached.result.fallback_model.clone(),
                    cached: true,
                    blocked: failure
                        .as_ref()
                        .is_some_and(|(kind, _)| *kind == WorkflowAgentFailureKind::Blocked),
                    skipped: failure
                        .as_ref()
                        .is_some_and(|(kind, _)| *kind == WorkflowAgentFailureKind::Skipped),
                    error: failure.as_ref().map(|(_, message)| message.clone()),
                    result_preview: (failure.is_none()).then(|| preview_json(&cached.result.value)),
                    prompt_preview,
                    ..AgentEventDetails::default()
                },
            )
            .await;
            return match failure {
                None => {
                    self.workflow_tool_result_with_artifact(
                        agent_success_value(input.result_mode, cached.result.value),
                        0,
                    )
                    .await
                }
                Some((kind, message)) => workflow_tool_result(
                    agent_failure_value(input.result_mode, kind, &message),
                    0,
                    None,
                ),
            };
        }
        self.check_budget()?;
        let dispatch_index = self.real_agent_count.fetch_add(1, Ordering::AcqRel);
        if dispatch_index >= self.config.max_agents {
            return Err(self.run_level_error(format!(
                "workflow agent cap exceeded (max {} agents per run)",
                self.config.max_agents
            )));
        }
        self.emit_agent(
            &input,
            &label,
            WorkflowAgentState::Queued,
            AgentEventDetails {
                queued_at,
                prompt_preview: prompt_preview.clone(),
                ..AgentEventDetails::default()
            },
        )
        .await;
        let _concurrency_permit = self.acquire_agent_permit(&tool_cancellation).await?;
        self.check_budget()?;
        if let Some(journal) = self.config.journal.as_ref() {
            journal
                .append_started(journal_key.clone())
                .await
                .map_err(|error| {
                    format!("workflow journal could not durably start agent {label}: {error}")
                })?;
        }

        let started_at = unix_seconds();
        let started = Instant::now();
        let mut attempt = 0_u32;
        let mut stall_retries_used = 0_u32;
        let mut throttle_retried = false;
        let mut agent_usage = WorkflowTokenUsage::default();
        loop {
            self.check_budget()?;
            let attempt_cancellation = CancellationToken::new();
            let action = Arc::new(AtomicUsize::new(AgentAction::None as usize));
            self.control
                .agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    input.index,
                    ActiveAgentControl {
                        action: Arc::clone(&action),
                        cancellation: attempt_cancellation.clone(),
                    },
                );
            self.emit_agent(
                &input,
                &label,
                WorkflowAgentState::Start,
                AgentEventDetails {
                    queued_at,
                    started_at: Some(started_at),
                    attempt,
                    tokens: Some(agent_usage.total_tokens),
                    tool_calls: Some(agent_usage.tool_uses),
                    prompt_preview: prompt_preview.clone(),
                    ..AgentEventDetails::default()
                },
            )
            .await;
            let request = WorkflowAgentRequest {
                index: input.index,
                invocation_id: input.invocation_id.clone(),
                prompt: input.prompt.clone(),
                options: WorkflowAgentOptions {
                    label: Some(label.clone()),
                    ..input.options.clone()
                },
                inputs: input.options.inputs.clone().map(|references| {
                    WorkflowAgentInputs::new(
                        references,
                        Arc::clone(&self.config.input_artifact_store),
                    )
                }),
                attempt,
            };
            let started_agent_id = Arc::new(Mutex::new(None::<String>));
            let started_agent_id_for_callback = Arc::clone(&started_agent_id);
            let previous_tokens = agent_usage.total_tokens;
            let previous_tool_calls = agent_usage.tool_uses;
            let on_started: WorkflowAgentStartedCallback<'_> = Box::new(move |agent_id| {
                *started_agent_id_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(agent_id);
            });
            let progress_input = input.clone();
            let progress_label = label.clone();
            let progress_prompt_preview = prompt_preview.clone();
            let last_tokens = Arc::new(AtomicU64::new(0));
            let last_tool_calls = Arc::new(AtomicU64::new(0));
            let progress_tokens = Arc::clone(&last_tokens);
            let progress_tool_calls = Arc::clone(&last_tool_calls);
            let progress_agent_id = Arc::clone(&started_agent_id);
            let last_activity = Arc::new(Mutex::new(None));
            let progress_activity = Arc::clone(&last_activity);
            let on_progress: WorkflowAgentProgressCallback<'_> = Box::new(move |update| {
                let progress_input = progress_input.clone();
                let progress_label = progress_label.clone();
                let progress_prompt_preview = progress_prompt_preview.clone();
                let progress_tokens = Arc::clone(&progress_tokens);
                let progress_tool_calls = Arc::clone(&progress_tool_calls);
                let progress_agent_id = Arc::clone(&progress_agent_id);
                let progress_activity = Arc::clone(&progress_activity);
                Box::pin(async move {
                    let usage = update.usage;
                    let tokens_changed = progress_tokens
                        .fetch_max(usage.total_tokens, Ordering::AcqRel)
                        < usage.total_tokens;
                    let tool_calls_changed = progress_tool_calls
                        .fetch_max(usage.tool_uses, Ordering::AcqRel)
                        < usage.tool_uses;
                    let activity_changed = {
                        let mut activity = progress_activity
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let changed = *activity != update.activity;
                        *activity = update.activity;
                        changed
                    };
                    if tokens_changed || tool_calls_changed || activity_changed {
                        let total_tokens = progress_tokens.load(Ordering::Acquire);
                        let total_tool_calls = progress_tool_calls.load(Ordering::Acquire);
                        self.emit_agent(
                            &progress_input,
                            &progress_label,
                            WorkflowAgentState::Start,
                            AgentEventDetails {
                                queued_at,
                                started_at: Some(started_at),
                                attempt,
                                agent_id: clone_agent_id(&progress_agent_id),
                                activity: update.activity,
                                tokens: Some(previous_tokens.saturating_add(total_tokens)),
                                tool_calls: Some(
                                    previous_tool_calls.saturating_add(total_tool_calls),
                                ),
                                duration_ms: Some(duration_millis(started.elapsed())),
                                prompt_preview: progress_prompt_preview,
                                ..AgentEventDetails::default()
                            },
                        )
                        .await;
                    }
                })
            });
            let agent = self.agent_runtime.run_agent_with_progress(
                request,
                attempt_cancellation.clone(),
                on_started,
                on_progress,
            );
            tokio::pin!(agent);
            let (result, cancelled_message) = tokio::select! {
                result = &mut agent => (result, None),
                _ = self.control.cancellation.cancelled() => {
                    attempt_cancellation.cancel();
                    (agent.await, Some("workflow cancelled"))
                }
                _ = tool_cancellation.cancelled() => {
                    attempt_cancellation.cancel();
                    (agent.await, Some("workflow agent call cancelled"))
                }
                _ = attempt_cancellation.cancelled() => (agent.await, None),
            };
            let cancelled_message = cancelled_message.or_else(|| {
                if self.control.cancellation.is_cancelled() {
                    Some("workflow cancelled")
                } else if tool_cancellation.is_cancelled() {
                    Some("workflow agent call cancelled")
                } else {
                    None
                }
            });
            self.remove_agent_control(input.index);
            let mut attempt_usage = WorkflowTokenUsage {
                total_tokens: last_tokens.load(Ordering::Acquire),
                tool_uses: last_tool_calls.load(Ordering::Acquire),
            };
            let final_usage = match &result {
                Ok(success) => &success.usage,
                Err(failure) => &failure.usage,
            };
            attempt_usage.total_tokens = attempt_usage.total_tokens.max(final_usage.total_tokens);
            attempt_usage.tool_uses = attempt_usage.tool_uses.max(final_usage.tool_uses);
            self.total_tokens
                .fetch_add(attempt_usage.total_tokens, Ordering::AcqRel);
            self.total_tool_calls
                .fetch_add(attempt_usage.tool_uses, Ordering::AcqRel);
            agent_usage.total_tokens = agent_usage
                .total_tokens
                .saturating_add(attempt_usage.total_tokens);
            agent_usage.tool_uses = agent_usage
                .tool_uses
                .saturating_add(attempt_usage.tool_uses);
            if let Some(message) = cancelled_message {
                return Err(self.run_level_error(message));
            }
            let action = match action.load(Ordering::Acquire) {
                value if value == AgentAction::Skip as usize => AgentAction::Skip,
                value if value == AgentAction::Retry as usize => AgentAction::Retry,
                _ => AgentAction::None,
            };
            if action == AgentAction::Skip {
                let message = "skipped by user";
                let value = agent_failure_value(
                    input.result_mode,
                    WorkflowAgentFailureKind::Skipped,
                    message,
                );
                self.append_failure(
                    &journal_key,
                    WorkflowAgentFailureKind::Skipped,
                    message,
                    &agent_usage,
                )
                .await?;
                self.emit_agent(
                    &input,
                    &label,
                    WorkflowAgentState::Error,
                    AgentEventDetails {
                        queued_at,
                        started_at: Some(started_at),
                        attempt,
                        agent_id: clone_agent_id(&started_agent_id),
                        skipped: true,
                        error: Some(message.to_string()),
                        tokens: Some(agent_usage.total_tokens),
                        tool_calls: Some(agent_usage.tool_uses),
                        duration_ms: Some(duration_millis(started.elapsed())),
                        prompt_preview,
                        ..AgentEventDetails::default()
                    },
                )
                .await;
                return workflow_tool_result(value, agent_usage.total_tokens, None);
            }
            if action == AgentAction::Retry {
                attempt = attempt.saturating_add(1);
                continue;
            }
            match result {
                Ok(result) => {
                    let value = agent_success_value(input.result_mode, result.value.clone());
                    let tool_result = self
                        .workflow_tool_result_with_artifact(value, agent_usage.total_tokens)
                        .await?;
                    if let Some(journal) = self.config.journal.as_ref() {
                        journal
                            .append_result(
                                journal_key.clone(),
                                WorkflowJournalResult {
                                    result: WorkflowAgentResult {
                                        usage: agent_usage.clone(),
                                        ..result.clone()
                                    },
                                    outcome: WorkflowAgentOutcome::Success,
                                },
                            )
                            .await
                            .map_err(|error| {
                                format!(
                                    "workflow journal could not persist result for agent {label}: {error}"
                                )
                            })?;
                    }
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Done,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: result.agent_id.clone(),
                            model: result.model.clone(),
                            fallback_model: result.fallback_model.clone(),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            result_preview: Some(preview_json(&result.value)),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    return Ok(tool_result);
                }
                Err(failure)
                    if failure.kind == WorkflowAgentFailureKind::Stalled
                        && stall_retries_used < self.config.stall_retries =>
                {
                    let backoff = stall_retry_backoff(
                        self.config.stall_retry_base_delay,
                        self.config.stall_retry_max_delay,
                        stall_retries_used,
                    );
                    self.record_log(
                        input.execution_generation,
                        format!(
                            "agent {label} made no progress; retrying in {}s (auto-retry {}/{})",
                            backoff.as_secs(),
                            stall_retries_used + 1,
                            self.config.stall_retries
                        ),
                    )
                    .await;
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.control.cancellation.cancelled() => {
                            return Err(self.run_level_error("workflow cancelled"));
                        }
                        _ = tool_cancellation.cancelled() => {
                            return Err(self.run_level_error("workflow agent call cancelled"));
                        }
                    }
                    stall_retries_used += 1;
                    attempt = attempt.saturating_add(1);
                }
                Err(failure)
                    if failure.kind == WorkflowAgentFailureKind::Throttled && !throttle_retried =>
                {
                    throttle_retried = true;
                    tokio::select! {
                        _ = tokio::time::sleep(self.config.throttle_retry_delay) => {}
                        _ = self.control.cancellation.cancelled() => {
                            return Err(self.run_level_error("workflow cancelled"));
                        }
                        _ = tool_cancellation.cancelled() => {
                            return Err(self.run_level_error("workflow agent call cancelled"));
                        }
                    }
                    attempt = attempt.saturating_add(1);
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Skipped => {
                    let message = if input.result_mode == AgentResultMode::Settled {
                        truncate_utf8(&failure.message, MAX_SETTLED_FAILURE_MESSAGE_BYTES)
                    } else {
                        failure.message
                    };
                    let value = agent_failure_value(
                        input.result_mode,
                        WorkflowAgentFailureKind::Skipped,
                        &message,
                    );
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            skipped: true,
                            error: Some(message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    return workflow_tool_result(value, agent_usage.total_tokens, None);
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::StructuredOutput => {
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            error: Some(failure.message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    let message = if input.result_mode == AgentResultMode::Settled {
                        truncate_utf8(&failure.message, MAX_SETTLED_FAILURE_MESSAGE_BYTES)
                    } else {
                        failure.message
                    };
                    if input.result_mode == AgentResultMode::Settled {
                        self.failures
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("{label}: {message}"));
                        self.append_failure(
                            &journal_key,
                            WorkflowAgentFailureKind::StructuredOutput,
                            &message,
                            &agent_usage,
                        )
                        .await?;
                        let value = agent_failure_value(
                            input.result_mode,
                            WorkflowAgentFailureKind::StructuredOutput,
                            &message,
                        );
                        return workflow_tool_result(value, agent_usage.total_tokens, None);
                    }
                    return Err(format!("{WORKFLOW_AGENT_ERROR_PREFIX}{message}"));
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Stalled => {
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            error: Some(failure.message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    let message = if input.result_mode == AgentResultMode::Settled {
                        truncate_utf8(&failure.message, MAX_SETTLED_FAILURE_MESSAGE_BYTES)
                    } else {
                        failure.message
                    };
                    if input.result_mode == AgentResultMode::Settled {
                        self.failures
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("{label}: {message}"));
                        self.append_failure(
                            &journal_key,
                            WorkflowAgentFailureKind::Stalled,
                            &message,
                            &agent_usage,
                        )
                        .await?;
                        let value = agent_failure_value(
                            input.result_mode,
                            WorkflowAgentFailureKind::Stalled,
                            &message,
                        );
                        return workflow_tool_result(value, agent_usage.total_tokens, None);
                    }
                    return Err(format!("{WORKFLOW_AGENT_ERROR_PREFIX}{message}"));
                }
                Err(failure) if failure.kind == WorkflowAgentFailureKind::Cancelled => {
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            error: Some(failure.message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    return Err(self.run_level_error(failure.message));
                }
                Err(failure) => {
                    let blocked = failure.kind == WorkflowAgentFailureKind::Blocked;
                    let kind = failure.kind;
                    let message = if input.result_mode == AgentResultMode::Settled {
                        truncate_utf8(&failure.message, MAX_SETTLED_FAILURE_MESSAGE_BYTES)
                    } else {
                        failure.message
                    };
                    self.failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(format!("{label}: {message}"));
                    self.emit_agent(
                        &input,
                        &label,
                        WorkflowAgentState::Error,
                        AgentEventDetails {
                            queued_at,
                            started_at: Some(started_at),
                            attempt,
                            agent_id: clone_agent_id(&started_agent_id),
                            blocked,
                            error: Some(message.clone()),
                            tokens: Some(agent_usage.total_tokens),
                            tool_calls: Some(agent_usage.tool_uses),
                            duration_ms: Some(duration_millis(started.elapsed())),
                            prompt_preview,
                            ..AgentEventDetails::default()
                        },
                    )
                    .await;
                    self.append_failure(&journal_key, kind, &message, &agent_usage)
                        .await?;
                    let value = agent_failure_value(input.result_mode, kind, &message);
                    return workflow_tool_result(value, agent_usage.total_tokens, None);
                }
            }
        }
    }

    async fn acquire_agent_permit(
        &self,
        tool_cancellation: &CancellationToken,
    ) -> Result<(OwnedSemaphorePermit, Option<OwnedSemaphorePermit>), String> {
        let local = tokio::select! {
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| "workflow concurrency limiter closed".to_string())?
            }
            _ = self.control.cancellation.cancelled() => {
                return Err(self.run_level_error("workflow cancelled"));
            }
            _ = tool_cancellation.cancelled() => {
                return Err(self.run_level_error("workflow agent call cancelled"));
            }
        };

        let global = if let Some(global) = self.config.global_concurrency.as_ref() {
            tokio::select! {
                permit = global.acquire_owned() => Some(
                    permit.map_err(|_| "global workflow concurrency limiter closed".to_string())?,
                ),
                _ = self.control.cancellation.cancelled() => {
                    return Err(self.run_level_error("workflow cancelled"));
                }
                _ = tool_cancellation.cancelled() => {
                    return Err(self.run_level_error("workflow agent call cancelled"));
                }
            }
        } else {
            None
        };

        Ok((local, global))
    }

    fn remove_agent_control(&self, index: usize) {
        self.control
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&index);
    }

    pub(super) async fn record_log(&self, execution_generation: u64, message: String) {
        let message = truncate_utf8(&sanitize_progress_text(&message), MAX_LOG_MESSAGE_BYTES);
        {
            let mut logs = self
                .logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            logs.push(message.clone());
        }
        self.emit(execution_generation, WorkflowEvent::WorkflowLog { message })
            .await;
    }

    async fn append_failure(
        &self,
        journal_key: &str,
        kind: WorkflowAgentFailureKind,
        message: &str,
        usage: &WorkflowTokenUsage,
    ) -> Result<(), String> {
        let Some(journal) = self.config.journal.as_ref() else {
            return Ok(());
        };
        let failed = WorkflowJournalResult {
            result: WorkflowAgentResult {
                value: JsonValue::Null,
                usage: usage.clone(),
                agent_id: None,
                model: None,
                fallback_model: None,
            },
            outcome: WorkflowAgentOutcome::Failure {
                kind,
                message: message.to_string(),
            },
        };
        journal
            .append_result(journal_key.to_string(), failed)
            .await
            .map_err(|error| format!("workflow journal could not persist agent failure: {error}"))
    }

    pub(super) async fn invoke_child(
        self: &Arc<Self>,
        input: ChildToolInput,
        tool_cancellation: CancellationToken,
        execution_generation: u64,
        invocation_prefix: String,
    ) -> Result<JsonValue, String> {
        let child_permit = tokio::select! {
            permit = Arc::clone(&self.child_semaphore).acquire_owned() => {
                permit.map_err(|_| "workflow child concurrency limiter closed".to_string())?
            }
            _ = self.control.cancellation.cancelled() => {
                return Err(self.run_level_error("workflow cancelled"));
            }
            _ = tool_cancellation.cancelled() => {
                return Err(self.run_level_error("child workflow call cancelled"));
            }
        };
        validate_workflow_input_value(&input.args, "child workflow arguments")?;
        let resolver = self.config.child_resolver.as_ref().ok_or_else(|| {
            "nested workflow resolution is unavailable without a host workflow resolver".to_string()
        })?;
        let result_tool_name = {
            let mut digest = Sha256::new();
            digest.update(invocation_prefix.as_bytes());
            digest.update([0]);
            digest.update(input.invocation_id.as_bytes());
            let digest = format!("{:x}", digest.finalize());
            format!("{RESULT_TOOL_NAME}_{}", &digest[..32])
        };
        let request = WorkflowChildRequest {
            name_or_ref: input.name_or_ref,
            args: input.args,
            args_present: input.args_present,
        };
        let resolved = tokio::select! {
            resolved = resolver.resolve_child(request) => resolved?,
            _ = self.control.cancellation.cancelled() => {
                return Err(self.run_level_error("workflow cancelled"));
            }
            _ = tool_cancellation.cancelled() => {
                return Err(self.run_level_error("child workflow call cancelled"));
            }
        };
        validate_workflow_input_value(&resolved.args, "resolved child workflow arguments")?;
        let (budget_total, budget_initial_spent) =
            workflow_budget_context(&self.config, self.total_tokens.load(Ordering::Acquire));
        let source = compile_workflow_source_with_context(
            &resolved.script,
            &resolved.args,
            WorkflowScriptContext {
                child_mode: true,
                phase_index: input.phase_index,
                phase_title: input.phase_title,
                result_tool_name: Some(result_tool_name.clone()),
                args_present: resolved.args_present,
                budget_total,
                budget_initial_spent,
                ..WorkflowScriptContext::default()
            },
        )
        .map_err(|error| error.to_string())?;
        let result = run_workflow_source(WorkflowSourceExecution {
            source,
            delegate: Arc::clone(self),
            control: Arc::clone(&self.control),
            invocation_cancellation: tool_cancellation,
            synchronous_timeout: self.config.synchronous_timeout,
            result_tool_name,
            allow_child: false,
            rerun_receiver: None,
            execution_generation,
            invocation_prefix: format!("{invocation_prefix}/{}", input.invocation_id),
        })
        .await
        .map_err(|error| {
            let message = execution_error_to_tool_error(error);
            if message.starts_with(WORKFLOW_RUN_ERROR_PREFIX) {
                self.record_run_error(message.clone());
            }
            message
        })?;
        let input_artifact_store = Arc::clone(&self.config.input_artifact_store);
        tokio::spawn(async move {
            let _child_permit = child_permit;
            workflow_tool_result_with_artifact(input_artifact_store, result.value, result.tokens)
                .await
        })
        .await
        .map_err(|error| format!("child workflow result persistence task failed: {error}"))?
    }

    async fn workflow_tool_result_with_artifact(
        &self,
        value: JsonValue,
        tokens: u64,
    ) -> Result<JsonValue, String> {
        workflow_tool_result_with_artifact(
            Arc::clone(&self.config.input_artifact_store),
            value,
            tokens,
        )
        .await
    }

    async fn emit_agent(
        &self,
        input: &AgentToolInput,
        label: &str,
        state: WorkflowAgentState,
        details: AgentEventDetails,
    ) {
        let progress = WorkflowAgentProgress {
            invocation_id: input.invocation_id.clone(),
            index: input.index,
            label: label.to_string(),
            phase_index: input.phase_index,
            phase_title: input.phase_title.clone(),
            agent_id: details.agent_id,
            model: details.model.or_else(|| input.options.model.clone()),
            fallback_model: details.fallback_model,
            isolation: input.options.isolation,
            state,
            activity: details.activity,
            blocked: details.blocked,
            skipped: details.skipped,
            awaiting_decision: details.awaiting_decision,
            cached: details.cached,
            attempt: details.attempt,
            error: details.error.map(|error| {
                truncate_utf8(
                    &sanitize_progress_text(&error),
                    MAX_WORKFLOW_PROGRESS_TEXT_BYTES,
                )
            }),
            tokens: details.tokens,
            tool_calls: details.tool_calls,
            duration_ms: details.duration_ms,
            result_preview: details.result_preview,
            prompt_preview: details.prompt_preview,
            queued_at: details.queued_at,
            started_at: details.started_at,
            last_progress_at: unix_seconds(),
        };
        self.control
            .record_agent(input.execution_generation, progress.clone());
        self.emit(
            input.execution_generation,
            WorkflowEvent::WorkflowAgent(Box::new(progress)),
        )
        .await;
    }
}

/// Returns whether a journaled outcome can stand in for a fresh agent call.
///
/// Only successful calls are deterministic results. Explicit user skips and
/// every runtime failure are journaled as failed and must re-execute on
/// resume, matching Claude's workflow journal semantics.
fn cached_outcome_is_replayable(outcome: &WorkflowAgentOutcome) -> bool {
    matches!(outcome, WorkflowAgentOutcome::Success)
}

fn agent_error(message: impl std::fmt::Display) -> String {
    format!("{WORKFLOW_AGENT_ERROR_PREFIX}{message}")
}

async fn workflow_tool_result_with_artifact(
    input_artifact_store: Arc<dyn WorkflowInputArtifactStore>,
    value: JsonValue,
    tokens: u64,
) -> Result<JsonValue, String> {
    validate_workflow_input_value(&value, "workflow tool result")?;
    let artifact = input_artifact_store.put(value.clone()).await?;
    workflow_tool_result(value, tokens, Some(artifact))
}

fn clone_agent_id(agent_id: &Mutex<Option<String>>) -> Option<String> {
    agent_id
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn agent_success_value(result_mode: AgentResultMode, value: JsonValue) -> JsonValue {
    match result_mode {
        AgentResultMode::Value => value,
        AgentResultMode::Settled => serde_json::json!({
            "status": "fulfilled",
            "value": value,
        }),
    }
}

fn agent_failure_value(
    result_mode: AgentResultMode,
    kind: WorkflowAgentFailureKind,
    message: &str,
) -> JsonValue {
    match result_mode {
        AgentResultMode::Value => JsonValue::Null,
        AgentResultMode::Settled => serde_json::json!({
            "status": "rejected",
            "reason": {
                "kind": failure_kind_name(kind),
                "message": truncate_utf8(message, MAX_SETTLED_FAILURE_MESSAGE_BYTES),
            },
        }),
    }
}

fn failure_kind_name(kind: WorkflowAgentFailureKind) -> &'static str {
    match kind {
        WorkflowAgentFailureKind::Failed => "failed",
        WorkflowAgentFailureKind::TerminalApi => "terminalApi",
        WorkflowAgentFailureKind::Cancelled => "cancelled",
        WorkflowAgentFailureKind::Stalled => "stalled",
        WorkflowAgentFailureKind::Throttled => "throttled",
        WorkflowAgentFailureKind::Blocked => "blocked",
        WorkflowAgentFailureKind::StructuredOutput => "structuredOutput",
        WorkflowAgentFailureKind::Skipped => "skipped",
    }
}
