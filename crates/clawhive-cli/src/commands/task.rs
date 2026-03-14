use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

use clawhive_schema::InboundMessage;

use crate::runtime::bootstrap::bootstrap;

#[derive(Subcommand)]
pub(crate) enum TaskCommands {
    #[command(about = "Trigger a one-off task")]
    Trigger {
        #[arg(help = "Agent ID")]
        agent: String,
        #[arg(help = "Task description")]
        task: String,
    },
}

pub(crate) async fn run(cmd: TaskCommands, root: &Path) -> Result<()> {
    let (_bus, _memory, gateway, _config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, None).await?;
    match cmd {
        TaskCommands::Trigger {
            agent: _agent,
            task,
        } => {
            let inbound = InboundMessage {
                trace_id: uuid::Uuid::new_v4(),
                channel_type: "cli".into(),
                connector_id: "cli".into(),
                conversation_scope: "task:cli".into(),
                user_scope: "user:cli".into(),
                text: task,
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
                Err(err) => eprintln!("Task failed: {err}"),
            }
        }
    }
    Ok(())
}
