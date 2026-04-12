//! Supervisor — PID 1 process that runs inside VMs.
//!
//! Receives commands over stdin (JSON lines), emits events to stdout.

use anyhow::Result;
use vm_pool_protocol::{OutputStream, VmCommand, VmEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

    info!("supervisor starting");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);

    emit_event(&mut stdout, VmEvent::Ready).await?;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            info!("stdin closed, shutting down");
            break;
        }

        let command: VmCommand = match serde_json::from_str(line.trim()) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("failed to parse command: {}", e);
                continue;
            }
        };

        match command {
            VmCommand::Ping => {
                emit_event(&mut stdout, VmEvent::Pong).await?;
            }
            VmCommand::Shutdown => {
                info!("shutdown requested");
                emit_event(&mut stdout, VmEvent::Shutdown).await?;
                break;
            }
            VmCommand::Execute { command } => {
                execute_command(&mut stdout, &command).await?;
            }
        }
    }

    info!("supervisor exiting");
    Ok(())
}

async fn execute_command(stdout: &mut tokio::io::Stdout, command: &str) -> Result<()> {
    info!("executing: {}", command);

    let output = tokio::process::Command::new("sh")
        .args(["-c", command])
        .output()
        .await;

    match output {
        Ok(output) => {
            let stdout_data = String::from_utf8_lossy(&output.stdout);
            if !stdout_data.is_empty() {
                emit_event(
                    stdout,
                    VmEvent::Output {
                        stream: OutputStream::Stdout,
                        data: stdout_data.into_owned(),
                    },
                )
                .await?;
            }

            let stderr_data = String::from_utf8_lossy(&output.stderr);
            if !stderr_data.is_empty() {
                emit_event(
                    stdout,
                    VmEvent::Output {
                        stream: OutputStream::Stderr,
                        data: stderr_data.into_owned(),
                    },
                )
                .await?;
            }

            let exit_code = output.status.code().unwrap_or(-1);
            emit_event(stdout, VmEvent::CommandCompleted { exit_code }).await?;
        }
        Err(e) => {
            emit_event(
                stdout,
                VmEvent::Output {
                    stream: OutputStream::Stderr,
                    data: format!("failed to execute command: {e}\n"),
                },
            )
            .await?;
            emit_event(stdout, VmEvent::CommandCompleted { exit_code: -1 }).await?;
        }
    }

    Ok(())
}

async fn emit_event(stdout: &mut tokio::io::Stdout, event: VmEvent) -> Result<()> {
    let json = serde_json::to_string(&event)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}
