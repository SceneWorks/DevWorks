use super::*;
use crate::ports::{Preparation, Prepared};
use crate::protocol::records::{McpPrompt, McpResource};
use crate::services::artifacts::observed::{Observed, ObservedFailure};
use std::time::{Duration, Instant};

enum ContentAnswer {
    Resources {
        scope: ConfigScope,
        version: i64,
        catalog_revision: Revision,
        server_id: Id,
        page: Page<McpResource>,
    },
    Prompts {
        scope: ConfigScope,
        version: i64,
        catalog_revision: Revision,
        server_id: Id,
        page: Page<McpPrompt>,
    },
}

impl McpClientService {
    pub(super) fn prepare_content(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let (requested_scope, server_id, catalog_revision, request, resource_uri, prompt_name) =
            match query.input() {
                OperationInput::Mcp(ops::Input::Resources(input)) => (
                    &input.scope,
                    &input.server_id,
                    &input.catalog_revision,
                    &input.page,
                    input.uri.as_ref().map(|uri| uri.0.as_str()),
                    None,
                ),
                OperationInput::Mcp(ops::Input::Prompts(input)) => (
                    &input.scope,
                    &input.server_id,
                    &input.catalog_revision,
                    &input.page,
                    None,
                    input.name.as_ref().map(|name| name.0.as_str()),
                ),
                _ => return Ok(None),
            };
        let native = self.native.get().ok_or_else(|| {
            crate::services::fault(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
                &CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        let (scope, version, server) = env
            .read(|snapshot| {
                if snapshot.stamp().store_id != *ctx.store_id()
                    || snapshot.stamp().restore_epoch != ctx.restore_epoch().get()
                    || snapshot.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                    || !crate::authority::authorization_holds(snapshot, query.authorization())?
                {
                    return Err(StorageError::ScopeDenied);
                }
                let requested_scope = super::requested_scope(requested_scope);
                let scope = active_server_scope(ctx, &requested_scope, snapshot.projects())
                    .map_err(|_| StorageError::ScopeDenied)?;
                let (version, state) = current(snapshot.configuration(), &scope)?;
                let mut server = state
                    .servers
                    .into_iter()
                    .find(|server| &server.public.id == server_id)
                    .ok_or_else(|| StorageError::NotFound {
                        kind: "mcp_server",
                        id: server_id.as_str().into(),
                    })?;
                load_catalog(snapshot.configuration(), &scope, &mut server)?;
                Ok((scope, version, server))
            })
            .map_err(|error| self.fault(error))?;
        if &server.public.catalog_revision != catalog_revision
            || !matches!(server.public.state, ItemState::Healthy)
        {
            return Err(self.fault(StorageError::ScopeDenied));
        }
        let deadline = env
            .deadline_ms
            .map(|until| {
                let now = env.write(|tx| Ok(tx.now_ms()))?;
                Ok::<_, StorageError>(Instant::now().checked_add(Duration::from_millis(
                    until.saturating_sub(now).max(0) as u64,
                )))
            })
            .transpose()
            .map_err(|error| self.fault(error))?
            .flatten();
        if let OperationInput::Mcp(ops::Input::Prompts(input)) = query.input()
            && input.arguments.is_some()
            && prompt_name.is_none()
        {
            return Err(crate::services::fault(
                DomainErrorCode::InvalidArgument,
                RetryAdvice::Never,
                &CorrelationId(self.ids.next_id(IdKind::Correlation)),
            ));
        }
        let mut caps: Vec<_> = server
            .capabilities
            .iter()
            .filter(|cap| {
                if resource_uri.is_some()
                    || matches!(query.input(), OperationInput::Mcp(ops::Input::Resources(_)))
                {
                    cap.kind.0 == "resource"
                        && resource_uri.is_none_or(|uri| {
                            server
                                .resource_uris
                                .get(cap.id.as_str())
                                .is_some_and(|found| found == uri)
                        })
                } else {
                    cap.kind.0 == "prompt" && prompt_name.is_none_or(|name| cap.name.0 == name)
                }
            })
            .cloned()
            .collect();
        caps.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        let mut bounded_page = request.clone();
        bounded_page.limit = Limit::new(request.limit.get().min(32)).expect("bounded MCP page");
        let operation = if matches!(query.input(), OperationInput::Mcp(ops::Input::Resources(_))) {
            "mcp.resources"
        } else {
            "mcp.prompts"
        };
        let binding = list_binding(
            ctx,
            operation,
            &scope,
            &[
                server_id.as_str(),
                catalog_revision.as_str(),
                resource_uri.unwrap_or(""),
                prompt_name.unwrap_or(""),
            ],
            version,
        );
        let cap_page = page(caps, &bounded_page, &binding).map_err(|error| self.fault(error))?;
        let mut published_bytes = 0usize;
        let mut charge = |content: &Value| -> Result<(), DomainFault> {
            let size = serde_json::to_vec(content)
                .map_err(|_| {
                    self.fault(StorageError::InvalidRecord(
                        "MCP content serialization".into(),
                    ))
                })?
                .len();
            published_bytes = published_bytes
                .checked_add(size)
                .filter(|total| *total <= 8 * 1024 * 1024)
                .ok_or_else(|| {
                    crate::services::fault(
                        DomainErrorCode::InvalidArgument,
                        RetryAdvice::Never,
                        &CorrelationId(self.ids.next_id(IdKind::Correlation)),
                    )
                })?;
            Ok(())
        };
        if matches!(query.input(), OperationInput::Mcp(ops::Input::Resources(_))) {
            let mut items = Vec::new();
            for cap in cap_page.items.0 {
                self.content_budget(env, deadline)?;
                let uri = server.resource_uris.get(cap.id.as_str()).ok_or_else(|| {
                    self.fault(StorageError::InvalidRecord(
                        "MCP resource URI missing".into(),
                    ))
                })?;
                let content = self.read_with_reconnect(
                    ctx,
                    query,
                    env,
                    deadline,
                    native,
                    &scope,
                    &server,
                    &cap,
                    |timeout| {
                        native
                            .remote
                            .read_resource_with_timeout(server_id, uri, timeout)
                    },
                )?;
                charge(&content)?;
                self.content_budget(env, deadline)?;
                let reference = self.publish_content(
                    ctx, query, env, deadline, native, &scope, &server, &cap, uri, content,
                )?;
                items.push(McpResource {
                    server_id: server_id.clone(),
                    uri: BoundedText(uri.clone()),
                    content: reference,
                    catalog_revision: catalog_revision.clone(),
                });
            }
            Ok(Some(Prepared::new(ContentAnswer::Resources {
                scope,
                version,
                catalog_revision: catalog_revision.clone(),
                server_id: server_id.clone(),
                page: Page {
                    items: BoundedList(items),
                    next_cursor: cap_page.next_cursor,
                    has_more: cap_page.has_more,
                },
            })))
        } else {
            let mut items = Vec::new();
            let args = match query.input() {
                OperationInput::Mcp(ops::Input::Prompts(input)) => input
                    .arguments
                    .as_ref()
                    .map(|a| a.value.0.clone())
                    .unwrap_or_else(|| json!({})),
                _ => unreachable!(),
            };
            for cap in cap_page.items.0 {
                self.content_budget(env, deadline)?;
                let reference = cap.input_schema.as_ref().ok_or_else(|| {
                    self.fault(StorageError::InvalidRecord(
                        "MCP prompt schema missing".into(),
                    ))
                })?;
                if let OperationInput::Mcp(ops::Input::Prompts(input)) = query.input()
                    && input.arguments.as_ref().is_some_and(|provided| {
                        provided.schema.schema_id != reference.schema_id
                            || provided.schema.revision != reference.revision
                            || provided.schema.sha256 != reference.sha256
                    })
                {
                    return Err(crate::services::fault(
                        DomainErrorCode::SchemaMismatch,
                        RetryAdvice::RefreshAndNewRequest,
                        &CorrelationId(self.ids.next_id(IdKind::Correlation)),
                    ));
                }
                let schema = server
                    .schemas
                    .get(reference.schema_id.as_str())
                    .ok_or_else(|| {
                        self.fault(StorageError::InvalidRecord(
                            "MCP prompt schema missing".into(),
                        ))
                    })?;
                native.validator.validate(schema, &args).map_err(|_| {
                    crate::services::fault(
                        DomainErrorCode::InvalidArgument,
                        RetryAdvice::Never,
                        &CorrelationId(self.ids.next_id(IdKind::Correlation)),
                    )
                })?;
                let content = self.read_with_reconnect(
                    ctx,
                    query,
                    env,
                    deadline,
                    native,
                    &scope,
                    &server,
                    &cap,
                    |timeout| {
                        native.remote.get_prompt_with_timeout(
                            server_id,
                            &cap.name.0,
                            args.clone(),
                            timeout,
                        )
                    },
                )?;
                charge(&content)?;
                self.content_budget(env, deadline)?;
                let reference = self.publish_content(
                    ctx,
                    query,
                    env,
                    deadline,
                    native,
                    &scope,
                    &server,
                    &cap,
                    &cap.name.0,
                    content,
                )?;
                items.push(McpPrompt {
                    server_id: server_id.clone(),
                    name: cap.name,
                    messages: reference,
                    catalog_revision: catalog_revision.clone(),
                });
            }
            Ok(Some(Prepared::new(ContentAnswer::Prompts {
                scope,
                version,
                catalog_revision: catalog_revision.clone(),
                server_id: server_id.clone(),
                page: Page {
                    items: BoundedList(items),
                    next_cursor: cap_page.next_cursor,
                    has_more: cap_page.has_more,
                },
            })))
        }
    }

    fn remote_content_fault(&self, fault: McpRemoteFault) -> DomainFault {
        crate::services::fault(
            match fault {
                McpRemoteFault::MissingCredential => DomainErrorCode::CredentialsRequired,
                McpRemoteFault::InputRequired | McpRemoteFault::InvalidConfiguration => {
                    DomainErrorCode::InvalidArgument
                }
                _ => DomainErrorCode::ExternalFailure,
            },
            RetryAdvice::RefreshAndNewRequest,
            &CorrelationId(self.ids.next_id(IdKind::Correlation)),
        )
    }

    fn content_budget(
        &self,
        env: &Preparation<'_>,
        deadline: Option<Instant>,
    ) -> Result<Duration, DomainFault> {
        env.read(|_| Ok(())).map_err(|error| self.fault(error))?;
        let remaining = deadline.map_or(Duration::from_secs(15), |until| {
            until.saturating_duration_since(Instant::now())
        });
        if remaining.is_zero() {
            return Err(self.fault(StorageError::Cancelled));
        }
        Ok(remaining.min(Duration::from_secs(15)))
    }

    #[allow(clippy::too_many_arguments)]
    fn read_with_reconnect(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
        deadline: Option<Instant>,
        native: &Native,
        scope: &ConfigScope,
        server: &StoredServer,
        cap: &McpCapability,
        read: impl Fn(Duration) -> Result<Value, McpRemoteFault>,
    ) -> Result<Value, DomainFault> {
        env.read(|snapshot| self.content_authorized(snapshot, ctx, query, scope, server, cap))
            .map_err(|error| self.fault(error))?;
        match read(self.content_budget(env, deadline)?) {
            Ok(value) => Ok(value),
            Err(McpRemoteFault::Unreachable) => {
                self.content_budget(env, deadline)?;
                native
                    .remote
                    .connect_with_timeout(
                        ctx,
                        &server.endpoint(scope),
                        self.content_budget(env, deadline)?,
                    )
                    .map_err(|fault| self.remote_content_fault(fault))?;
                env.read(|snapshot| {
                    self.content_authorized(snapshot, ctx, query, scope, server, cap)
                })
                .map_err(|error| self.fault(error))?;
                read(self.content_budget(env, deadline)?)
                    .map_err(|fault| self.remote_content_fault(fault))
            }
            Err(fault) => Err(self.remote_content_fault(fault)),
        }
    }

    fn content_authorized(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        scope: &ConfigScope,
        server: &StoredServer,
        cap: &McpCapability,
    ) -> Result<(), StorageError> {
        let (_, state) = current(snapshot.configuration(), scope)?;
        let binding = match ctx.principal_kind() {
            PrincipalKind::Human | PrincipalKind::Host => None,
            _ => snapshot.authority().principal(ctx.principal())?,
        };
        let policy =
            crate::authority::policy::resolve_policy(snapshot.configuration(), ctx.project_id())?;
        let suffix = cap
            .id
            .as_str()
            .strip_prefix("mcp-")
            .ok_or(StorageError::ScopeDenied)?;
        if !crate::authority::authorization_holds(snapshot, query.authorization())?
            || !matches!(
                crate::authority::policy::outgoing_mcp_tool_visibility(
                    ctx,
                    binding.as_ref(),
                    &policy,
                    &format!("mcp/{suffix}")
                ),
                crate::authority::policy::Verdict::Allow
            )
            || !state.servers.iter().any(|candidate| {
                // Every catalog change advances the catalog revision, so an unchanged revision
                // still holds the catalog `server` was read with.
                candidate.public.id == server.public.id
                    && candidate.public.revision == server.public.revision
                    && candidate.public.catalog_revision == server.public.catalog_revision
            })
            || !server
                .capabilities
                .iter()
                .any(|current| current.id == cap.id)
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_content(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
        deadline: Option<Instant>,
        native: &Native,
        scope: &ConfigScope,
        server: &StoredServer,
        cap: &McpCapability,
        name: &str,
        content: Value,
    ) -> Result<crate::protocol::provider::ArtifactRef, DomainFault> {
        let bytes = serde_json::to_vec(&content).map_err(|_| {
            self.fault(StorageError::InvalidRecord(
                "MCP content serialization".into(),
            ))
        })?;
        let scope_for_write = scope.clone();
        let id_for_write = server.public.id.clone();
        let server_revision = server.public.revision.clone();
        let catalog_revision = server.public.catalog_revision.clone();
        let capability_id = cap.id.clone();
        let cancellation = env.cancellation.cloned();
        let deadline_ms = env.deadline_ms;
        let observed = Observed {
            project: ctx
                .project_id()
                .cloned()
                .or_else(|| scope.project_id().cloned()),
            title: format!("MCP {} {}", cap.kind.0, name),
            media_type: "application/json".into(),
            tags: vec!["mcp".into(), "untrusted".into()],
            source: json!({"kind":"McpContent","serverId":server.public.id,"capabilityId":cap.id,"catalogRevision":catalog_revision,"trust":"Untrusted"}),
            actor_audience: true,
        };
        native
            .artifacts
            .publish_observed(
                &*native.store,
                ctx,
                query.authorization(),
                observed,
                &bytes,
                move |tx, _| {
                    if cancellation
                        .as_ref()
                        .is_some_and(crate::ports::storage::Cancellation::is_cancelled)
                        || deadline.is_some_and(|until| Instant::now() >= until)
                        || deadline_ms.is_some_and(|until| tx.now_ms() >= until)
                    {
                        return Err(StorageError::Cancelled);
                    }
                    let (_, state) = current(tx.configuration(), &scope_for_write)?;
                    let mut current_server = state
                        .servers
                        .into_iter()
                        .find(|candidate| {
                            candidate.public.id == id_for_write
                                && candidate.public.revision == server_revision
                                && candidate.public.catalog_revision == catalog_revision
                        })
                        .ok_or(StorageError::ScopeDenied)?;
                    load_catalog(tx.configuration(), &scope_for_write, &mut current_server)?;
                    if !current_server
                        .capabilities
                        .iter()
                        .any(|candidate| candidate.id == capability_id)
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    Ok(())
                },
            )
            .map(|(reference, ())| reference)
            .map_err(|error| match error {
                ObservedFailure::Refused(_) => self.fault(StorageError::ScopeDenied),
                ObservedFailure::Storage(error) => self.fault(error),
            })
    }

    pub(super) fn content_answer(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<Option<QueryAnswer>, DomainFault> {
        let Some(prepared) = query.prepared::<ContentAnswer>() else {
            return Ok(None);
        };
        let (scope, version, catalog_revision, server_id) = match prepared {
            ContentAnswer::Resources {
                scope,
                version,
                catalog_revision,
                server_id,
                ..
            }
            | ContentAnswer::Prompts {
                scope,
                version,
                catalog_revision,
                server_id,
                ..
            } => (scope, *version, catalog_revision, server_id),
        };
        let (current_version, state) =
            current(snapshot.configuration(), scope).map_err(|error| self.fault(error))?;
        if current_version != version
            || !state.servers.iter().any(|server| {
                &server.public.id == server_id
                    && &server.public.catalog_revision == catalog_revision
            })
        {
            return Err(self.fault(StorageError::ScopeDenied));
        }
        let references: Vec<_> = match prepared {
            ContentAnswer::Resources { page, .. } => {
                page.items.0.iter().map(|item| &item.content).collect()
            }
            ContentAnswer::Prompts { page, .. } => {
                page.items.0.iter().map(|item| &item.messages).collect()
            }
        };
        for reference in references {
            if !snapshot
                .artifacts()
                .version(&reference.version_id)
                .map_err(|error| self.fault(error))?
                .is_some_and(|version| version.live())
            {
                return Err(self.fault(StorageError::ScopeDenied));
            }
        }
        let result = match prepared {
            ContentAnswer::Resources { page, .. } => OperationResult::McpResources {
                value: page.clone(),
            },
            ContentAnswer::Prompts { page, .. } => OperationResult::McpPrompts {
                value: page.clone(),
            },
        };
        Ok(Some(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(version)),
            result,
        }))
    }
}
