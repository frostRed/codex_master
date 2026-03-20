use agent_client_protocol::{
    Agent, Client, ClientSideConnection, ContentBlock, Implementation, InitializeRequest,
    NewSessionRequest, PermissionOption, PermissionOptionKind, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
};
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::io::Write as _;
use std::process::Stdio;
use std::rc::Rc;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, Command};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Default)]
struct RelayState {
    active_session: Option<SessionId>,
    printed_output: bool,
}

#[derive(Clone, Default)]
struct RelayClient {
    state: Rc<RefCell<RelayState>>,
}

impl RelayClient {
    fn begin_prompt(&self, session_id: &SessionId) {
        let mut state = self.state.borrow_mut();
        state.active_session = Some(session_id.clone());
        state.printed_output = false;
    }

    fn end_prompt(&self) -> bool {
        let mut state = self.state.borrow_mut();
        state.active_session = None;
        std::mem::take(&mut state.printed_output)
    }
}

fn pick_permission_option(options: &[PermissionOption]) -> Option<PermissionOption> {
    options
        .iter()
        .find(|opt| matches!(opt.kind, PermissionOptionKind::AllowOnce))
        .or_else(|| {
            options
                .iter()
                .find(|opt| matches!(opt.kind, PermissionOptionKind::RejectOnce))
        })
        .or_else(|| {
            options
                .iter()
                .find(|opt| matches!(opt.kind, PermissionOptionKind::AllowAlways))
        })
        .or_else(|| {
            options
                .iter()
                .find(|opt| matches!(opt.kind, PermissionOptionKind::RejectAlways))
        })
        .cloned()
}

#[async_trait::async_trait(?Send)]
impl Client for RelayClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let outcome = match pick_permission_option(&args.options) {
            Some(option) => {
                eprintln!(
                    "\n[permission] 自动选择: {} ({:?})",
                    option.name, option.kind
                );
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id))
            }
            None => RequestPermissionOutcome::Cancelled,
        };

        Ok(RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        let mut state = self.state.borrow_mut();
        if state.active_session.as_ref() != Some(&args.session_id) {
            return Ok(());
        }

        if let SessionUpdate::AgentMessageChunk(chunk) = args.update
            && let ContentBlock::Text(text) = chunk.content
        {
            print!("{}", text.text);
            let _ = std::io::stdout().flush();
            state.printed_output = true;
        }

        Ok(())
    }
}

async fn stream_stderr(stderr: ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        eprintln!("[codex-acp] {line}");
    }
}

async fn create_session(agent: &ClientSideConnection, cwd: &std::path::Path) -> Result<SessionId> {
    let response = agent
        .new_session(NewSessionRequest::new(cwd))
        .await
        .context("创建会话失败")?;
    Ok(response.session_id)
}

async fn run() -> Result<()> {
    let codex_acp_bin = std::env::var("CODEX_ACP_BIN").unwrap_or_else(|_| "codex-acp".to_string());
    let cwd = std::env::current_dir().context("无法获取当前工作目录")?;

    let mut child = Command::new(&codex_acp_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("启动 `{codex_acp_bin}` 失败，请先安装 codex-acp"))?;

    let child_stdin = child.stdin.take().context("无法获取 codex-acp stdin")?;
    let child_stdout = child.stdout.take().context("无法获取 codex-acp stdout")?;
    let child_stderr = child.stderr.take().context("无法获取 codex-acp stderr")?;

    tokio::task::spawn_local(stream_stderr(child_stderr));

    let relay_client = RelayClient::default();
    let (agent, io_task) = ClientSideConnection::new(
        relay_client.clone(),
        child_stdin.compat_write(),
        child_stdout.compat(),
        |future| {
            tokio::task::spawn_local(future);
        },
    );

    tokio::task::spawn_local(async move {
        if let Err(err) = io_task.await {
            eprintln!("\n[acp-io] {err}");
        }
    });

    let init_response = agent
        .initialize(
            InitializeRequest::new(ProtocolVersion::LATEST)
                .client_info(Implementation::new("codex-master-relay", "0.1.0").title("Relay")),
        )
        .await
        .context("ACP 初始化失败")?;

    let agent_name = init_response
        .agent_info
        .as_ref()
        .and_then(|info| info.title.as_deref().or(Some(info.name.as_str())))
        .unwrap_or("codex-acp");

    println!("已连接 {agent_name}。输入 /new 开新会话，/exit 退出。");

    let mut current_session: Option<SessionId> = None;
    let mut lines = BufReader::new(io::stdin()).lines();

    loop {
        print!("you> ");
        std::io::stdout().flush().context("输出提示符失败")?;

        let Some(line) = lines.next_line().await.context("读取输入失败")? else {
            break;
        };
        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        if matches!(trimmed, "/exit" | "/quit") {
            break;
        }

        if trimmed == "/new" {
            let session_id = create_session(&agent, &cwd).await?;
            println!("[new] session: {session_id}");
            current_session = Some(session_id);
            continue;
        }

        if current_session.is_none() {
            let session_id = create_session(&agent, &cwd).await?;
            println!("[auto] session: {session_id}");
            current_session = Some(session_id);
        }

        let session_id = current_session
            .clone()
            .expect("session should have been created");
        relay_client.begin_prompt(&session_id);

        let prompt_result = agent
            .prompt(PromptRequest::new(session_id, vec![line.into()]))
            .await;

        let printed = relay_client.end_prompt();
        if printed {
            println!();
        }

        match prompt_result {
            Ok(response) => {
                if !matches!(response.stop_reason, StopReason::EndTurn) {
                    println!("[stop_reason] {:?}", response.stop_reason);
                }
            }
            Err(err) => {
                eprintln!("[prompt-error] {err}");
            }
        }
    }

    let _ = child.start_kill();
    let _ = child.wait().await;

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tokio::task::LocalSet::new().run_until(run()).await
}
