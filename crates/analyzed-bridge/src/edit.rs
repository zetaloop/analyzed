use std::{error::Error, path::Path};

use ra_ap_syntax::{
    AstNode, Edition, SourceFile, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken,
    ast::{
        self, HasLoopBody, HasName, HasVisibility,
        edit::{AstNodeEdit, IndentLevel},
        make,
    },
    hacks,
    syntax_editor::{Position, SyntaxEditor},
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

fn open(source: &str) -> Result<(SyntaxEditor, SyntaxNode), Box<dyn Error>> {
    let file = parse_file(source)?;
    Ok(SyntaxEditor::new(file.syntax().clone()))
}

fn commit(source: &mut String, editor: SyntaxEditor) -> Result<(), Box<dyn Error>> {
    let text = editor.finish().new_root().to_string();
    parse_file(&text)?;
    *source = text;
    Ok(())
}

fn named<N: Host>(root: &SyntaxNode, name: &str) -> Result<N, Box<dyn Error>> {
    let (scope, name) = match name.split_once("::") {
        Some((parent, child)) => {
            let adt = root
                .descendants()
                .filter_map(ast::Adt::cast)
                .find(|item| item.name().is_some_and(|it| it.text() == parent))
                .ok_or_else(|| format!("could not find item `{parent}`"))?;
            (adt.syntax().clone(), child)
        }
        None => (root.clone(), name),
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
    let (editor, root) = open(source)?;
    let item: N = named(&root, name)?;
    let name = item
        .name()
        .ok_or_else(|| format!("{} has no name", N::KIND))?;
    editor.replace(name.syntax(), make::name(replacement).syntax().clone());
    commit(source, editor)
}

pub trait VisibilityHost: Host + HasVisibility {
    fn visibility_slot(&self) -> Result<SyntaxElement, Box<dyn Error>>;
}

impl VisibilityHost for ast::Fn {
    fn visibility_slot(&self) -> Result<SyntaxElement, Box<dyn Error>> {
        Ok(self.fn_token().ok_or("function has no fn token")?.into())
    }
}

impl VisibilityHost for ast::Enum {
    fn visibility_slot(&self) -> Result<SyntaxElement, Box<dyn Error>> {
        self.syntax()
            .first_child_or_token()
            .ok_or_else(|| "enum has no content".into())
    }
}

impl VisibilityHost for ast::RecordField {
    fn visibility_slot(&self) -> Result<SyntaxElement, Box<dyn Error>> {
        Ok(self
            .name()
            .ok_or("field has no name")?
            .syntax()
            .clone()
            .into())
    }
}

pub fn set_visibility<N: VisibilityHost>(
    source: &mut String,
    name: &str,
    visibility: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let item: N = named(&root, name)?;
    if let Some(existing) = item.visibility() {
        editor.replace(
            existing.syntax(),
            visibility_node(visibility)?.syntax().clone(),
        );
    } else {
        editor.insert_all(
            Position::before(item.visibility_slot()?),
            vec![
                visibility_node(visibility)?.syntax().clone().into(),
                make::tokens::single_space().into(),
            ],
        );
    }
    commit(source, editor)
}

pub fn add_attr<N: Host>(
    source: &mut String,
    name: &str,
    attribute: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let item: N = named(&root, name)?;
    let indent = indent_before(&item.syntax().clone().into());
    let file = parse_file(&format!("{attribute}\nfn w() {{}}"))?;
    let attr = one(
        file.syntax().descendants().filter_map(ast::Attr::cast),
        "attribute in wrapper",
    )?;
    editor.insert_all(
        Position::first_child_of(item.syntax()),
        vec![
            attr.syntax().clone().into(),
            make::tokens::whitespace(&format!("\n{indent}")).into(),
        ],
    );
    commit(source, editor)
}

pub struct Field<'a> {
    pub vis: Option<&'a str>,
    pub name: &'a str,
    pub ty: &'a str,
}

pub struct Variant<'a> {
    pub name: &'a str,
    pub tuple_fields: &'a [&'a str],
}

