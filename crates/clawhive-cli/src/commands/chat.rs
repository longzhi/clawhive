use std::io::Write;
use std::path::Path;

use anyhow::Result;
use clawhive_core::SecurityMode;
use clawhive_schema::InboundMessage;

use crate::runtime::bootstrap::{bootstrap, resolve_security_override};

pub(crate) async fn run(
    root: &Path,
    agent: String,
    security: Option<SecurityMode>,
    no_security: bool,
) -> Result<()> {
    let security_override = resolve_security_override(security, no_security);
    run_repl(root, &agent, security_override).await
}

async fn run_repl(
    root: &Path,
    _agent_id: &str,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    let (_bus, _memory, gateway, _config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, security_override).await?;

    println!("clawhive REPL. Type 'quit' to exit.");
    println!("---");

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        stdin.read_line(&mut input)?;
        let input = input.trim();
        if input == "quit" || input == "exit" {
            break;
        }
        if input.is_empty() {
            continue;
        }

        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "repl".into(),
            connector_id: "repl".into(),
            conversation_scope: "repl:0".into(),
            user_scope: "user:local".into(),
            text: input.to_string(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            message_source: None,
        };

        match gateway.handle_inbound(inbound).await {
            Ok(out) => println!("{}", out.text),
            Err(err) => eprintln!("Error: {err}"),
        }
    }

    Ok(())
}
