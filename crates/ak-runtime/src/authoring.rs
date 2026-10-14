//! Refine and Integrate: building the requests and turning the model's reply
//! into a reviewed proposal.

use ak_core::agent::AgentDefinition;

use crate::error::RuntimeError;
use crate::utility::UtilityRequest;

/// Another agent on the team, as the prompts describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    pub handle: String,
    pub description: String,
}

pub const REFINE_SYSTEM_PROMPT: &str = r#"You are an editor for agent definition files used by AgenticKanban, a kanban board where Claude Code agents work as teammates alongside a human.

A definition file has YAML frontmatter and a markdown body. The frontmatter keys are name, description, model, permission_profile, budgets, and possibly others. You rewrite the body and the description for clarity and structure. You keep everything else exactly as it is.

House skeleton for the body, in this order:
1. A level-one heading: `# @<handle> — <role>`.
2. One identity paragraph: who the agent is, and that it acts only through its worktree and the board tools, as the shared conventions describe.
3. The ownership contract as four bold lines: **You own:**, **You review:**, **You never:**, **Done means:**.
4. A `## Team` section: who to hand off to and consult. Keep what the draft says. If the draft says nothing, leave a one-line placeholder.
5. Further sections only where the draft has material for them: background, non-negotiable conventions, where to look first, a before-you-finish checklist.

House style: short sentences, second person, concrete nouns, no hedging, no filler, no marketing language. Use lists where the content is a list.

Rules:
- Restructure, merge duplicates, and clarify. Do not invent responsibilities, tools, conventions, file paths or teammates that the draft does not state or clearly imply.
- Keep every @handle the draft mentions. Do not add new ones.
- The description is one paragraph: what the agent owns and when to involve it. Other agents read it to decide whom to contact.
- Do not change name, permission_profile, budgets, model, or any frontmatter key you do not recognise.
- Write the description as a YAML block scalar: `description: >-` on its own line, then the text indented by two spaces. Never put a colon-space inside an unquoted single-line value.
- Return the complete file: frontmatter, then body. No commentary before or after. No code fences."#;

pub const REFINE_INSTRUCTION: &str = "Refine the agent definition file supplied as input into the house skeleton and style. Return the complete file, frontmatter included, and nothing else.";