pub trait ListHost: Host {
    type Item<'a>;
    fn append_items(
        &self,
        editor: &SyntaxEditor,
        items: &[Self::Item<'_>],
    ) -> Result<(), Box<dyn Error>>;
}

impl ListHost for ast::Struct {
    type Item<'a> = Field<'a>;

    fn append_items(
        &self,
        editor: &SyntaxEditor,
        items: &[Field<'_>],
    ) -> Result<(), Box<dyn Error>> {
        let Some(ast::FieldList::RecordFieldList(fields)) = self.field_list() else {
            return Err("struct has no record field list".into());
        };
        let close = fields
            .r_curly_token()
            .ok_or("record struct has no closing brace")?;
        let indent = IndentLevel::from_node(fields.syntax()) + 1;
        let mut elements = Vec::new();
        for item in items {
            elements.extend([
                make::tokens::whitespace(&indent.to_string()).into(),
                make::record_field(
                    item.vis.map(visibility_node).transpose()?,
                    make::name(item.name),
                    make::ty(item.ty),
                )
                .syntax()
                .clone()
                .into(),
                make::token(SyntaxKind::COMMA).into(),
                make::tokens::single_newline().into(),
            ]);
        }
        editor.insert_all(Position::before(close), elements);
        Ok(())
    }
}

impl ListHost for ast::Enum {
    type Item<'a> = Variant<'a>;

    fn append_items(
        &self,
        editor: &SyntaxEditor,
        items: &[Variant<'_>],
    ) -> Result<(), Box<dyn Error>> {
        let list = self.variant_list().ok_or("enum has no variant list")?;
        for item in items {
            let fields = (!item.tuple_fields.is_empty()).then(|| {
                make::tuple_field_list(
                    item.tuple_fields
                        .iter()
                        .map(|ty| make::tuple_field(None, make::ty(ty))),
                )
                .into()
            });
            list.add_variant(
                editor,
                &make::variant(None, make::name(item.name), fields, None),
            );
        }
        Ok(())
    }
}

impl ListHost for ast::Fn {
    type Item<'a> = Param<'a>;

