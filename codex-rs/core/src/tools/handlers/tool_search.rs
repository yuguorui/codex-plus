use crate::function_tool::FunctionCallError;
use crate::tools::code_mode::prepare_code_mode_tool_definitions;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::ToolSearchSourceListing;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use bm25::Embedder;
use bm25::EmbedderBuilder;
use bm25::Language;
use bm25::Scorer;
use codex_prompts::ResolvedModelMessages;
use codex_tools::IndirectNamespacePrefixes;
use codex_tools::JsonToolOutput;
use codex_tools::LoadableToolSpec;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::TOOL_SEARCH_DEFAULT_LIMIT;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSearchEntry;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchTransport;
use codex_tools::ToolSpec;
use codex_tools::coalesce_loadable_tool_specs;
use serde::Serialize;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use tracing::instrument;

#[derive(Serialize)]
struct CodeModeSearchResult {
    name: String,
    description: String,
}

pub struct ToolSearchHandler {
    search_infos: Vec<ToolSearchInfo>,
    source_listing: ToolSearchSourceListing,
    spec: ToolSpec,
    search_embedder: Embedder,
    search_scorer: Scorer<usize>,
    activated_tools: Arc<Mutex<BTreeSet<ToolName>>>,
}

#[derive(Default)]
pub(crate) struct ToolSearchHandlerCache {
    cached: Mutex<Option<CachedToolSearchHandler>>,
    /// Tools surfaced by a top-level tool_search call on this thread.
    ///
    /// Providers without native `tool_search` support load discovered tools
    /// client-side: the spec planner re-declares these tools on the next
    /// request so the model can actually call them. The set grows
    /// monotonically for the lifetime of the session, mirroring how a native
    /// provider's server-side loaded-tool state accumulates; resumed and
    /// forked sessions re-derive it from their initial history via
    /// [`Self::recover_activations`].
    activated_tools: Arc<Mutex<BTreeSet<ToolName>>>,
}

struct CachedToolSearchHandler {
    handler: Arc<ToolSearchHandler>,
    sources: Vec<ToolSearchSource>,
}

enum ToolSearchSource {
    Immutable(Weak<dyn CoreToolRuntime>),
    Dynamic(Box<ToolSearchInfo>),
}

impl ToolSearchHandlerCache {
    #[instrument(level = "trace", skip_all)]
    pub(crate) fn get_or_build(
        &self,
        registry: &ToolRegistry,
        source_listing: ToolSearchSourceListing,
    ) -> Arc<ToolSearchHandler> {
        let activated = self.activated_deferred_tools();
        let sources = registry
            .entries()
            .filter_map(|tool| {
                // Activated tools stay in the search corpus after their
                // exposure is upgraded to Direct, so repeating the same query
                // returns the same tools instead of only undiscovered ones.
                if !tool.exposure.is_deferred()
                    && !activated.contains(&tool.runtime.tool_name().with_default_namespace())
                {
                    return None;
                }
                if let Some(runtime) = tool.cached_runtime() {
                    Some(ToolSearchSource::Immutable(Arc::downgrade(runtime)))
                } else {
                    registry
                        .search_info(tool)
                        .map(Box::new)
                        .map(ToolSearchSource::Dynamic)
                }
            })
            .collect::<Vec<_>>();

        {
            let cached = self.cached();
            if let Some(cached) = cached.as_ref()
                && cached.handler.source_listing == source_listing
                && Self::sources_match(&cached.sources, &sources)
            {
                return Arc::clone(&cached.handler);
            }
        }

        let search_infos = sources
            .iter()
            .filter_map(|source| match source {
                ToolSearchSource::Immutable(runtime) => {
                    runtime.upgrade().and_then(|runtime| runtime.search_info())
                }
                ToolSearchSource::Dynamic(search_info) => Some(search_info.as_ref().clone()),
            })
            .collect();

        let handler = Arc::new(ToolSearchHandler::new(
            search_infos,
            source_listing,
            Arc::clone(&self.activated_tools),
        ));
        let mut cached = self.cached();
        if let Some(cached) = cached.as_ref()
            && cached.handler.source_listing == source_listing
            && Self::sources_match(&cached.sources, &sources)
        {
            return Arc::clone(&cached.handler);
        }
        *cached = Some(CachedToolSearchHandler {
            handler: Arc::clone(&handler),
            sources,
        });
        handler
    }

