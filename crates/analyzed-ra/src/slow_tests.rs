use lsp_server::Connection;
use ra_ap_rust_analyzer::config::Config;

pub(crate) fn skip_slow_tests() -> bool {
    (std::env::var("CI").is_err() && std::env::var("RUN_SLOW_TESTS").is_err())
        || std::env::var("SKIP_SLOW_TESTS").is_ok()
}

pub(crate) trait AnalyzedUriPath {
    fn analyzed_uri_path(self) -> String;
}

impl AnalyzedUriPath for String {
    fn analyzed_uri_path(self) -> String {
        let path = self.replace('\\', "/");
        let mut chars = path.chars();
        match (chars.next(), chars.next()) {
            (Some(drive), Some(':')) if drive.is_ascii_alphabetic() => {
                format!("/{}:{}", drive.to_ascii_lowercase(), chars.as_str())
            }
            _ => path,
        }
    }
}

pub(crate) fn run_server(config: Config, connection: Connection) {
    ra_ap_rust_analyzer::run_shared_rust_analyzer_lsp_session_with_config(config, connection)
        .unwrap()
}