    fn append_items(
        &self,
        editor: &SyntaxEditor,
        items: &[Param<'_>],
    ) -> Result<(), Box<dyn Error>> {
        let list = self.param_list().ok_or("function has no parameter list")?;
        let close = list
            .r_paren_token()
            .ok_or("parameter list has no closing paren")?;
        let mut elements = Vec::new();
        for item in items {
            elements.extend([
                make::token(SyntaxKind::COMMA).into(),
                make::tokens::single_space().into(),
                make::param(
                    make::ident_pat(false, false, make::name(item.name)).into(),
                    make::ty(item.ty),
                )
                .syntax()
                .clone()
                .into(),
            ]);
        }
        editor.insert_all(Position::before(close), elements);
        Ok(())
    }
}

pub fn append<N: ListHost>(
    source: &mut String,
    name: &str,
    items: &[N::Item<'_>],
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let item: N = named(&root, name)?;
    item.append_items(&editor, items)?;
    commit(source, editor)
}

fn record_exprs_in(function: &ast::Fn, path_tail: &str) -> Vec<ast::RecordExpr> {
    function
        .syntax()
        .descendants()
        .filter_map(ast::RecordExpr::cast)
        .filter(|record| {
            record
                .path()
                .is_some_and(|path| path.syntax().text().to_string().ends_with(path_tail))
        })
        .collect()
}

pub struct FieldInit<'a> {
    pub name: &'a str,
    pub value: Option<&'a str>,
}

pub fn append_record_fields(
    source: &mut String,
    function: &str,
    path_tail: &str,
    fields: &[FieldInit<'_>],
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function: ast::Fn = named(&root, function)?;
    for record in record_exprs_in(&function, path_tail) {
        let Some(field_list) = record.record_expr_field_list() else {
            continue;
        };
        let fields = fields
            .iter()
            .map(|field| {
                Ok(make::record_expr_field(
                    make::name_ref(field.name),
                    field.value.map(expr_node).transpose()?,
                ))
            })
            .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
        field_list.add_fields(&editor, fields);
        return commit(source, editor);
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
    let (editor, root) = open(source)?;
    let function: ast::Fn = named(&root, function)?;
    for record in record_exprs_in(&function, path_tail) {
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
            editor.replace(expr.syntax(), expr_node(value)?.syntax().clone());
            return commit(source, editor);
        }
    }
    Err(format!("function has no `{path_tail}.{field}` field").into())
}

pub fn add_rest_pattern(
    source: &mut String,
    function: &str,
    path_tail: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function: ast::Fn = named(&root, function)?;
    for record in function
        .syntax()
        .descendants()
        .filter_map(ast::RecordPat::cast)
    {
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
        let last = fields
            .fields()
            .last()
            .ok_or("record pattern has no fields")?;
        editor.insert_all(
            Position::after(last.syntax()),
            vec![
                make::token(SyntaxKind::COMMA).into(),
                make::tokens::single_space().into(),
                make::rest_pat().syntax().clone().into(),
            ],
        );
        return commit(source, editor);
    }
    Err(format!("function has no `{path_tail}` record pattern").into())
}

pub fn rename_path_root(
    source: &mut String,
    function: &str,
    root_name: &str,
    replacement: &str,
) -> Result<usize, Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function: ast::Fn = named(&root, function)?;
    let mut count = 0;
    for node in function.syntax().descendants() {
        let Some(segment) = ast::PathSegment::cast(node) else {
            continue;
        };
        let Some(name) = segment.name_ref() else {
            continue;
        };
        if name.text() != root_name {
            continue;
        }
        let Some(path) = segment.syntax().parent().and_then(ast::Path::cast) else {
            continue;
        };
        if path.qualifier().is_some() {
            continue;
        }
        editor.replace(name.syntax(), make::name_ref(replacement).syntax().clone());
        count += 1;
    }
    if count > 0 {
        commit(source, editor)?;
    }
    Ok(count)
}

pub fn add_use(
    source: &mut String,
    visibility: Option<&str>,
    path: &str,
) -> Result<(), Box<dyn Error>> {
    let item = make::use_(
        std::iter::empty(),
        visibility.map(visibility_node).transpose()?,
        make::use_tree(make::path_from_text(path), None, None, false),
    );
    let statement = item.to_string();
    if source.contains(&statement) {
        return Err(format!("source already contains `{statement}`").into());
    }

    let (editor, root) = open(source)?;
    let anchor = root
        .children()
        .find(|node| ast::Use::can_cast(node.kind()))
        .or_else(|| {
            root.children()
                .find(|node| ast::Item::can_cast(node.kind()))
        })
        .ok_or("source has no items")?;
    editor.insert_all(
        Position::before(&anchor),
        vec![
            item.syntax().clone().into(),
            make::tokens::single_newline().into(),
        ],
    );
    commit(source, editor)
}

pub fn retarget_use(source: &mut String, name: &str, path: &str) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let matches = root
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
                for element in use_tree_removal(tree) {
                    editor.delete(element);
                }
                commit(source, editor)?;
                add_use(source, None, path)
            } else {
                let replacement = make::use_tree(make::path_from_text(path), None, None, false);
                editor.replace(tree.syntax(), replacement.syntax().clone());
                commit(source, editor)
            }
        }
        [] => Err(format!("could not find use tree `{name}`").into()),
        _ => Err(format!("found multiple use trees `{name}`").into()),
    }
}

fn use_tree_removal(tree: &ast::UseTree) -> Vec<SyntaxElement> {
    let mut elements = vec![SyntaxElement::from(tree.syntax().clone())];

    let mut after = Vec::new();
    let mut cursor = tree.syntax().next_sibling_or_token();
    while let Some(element) = cursor {
        cursor = element.next_sibling_or_token();
        match element.kind() {
            SyntaxKind::WHITESPACE => after.push(element),
            SyntaxKind::COMMA => {
                after.push(element);
                while let Some(trailing) = cursor.clone() {
                    if trailing.kind() != SyntaxKind::WHITESPACE {
                        break;
                    }
                    cursor = trailing.next_sibling_or_token();
                    after.push(trailing);
                }
                elements.extend(after);
                return elements;
            }
            _ => break,
        }
    }

    let mut before = Vec::new();
    let mut cursor = tree.syntax().prev_sibling_or_token();
    while let Some(element) = cursor {
        cursor = element.prev_sibling_or_token();
        match element.kind() {
            SyntaxKind::WHITESPACE => before.push(element),
            SyntaxKind::COMMA => {
                before.push(element);
                elements.extend(before);
                return elements;
            }
            _ => break,
        }
    }

    elements
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
    Statement { statement: SyntaxNode },
    LoopBody { list: ast::StmtList },
    ThroughTail { from: SyntaxNode, tail: SyntaxNode },
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
            statement: statement.syntax().clone(),
        },
    })
}

