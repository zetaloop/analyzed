pub(crate) fn skip_slow_tests() -> bool {
    if std::env::var_os("SKIP_SLOW_TESTS").is_some() {
        return true;
    }

    std::env::var_os("RUN_SLOW_TESTS").is_none() && std::env::var_os("CI").is_none()
}

pub(crate) fn lines_match(expected: &str, actual: &str) -> bool {
    let expected = normalize_uri(expected.to_owned());
    crate::support::_original_lines_match(&expected, actual)
}

pub(crate) fn normalize_uri(mut uri: String) -> String {
    let Some(path_start) = uri.find("file://").map(|index| index + "file://".len()) else {
        return uri;
    };
    let mut path = uri[path_start..].chars();
    let (Some(drive), Some(':')) = (path.next(), path.next()) else {
        return uri;
    };
    if drive.is_ascii_alphabetic() {
        uri.replace_range(
            path_start..path_start + 2,
            &format!("/{}:", drive.to_ascii_lowercase()),
        );
    }
    uri
}
