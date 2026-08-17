use vfs::{AbsPath, VfsPath};

use super::{_location, DiagnosticSpan, DiagnosticsMapConfig, GlobalStateSnapshot, resolve_path};

pub(super) fn location(
    config: &DiagnosticsMapConfig,
    workspace_root: &AbsPath,
    span: &DiagnosticSpan,
    snap: &GlobalStateSnapshot,
) -> lsp_types::Location {
    let mut location = _location(config, workspace_root, span, snap);
    let file_name = VfsPath::from(resolve_path(config, workspace_root, &span.file_name));
    if let Ok(Some(file_id)) = snap.base_vfs_path_to_file_id(&file_name) {
        location.uri = snap.file_id_to_url(file_id);
    }
    location
}