pub fn for_body(loop_expr: &ast::ForExpr) -> Result<Selection, Box<dyn Error>> {
    let list = loop_expr
        .loop_body()
        .and_then(|body| body.stmt_list())
        .ok_or("for loop has no statement list")?;
    Ok(Selection {
        kind: SelectionKind::LoopBody { list },
    })
}

pub fn through_tail(from: &impl AstNode, function: &ast::Fn) -> Result<Selection, Box<dyn Error>> {
    let tail = function
        .body()
        .and_then(|body| body.stmt_list())
        .and_then(|list| list.tail_expr())
        .ok_or("function has no tail expression")?;
    Ok(Selection {
        kind: SelectionKind::ThroughTail {
            from: from.syntax().clone(),
            tail: tail.syntax().clone(),
        },
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
    method: Method<'_>,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function_node: ast::Fn = named(&root, function)?;
    let function_level = IndentLevel::from_node(function_node.syntax());
    let selection = select(&function_node)?;
    let call = make::expr_method_call(
        make::ext::expr_self(),
        make::name_ref(method.name),
        make::arg_list(
            method
                .args
                .iter()
                .map(|arg| expr_node(arg))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    let mut body = match selection.kind {
        SelectionKind::Statement { statement } => {
            let region = vec![SyntaxElement::from(statement.clone())];
            editor.replace(&statement, make::expr_stmt(call.into()).syntax().clone());
            region
        }
        SelectionKind::LoopBody { list } => {
            let open_brace = list
                .l_curly_token()
                .ok_or("for loop has no opening brace")?;
            let close_brace = list
                .r_curly_token()
                .ok_or("for loop has no closing brace")?;
            let first_statement = list.statements().next().ok_or("for loop has empty body")?;
            let inner = elements_between(list.syntax(), &open_brace, &close_brace)?;
            let call_indent = indent_before(&first_statement.syntax().clone().into());
            let close_indent = indent_before(&close_brace.into());
            replace_elements(
                &editor,
                inner.clone(),
                vec![
                    make::tokens::whitespace(&format!("\n{call_indent}")).into(),
                    make::expr_stmt(call.into()).syntax().clone().into(),
                    make::tokens::whitespace(&format!("\n{close_indent}")).into(),
                ],
            )?;
            inner
        }
        SelectionKind::ThroughTail { from, tail } => {
            let list = tail
                .parent()
                .ok_or_else(|| format!("function `{function}` has no statement list"))?;
            let start = child_of(&list, &from)?;
            let range = element_range(&list, &start.into(), &tail.into())?;
            replace_elements(&editor, range.clone(), vec![call.syntax().clone().into()])?;
            range
        }
        SelectionKind::ParamsTail => {
            let list = function_node
                .body()
                .and_then(|body| body.stmt_list())
                .ok_or_else(|| format!("function `{function}` has no statement list"))?;
            let mut anchor: Option<SyntaxNode> = None;
            for param in method.params {
                let mut definition = None;
                for statement in list.statements() {
                    if let ast::Stmt::LetStmt(let_statement) = &statement
                        && pat_is_ident(let_statement.pat(), param.name)
                    {
                        definition = Some(statement.syntax().clone());
                    }
                }
                let definition = definition.ok_or_else(|| {
                    format!(
                        "function `{function}` does not define parameter `{}`",
                        param.name
                    )
                })?;
                if anchor.as_ref().is_none_or(|current| {
                    current.text_range().end() < definition.text_range().end()
                }) {
                    anchor = Some(definition);
                }
            }
            let anchor = anchor.ok_or_else(|| {
                format!("extracted method for `{function}` declares no parameters")
            })?;
            let first_statement = list
                .statements()
                .find(|statement| {
                    statement.syntax().text_range().start() > anchor.text_range().end()
                })
                .ok_or_else(|| {
                    format!("function `{function}` has no statements after its parameters")
                })?;
            let close_brace = list
                .r_curly_token()
                .ok_or_else(|| format!("function `{function}` has no closing brace"))?;
            let elements = list.syntax().children_with_tokens().collect::<Vec<_>>();
            let start = elements
                .iter()
                .position(|element| element.as_node() == Some(&anchor))
                .ok_or("anchor is not a direct child")?;
            let end = elements
                .iter()
                .position(|element| element.as_token() == Some(&close_brace))
                .ok_or("closing brace is not a direct child")?;
            let range = elements[start + 1..end].to_vec();
            let call_indent = indent_before(&first_statement.syntax().clone().into());
            replace_elements(
                &editor,
                range.clone(),
                vec![
                    make::tokens::whitespace(&format!("\n{call_indent}")).into(),
                    make::expr_stmt(call.into()).syntax().clone().into(),
                    make::tokens::whitespace(&format!("\n{function_level}")).into(),
                ],
            )?;
            range
        }
    };
    while body
        .first()
        .is_some_and(|element| element.kind() == SyntaxKind::WHITESPACE)
    {
        body.remove(0);
    }
    while body
        .last()
        .is_some_and(|element| element.kind() == SyntaxKind::WHITESPACE)
    {
        body.pop();
    }
    let first = body.first().ok_or("extracted selection is empty")?;
    let body_level = IndentLevel::from_element(first);

    let receiver = method
        .receiver
        .map(|receiver| match receiver {
            "&self" => Ok(make::self_param()),
            "&mut self" => Ok(make::mut_self_param()),
            _ => Err(format!("unsupported receiver `{receiver}`")),
        })
        .transpose()?;
    let params = make::param_list(
        receiver,
        method.params.iter().map(|param| {
            make::param(
                make::ident_pat(false, false, make::name(param.name)).into(),
                make::ty(param.ty),
            )
        }),
    );
    let method_node = make::fn_(
        std::iter::empty(),
        None,
        make::name(method.name),
        None,
        None,
        params,
        make::block_expr(std::iter::empty(), None),
        method.return_ty.map(|ty| make::ret_type(make::ty(ty))),
        false,
        false,
        false,
        false,
    );
    let (body_editor, method_node) = SyntaxEditor::with_ast_node(&method_node);
    let list = method_node
        .body()
        .and_then(|body| body.stmt_list())
        .ok_or("extracted method has no statement list")?;
    let open_brace = list
        .l_curly_token()
        .ok_or("extracted method has no opening brace")?;
    let close_brace = list
        .r_curly_token()
        .ok_or("extracted method has no closing brace")?;
    for element in list.syntax().children_with_tokens() {
        if element.as_token() != Some(&open_brace) && element.as_token() != Some(&close_brace) {
            body_editor.delete(element);
        }
    }
    let mut content = Vec::with_capacity(body.len() + 2);
    content.push(make::tokens::whitespace(&format!("\n{body_level}")).into());
    content.extend(body);
    content.push(make::tokens::whitespace(&format!("\n{}", IndentLevel(body_level.0 - 1))).into());
    body_editor.insert_all(Position::after(open_brace), content);
    let method_node = ast::Fn::cast(body_editor.finish().new_root().clone())
        .ok_or("extracted method is not a function")?;
    let target_level = function_level + 1;
    let method_node = if target_level.0 > body_level.0 {
        method_node.indent(IndentLevel(target_level.0 - body_level.0))
    } else if body_level.0 > target_level.0 {
        method_node.dedent(IndentLevel(body_level.0 - target_level.0))
    } else {
        method_node
    };
    editor.insert_all(
        Position::after(function_node.syntax()),
        vec![
            make::tokens::whitespace(&format!("\n\n{function_level}")).into(),
            method_node.syntax().clone().into(),
        ],
    );
    commit(source, editor)
}

pub fn redirect_call(
    source: &mut String,
    function: &str,
    from: &str,
    to: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function_node: ast::Fn = named(&root, function)?;
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
    editor.replace(name.syntax(), make::name_ref(to).syntax().clone());
    commit(source, editor)
}

fn visibility_node(visibility: &str) -> Result<ast::Visibility, Box<dyn Error>> {
    match visibility {
        "pub" => Ok(make::visibility_pub()),
        "pub(crate)" => Ok(make::visibility_pub_crate()),
        _ => Err(format!("unsupported visibility `{visibility}`").into()),
    }
}

fn expr_node(expr: &str) -> Result<ast::Expr, Box<dyn Error>> {
    hacks::parse_expr_from_str(expr, Edition::CURRENT)
        .ok_or_else(|| format!("could not parse expression `{expr}`").into())
}

fn parse_file(source: &str) -> Result<SourceFile, Box<dyn Error>> {
    let parsed = SourceFile::parse(source, Edition::CURRENT);
    let errors = parsed.errors();
    if !errors.is_empty() {
        return Err(format!("could not parse Rust source: {errors:?}").into());
    }
    Ok(parsed.tree())
}

fn pat_is_ident(pattern: Option<ast::Pat>, name: &str) -> bool {
    matches!(pattern, Some(ast::Pat::IdentPat(pattern)) if pattern.name().is_some_and(|it| it.text() == name))
}

fn child_of(list: &SyntaxNode, node: &SyntaxNode) -> Result<SyntaxNode, Box<dyn Error>> {
    node.ancestors()
        .find(|candidate| candidate.parent().as_ref() == Some(list))
        .ok_or_else(|| "node is not part of the statement list".into())
}

fn element_range(
    parent: &SyntaxNode,
    first: &SyntaxElement,
    last: &SyntaxElement,
) -> Result<Vec<SyntaxElement>, Box<dyn Error>> {
    let elements = parent.children_with_tokens().collect::<Vec<_>>();
    let start = elements
        .iter()
        .position(|element| element == first)
        .ok_or("range start is not a direct child")?;
    let end = elements
        .iter()
        .position(|element| element == last)
        .ok_or("range end is not a direct child")?;
    if end < start {
        return Err("range end precedes its start".into());
    }
    Ok(elements[start..=end].to_vec())
}

fn elements_between(
    parent: &SyntaxNode,
    open: &SyntaxToken,
    close: &SyntaxToken,
) -> Result<Vec<SyntaxElement>, Box<dyn Error>> {
    let elements = parent.children_with_tokens().collect::<Vec<_>>();
    let start = elements
        .iter()
        .position(|element| element.as_token() == Some(open))
        .ok_or("opening token is not a direct child")?;
    let end = elements
        .iter()
        .position(|element| element.as_token() == Some(close))
        .ok_or("closing token is not a direct child")?;
    if end <= start + 1 {
        return Err("delimited range is empty".into());
    }
    Ok(elements[start + 1..end].to_vec())
}

fn replace_elements(
    editor: &SyntaxEditor,
    old: Vec<SyntaxElement>,
    new: Vec<SyntaxElement>,
) -> Result<(), Box<dyn Error>> {
    let first = old.first().ok_or("nothing to replace")?.clone();
    let last = old.last().ok_or("nothing to replace")?.clone();
    editor.replace_all(first..=last, new);
    Ok(())
}

fn indent_before(element: &SyntaxElement) -> String {
    let whitespace = match element.prev_sibling_or_token() {
        Some(previous) if previous.kind() == SyntaxKind::WHITESPACE => previous,
        _ => return String::new(),
    };
    let text = whitespace.to_string();
    text.rsplit('\n').next().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_use_before_existing_imports() {
        let mut source = String::from("#![allow(clippy::all)]\n\nuse std::path::Path;\n");

        add_use(&mut source, None, "crate::patched::run_flycheck").unwrap();

        assert_eq!(
            source,
            "#![allow(clippy::all)]\n\nuse crate::patched::run_flycheck;\nuse std::path::Path;\n"
        );
    }
}
