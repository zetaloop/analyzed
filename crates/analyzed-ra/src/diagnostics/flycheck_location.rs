use vfs::{AbsPath, VfsPath};

use super::{DiagnosticSpan, DiagnosticsMapConfig, GlobalStateSnapshot, position, resolve_path};

pub(super) fn location(
    config: &DiagnosticsMapConfig,
    workspace_root: &AbsPath,
    span: &DiagnosticSpan,
    snap: &GlobalStateSnapshot,
) -> lsp_types::Location {
    let file_name = resolve_path(config, workspace_root, &span.file_name);
    let uri = snap
        .base_vfs_path_to_file_id(&VfsPath::from(file_name.clone()))
        .ok()
        .flatten()
        .map(|file_id| snap.file_id_to_url(file_id))
        .unwrap_or_else(|| crate::lsp::to_proto::url_from_abs_path(&file_name));
    let position_encoding = snap.config.negotiated_encoding();
    lsp_types::Location::new(
        uri,
        lsp_types::Range::new(
            position(
                &position_encoding,
                span,
                span.line_start,
                span.column_start.saturating_sub(1),
            ),
            position(
                &position_encoding,
                span,
                span.line_end,
                span.column_end.saturating_sub(1),
            ),
        ),
    )
}