    fn sources_match(cached_sources: &[ToolSearchSource], sources: &[ToolSearchSource]) -> bool {
        cached_sources.len() == sources.len()
            && cached_sources
                .iter()
                .zip(sources)
                .all(|(cached, current)| match (cached, current) {
                    (ToolSearchSource::Immutable(cached), ToolSearchSource::Immutable(current)) => {
                        Weak::ptr_eq(cached, current)
                    }
                    (ToolSearchSource::Dynamic(cached), ToolSearchSource::Dynamic(current)) => {
                        cached == current
                    }
                    (ToolSearchSource::Immutable(_), ToolSearchSource::Dynamic(_))
                    | (ToolSearchSource::Dynamic(_), ToolSearchSource::Immutable(_)) => false,
                })
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, Option<CachedToolSearchHandler>> {
        match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Tools already surfaced to the model by a top-level tool_search call.
    pub(crate) fn activated_deferred_tools(&self) -> BTreeSet<ToolName> {
        match self.activated_tools.lock() {
            Ok(activated) => activated.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Records tools surfaced by a top-level tool_search call.
    ///
    /// Test seam for the production path in
    /// [`ToolSearchHandler::record_activated_tools`], which shares this set
    /// through the handler's `Arc` clone.
    #[cfg(test)]
    pub(crate) fn activate_tools(&self, names: impl IntoIterator<Item = ToolName>) {
        let mut activated = match self.activated_tools.lock() {
            Ok(activated) => activated,
            Err(poisoned) => poisoned.into_inner(),
        };
        activated.extend(names.into_iter().map(|name| name.with_default_namespace()));
    }

    /// Re-derives the activation set from a resumed or forked thread's history.
    ///
    /// The function-backed transport records `tool_search` as a function call
    /// whose output carries the discovered tool specifications as JSON. New
    /// sessions for the same thread start with an empty in-memory set, so
    /// replaying those pairs keeps previously discovered tools callable.
    pub(crate) fn recover_activations<'a>(
        &self,
        items: impl IntoIterator<Item = &'a codex_history::RolloutItem>,
    ) {
        use codex_history::RolloutItem;
        use codex_protocol::models::ResponseItem;

        let mut search_call_ids = std::collections::BTreeSet::new();
        let mut discovered = Vec::<ToolName>::new();
        for item in items {
            let RolloutItem::ResponseItem(envelope) = item else {
                continue;
            };
            match &envelope.item {
                ResponseItem::FunctionCall {
                    name,
                    namespace,
                    call_id,
                    ..
                } if name == TOOL_SEARCH_TOOL_NAME
                    && ToolName::new(namespace.clone(), name.clone())
                        .with_default_namespace()
                        .is_default_namespace() =>
                {
                    search_call_ids.insert(call_id.clone());
                }
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } => {
                    let Some(call_id) = call_id.as_ref() else {
                        continue;
                    };
                    if !search_call_ids.contains(call_id) {
                        continue;
                    }
                    let Some(text) = output.text_content() else {
                        continue;
                    };
                    let Ok(specs) = serde_json::from_str::<serde_json::Value>(text) else {
                        continue;
                    };
                    let Some(specs) = specs.as_array() else {
                        continue;
                    };
                    for spec in specs {
                        discovered.extend(parse_discovered_tool_names(spec));
                    }
                }
                _ => {}
            }
        }
        if discovered.is_empty() {
            return;
        }
        self.activate_tools_for_recovery(discovered);
    }

    fn activate_tools_for_recovery(&self, names: Vec<ToolName>) {
        let mut activated = match self.activated_tools.lock() {
            Ok(activated) => activated,
            Err(poisoned) => poisoned.into_inner(),
        };
        activated.extend(names.into_iter().map(|name| name.with_default_namespace()));
    }
}

/// Extracts tool identities from one serialized [`LoadableToolSpec`] entry.
fn parse_discovered_tool_names(spec: &serde_json::Value) -> impl Iterator<Item = ToolName> + '_ {
    let mut names = Vec::new();
    match spec.get("type").and_then(serde_json::Value::as_str) {
        Some("namespace") => {
            let Some(namespace) = spec.get("name").and_then(serde_json::Value::as_str) else {
                return names.into_iter();
            };
            if let Some(tools) = spec.get("tools").and_then(serde_json::Value::as_array) {
                for tool in tools {
                    if let Some(name) = tool.get("name").and_then(serde_json::Value::as_str) {
                        names.push(ToolName::namespaced(namespace, name));
                    }
                }
            }
        }
        Some("function") | Some("custom") => {
            if let Some(name) = spec.get("name").and_then(serde_json::Value::as_str) {
                names.push(ToolName::plain(name));
            }
        }
        _ => {}
    }
    names.into_iter()
}

impl ToolSearchHandler {
    #[instrument(
        level = "trace",
        skip_all,
        fields(search_info_count = search_infos.len())
    )]
    pub(crate) fn new(
        mut search_infos: Vec<ToolSearchInfo>,
        source_listing: ToolSearchSourceListing,
        activated_tools: Arc<Mutex<BTreeSet<ToolName>>>,
    ) -> Self {
        // BM25 resolves score ties by document id, so index the corpus in a
        // stable name order to keep repeated searches reproducible.
        search_infos.sort_by(|left, right| {
            left.entry
                .tool_names()
                .next()
                .cmp(&right.entry.tool_names().next())
        });
        let search_source_infos = search_infos
            .iter()
            .filter_map(|search_info| search_info.source_info.clone())
            .collect::<Vec<_>>();
        let spec = create_tool_search_tool(
            &search_source_infos,
            TOOL_SEARCH_DEFAULT_LIMIT,
            source_listing,
        );
        let corpus = search_infos
            .iter()
            .map(|search_info| search_info.entry.search_text.as_str())
            .collect::<Vec<_>>();
        let search_embedder =
            EmbedderBuilder::with_fit_to_corpus(Language::English, &corpus).build();
        let mut search_scorer = Scorer::new();
        for (id, text) in corpus.into_iter().enumerate() {
            search_scorer.upsert(&id, search_embedder.embed(text));
        }

        Self {
            search_infos,
            source_listing,
            spec,
            search_embedder,
            search_scorer,
            activated_tools,
        }
    }
}

