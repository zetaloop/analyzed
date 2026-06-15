use crate::{
    diagnostics::DiagnosticsMapConfig, flycheck::DiagnosticSpan,
    global_state::GlobalStateSnapshot, line_index::PositionEncoding,
    lsp::to_proto::url_from_abs_path,
};
use vfs::{AbsPath, AbsPathBuf, VfsPath};

pub(crate) fn location(
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
        .unwrap_or_else(|| url_from_abs_path(&file_name));

    let range = {
        let position_encoding = snap.config.negotiated_encoding();
        lsp_types::Range::new(
            position(
                &position_encoding,
                span,
                span.line_start,
                span.column_start.saturating_sub(1),
            ),
            position(&position_encoding, span, span.line_end, span.column_end.saturating_sub(1)),
        )
    };
    lsp_types::Location::new(uri, range)
}

fn position(
    position_encoding: &PositionEncoding,
    span: &DiagnosticSpan,
    line_number: usize,
    column_offset_utf32: usize,
) -> lsp_types::Position {
    let line_index = line_number - span.line_start;

    let column_offset_encoded = match span.text.get(line_index) {
        Some(line) if line.text.is_ascii() => column_offset_utf32,
        Some(line) => {
            let line_prefix_len = line
                .text
                .char_indices()
                .take(column_offset_utf32)
                .last()
                .map(|(pos, c)| pos + c.len_utf8())
                .unwrap_or(0);
            let line_prefix = &line.text[..line_prefix_len];
            match position_encoding {
                PositionEncoding::Utf8 => line_prefix.len(),
                PositionEncoding::Wide(enc) => enc.measure(line_prefix),
            }
        }
        None => column_offset_utf32,
    };

    lsp_types::Position {
        line: (line_number as u32).saturating_sub(1),
        character: column_offset_encoded as u32,
    }
}

fn resolve_path(
    config: &DiagnosticsMapConfig,
    workspace_root: &AbsPath,
    file_name: &str,
) -> AbsPathBuf {
    match config
        .remap_prefix
        .iter()
        .find_map(|(from, to)| file_name.strip_prefix(from).map(|file_name| (to, file_name)))
    {
        Some((to, file_name)) => workspace_root.join(format!("{to}{file_name}")),
        None => workspace_root.join(file_name),
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn test_global_state(
    sender: crossbeam_channel::Sender<lsp_server::Message>,
    workspace_root: AbsPathBuf,
    caps: lsp_types::ClientCapabilities,
) -> crate::global_state::GlobalState {
    let ra_config = crate::config::Config::new(workspace_root, caps, Vec::new(), None);
    let registry = crate::analyzed_bridge::shared_analyzer_registry();
    let provider =
        crate::analyzed_bridge::SharedAnalyzerProvider::new(move |key, config, reload_path| {
            registry.register(key, config, reload_path)
        });
    let (key, shared_config) =
        crate::analyzed_bridge::shared_analyzer_context_from_config(&ra_config).unwrap();
    let session = provider.resolve(key, shared_config).unwrap();
    let analyzed_shared = session.runtime();
    let analyzed_workspaces = session.workspaces().unwrap();
    crate::global_state::GlobalState::new(
        sender,
        ra_config,
        provider,
        analyzed_shared,
        analyzed_workspaces,
    )
}
