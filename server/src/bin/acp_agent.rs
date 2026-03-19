#[tokio::main(flavor = "current_thread")]
async fn main() -> agent_client_protocol::Result<()> {
    codex_companion_server::init_tracing();
    codex_companion_server::acp::serve_stdio(codex_companion_server::ServerConfig::from_env()).await
}
