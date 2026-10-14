//! Refine and Integrate: building the requests and turning the model's reply
//! into a reviewed proposal.

use std::collections::BTreeMap;

use ak_core::agent::AgentDefinition;

use crate::error::RuntimeError;
use crate::utility::UtilityRequest;

/// Another agent on the team, as the Refine prompt describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    pub handle: String,
    pub description: String,
}

/// An agent on the team with its ownership contract, as the Integrate
/// prompt describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamMember {
    pub handle: String,
    pub description: String,
    pub ownership: Vec<String>,
}

impl TeamMember {
    pub fn from_definition(def: &AgentDefinition) -> Self {
        Self {
            handle: def.handle().to_string(),
            description: def.frontmatter.description.clone(),
            ownership: ownership_lines(&def.body),
        }
    }
}

/// The four bold contract lines of a body, in order, if present.
pub fn ownership_lines(body: &str) -> Vec<String> {
    const MARKERS: [&str; 4] = [
        "**You own:**",
        "**You review:**",
        "**You never:**",
        "**Done means:**",
    ];
    body.lines()
        .map(str::trim)
        .filter(|l| MARKERS.iter().any(|m| l.starts_with(m)))
        .map(str::to_string)
        .collect()
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

pub const INTEGRATE_SYSTEM_PROMPT: &str = r#"You are an editor for agent definition files used by AgenticKanban, a kanban board where Claude Code agents work as teammates alongside a human.

A definition file has YAML frontmatter and a markdown body. The body follows a house skeleton: a level-one heading `# @<handle> — <role>`, an identity paragraph, the ownership contract as four bold lines (**You own:**, **You review:**, **You never:**, **Done means:**), a `## Team` section, then free-form sections.

Your job is Integrate. One agent on the team (the changed agent) has been created or changed. You update one target agent's definition so it knows how to work with the changed agent.

Rules:
- Change only what the changed agent's existence requires. Prefer the `## Team` section: add or rewrite the entry for the changed agent there. An entry says what to hand off to them, when to consult them before touching shared ground, and what done means for the pair. Keep the other entries unless they are now wrong.
- If the `## Team` section is missing or a placeholder, create it with the entry.
- Touch the ownership contract only to resolve a real overlap with the changed agent, and keep the change to the line in question.
- Leave the description alone unless an overlap makes it wrong.
- Do not invent responsibilities, tools, conventions or file paths that neither definition states.
- Do not change name, permission_profile, budgets, model, or any frontmatter key you do not recognise.
- House style: short sentences, second person, concrete nouns, no hedging, no filler.
- If you change the description, write it as a YAML block scalar: `description: >-` on its own line, then the text indented by two spaces. Never put a colon-space inside an unquoted single-line value.
- Return the complete target file: frontmatter, then body. No commentary before or after. No code fences."#;

pub const INTEGRATE_INSTRUCTION: &str = "Update the target agent definition supplied as input so it works well with the changed agent. Return the complete target file, frontmatter included, and nothing else.";

pub const INTEGRATE_SELF_INSTRUCTION: &str = "The target is the changed agent itself. Write or rewrite its `## Team` section from the roster supplied as input, one entry per teammate, and change nothing else. Return the complete file, frontmatter included, and nothing else.";

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

/// One Integrate call: the roster, the changed agent, and the target to
/// update. When the target is the changed agent itself, the call writes its
/// `## Team` section from the roster instead.
pub fn integrate_request(
    changed: &AgentDefinition,
    target: &AgentDefinition,
    team: &[TeamMember],
    model: Option<String>,
) -> UtilityRequest {
    let is_self = target.handle() == changed.handle();
    let mut roster = String::new();
    for m in team.iter().filter(|m| m.handle != target.handle()) {
        roster.push_str(&format!("## @{}\n\n{}\n", m.handle, m.description.trim()));
        for line in &m.ownership {
            roster.push_str(&format!("{line}\n"));
        }
        roster.push('\n');
    }
    if roster.is_empty() {
        roster.push_str("(no other agents)\n\n");
    }
    let input = if is_self {
        format!(
            "# Team roster: the teammates of @{handle}\n\nEach teammate's description and ownership lines.\n\n{roster}# The target to update: @{handle}\n\nThis is the changed agent itself. Rewrite its `## Team` section from the roster and change nothing else.\n\n{file}",
            handle = changed.handle(),
            file = changed.render()
        )
    } else {
        format!(
            "# Team roster\n\nEvery other agent's description and ownership lines, for context.\n\n{roster}# The changed agent: @{changed_handle}\n\n{changed_file}\n# The target to update: @{target_handle}\n\n{target_file}",
            changed_handle = changed.handle(),
            changed_file = changed.render(),
            target_handle = target.handle(),
            target_file = target.render()
        )
    };
    UtilityRequest {
        system_prompt: INTEGRATE_SYSTEM_PROMPT.to_string(),
        instruction: if is_self {
            INTEGRATE_SELF_INSTRUCTION.to_string()
        } else {
            INTEGRATE_INSTRUCTION.to_string()
        },
        input,
        model,
        label: format!("integrate @{} into @{}", changed.handle(), target.handle()),
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
    apply_returned(draft, returned)
}

/// Like `apply_refine` against the target, plus a warning for every part of
/// the file outside the `## Team` section that the proposal changes. The
/// human decides; nothing is discarded.
pub fn apply_integrate(target: &AgentDefinition, returned: &str) -> Result<Proposal, RuntimeError> {
    let mut proposal = apply_returned(target, returned)?;
    if proposal.definition.frontmatter.description.trim() != target.frontmatter.description.trim() {
        proposal.warnings.push("changes the description".into());
    }
    let before = sections(&target.body);
    let after = sections(&proposal.definition.body);
    let mut touched: Vec<String> = Vec::new();
    for (title, content) in &after {
        if title == "Team" {
            continue;
        }
        match before.get(title) {
            Some(old) if old.trim() == content.trim() => {}
            Some(_) => touched.push(describe_section(title)),
            None => touched.push(format!("adds section {title}")),
        }
    }
    for title in before.keys() {
        if title != "Team" && !after.contains_key(title) {
            touched.push(format!("removes section {title}"));
        }
    }
    if !touched.is_empty() {
        proposal
            .warnings
            .push(format!("beyond the Team section: {}", touched.join(", ")));
    }
    Ok(proposal)
}

fn describe_section(title: &str) -> String {
    if title.is_empty() {
        "changes the identity or ownership contract".to_string()
    } else {
        format!("changes section {title}")
    }
}

/// The body split at its `## ` headings. The preamble (heading, identity,
/// ownership contract) has the empty title.
fn sections(body: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut title = String::new();
    let mut content = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            out.entry(title.clone())
                .and_modify(|c| c.push_str(&content))
                .or_insert_with(|| content.clone());
            title = rest.trim().to_string();
            content = String::new();
        } else {
            content.push_str(line);
            content.push('\n');
        }
    }
    out.entry(title)
        .and_modify(|c| c.push_str(&content))
        .or_insert(content);
    out
}