impl ToolExecutor<ToolInvocation> for ToolSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ToolSearchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            payload,
            source,
            step_context,
            ..
        } = invocation;
        let nested = matches!(source, ToolCallSource::CodeMode { .. });

        let (args, transport) = match payload {
            ToolPayload::ToolSearch {
                arguments,
                transport,
            } => (arguments, transport),
            ToolPayload::Function { arguments } if nested => {
                // Nested Code Mode searches always run client-side and their
                // results return through the code-mode result channel.
                (
                    super::parse_arguments(&arguments)?,
                    ToolSearchTransport::Native,
                )
            }
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{TOOL_SEARCH_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        let query = args.query.trim();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "query must not be empty".to_string(),
            ));
        }
        let limit = args.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT);

        if limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "limit must be greater than zero".to_string(),
            ));
        }

        let model_messages = ResolvedModelMessages::from_model(&step_context.settings.model_info);
        let indirect_prefixes = IndirectNamespacePrefixes::new(
            model_messages.indirect_description_prefixes(),
            step_context.tool_router.mcp_namespaces(),
        )
        .map_err(|error| FunctionCallError::Fatal(error.to_string()))?;

        if nested {
            let tools = self.search_code_mode(
                query,
                limit,
                &step_context.tool_router,
                step_context
                    .turn
                    .config
                    .code_mode
                    .tool_input_schema_max_bytes,
                &indirect_prefixes,
            );
            return Ok(boxed_tool_output(JsonToolOutput::new(serde_json::json!({
                "tools": tools,
            }))));
        }

        if self.search_infos.is_empty() {
            return Ok(self.top_level_output(Vec::new(), transport));
        }

        let matched = self.search_entries(query).take(limit).collect::<Vec<_>>();
        let mut tools = coalesce_loadable_tool_specs(
            matched.into_iter().map(ToolSearchEntry::to_loadable_spec),
        );
        indirect_prefixes.apply_search(&mut tools);

        self.record_activated_tools(loadable_tool_names(&tools));
        Ok(self.top_level_output(tools, transport))
    }

    /// Shapes a top-level search result for the transport that declared
    /// `tool_search` to the model.
    ///
    /// Native transports pair a `tool_search_call` with a `tool_search_output`
    /// item. Function-backed transports declared `tool_search` as a plain
    /// function, so the model's history contains a `function_call` and the
    /// result must travel as the matching `function_call_output` (the same
    /// mapping the chat endpoint applies in
    /// `codex-api/src/endpoint/chat.rs`).
    fn top_level_output(
        &self,
        tools: Vec<LoadableToolSpec>,
        transport: ToolSearchTransport,
    ) -> Box<dyn crate::tools::context::ToolOutput> {
        match transport {
            ToolSearchTransport::Native => boxed_tool_output(ToolSearchOutput { tools }),
            ToolSearchTransport::Function => {
                let tools = tools
                    .into_iter()
                    .map(|tool| {
                        serde_json::to_value(tool).unwrap_or_else(|error| {
                            tracing::error!(
                                error = %error,
                                "failed to serialize tool_search result"
                            );
                            serde_json::json!({})
                        })
                    })
                    .collect::<Vec<_>>();
                boxed_tool_output(JsonToolOutput::new(serde_json::Value::Array(tools)))
            }
        }
    }

    /// Remembers which deferred tools a top-level search surfaced so the spec
    /// planner can re-declare them for providers without native loading.
    fn record_activated_tools<'a>(&self, names: impl Iterator<Item = ToolName> + 'a) {
        let mut activated = match self.activated_tools.lock() {
            Ok(activated) => activated,
            Err(poisoned) => poisoned.into_inner(),
        };
        activated.extend(names.map(|name| name.with_default_namespace()));
    }
}

