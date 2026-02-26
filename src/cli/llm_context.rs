use anyhow::Result;

pub fn run() -> Result<()> {
    let doc = include_str!("../../docs/llm.md");
    print!("{doc}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use crate::cli::Cli;

    /// Ensures every subcommand and every flag in the CLI appears in docs/llm.md.
    /// If this test fails, someone added a flag or subcommand without updating the doc.
    #[test]
    fn llm_doc_covers_all_subcommands_and_flags() {
        let doc = include_str!("../../docs/llm.md");
        let app = Cli::command();

        for sub in app.get_subcommands() {
            let name = sub.get_name();

            // llm-context is hidden and self-referential — skip it
            if name == "llm-context" {
                continue;
            }

            assert!(
                doc.contains(&format!("`esk {name}")) || doc.contains(&format!("esk {name}")),
                "subcommand `esk {name}` not found in docs/llm.md"
            );

            for arg in sub.get_arguments() {
                if arg.get_id() == "help" {
                    continue;
                }

                let long = arg.get_long();
                let is_positional = long.is_none() && arg.get_short().is_none();

                if is_positional {
                    // Positional args should appear as <ARG> or by name in the doc
                    let id = arg.get_id().as_str();
                    let upper = id.to_uppercase();
                    assert!(
                        doc.contains(id) || doc.contains(&upper),
                        "positional arg `{id}` of `esk {name}` not found in docs/llm.md"
                    );
                } else if let Some(flag) = long {
                    assert!(
                        doc.contains(&format!("`--{flag}`"))
                            || doc.contains(&format!("`--{flag} ")),
                        "flag `--{flag}` of `esk {name}` not found in docs/llm.md"
                    );
                }
            }
        }
    }
}
