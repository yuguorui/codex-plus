use codex_tools::ToolExecutionEnvironment;

use crate::environment_selection::opaque_executor_id;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::apply_granted_turn_permissions;

pub(super) async fn project_execution_environments(
    invocation: &ToolInvocation,
) -> Vec<ToolExecutionEnvironment> {
    let mut execution_environments = Vec::new();
    for environment in invocation.step_context.environments.turn_environments() {
        let cwd = environment.cwd().clone();
        let additional_permissions = apply_granted_turn_permissions(
            &invocation.step_context,
            environment,
            &cwd,
            SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
        )
        .additional_permissions;
        let file_system_sandbox_context = environment.sandbox_context(additional_permissions);
        let environment_id = environment.selection.environment_id.clone();
        let file_system = environment.environment.get_filesystem();
        let executor_id = opaque_executor_id(&environment.environment);
        execution_environments.push(ToolExecutionEnvironment::new(
            environment_id,
            cwd,
            Some(environment.selection()),
            environment.environment.is_remote(),
            executor_id,
            file_system,
            file_system_sandbox_context,
            environment.environment.clone(),
        ));
    }
    execution_environments
}
