use std::{error::Error, path::Path};

use ra_ap_syntax::{
    AstNode, Edition, SourceFile, SyntaxNode, TextRange, TextSize,
    ast::{self, HasLoopBody, HasName, HasVisibility},
};

pub trait Host: AstNode + HasName {
    const KIND: &'static str;
}

impl Host for ast::Fn {
    const KIND: &'static str = "function";
}

impl Host for ast::Struct {
    const KIND: &'static str = "struct";
}

impl Host for ast::Enum {
    const KIND: &'static str = "enum";
}

impl Host for ast::RecordField {
    const KIND: &'static str = "field";
}

impl Host for ast::Variant {
    const KIND: &'static str = "variant";
}

fn named<N: Host>(source: &str, name: &str) -> Result<N, Box<dyn Error>> {
    let root = parse(source)?;
    let (scope, name) = match name.split_once("::") {
        Some((parent, child)) => {
            let adt = root
                .descendants()
                .filter_map(ast::Adt::cast)
                .find(|item| item.name().is_some_and(|it| it.text() == parent))
                .ok_or_else(|| format!("could not find item `{parent}`"))?;
            (adt.syntax().clone(), child)
        }
        None => (root, name),
    };
    scope
        .descendants()
        .filter_map(N::cast)
        .find(|item| item.name().is_some_and(|it| it.text() == name))
        .ok_or_else(|| format!("could not find {} `{name}`", N::KIND).into())
}

pub fn one<T>(
    mut candidates: impl Iterator<Item = T>,
    description: &str,
) -> Result<T, Box<dyn Error>> {
    let first = candidates
        .next()
        .ok_or_else(|| format!("no {description}"))?;
    if candidates.next().is_some() {
        return Err(format!("more than one {description}").into());
    }
    Ok(first)
}

pub fn calls(scope: &impl AstNode, name: &str) -> impl Iterator<Item = ast::MethodCallExpr> {
    scope
        .syntax()
        .descendants()
        .filter_map(ast::MethodCallExpr::cast)
        .filter(|call| call.name_ref().is_some_and(|it| it.text() == name))
        .collect::<Vec<_>>()
        .into_iter()
}

