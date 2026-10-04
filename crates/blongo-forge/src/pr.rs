//! Asking the thread's agent to draft a pull request, and reading the
//! draft out of its answer.

use blongo_protocol::forge::PrDraft;

/// Longest answer searched for the draft.
const MAX_ANSWER: usize = 64 * 1024;

/// What the draft prompt tells the agent about the branch.
pub struct DraftFacts<'a> {
    pub branch: &'a str,
    pub base: &'a str,
    pub commits: &'a [String],
    pub uncommitted: &'a [String],
    pub diff_stat: &'a str,
    pub template: Option<&'a str>,
}

/// The message sent to the agent: what is on the branch and the JSON
/// shape to answer with.
pub fn draft_prompt(facts: &DraftFacts) -> String {
    let mut p = String::from(
        "Draft a pull request for the work in this thread. Do not change any files, commit or \
         push; only answer.\n\n",
    );
    p.push_str(&format!("Branch {} into {}.\n", facts.branch, facts.base));
    if !facts.commits.is_empty() {
        p.push_str("\nCommits:\n");
        for c in facts.commits {
            p.push_str(&format!("- {c}\n"));
        }
    }
    if !facts.uncommitted.is_empty() {
        p.push_str(&format!(
            "\n{} uncommitted files will be committed with the pull request: {}\n",
            facts.uncommitted.len(),
            facts
                .uncommitted
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !facts.diff_stat.is_empty() {
        p.push_str(&format!(
            "\nChanged files:\n```\n{}\n```\n",
            facts.diff_stat
        ));
    }
    if let Some(t) = facts.template {
        p.push_str(&format!(
            "\nThe repository's pull request template (follow its headings):\n```markdown\n{}\n```\n",
            t.trim()
        ));
    }
    p.push_str(
        "\nWrite in the language this conversation uses. Look at the changes as needed. Answer \
         with only this JSON in a ```json block:\n```json\n{\"title\": \"one line, under 72 \
         characters\", \"body\": \"markdown description: what changes and why, how it was \
         tested\", \"commit_message\": \"a message for the uncommitted changes, or null\"}\n```\n",
    );
    p
}

/// The draft in an agent's answer: the last ```json block that holds
/// one, else the last JSON object with a `title`. Fields are cut to what
/// GitHub accepts.
pub fn parse_draft(answer: &str) -> Option<PrDraft> {
    let mut end = answer.len().min(MAX_ANSWER);
    while !answer.is_char_boundary(end) {
        end -= 1;
    }
    let text = &answer[..end];
    let fenced = text
        .rmatch_indices("```json")
        .find_map(|(i, _)| object_in(&text[i + "```json".len()..]));
    let draft = fenced.or_else(|| object_in(text))?;
    let title: String = draft
        .title
        .trim()
        .replace(['\n', '\r'], " ")
        .chars()
        .take(256)
        .collect();
    if title.is_empty() {
        return None;
    }
    Some(PrDraft {
        title,
        body: draft.body.trim().chars().take(65_536).collect(),
        commit_message: draft
            .commit_message
            .map(|m| m.trim().chars().take(4_000).collect::<String>())
            .filter(|m| !m.is_empty()),
    })
}

/// The last `{…}` in `text` that reads as a draft.
fn object_in(text: &str) -> Option<PrDraft> {
    text.match_indices('{').rev().find_map(|(i, _)| {
        serde_json::Deserializer::from_str(&text[i..])
            .into_iter::<PrDraft>()
            .next()?
            .ok()
            .filter(|d| !d.title.trim().is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_drafts() {
        let d = parse_draft(
            "Here it is:\n```json\n{\"title\": \"Fix IME\\ncursor\", \"body\": \"Why {x}\", \
             \"commit_message\": null}\n```\nDone.",
        )
        .unwrap();
        assert_eq!(d.title, "Fix IME cursor");
        assert_eq!(d.body, "Why {x}");
        assert_eq!(d.commit_message, None);
        // The last block wins; a bare object works too.
        let d = parse_draft(
            "```json\n{\"title\": \"old\"}\n```\n```json\n{\"title\": \"new\", \"body\": \"b\", \
             \"commit_message\": \"msg\"}\n```",
        )
        .unwrap();
        assert_eq!(
            (d.title.as_str(), d.commit_message.as_deref()),
            ("new", Some("msg"))
        );
        let d = parse_draft("ok {\"title\": \"T\", \"body\": \"{\\\"a\\\":1}\"} bye").unwrap();
        assert_eq!(d.title, "T");
        assert!(parse_draft("no json here").is_none());
        assert!(parse_draft("{\"title\": \"  \"}").is_none());
        assert!(
            parse_draft(&format!("{{\"title\": \"{}\"}}", "x".repeat(300)))
                .unwrap()
                .title
                .len()
                == 256
        );
    }

    #[test]
    fn prompt_carries_the_facts() {
        let p = draft_prompt(&DraftFacts {
            branch: "blongo/x",
            base: "main",
            commits: &["Add parser".into()],
            uncommitted: &["a.rs".into()],
            diff_stat: " a.rs | 2 +-",
            template: Some("## Why"),
        });
        for want in [
            "blongo/x into main",
            "- Add parser",
            "a.rs",
            "## Why",
            "```json",
        ] {
            assert!(p.contains(want), "{want}");
        }
        // The example in the prompt is itself a valid draft (an agent that
        // echoes it still answers).
        assert!(parse_draft(&p).is_some());
    }
}
