pub(crate) const GUIDANCE: &str = include_str!("../assets/project-authority.md");
pub(crate) const PLACEHOLDER: &str = "{{OPSX_BUILD_PROJECT_AUTHORITY}}";

pub(crate) fn render(template: &str) -> String {
    template.replace(PLACEHOLDER, GUIDANCE.trim())
}
