pub(crate) fn skip_slow_tests() -> bool {
    if std::env::var_os("SKIP_SLOW_TESTS").is_some() {
        return true;
    }

    std::env::var_os("RUN_SLOW_TESTS").is_none() && std::env::var_os("CI").is_none()
}

pub(crate) trait UriPath {
    fn uri_path(self) -> String;
}

impl UriPath for String {
    fn uri_path(self) -> String {
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

