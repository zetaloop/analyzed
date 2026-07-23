use super::RatomlTest;

pub(super) trait FixturePath {
    fn fixture_path(&self, fixture: &str) -> lsp_types::Uri;
}

impl FixturePath for RatomlTest {
    fn fixture_path(&self, fixture: &str) -> lsp_types::Uri {
        let uri = self._original_fixture_path(fixture);
        let uri = crate::test_support::normalize_uri(uri.as_str().to_owned());
        lsp_types::Uri::parse(&uri).unwrap()
    }
}
