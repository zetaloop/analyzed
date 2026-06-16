pub fn main_loop(
    config: crate::config::Config,
    connection: lsp_server::Connection,
) -> anyhow::Result<()> {
    crate::run_shared_rust_analyzer_lsp_session_with_config(config, connection)
}
