use crate::{
    RetentionDecision,
    preview::{DiscoveryOutcome, RustTargetPreview},
};

pub fn render_rust_target_preview(preview: &RustTargetPreview) -> String {
    let project = format!("{:?}", preview.project_name());
    let target = match preview.outcome() {
        DiscoveryOutcome::Discovered(path) => format!("{path:?}"),
        DiscoveryOutcome::NoTarget | DiscoveryOutcome::Unsupported => "-".to_string(),
    };
    let discovery = match preview.outcome() {
        DiscoveryOutcome::Discovered(_) => "discovered",
        DiscoveryOutcome::NoTarget => "no-target/no-artifacts",
        DiscoveryOutcome::Unsupported => "unsupported/skipped",
    };
    let (decision, reason) = match preview.decision() {
        RetentionDecision::KeepProtected => {
            ("keep-protected", "target overlaps a configured keep path")
        }
        RetentionDecision::KeepPersistent => (
            "keep-persistent",
            "persistent artifacts are retained by this profile",
        ),
        RetentionDecision::SkipUnregistered => ("skip-unregistered", "project is not registered"),
        RetentionDecision::CategoryDisabled => (
            "category-disabled",
            "rust_target is not enabled in this profile",
        ),
        RetentionDecision::Eligible => (
            "retention-eligible",
            "passed retention rules only; not removal authorization",
        ),
    };
    format!(
        "project: {project}\n\
         target: {target}\n\
         discovery: {discovery}\n\
         decision: {decision}\n\
         reason: {reason}\n\
         safety: Cargo metadata --no-deps --offline --locked may be invoked only when the project is registered, rust_target is enabled, and the anticipated target is not already protected; no build/clean requested, no target contents enumerated, no cleanup executed; Cargo docs do not guarantee zero side effects or code execution\n"
    )
}