pub fn refine_request(
    draft: &AgentDefinition,
    roster: &[RosterEntry],
    model: Option<String>,
) -> UtilityRequest {
    let others: Vec<String> = roster
        .iter()
        .filter(|r| r.handle != draft.handle())
        .map(|r| format!("- @{}: {}", r.handle, r.description.trim()))
        .collect();
    let roster_text = if others.is_empty() {
        "(no other agents yet)".to_string()
    } else {
        others.join("\n")
    };
    let input = format!(
        "# Other agents on this team\n\nFor reference only. Keep the draft's own mentions; do not add relationships the draft does not already imply.\n\n{roster_text}\n\n# Draft definition file to refine\n\n{}",
        draft.render()
    );
    UtilityRequest {
        system_prompt: REFINE_SYSTEM_PROMPT.to_string(),
        instruction: REFINE_INSTRUCTION.to_string(),
        input,
        model,
        label: format!("refine @{}", draft.handle()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub definition: AgentDefinition,
    pub warnings: Vec<String>,
}

/// Turns the model's reply into a proposal. Strips code fences, parses the
/// file, and restores the frontmatter keys the harness owns, warning when the
/// model had touched them.
pub fn apply_refine(draft: &AgentDefinition, returned: &str) -> Result<Proposal, RuntimeError> {
    let text = strip_fences(returned);
    let mut def = AgentDefinition::parse(text).map_err(|e| {
        RuntimeError::Utility(format!("the reply was not a valid definition file: {e}"))
    })?;
    if def.body.trim().is_empty() {
        return Err(RuntimeError::Utility("the reply had an empty body".into()));
    }

    let mut warnings = Vec::new();
    let draft_fm = &draft.frontmatter;
    let fm = &mut def.frontmatter;
    if fm.name != draft_fm.name {
        warnings.push(format!(
            "handle changed to @{}; kept @{}",
            fm.name, draft_fm.name
        ));
        fm.name = draft_fm.name.clone();
    }
    if fm.permission_profile != draft_fm.permission_profile {
        warnings.push("permission profile changed; kept the original".into());
        fm.permission_profile = draft_fm.permission_profile.clone();
    }
    if fm.budgets != draft_fm.budgets {
        warnings.push("budgets changed; kept the original".into());
        fm.budgets = draft_fm.budgets.clone();
    }
    if fm.model != draft_fm.model {
        warnings.push("model changed; kept the original".into());
        fm.model = draft_fm.model.clone();
    }
    if fm.extra != draft_fm.extra {
        warnings.push("other frontmatter keys changed; kept the originals".into());
        fm.extra = draft_fm.extra.clone();
    }
    if fm.description.trim().is_empty() {
        warnings.push("the description came back empty; kept the original".into());
        fm.description = draft_fm.description.clone();
    }
    // Fence stripping trims the text; keep bodies in the canonical form the
    // renderer produces, which ends with exactly one newline.
    def.body = format!("{}\n", def.body.trim_end());
    Ok(Proposal {
        definition: def,
        warnings,
    })
}

/// Removes one surrounding ```fence``` if present, and any text before it.
fn strip_fences(s: &str) -> &str {
    let trimmed = s.trim();
    let Some(fence_start) = trimmed.find("```") else {
        return trimmed;
    };
    // Only treat it as a fence when the frontmatter is inside it.
    let after = &trimmed[fence_start + 3..];
    let Some(newline) = after.find('\n') else {
        return trimmed;
    };
    let inner = &after[newline + 1..];
    let end = inner.rfind("```").unwrap_or(inner.len());
    let candidate = inner[..end].trim();
    if candidate.starts_with("---") {
        candidate
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ak_core::agent::Budgets;

    fn draft() -> AgentDefinition {
        let mut d = AgentDefinition::template("dba", "Database administrator");
        d.frontmatter.model = Some("sonnet".into());
        d.frontmatter.budgets = Some(Budgets {
            max_usd_per_run: Some(3.0),
            ..Default::default()
        });
        d.frontmatter.extra.insert(
            "tools".into(),
            ak_core::serde_norway::Value::String("Read".into()),
        );
        d
    }

    #[test]
    fn request_includes_roster_and_rendered_draft() {
        let d = draft();
        let roster = vec![
            RosterEntry {
                handle: "backend".into(),
                description: "Owns the API".into(),
            },
            RosterEntry {
                handle: "dba".into(),
                description: "self, must be excluded".into(),
            },
        ];
        let req = refine_request(&d, &roster, Some("opus".into()));
        assert!(req.input.contains("- @backend: Owns the API"));
        assert!(!req.input.contains("self, must be excluded"));
        assert!(req.input.contains("name: dba"));
        assert_eq!(req.model.as_deref(), Some("opus"));
        assert!(req.system_prompt.contains("**You own:**"));
    }

    #[test]
    fn apply_strips_fences_and_restores_owned_keys() {
        let d = draft();
        let reply = "Here you go:\n```markdown\n---\nname: database\ndescription: Owns the schema. Involve for migrations.\nmodel: opus\npermission_profile: trusted\n---\n\n# @dba — database administrator\n\n**You own:** schema\n```\n";
        let p = apply_refine(&d, reply).unwrap();
        let fm = &p.definition.frontmatter;
        assert_eq!(fm.name, "dba");
        assert_eq!(fm.permission_profile, "supervised");
        assert_eq!(fm.model.as_deref(), Some("sonnet"));
        assert_eq!(fm.budgets, d.frontmatter.budgets);
        assert!(fm.extra.contains_key("tools"));
        assert_eq!(fm.description, "Owns the schema. Involve for migrations.");
        assert!(p.definition.body.contains("**You own:** schema"));
        assert_eq!(p.warnings.len(), 5, "{:?}", p.warnings);
    }

    #[test]
    fn apply_rejects_non_definitions_and_empty_bodies() {
        let d = draft();
        assert!(apply_refine(&d, "Sorry, I cannot do that.").is_err());
        assert!(apply_refine(&d, "---\nname: dba\n---\n\n   \n").is_err());
    }

    #[test]
    fn clean_reply_has_no_warnings() {
        let d = draft();
        let p = apply_refine(&d, &d.render()).unwrap();
        assert!(p.warnings.is_empty());
        assert_eq!(p.definition, d);
    }
}