/// Enumerates the tool identities in finalized search results.
fn loadable_tool_names(specs: &[LoadableToolSpec]) -> impl Iterator<Item = ToolName> + '_ {
    specs.iter().flat_map(|spec| match spec {
        LoadableToolSpec::Function(tool) => vec![ToolName::plain(&tool.name)],
        LoadableToolSpec::Namespace(namespace) => namespace
            .tools
            .iter()
            .map(|tool| {
                ToolName::namespaced(
                    &namespace.name,
                    match tool {
                        ResponsesApiNamespaceTool::Function(tool) => &tool.name,
                        ResponsesApiNamespaceTool::Custom(tool) => &tool.name,
                    },
                )
            })
            .collect::<Vec<_>>(),
    })
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn search_code_mode(
        &self,
        query: &str,
        limit: usize,
        router: &ToolRouter,
        code_mode_input_schema_max_bytes: Option<usize>,
        indirect_prefixes: &IndirectNamespacePrefixes<'_>,
    ) -> Vec<CodeModeSearchResult> {
        let mut tools = Vec::new();
        // Apply the exact step's nested-tool policy before limiting ranked results.
        for entry in self.search_entries(query) {
            for candidate_name in entry.tool_names() {
                let candidate_name = candidate_name.with_default_namespace();
                let name = codex_code_mode::normalize_code_mode_identifier(
                    &codex_tools::code_mode_name_for_tool_name(&candidate_name),
                );
                let Some(tool_name) =
                    router
                        .code_mode_tool_names()
                        .get(&name)
                        .filter(|tool_name| {
                            (*tool_name).clone().with_default_namespace() == candidate_name
                        })
                else {
                    continue;
                };
                let Some(tool) = router.registered_tool(tool_name) else {
                    continue;
                };
                let definitions = prepare_code_mode_tool_definitions(
                    tool.cached_runtime().map(Arc::as_ref),
                    || Cow::Owned(Arc::unwrap_or_clone(tool.spec())),
                    code_mode_input_schema_max_bytes,
                );
                let Some(definition) = definitions.iter().find(|definition| {
                    definition.tool_name.clone().with_default_namespace() == candidate_name
                }) else {
                    continue;
                };
                let mut description = definition.description.clone();
                indirect_prefixes
                    .apply_code_mode_description(&definition.tool_name, &mut description);
                tools.push(CodeModeSearchResult { name, description });
                if tools.len() == limit {
                    return tools;
                }
            }
        }
        tools
    }

    /// Search helper retained for tests; production paths fold ranking and
    /// result collection into a single pass over [`Self::search_entries`].
    #[cfg(test)]
    fn search(&self, query: &str, limit: usize) -> Vec<LoadableToolSpec> {
        coalesce_loadable_tool_specs(
            self.search_entries(query)
                .take(limit)
                .map(ToolSearchEntry::to_loadable_spec),
        )
    }

    fn search_entries(&self, query: &str) -> impl Iterator<Item = &ToolSearchEntry> + '_ {
        let mut matches = self
            .search_scorer
            .matches(&self.search_embedder.embed(query));
        // `bm25::Scorer::matches` gathers candidates through a `HashSet`
        // before sorting by score, so equally scored tools otherwise arrive in
        // a nondeterministic order. The corpus is name-sorted, which makes the
        // document id a stable tie-breaker for repeated identical queries.
        matches.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.id.cmp(&right.id))
        });
        matches
            .into_iter()
            .filter_map(|result| self.search_infos.get(result.id))
            .map(|search_info| &search_info.entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::handlers::DynamicToolHandler;
    use crate::tools::handlers::McpHandler;
    use crate::tools::registry::ToolExposure;
    use codex_mcp::ToolInfo;
    use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
    use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
    use codex_tools::ResponsesApiNamespace;
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::ResponsesApiTool;
    use pretty_assertions::assert_eq;
    use rmcp::model::Tool;
    use std::sync::Arc;

    #[test]
    fn cache_reuses_immutable_handlers_and_rebuilds_for_current_registry_changes() {
        let cache = ToolSearchHandlerCache::default();
        let mut info = tool_info("calendar", "create_event", "Create events");
        info.callable_namespace = "functions".to_string();
        let runtime: Arc<dyn CoreToolRuntime> =
            Arc::new(McpHandler::new(info.clone()).expect("MCP tool should convert"));
        let mut registry = ToolRegistry::default();
        registry.register_trusted_with_exposure(Arc::clone(&runtime), ToolExposure::Deferred);

        let first = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        let second = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        assert!(Arc::ptr_eq(&first, &second));

        let without_sources = cache.get_or_build(&registry, ToolSearchSourceListing::Omit);
        assert!(!Arc::ptr_eq(&first, &without_sources));

        let mut replacement_registry = ToolRegistry::default();
        let replacement =
            Arc::new(McpHandler::new(info).expect("replacement MCP tool should convert"));
        replacement_registry.register_trusted_with_exposure(replacement, ToolExposure::Deferred);
        let replacement = cache.get_or_build(&replacement_registry, ToolSearchSourceListing::Omit);
        assert!(!Arc::ptr_eq(&without_sources, &replacement));

        registry
            .apply_functions_namespace_function_prefixes(
                Some(&std::collections::BTreeMap::from([(
                    "create_event".to_string(),
                    "Guidance.".to_string(),
                )])),
                [ToolName::plain("create_event")],
            )
            .expect("valid function prefixes");
        let prefixed = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        let reused = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        assert!(Arc::ptr_eq(&prefixed, &reused));
        assert_eq!(
            prefixed.search_infos[0].entry.search_text,
            first.search_infos[0].entry.search_text
        );
        let mut expected = first.search("Create events", /*limit*/ 1);
        let LoadableToolSpec::Namespace(namespace) = &mut expected[0] else {
            panic!("namespace")
        };
        let ResponsesApiNamespaceTool::Function(tool) = &mut namespace.tools[0] else {
            panic!("function")
        };
        tool.description = format!("Guidance.\n\n{}", tool.description);
        assert_eq!(prefixed.search("Create events", /*limit*/ 1), expected);
        registry
            .apply_functions_namespace_function_prefixes(/*prefixes*/ None, [])
            .expect("clear prefixes");
        let cleared = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        assert_eq!(
            cleared.search("Create events", /*limit*/ 1),
            first.search("Create events", /*limit*/ 1)
        );

        let mut disabled_registry = ToolRegistry::default();
        disabled_registry.register_trusted_with_exposure(runtime, ToolExposure::Direct);
        let disabled = cache.get_or_build(&disabled_registry, ToolSearchSourceListing::Omit);
        assert!(!Arc::ptr_eq(&replacement, &disabled));
        assert!(disabled.search_infos.is_empty());
    }

    #[test]
    fn top_level_search_bookkeeping_records_activated_tools_in_cache() {
        let cache = ToolSearchHandlerCache::default();
        let runtime: Arc<dyn CoreToolRuntime> = Arc::new(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        );
        let mut registry = ToolRegistry::default();
        registry.register_trusted_with_exposure(runtime, ToolExposure::Deferred);

        let handler = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        assert!(
            cache.activated_deferred_tools().is_empty(),
            "nothing is activated before a search"
        );

        handler.record_activated_tools(
            handler
                .search_entries("Create events")
                .flat_map(|entry| entry.tool_names()),
        );
        let activated = cache.activated_deferred_tools();
        assert!(
            activated.iter().any(|name| name.name == "create_event"),
            "expected create_event in activated set, got {activated:?}"
        );

        // Cache-level activation writes to the same thread-scoped set.
        cache.activate_tools([ToolName::plain("another_tool")]);
        assert!(
            cache
                .activated_deferred_tools()
                .contains(&ToolName::plain("another_tool").with_default_namespace())
        );
    }

    #[test]
    fn identical_queries_return_identical_results_in_name_order() {
        let cache = ToolSearchHandlerCache::default();
        let mut registry = ToolRegistry::default();
        // Reverse registration order: the corpus order must come from names.
        for name in [
            "theta", "zeta", "eta", "epsilon", "delta", "gamma", "beta", "alpha",
        ] {
            let runtime: Arc<dyn CoreToolRuntime> = Arc::new(
                McpHandler::new(tool_info("calendar", name, "Create events"))
                    .expect("MCP tool should convert"),
            );
            registry.register_trusted_with_exposure(runtime, ToolExposure::Deferred);
        }

        let handler = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        let first = handler.search("Create events", 8);
        let second = handler.search("Create events", 8);
        assert_eq!(first, second, "identical queries must be idempotent");
        assert_eq!(
            loadable_names(&first),
            vec![
                "alpha", "beta", "delta", "epsilon", "eta", "gamma", "theta", "zeta"
            ],
            "score ties must break by tool name"
        );
    }

    #[test]
    fn activated_tools_remain_searchable_after_exposure_upgrade() {
        let cache = ToolSearchHandlerCache::default();
        let mut registry = ToolRegistry::default();
        let runtime: Arc<dyn CoreToolRuntime> = Arc::new(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        );
        registry.register_trusted_with_exposure(runtime, ToolExposure::Deferred);

        let before = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        let expected = before.search("Create events", 1);
        assert!(!expected.is_empty());

        cache.activate_tools([ToolName::namespaced("mcp__calendar", "create_event")]);
        for tool in registry.entries_mut() {
            tool.exposure = ToolExposure::Direct;
        }

        let after = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        assert_eq!(
            after.search("Create events", 1),
            expected,
            "activated tools must stay reachable through tool_search"
        );
    }

    fn loadable_names(specs: &[LoadableToolSpec]) -> Vec<&str> {
        specs
            .iter()
            .flat_map(|spec| match spec {
                LoadableToolSpec::Function(tool) => vec![tool.name.as_str()],
                LoadableToolSpec::Namespace(namespace) => namespace
                    .tools
                    .iter()
                    .map(|tool| match tool {
                        ResponsesApiNamespaceTool::Function(tool) => tool.name.as_str(),
                        ResponsesApiNamespaceTool::Custom(tool) => tool.name.as_str(),
                    })
                    .collect::<Vec<_>>(),
            })
            .collect()
    }

    #[test]
    fn top_level_output_pairs_with_the_transport_that_declared_tool_search() {
        use codex_protocol::models::ResponseInputItem;
        use codex_protocol::models::SearchToolCallParams;

        let cache = ToolSearchHandlerCache::default();
        let runtime: Arc<dyn CoreToolRuntime> = Arc::new(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        );
        let mut registry = ToolRegistry::default();
        registry.register_trusted_with_exposure(runtime, ToolExposure::Deferred);
        let handler = cache.get_or_build(&registry, ToolSearchSourceListing::Include);
        let tools = handler.search("Create events", /*limit*/ 1);
        assert!(!tools.is_empty());
        let payload = ToolPayload::ToolSearch {
            arguments: SearchToolCallParams {
                query: "calendar".to_string(),
                limit: None,
            },
            transport: ToolSearchTransport::Native,
        };

        let native_item = handler
            .top_level_output(tools.clone(), ToolSearchTransport::Native)
            .to_response_item("call-1", &payload);
        assert!(
            matches!(native_item, ResponseInputItem::ToolSearchOutput { .. }),
            "native transport keeps tool_search_output, got {native_item:?}"
        );

        let shim_item = handler
            .top_level_output(tools, ToolSearchTransport::Function)
            .to_response_item("call-1", &payload);
        let ResponseInputItem::FunctionCallOutput { call_id, output } = shim_item else {
            panic!("function transport should pair a function_call_output")
        };
        assert_eq!(call_id, "call-1");
        let payload_text = serde_json::to_value(&output)
            .unwrap_or_default()
            .to_string();
        assert!(
            payload_text.contains("create_event"),
            "function output should carry the discovered tool specs: {payload_text}"
        );
    }

    #[test]
    fn recover_activations_replays_function_transport_search_results() {
        use codex_history::ResponseItemEnvelope;
        use codex_history::RolloutItem;
        use codex_protocol::models::FunctionCallOutputPayload;
        use codex_protocol::models::ResponseItem;

        let cache = ToolSearchHandlerCache::default();
        let items = vec![
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                status: None,
                encrypted_content: None,
                id: None,
                name: "tool_search".to_string(),
                namespace: None,
                arguments: r#"{"query":"github repo"}"#.to_string(),
                encrypted_function_args: None,
                call_id: "call-search".to_string(),
                internal_chat_message_metadata_passthrough: None,
            })),
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some("call-search".to_string()),
                    name: Some("tool_search".to_string()),
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text(
                        r#"[{"type":"namespace","name":"mcp__zread","tools":[{"type":"function","name":"get_repo_structure"},{"type":"function","name":"read_file"}]}]"#
                            .to_string(),
                    ),
                    internal_chat_message_metadata_passthrough: None,
                },
            )),
            // An unrelated function output must not activate anything.
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some("call-other".to_string()),
                    name: None,
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text(
                        r#"[{"type":"namespace","name":"mcp__hidden","tools":[{"type":"function","name":"secret"}]}]"#
                            .to_string(),
                    ),
                    internal_chat_message_metadata_passthrough: None,
                },
            )),
        ];

        cache.recover_activations(items.iter());
        let activated = cache.activated_deferred_tools();
        assert!(
            activated.contains(&ToolName::namespaced("mcp__zread", "get_repo_structure")),
            "expected recovered zread tools, got {activated:?}"
        );
        assert!(activated.contains(&ToolName::namespaced("mcp__zread", "read_file")));
        assert!(!activated.contains(&ToolName::namespaced("mcp__hidden", "secret")));
    }

    #[test]
    fn cache_rechecks_dynamic_tool_metadata_while_reusing_immutable_mcp_handlers() {
        let cache = ToolSearchHandlerCache::default();
        let mcp_runtime: Arc<dyn CoreToolRuntime> = Arc::new(
            McpHandler::new(tool_info("calendar", "create_event", "Create events"))
                .expect("MCP tool should convert"),
        );
        let mut dynamic_tool = DynamicToolFunctionSpec {
            name: "lookup".to_string(),
            description: "Search current records".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            defer_loading: true,
        };

        let mut first_registry = ToolRegistry::default();
        first_registry
            .register_trusted_with_exposure(Arc::clone(&mcp_runtime), ToolExposure::Deferred);
        first_registry.register_external_with_exposure(
            Arc::new(DynamicToolHandler::new(&dynamic_tool).expect("dynamic tool should convert")),
            ToolExposure::Deferred,
        );
        let first = cache.get_or_build(&first_registry, ToolSearchSourceListing::Include);

        let mut equivalent_registry = ToolRegistry::default();
        equivalent_registry
            .register_trusted_with_exposure(Arc::clone(&mcp_runtime), ToolExposure::Deferred);
        equivalent_registry.register_external_with_exposure(
            Arc::new(DynamicToolHandler::new(&dynamic_tool).expect("dynamic tool should convert")),
            ToolExposure::Deferred,
        );
        let equivalent = cache.get_or_build(&equivalent_registry, ToolSearchSourceListing::Include);
        assert!(Arc::ptr_eq(&first, &equivalent));

        dynamic_tool.description = "Search refreshed records".to_string();
        let mut refreshed_registry = ToolRegistry::default();
        refreshed_registry.register_trusted_with_exposure(mcp_runtime, ToolExposure::Deferred);
        refreshed_registry.register_external_with_exposure(
            Arc::new(DynamicToolHandler::new(&dynamic_tool).expect("dynamic tool should convert")),
            ToolExposure::Deferred,
        );
        let refreshed = cache.get_or_build(&refreshed_registry, ToolSearchSourceListing::Include);
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert!(
            refreshed
                .search_infos
                .iter()
                .any(|info| info.entry.search_text.contains("refreshed")),
            "refreshed dynamic metadata must reach the rebuilt handler"
        );
    }

    #[test]
    fn mixed_search_results_coalesce_mcp_namespaces() {
        let dynamic_namespace = DynamicToolNamespaceSpec {
            name: "codex_app".to_string(),
            description: "Tools in the codex_app namespace.".to_string(),
            tools: Vec::new(),
        };
        let dynamic_tools = [DynamicToolFunctionSpec {
            name: "automation_update".to_string(),
            description: "Create, update, view, or delete recurring automations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": { "type": "string" },
                },
                "required": ["mode"],
                "additionalProperties": false,
            }),
            defer_loading: true,
        }];
        let mcp_tools = [
            tool_info("calendar", "create_event", "Create events"),
            tool_info("calendar", "list_events", "List events"),
        ];
        let mut search_infos = mcp_tools
            .iter()
            .map(|tool| {
                McpHandler::new(tool.clone())
                    .expect("MCP tool should convert")
                    .search_info()
                    .expect("MCP handler should return search info")
            })
            .collect::<Vec<_>>();
        search_infos.extend(dynamic_tools.iter().map(|tool| {
            DynamicToolHandler::new_in_namespace(&dynamic_namespace, tool)
                .expect("dynamic tool should convert")
                .search_info()
                .expect("dynamic handler should return search info")
        }));
        let handler = ToolSearchHandler::new(
            search_infos,
            ToolSearchSourceListing::Include,
            Arc::new(Mutex::new(BTreeSet::new())),
        );
        let find = |name: &str| {
            handler
                .search_infos
                .iter()
                .map(|info| &info.entry)
                .find(|entry| entry.tool_names().any(|tool| tool.name == name))
                .expect("search entry should exist")
        };
        let results = [
            find("create_event"),
            find("automation_update"),
            find("list_events"),
        ];

        let tools = coalesce_loadable_tool_specs(
            results.into_iter().map(ToolSearchEntry::to_loadable_spec),
        );

        assert_eq!(
            tools,
            vec![
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: "mcp__calendar".to_string(),
                    description: "Tools in the mcp__calendar namespace.".to_string(),
                    tools: vec![
                        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                            name: "create_event".to_string(),
                            description: "Create events desktop tool".to_string(),
                            strict: false,
                            defer_loading: Some(true),
                            parameters: codex_tools::JsonSchema::object(
                                Default::default(),
                                /*required*/ None,
                                Some(false.into()),
                            ),
                            output_schema: None,
                        }),
                        ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                            name: "list_events".to_string(),
                            description: "List events desktop tool".to_string(),
                            strict: false,
                            defer_loading: Some(true),
                            parameters: codex_tools::JsonSchema::object(
                                Default::default(),
                                /*required*/ None,
                                Some(false.into()),
                            ),
                            output_schema: None,
                        }),
                    ],
                }),
                LoadableToolSpec::Namespace(ResponsesApiNamespace {
                    name: "codex_app".to_string(),
                    description: "Tools in the codex_app namespace.".to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                        name: "automation_update".to_string(),
                        description: "Create, update, view, or delete recurring automations."
                            .to_string(),
                        strict: false,
                        defer_loading: Some(true),
                        parameters: codex_tools::JsonSchema::object(
                            std::collections::BTreeMap::from([(
                                "mode".to_string(),
                                codex_tools::JsonSchema::string(/*description*/ None),
                            )]),
                            Some(vec!["mode".to_string()]),
                            Some(false.into()),
                        ),
                        output_schema: None,
                    })],
                }),
            ],
        );
    }

    fn tool_info(server_name: &str, tool_name: &str, description_prefix: &str) -> ToolInfo {
        ToolInfo {
            server_key: codex_mcp::McpServerKey::unscoped(server_name),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: tool_name.to_string(),
            callable_namespace: format!("mcp__{server_name}"),
            namespace_description: None,
            tool: Tool::new(
                tool_name.to_string(),
                format!("{description_prefix} desktop tool"),
                Arc::new(rmcp::model::object(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }))),
            ),
            openai_file_input_optional_fields: Default::default(),
            connector_id: None,
            connector_name: None,
            plugin_display_names: Vec::new(),
        }
    }
}