pub fn arms(scope: &impl AstNode, variant: &str) -> impl Iterator<Item = ast::MatchArm> {
    scope
        .syntax()
        .descendants()
        .filter_map(ast::MatchArm::cast)
        .filter(|arm| {
            arm.pat().is_some_and(|pat| {
                pat.syntax()
                    .descendants()
                    .filter_map(ast::Path::cast)
                    .any(|path| path.syntax().text() == variant)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
}

pub fn for_loops(scope: &impl AstNode) -> impl Iterator<Item = ast::ForExpr> {
    scope
        .syntax()
        .descendants()
        .filter_map(ast::ForExpr::cast)
        .collect::<Vec<_>>()
        .into_iter()
}

pub fn ifs_referencing(scope: &impl AstNode, field: &str) -> impl Iterator<Item = ast::IfExpr> {
    ifs_where(scope, |condition| {
        condition
            .syntax()
            .descendants()
            .filter_map(ast::FieldExpr::cast)
            .any(|expr| expr.name_ref().is_some_and(|it| it.text() == field))
    })
    .into_iter()
}

pub fn ifs_calling(scope: &impl AstNode, method: &str) -> impl Iterator<Item = ast::IfExpr> {
    ifs_where(scope, |condition| {
        condition
            .syntax()
            .descendants()
            .filter_map(ast::MethodCallExpr::cast)
            .any(|call| call.name_ref().is_some_and(|it| it.text() == method))
    })
    .into_iter()
}

fn ifs_where(scope: &impl AstNode, matches: impl Fn(&ast::Expr) -> bool) -> Vec<ast::IfExpr> {
    scope
        .syntax()
        .descendants()
        .filter_map(ast::IfExpr::cast)
        .filter(|if_expr| {
            if_expr
                .condition()
                .is_some_and(|condition| matches(&condition))
        })
        .collect()
}

pub fn rename<N: Host>(
    source: &mut String,
    name: &str,
    replacement: &str,
) -> Result<(), Box<dyn Error>> {
    let item: N = named(source, name)?;
    let name = item
        .name()
        .ok_or_else(|| format!("{} has no name", N::KIND))?;
    replace_text_range(source, name.syntax().text_range(), replacement);
    Ok(())
}

pub trait VisibilityHost: Host + HasVisibility {
    fn visibility_offset(&self) -> Result<TextSize, Box<dyn Error>>;
}

impl VisibilityHost for ast::Fn {
    fn visibility_offset(&self) -> Result<TextSize, Box<dyn Error>> {
        Ok(self
            .fn_token()
            .ok_or("function has no fn token")?
            .text_range()
            .start())
    }
}

impl VisibilityHost for ast::Enum {
    fn visibility_offset(&self) -> Result<TextSize, Box<dyn Error>> {
        Ok(self.syntax().text_range().start())
    }
}

impl VisibilityHost for ast::RecordField {
    fn visibility_offset(&self) -> Result<TextSize, Box<dyn Error>> {
        Ok(self
            .name()
            .ok_or("field has no name")?
            .syntax()
            .text_range()
            .start())
    }
}

pub fn set_visibility<N: VisibilityHost>(
    source: &mut String,
    name: &str,
    visibility: &str,
) -> Result<(), Box<dyn Error>> {
    let item: N = named(source, name)?;
    if let Some(existing) = item.visibility() {
        replace_text_range(source, existing.syntax().text_range(), visibility);
    } else {
        let start = text_offset(item.visibility_offset()?);
        source.insert_str(start, &format!("{visibility} "));
    }
    Ok(())
}

pub fn add_attr<N: Host>(
    source: &mut String,
    name: &str,
    attribute: &str,
) -> Result<(), Box<dyn Error>> {
    let item: N = named(source, name)?;
    let start = text_offset(item.syntax().text_range().start());
    let indent = line_indent(source, start);
    source.insert_str(start, &format!("{indent}{attribute}\n"));
    Ok(())
}

pub trait ListHost: Host {
    fn list_end(&self) -> Result<TextSize, Box<dyn Error>>;
}

impl ListHost for ast::Struct {
    fn list_end(&self) -> Result<TextSize, Box<dyn Error>> {
        let Some(ast::FieldList::RecordFieldList(fields)) = self.field_list() else {
            return Err("struct has no record field list".into());
        };
        Ok(fields
            .r_curly_token()
            .ok_or("record struct has no closing brace")?
            .text_range()
            .start())
    }
}

impl ListHost for ast::Enum {
    fn list_end(&self) -> Result<TextSize, Box<dyn Error>> {
        Ok(self
            .variant_list()
            .ok_or("enum has no variant list")?
            .r_curly_token()
            .ok_or("enum has no closing brace")?
            .text_range()
            .start())
    }
}

impl ListHost for ast::Fn {
    fn list_end(&self) -> Result<TextSize, Box<dyn Error>> {
        Ok(self
            .param_list()
            .ok_or("function has no parameter list")?
            .r_paren_token()
            .ok_or("parameter list has no closing paren")?
            .text_range()
            .start())
    }
}

pub fn append<N: ListHost>(
    source: &mut String,
    name: &str,
    items: &str,
) -> Result<(), Box<dyn Error>> {
    let item: N = named(source, name)?;
    let start = text_offset(item.list_end()?);
    source.insert_str(start, items);
    Ok(())
}

fn record_expr_in(function: &ast::Fn, path_tail: &str) -> impl Iterator<Item = ast::RecordExpr> {
    let path_tail = path_tail.to_owned();
    function
        .syntax()
        .descendants()
        .filter_map(ast::RecordExpr::cast)
        .filter(move |record| {
            record
                .path()
                .is_some_and(|path| path.syntax().text().to_string().ends_with(&path_tail))
        })
        .collect::<Vec<_>>()
        .into_iter()
}

pub fn append_record_fields(
    source: &mut String,
    function: &str,
    path_tail: &str,
    fields: &str,
) -> Result<(), Box<dyn Error>> {
    let function: ast::Fn = named(source, function)?;
    for record in record_expr_in(&function, path_tail) {
        let Some(field_list) = record.record_expr_field_list() else {
            continue;
        };
        let token = field_list
            .r_curly_token()
            .ok_or("record expression has no closing brace")?;
        let start = text_offset(token.text_range().start());
        source.insert_str(start, fields);
        return Ok(());
    }
    Err(format!("function has no `{path_tail}` record expression").into())
}

pub fn set_record_field(
    source: &mut String,
    function: &str,
    path_tail: &str,
    field: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let function: ast::Fn = named(source, function)?;
    for record in record_expr_in(&function, path_tail) {
        let Some(field_list) = record.record_expr_field_list() else {
            continue;
        };
        for record_field in field_list.fields() {
            let Some(name) = record_field.name_ref() else {
                continue;
            };
            if name.text() != field {
                continue;
            }
            let Some(expr) = record_field.expr() else {
                return Err(format!("record field `{field}` has no expression").into());
            };
            replace_text_range(source, expr.syntax().text_range(), value);
            return Ok(());
        }
    }
    Err(format!("function has no `{path_tail}.{field}` field").into())
}

pub fn add_rest_pattern(
    source: &mut String,
    function: &str,
    path_tail: &str,
) -> Result<(), Box<dyn Error>> {
    let function: ast::Fn = named(source, function)?;
    for node in function.syntax().descendants() {
        let Some(record) = ast::RecordPat::cast(node) else {
            continue;
        };
        let Some(path) = record.path() else {
            continue;
        };
        if !path.syntax().text().to_string().ends_with(path_tail) {
            continue;
        }
        let Some(fields) = record.record_pat_field_list() else {
            continue;
        };
        if fields.rest_pat().is_some() {
            return Ok(());
        }
        let token = fields
            .r_curly_token()
            .ok_or("record pattern has no closing brace")?;
        let start = text_offset(token.text_range().start());
        source.insert_str(start, ", ..");
        return Ok(());
    }
    Err(format!("function has no `{path_tail}` record pattern").into())
}

pub fn rename_path_root(
    source: &mut String,
    function: &str,
    root: &str,
    replacement: &str,
) -> Result<usize, Box<dyn Error>> {
    let function: ast::Fn = named(source, function)?;
    let mut ranges = Vec::new();
    for node in function.syntax().descendants() {
        let Some(segment) = ast::PathSegment::cast(node) else {
            continue;
        };
        let Some(name) = segment.name_ref() else {
            continue;
        };
        if name.text() != root {
            continue;
        }
        let Some(path) = segment.syntax().parent().and_then(ast::Path::cast) else {
            continue;
        };
        if path.qualifier().is_some() {
            continue;
        }
        ranges.push(name.syntax().text_range());
    }
    let count = ranges.len();
    for range in ranges.into_iter().rev() {
        replace_text_range(source, range, replacement);
    }
    Ok(count)
}

pub fn add_use(source: &mut String, path: &str) -> Result<(), Box<dyn Error>> {
    insert_use(source, &format!("use {path};\n"))
}

pub fn add_pub_use(source: &mut String, path: &str) -> Result<(), Box<dyn Error>> {
    insert_use(source, &format!("pub(crate) use {path};\n"))
}

fn insert_use(source: &mut String, statement: &str) -> Result<(), Box<dyn Error>> {
    if source.contains(statement) {
        return Err(format!("source already contains `{}`", statement.trim_end()).into());
    }

    let index =
        first_use_index(source).unwrap_or_else(|| insertion_index_after_inner_attrs(source));
    source.insert_str(index, statement);
    Ok(())
}

pub fn retarget_use(
    source: &mut String,
    name: &str,
    path: &str,
    alias: &str,
) -> Result<(), Box<dyn Error>> {
    let file = parse(source)?;
    let matches = file
        .descendants()
        .filter_map(ast::UseTree::cast)
        .filter_map(|tree| {
            let path = tree.path()?;
            let segment = path.segment()?;
            let name_ref = segment.name_ref()?;
            (name_ref.text() == name && tree.rename().is_none()).then_some(tree)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [tree] => {
            if tree
                .syntax()
                .ancestors()
                .find_map(ast::UseTreeList::cast)
                .is_some()
            {
                let range = use_tree_removal_range(source, tree)?;
                source.replace_range(range, "");
                add_use(source, &format!("{path} as {alias}"))?;
            } else {
                replace_text_range(
                    source,
                    tree.syntax().text_range(),
                    &format!("{path} as {alias}"),
                );
            }
            parse(source)?;
            Ok(())
        }
        [] => Err(format!("could not find use tree `{name}`").into()),
        _ => Err(format!("found multiple use trees `{name}`").into()),
    }
}

pub fn mount_module(source: &mut String, visibility: Option<&str>, name: &str, path: &Path) {
    let visibility = visibility.map_or(String::new(), |visibility| format!("{visibility} "));
    source.insert_str(
        0,
        &format!(
            "#[path = {:?}]\n{visibility}mod {name};\n\n",
            path.to_string_lossy().into_owned()
        ),
    );
}

pub struct Param<'a> {
    pub name: &'a str,
    pub ty: &'a str,
}

pub struct Method<'a> {
    pub name: &'a str,
    pub receiver: Option<&'a str>,
    pub params: &'a [Param<'a>],
    pub args: &'a [&'a str],
    pub return_ty: Option<&'a str>,
}

pub struct Selection {
    kind: SelectionKind,
}

enum SelectionKind {
    Statement {
        range: TextRange,
    },
    LoopBody {
        open_end: TextSize,
        close_start: TextSize,
        first_stmt_start: TextSize,
    },
    ThroughTail {
        start: TextSize,
        end: TextSize,
    },
    ParamsTail,
}

pub fn stmt(node: &impl AstNode) -> Result<Selection, Box<dyn Error>> {
    let statement = node
        .syntax()
        .ancestors()
        .find_map(ast::Stmt::cast)
        .ok_or("node is not part of a statement")?;
    Ok(Selection {
        kind: SelectionKind::Statement {
            range: statement.syntax().text_range(),
        },
    })
}

pub fn for_body(loop_expr: &ast::ForExpr) -> Result<Selection, Box<dyn Error>> {
    let body = loop_expr.loop_body().ok_or("for loop has no body")?;
    let stmt_list = body.stmt_list().ok_or("for loop has no statement list")?;
    let first_statement = stmt_list
        .statements()
        .next()
        .ok_or("for loop has empty body")?;
    let opening_brace = stmt_list
        .l_curly_token()
        .ok_or("for loop has no opening brace")?;
    let closing_brace = stmt_list
        .r_curly_token()
        .ok_or("for loop has no closing brace")?;
    Ok(Selection {
        kind: SelectionKind::LoopBody {
            open_end: opening_brace.text_range().end(),
            close_start: closing_brace.text_range().start(),
            first_stmt_start: first_statement.syntax().text_range().start(),
        },
    })
}

pub fn through_tail(from: &impl AstNode, function: &ast::Fn) -> Result<Selection, Box<dyn Error>> {
    let tail = function
        .body()
        .and_then(|body| body.stmt_list())
        .and_then(|list| list.tail_expr())
        .ok_or("function has no tail expression")?;
    let start = from.syntax().text_range().start();
    let end = tail.syntax().text_range().end();
    if end <= start {
        return Err("function tail expression precedes the selection".into());
    }
    Ok(Selection {
        kind: SelectionKind::ThroughTail { start, end },
    })
}

pub fn params_tail() -> Selection {
    Selection {
        kind: SelectionKind::ParamsTail,
    }
}

pub fn extract(
    source: &mut String,
    function: &str,
    select: impl FnOnce(&ast::Fn) -> Result<Selection, Box<dyn Error>>,
    method: &Method<'_>,
) -> Result<(), Box<dyn Error>> {
    let function_node: ast::Fn = named(source, function)?;
    let function_end = text_offset(function_node.syntax().text_range().end());
    let function_indent = line_indent(
        source,
        text_offset(function_node.syntax().text_range().start()),
    )
    .to_owned();
    let selection = select(&function_node)?;
    let extraction = match selection.kind {
        SelectionKind::Statement { range } => {
            let first_start = text_offset(range.start());
            let range = line_start_offset(source, first_start)..text_offset(range.end());
            let call_indent = line_indent(source, first_start).to_owned();
            let method_body_indent = format!("{function_indent}    ");
            Extraction {
                call_indent: call_indent.clone(),
                close_indent: String::new(),
                body: normalize_statement_body(
                    &source[range.clone()],
                    &call_indent,
                    &method_body_indent,
                ),
                expression: false,
                range,
            }
        }
        SelectionKind::LoopBody {
            open_end,
            close_start,
            first_stmt_start,
        } => {
            let range = text_offset(open_end)..text_offset(close_start);
            Extraction {
                call_indent: line_indent(source, text_offset(first_stmt_start)).to_owned(),
                close_indent: line_indent(source, text_offset(close_start)).to_owned(),
                body: source[range.clone()].trim_end().to_owned(),
                expression: false,
                range,
            }
        }
        SelectionKind::ThroughTail { start, end } => {
            let range = text_offset(start)..text_offset(end);
            let call_indent = line_indent(source, range.start).to_owned();
            Extraction {
                body: format!("\n{call_indent}{}", &source[range.clone()]),
                call_indent,
                close_indent: String::new(),
                expression: true,
                range,
            }
        }
        SelectionKind::ParamsTail => {
            let stmt_list = function_node
                .body()
                .and_then(|body| body.stmt_list())
                .ok_or_else(|| format!("function `{function}` has no statement list"))?;
            let mut anchor = None;
            for param in method.params {
                let mut end = None;
                for statement in stmt_list.statements() {
                    if let ast::Stmt::LetStmt(let_statement) = &statement
                        && pat_is_ident(let_statement.pat(), param.name)
                    {
                        end = Some(statement.syntax().text_range().end());
                    }
                }
                let end = end.ok_or_else(|| {
                    format!(
                        "function `{function}` does not define parameter `{}`",
                        param.name
                    )
                })?;
                if anchor.is_none_or(|current| current < end) {
                    anchor = Some(end);
                }
            }
            let anchor = anchor.ok_or_else(|| {
                format!("extracted method for `{function}` declares no parameters")
            })?;
            let first_extracted_stmt = stmt_list
                .statements()
                .find(|statement| statement.syntax().text_range().start() > anchor)
                .ok_or_else(|| {
                    format!("function `{function}` has no statements after its parameters")
                })?;
            let closing_brace = stmt_list
                .r_curly_token()
                .ok_or_else(|| format!("function `{function}` has no closing brace"))?;
            let range = text_offset(anchor)..text_offset(closing_brace.text_range().start());
            Extraction {
                call_indent: line_indent(
                    source,
                    text_offset(first_extracted_stmt.syntax().text_range().start()),
                )
                .to_owned(),
                close_indent: function_indent.clone(),
                body: source[range.clone()].trim_end().to_owned(),
                expression: false,
                range,
            }
        }
    };

    let params = method
        .params
        .iter()
        .map(|param| format!("{}: {}", param.name, param.ty))
        .collect::<Vec<_>>()
        .join(", ");
    let params = match (method.receiver, params.is_empty()) {
        (Some(receiver), true) => receiver.to_owned(),
        (Some(receiver), false) => format!("{receiver}, {params}"),
        (None, _) => params,
    };
    let args = method.args.join(", ");
    let replacement = if extraction.expression {
        format!("self.{}({args})", method.name)
    } else {
        format!(
            "\n{}self.{}({args});\n{}",
            extraction.call_indent, method.name, extraction.close_indent,
        )
    };

    let return_ty = method
        .return_ty
        .map_or(String::new(), |ty| format!(" -> {ty}"));
    let extracted = format!(
        "\n\n{function_indent}fn {}({params}){return_ty} {{{}\n{function_indent}}}",
        method.name, extraction.body,
    );
    source.insert_str(function_end, &extracted);
    source.replace_range(extraction.range, &replacement);
    parse(source)?;
    Ok(())
}

pub fn redirect_call(
    source: &mut String,
    function: &str,
    from: &str,
    to: &str,
) -> Result<(), Box<dyn Error>> {
    let function_node: ast::Fn = named(source, function)?;
    let body_stmt_list = function_node
        .body()
        .and_then(|body| body.stmt_list())
        .ok_or_else(|| format!("function `{function}` has no statement list"))?
        .syntax()
        .clone();
    let call = one(
        calls(&function_node, from).filter(|call| {
            call.syntax()
                .ancestors()
                .filter_map(ast::StmtList::cast)
                .next()
                .is_some_and(|list| *list.syntax() == body_stmt_list)
        }),
        &format!("top-level `{from}` call in `{function}`"),
    )?;
    let name = call
        .name_ref()
        .ok_or_else(|| format!("`{from}` call has no method name"))?;
    source.replace_range(text_range(name.syntax().text_range()), to);
    parse(source)?;
    Ok(())
}

struct Extraction {
    range: std::ops::Range<usize>,
    body: String,
    call_indent: String,
    close_indent: String,
    expression: bool,
}

fn parse(source: &str) -> Result<SyntaxNode, Box<dyn Error>> {
    let parsed = SourceFile::parse(source, Edition::CURRENT);
    let errors = parsed.errors();
    if !errors.is_empty() {
        return Err(format!("could not parse Rust source: {errors:?}").into());
    }
    Ok(parsed.syntax_node())
}

fn pat_is_ident(pattern: Option<ast::Pat>, name: &str) -> bool {
    matches!(pattern, Some(ast::Pat::IdentPat(pattern)) if pattern.name().is_some_and(|it| it.text() == name))
}

fn normalize_statement_body(body: &str, source_indent: &str, target_indent: &str) -> String {
    let mut normalized = String::new();
    for line in body.trim_end().lines() {
        normalized.push('\n');
        normalized.push_str(target_indent);
        normalized.push_str(line.strip_prefix(source_indent).unwrap_or(line));
    }
    normalized
}

fn use_tree_removal_range(
    source: &str,
    tree: &ast::UseTree,
) -> Result<std::ops::Range<usize>, Box<dyn Error>> {
    let start = text_offset(tree.syntax().text_range().start());
    let end = text_offset(tree.syntax().text_range().end());
    let bytes = source.as_bytes();

    let mut after = end;
    while after < bytes.len() && bytes[after].is_ascii_whitespace() {
        after += 1;
    }
    if after < bytes.len() && bytes[after] == b',' {
        after += 1;
        while after < bytes.len() && bytes[after].is_ascii_whitespace() {
            after += 1;
        }
        return Ok(start..after);
    }

    let mut before = start;
    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    if before > 0 && bytes[before - 1] == b',' {
        return Ok(before - 1..end);
    }
    Ok(start..end)
}

fn replace_text_range(source: &mut String, range: TextRange, replacement: &str) {
    source.replace_range(text_range(range), replacement);
}

fn text_range(range: TextRange) -> std::ops::Range<usize> {
    text_offset(range.start())..text_offset(range.end())
}

fn text_offset(size: TextSize) -> usize {
    u32::from(size) as usize
}

fn line_indent(source: &str, offset: usize) -> &str {
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let indent_len = source[line_start..offset]
        .chars()
        .take_while(|value| value.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    &source[line_start..line_start + indent_len]
}

fn line_start_offset(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map_or(0, |index| index + 1)
}

fn first_use_index(source: &str) -> Option<usize> {
    let mut index = 0;
    for line in source.split_inclusive('\n') {
        if line.starts_with("use ") || line.starts_with("pub use ") {
            return Some(index);
        }
        index += line.len();
    }
    None
}

fn insertion_index_after_inner_attrs(source: &str) -> usize {
    let mut index = 0;
    for line in source.split_inclusive('\n') {
        if !(line.starts_with("#![") || line.starts_with("//!")) {
            return index;
        }
        index += line.len();
    }
    index
}
