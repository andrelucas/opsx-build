pub(crate) const GUIDANCE: &str = include_str!("../assets/project-authority.md");
pub(crate) const PLACEHOLDER: &str = "{{OPSX_BUILD_PROJECT_AUTHORITY}}";

pub(crate) const ORDINARY_REVIEW: &str = "This run uses the ordinary context workflow: apply the anti-Karen rule. Report RETRY only for a material defect that prevents delivery or verification of the requested behaviour, stating its concrete consequence and minimum correction. Treat wording, citation bookkeeping, preferred designs and harmless explanatory mistakes as non-blocking observations. Return VERIFIED when required outcomes and checks pass. Preserve actual correctness requirements and required tests; do not invent additional obligations.";

pub(crate) fn render(template: &str) -> String {
    template.replace(PLACEHOLDER, GUIDANCE.trim())
}