fn apply_returned(base: &AgentDefinition, returned: &str) -> Result<Proposal, RuntimeError> {
    let text = strip_fences(returned);
    let mut def = AgentDefinition::parse(text).map_err(|e| {
        RuntimeError::Utility(format!("the reply was not a valid definition file: {e}"))
    })?;
    if def.body.trim().is_empty() {
        return Err(RuntimeError::Utility("the reply had an empty body".into()));
    }

    let mut warnings = Vec::new();
    let base_fm = &base.frontmatter;
    let fm = &mut def.frontmatter;
    if fm.name != base_fm.name {
        warnings.push(format!(
            "handle changed to @{}; kept @{}",
            fm.name, base_fm.name
        ));
        fm.name = base_fm.name.clone();
    }
    if fm.permission_profile != base_fm.permission_profile {
        warnings.push("permission profile changed; kept the original".into());
        fm.permission_profile = base_fm.permission_profile.clone();
    }
    if fm.budgets != base_fm.budgets {
        warnings.push("budgets changed; kept the original".into());
        fm.budgets = base_fm.budgets.clone();
    }
    if fm.model != base_fm.model {
        warnings.push("model changed; kept the original".into());
        fm.model = base_fm.model.clone();
    }
    if fm.extra != base_fm.extra {
        warnings.push("other frontmatter keys changed; kept the originals".into());
        fm.extra = base_fm.extra.clone();
    }
    if fm.description.trim().is_empty() {
        warnings.push("the description came back empty; kept the original".into());
        fm.description = base_fm.description.clone();
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

    fn backend() -> AgentDefinition {
        let mut d = AgentDefinition::template("backend", "Backend developer");
        d.frontmatter.description = "Owns the API and application services.".into();
        d.body = "# @backend — backend developer\n\nYou are **@backend**.\n\n**You own:** `src/api/`\n**You review:** API changes\n**You never:** touch the schema\n**Done means:** tests pass\n\n## Team\n\nNobody yet.\n\n## Background\n\nAxum service.\n".into();
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

    #[test]
    fn ownership_lines_are_the_contract() {
        let lines = ownership_lines(&backend().body);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "**You own:** `src/api/`");
        assert_eq!(lines[3], "**Done means:** tests pass");
        assert!(ownership_lines("nothing here").is_empty());
    }

    #[test]
    fn integrate_request_carries_roster_and_both_files() {
        let changed = draft();
        let target = backend();
        let team = vec![
            TeamMember::from_definition(&changed),
            TeamMember::from_definition(&target),
            TeamMember {
                handle: "qa".into(),
                description: "Tests everything.".into(),
                ownership: vec!["**You own:** tests".into()],
            },
        ];
        let req = integrate_request(&changed, &target, &team, None);
        assert_eq!(req.instruction, INTEGRATE_INSTRUCTION);
        assert!(req
            .input
            .contains("## @qa\n\nTests everything.\n**You own:** tests"));
        assert!(req.input.contains("# The changed agent: @dba"));
        assert!(req.input.contains("# The target to update: @backend"));
        assert!(req.input.contains("name: backend"));
        // The target is not listed as its own teammate.
        assert!(!req.input.contains("## @backend"));
        assert_eq!(req.label, "integrate @dba into @backend");

        let me = integrate_request(&changed, &changed, &team, Some("opus".into()));
        assert_eq!(me.instruction, INTEGRATE_SELF_INSTRUCTION);
        assert!(me.input.contains("## @backend"));
        assert!(!me.input.contains("## @dba"));
        assert!(me.input.contains("This is the changed agent itself"));
        assert_eq!(me.model.as_deref(), Some("opus"));
    }

    #[test]
    fn integrate_warns_beyond_the_team_section() {
        let target = backend();
        let mut reply = target.clone();
        reply.body = reply.body.replace(
            "Nobody yet.",
            "- **@dba** owns the schema. Hand migrations to them.",
        );
        let p = apply_integrate(&target, &reply.render()).unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert!(p.definition.body.contains("@dba"));

        let mut reply = target.clone();
        reply.body = reply
            .body
            .replace("Axum service.", "Axum service. Ask @dba.")
            .replace(
                "**You never:** touch the schema",
                "**You never:** touch migrations",
            )
            + "\n## Checklist\n\n- run tests\n";
        reply.frontmatter.description = "Owns the API. Not the schema.".into();
        let p = apply_integrate(&target, &reply.render()).unwrap();
        let joined = p.warnings.join(" | ");
        assert!(joined.contains("changes the description"), "{joined}");
        assert!(
            joined.contains("identity or ownership contract"),
            "{joined}"
        );
        assert!(joined.contains("changes section Background"), "{joined}");
        assert!(joined.contains("adds section Checklist"), "{joined}");

        let mut reply = target.clone();
        reply.body = reply.body.replace("## Background\n\nAxum service.\n", "");
        let p = apply_integrate(&target, &reply.render()).unwrap();
        assert!(p.warnings.join(" ").contains("removes section Background"));
    }
}
