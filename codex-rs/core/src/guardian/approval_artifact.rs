#[cfg(test)]
pub(crate) use codex_guardian_reviewer::GUARDIAN_APPROVAL_ARTIFACT_PAGE_BYTES;
pub(crate) use codex_guardian_reviewer::GuardianApprovalArtifact;

#[cfg(test)]
#[path = "approval_artifact_tests.rs"]
mod tests;
