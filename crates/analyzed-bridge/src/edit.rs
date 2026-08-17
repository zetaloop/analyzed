use std::{error::Error, path::Path};

use ra_ap_syntax::{
    AstNode, Edition, SourceFile, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken,
    ast::{
        self, HasArgList, HasLoopBody, HasName, HasVisibility,
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
    calls_in(scope.syntax(), name)
}

fn calls_in(scope: &SyntaxNode, name: &str) -> impl Iterator<Item = ast::MethodCallExpr> {
    scope
        .descendants()
        .filter_map(ast::MethodCallExpr::cast)
        .filter(|call| call.name_ref().is_some_and(|it| it.text() == name))
        .collect::<Vec<_>>()
        .into_iter()
}

pub fn arms(
    scope: &impl AstNode,
    type_name: &str,
    variant_name: &str,
) -> impl Iterator<Item = ast::MatchArm> {
    scope
        .syntax()
        .descendants()
        .filter_map(ast::MatchArm::cast)
        .filter(move |arm| {
            arm.pat().is_some_and(|pat| {
                pat.syntax()
                    .descendants()
                    .filter_map(ast::Path::cast)
                    .any(|path| path_ends_with(&path, &[type_name, variant_name]))
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
                make::tokens::whitespace(" ").into(),
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
                make::tokens::whitespace("\n").into(),
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
                make::tokens::whitespace(" ").into(),
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

fn record_exprs_in(function: &ast::Fn, record_name: &str) -> Vec<ast::RecordExpr> {
    function
        .syntax()
        .descendants()
        .filter_map(ast::RecordExpr::cast)
        .filter(|record| {
            record.path().is_some_and(|path| {
                path.segment()
                    .and_then(|segment| segment.name_ref())
                    .is_some_and(|name| name.text() == record_name)
            })
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
    record_name: &str,
    fields: &[FieldInit<'_>],
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function: ast::Fn = named(&root, function)?;
    for record in record_exprs_in(&function, record_name) {
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
    Err(format!("function has no `{record_name}` record expression").into())
}

pub fn set_record_field(
    source: &mut String,
    function: &str,
    record_name: &str,
    field: &str,
    value: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function: ast::Fn = named(&root, function)?;
    for record in record_exprs_in(&function, record_name) {
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
    Err(format!("function has no `{record_name}.{field}` field").into())
}

pub fn add_rest_pattern(
    source: &mut String,
    function: &str,
    record_name: &str,
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
        if path
            .segment()
            .and_then(|segment| segment.name_ref())
            .is_none_or(|name| name.text() != record_name)
        {
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
                make::tokens::whitespace(" ").into(),
                make::rest_pat().syntax().clone().into(),
            ],
        );
        return commit(source, editor);
    }
    Err(format!("function has no `{record_name}` record pattern").into())
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
    insert_use(source, item)
}

pub fn add_use_alias(
    source: &mut String,
    visibility: Option<&str>,
    path: &str,
    alias: &str,
) -> Result<(), Box<dyn Error>> {
    let visibility = visibility.map_or(String::new(), |visibility| format!("{visibility} "));
    let file = parse_file(&format!("{visibility}use {path} as {alias};"))?;
    let item = one(
        file.syntax().children().filter_map(ast::Use::cast),
        "use item",
    )?;
    insert_use(source, item)
}

fn insert_use(source: &mut String, item: ast::Use) -> Result<(), Box<dyn Error>> {
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
            make::tokens::whitespace("\n").into(),
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

pub fn mount_module(
    source: &mut String,
    visibility: Option<&str>,
    name: &str,
    path: &Path,
) -> Result<(), Box<dyn Error>> {
    let visibility = visibility.map_or(String::new(), |visibility| format!("{visibility} "));
    let file = parse_file(&format!(
        "#[path = {:?}]\n{visibility}mod {name};",
        path.to_string_lossy()
    ))?;
    let module = one(
        file.syntax().children().filter_map(ast::Module::cast),
        "module in wrapper",
    )?;
    let (editor, root) = open(source)?;
    let anchor = root
        .children()
        .find(|node| ast::Item::can_cast(node.kind()))
        .ok_or("source has no items")?;
    editor.insert_all(
        Position::before(&anchor),
        vec![
            module.syntax().clone().into(),
            make::tokens::whitespace("\n\n").into(),
        ],
    );
    commit(source, editor)
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

pub struct Function<'a> {
    pub name: &'a str,
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

pub fn extract_match_arm(
    source: &mut String,
    scope: Scope<'_>,
    function: Function<'_>,
) -> Result<(), Box<dyn Error>> {
    let Scope::MatchArm {
        function: parent_name,
        type_name,
        variant_name,
    } = scope
    else {
        return Err("match-arm extraction requires a match-arm scope".into());
    };
    let (editor, root) = open(source)?;
    let parent: ast::Fn = named(&root, parent_name)?;
    let arm = one(
        arms(&parent, type_name, variant_name),
        &format!("`{type_name}::{variant_name}` arm in `{parent_name}`"),
    )?;
    let expression = arm.expr().ok_or_else(|| {
        format!("`{type_name}::{variant_name}` arm in `{parent_name}` has no expression")
    })?;
    let body = expression.clone().syntax().clone();
    let call = make::expr_call(
        path_expr(function.name)?,
        make::arg_list(
            function
                .args
                .iter()
                .map(|arg| expr_node(arg))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    let call: ast::Expr = make::block_expr(std::iter::empty(), Some(call.into())).into();
    editor.replace(expression.syntax(), call.syntax().clone());

    let params = make::param_list(
        None,
        function.params.iter().map(|param| {
            make::param(
                make::ident_pat(false, false, make::name(param.name)).into(),
                make::ty(param.ty),
            )
        }),
    );
    let function_node = make::fn_(
        std::iter::empty(),
        None,
        make::name(function.name),
        None,
        None,
        params,
        make::block_expr(std::iter::empty(), None),
        function.return_ty.map(|ty| make::ret_type(make::ty(ty))),
        false,
        false,
        false,
        false,
    );
    let (function_editor, function_node) = SyntaxEditor::with_ast_node(&function_node);
    let list = function_node
        .body()
        .and_then(|body| body.stmt_list())
        .ok_or("extracted function has no statement list")?;
    function_editor.insert_all(
        Position::after(
            list.l_curly_token()
                .ok_or("extracted function has no opening brace")?,
        ),
        vec![
            make::tokens::whitespace("\n    ").into(),
            body.into(),
            make::tokens::whitespace("\n").into(),
        ],
    );
    let function_node = ast::Fn::cast(function_editor.finish().new_root().clone())
        .ok_or("extracted function is not a function")?;
    let level = IndentLevel::from_node(parent.syntax());
    editor.insert_all(
        Position::after(parent.syntax()),
        vec![
            make::tokens::whitespace(&format!("\n\n{level}")).into(),
            function_node.syntax().clone().into(),
        ],
    );
    commit(source, editor)
}

pub struct MatchArm<'a> {
    pub pattern: &'a str,
    pub expression: &'a str,
}

pub fn append_match_arms(
    source: &mut String,
    scope: Scope<'_>,
    items: &[MatchArm<'_>],
) -> Result<(), Box<dyn Error>> {
    let Scope::MatchArm {
        function,
        type_name,
        variant_name,
    } = scope
    else {
        return Err("match-arm append requires a match-arm scope".into());
    };
    let (editor, root) = open(source)?;
    let function_node: ast::Fn = named(&root, function)?;
    let anchor = one(
        arms(&function_node, type_name, variant_name),
        &format!("`{type_name}::{variant_name}` arm in `{function}`"),
    )?;
    let list = anchor
        .syntax()
        .parent()
        .and_then(ast::MatchArmList::cast)
        .ok_or_else(|| format!("`{type_name}::{variant_name}` is not in a match arm list"))?;
    let close = list
        .r_curly_token()
        .ok_or_else(|| format!("match arm list in `{function}` has no closing brace"))?;
    let trailing = close
        .prev_sibling_or_token()
        .filter(|element| element.kind() == SyntaxKind::WHITESPACE);
    let position = trailing.clone().unwrap_or_else(|| close.clone().into());
    let level = IndentLevel::from_node(anchor.syntax());
    let mut elements = Vec::with_capacity(items.len() * 2 + 1);
    for item in items {
        elements.extend([
            make::tokens::whitespace(&format!("\n{level}")).into(),
            make::match_arm(
                make::path_pat(make::path_from_text(item.pattern)),
                None,
                expr_node(item.expression)?,
            )
            .syntax()
            .clone()
            .into(),
        ]);
    }
    if trailing.is_none() {
        let level = IndentLevel::from_node(list.syntax());
        elements.push(make::tokens::whitespace(&format!("\n{level}")).into());
    }
    editor.insert_all(Position::before(position), elements);
    commit(source, editor)
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
    let arguments = make::arg_list(
        method
            .args
            .iter()
            .map(|arg| expr_node(arg))
            .collect::<Result<Vec<_>, _>>()?,
    );
    let call: ast::Expr = match method.receiver {
        Some(_) => make::expr_method_call(
            make::ext::expr_self(),
            make::name_ref(method.name),
            arguments,
        )
        .into(),
        None => make::expr_call(path_expr(method.name)?, arguments).into(),
    };
    let mut body = match selection.kind {
        SelectionKind::Statement { statement } => {
            let region = vec![SyntaxElement::from(statement.clone())];
            editor.replace(&statement, make::expr_stmt(call).syntax().clone());
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
                    make::expr_stmt(call).syntax().clone().into(),
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
                    make::expr_stmt(call).syntax().clone().into(),
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

#[derive(Clone, Copy)]
pub enum Scope<'a> {
    Function(&'a str),
    ForLoop {
        function: &'a str,
    },
    MethodArgument {
        function: &'a str,
        method: &'a str,
    },
    IfLet {
        function: &'a str,
        type_name: &'a str,
        variant_name: &'a str,
    },
    MatchArm {
        function: &'a str,
        type_name: &'a str,
        variant_name: &'a str,
    },
}

struct ResolvedScope {
    syntax: SyntaxNode,
    statements: ast::StmtList,
    description: String,
}

impl Scope<'_> {
    fn resolve(self, root: &SyntaxNode) -> Result<ResolvedScope, Box<dyn Error>> {
        let function_name = match self {
            Scope::Function(function)
            | Scope::ForLoop { function }
            | Scope::MethodArgument { function, .. }
            | Scope::IfLet { function, .. }
            | Scope::MatchArm { function, .. } => function,
        };
        let function: ast::Fn = named(root, function_name)?;
        let (syntax, statements, description) = match self {
            Scope::Function(_) => {
                let body = function
                    .body()
                    .ok_or_else(|| format!("function `{function_name}` has no body"))?;
                let statements = body
                    .stmt_list()
                    .ok_or_else(|| format!("function `{function_name}` has no statement list"))?;
                (
                    function.syntax().clone(),
                    statements,
                    format!("`{function_name}`"),
                )
            }
            Scope::ForLoop { .. } => {
                let loop_expr = one(
                    function
                        .syntax()
                        .descendants()
                        .filter_map(ast::ForExpr::cast),
                    &format!("for loop in `{function_name}`"),
                )?;
                let statements = loop_expr
                    .loop_body()
                    .and_then(|body| body.stmt_list())
                    .ok_or_else(|| {
                        format!("for loop in `{function_name}` has no statement list")
                    })?;
                (
                    loop_expr.syntax().clone(),
                    statements,
                    format!("for loop in `{function_name}`"),
                )
            }
            Scope::MethodArgument { method, .. } => {
                let call = one(
                    calls_in(function.syntax(), method),
                    &format!("`{method}` call in `{function_name}`"),
                )?;
                let block = one(
                    call.arg_list()
                        .into_iter()
                        .flat_map(|arguments| arguments.args())
                        .filter_map(|argument| match argument {
                            ast::Expr::BlockExpr(block) => Some(block),
                            _ => None,
                        }),
                    &format!("block argument to `{method}` in `{function_name}`"),
                )?;
                let statements = block.stmt_list().ok_or_else(|| {
                    format!(
                        "block argument to `{method}` in `{function_name}` has no statement list"
                    )
                })?;
                (
                    block.syntax().clone(),
                    statements,
                    format!("block argument to `{method}` in `{function_name}`"),
                )
            }
            Scope::IfLet {
                type_name,
                variant_name,
                ..
            } => {
                let branch = one(
                    function
                        .syntax()
                        .descendants()
                        .filter_map(ast::IfExpr::cast)
                        .filter(|branch| {
                            branch.condition().is_some_and(|condition| {
                                condition
                                    .syntax()
                                    .descendants()
                                    .filter_map(ast::LetExpr::cast)
                                    .filter_map(|let_expr| let_expr.pat())
                                    .any(|pattern| {
                                        pattern
                                            .syntax()
                                            .descendants()
                                            .filter_map(ast::Path::cast)
                                            .any(|path| {
                                                path_ends_with(&path, &[type_name, variant_name])
                                            })
                                    })
                            })
                        }),
                    &format!("`if let {type_name}::{variant_name}` branch in `{function_name}`"),
                )?;
                let block = branch.then_branch().ok_or_else(|| {
                    format!(
                        "`if let {type_name}::{variant_name}` branch in `{function_name}` has no body"
                    )
                })?;
                let statements = block.stmt_list().ok_or_else(|| {
                    format!(
                        "`if let {type_name}::{variant_name}` branch in `{function_name}` has no statement list"
                    )
                })?;
                (
                    block.syntax().clone(),
                    statements,
                    format!("`if let {type_name}::{variant_name}` branch in `{function_name}`"),
                )
            }
            Scope::MatchArm {
                type_name,
                variant_name,
                ..
            } => {
                let arm = one(
                    arms(&function, type_name, variant_name),
                    &format!("`{type_name}::{variant_name}` arm in `{function_name}`"),
                )?;
                let statements = match arm.expr() {
                    Some(ast::Expr::BlockExpr(block)) => block.stmt_list().ok_or_else(|| {
                        format!(
                            "`{type_name}::{variant_name}` arm in `{function_name}` has no statement list"
                        )
                    })?,
                    _ => {
                        return Err(format!(
                            "`{type_name}::{variant_name}` arm in `{function_name}` is not a block"
                        )
                        .into());
                    }
                };
                (
                    arm.syntax().clone(),
                    statements,
                    format!("`{type_name}::{variant_name}` arm in `{function_name}`"),
                )
            }
        };
        Ok(ResolvedScope {
            syntax,
            statements,
            description,
        })
    }
}

fn method_call_is_in_scope(call: &SyntaxNode, scope: &ResolvedScope) -> bool {
    match scope.syntax.kind() {
        SyntaxKind::CLOSURE_EXPR => call.ancestors().any(|ancestor| ancestor == scope.syntax),
        _ => call
            .ancestors()
            .filter_map(ast::StmtList::cast)
            .next()
            .is_some_and(|list| list == scope.statements),
    }
}

pub fn redirect_call(
    source: &mut String,
    scope: Scope<'_>,
    from: &str,
    to: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let scope = scope.resolve(&root)?;
    let expected =
        path_names(&named_path(from)?).ok_or_else(|| format!("`{from}` is not a named path"))?;
    let methods = calls_in(&scope.syntax, from)
        .filter(|call| method_call_is_in_scope(call.syntax(), &scope))
        .filter_map(|call| call.name_ref().map(|name| (name.syntax().clone(), true)));
    let functions = scope
        .syntax
        .descendants()
        .filter_map(ast::CallExpr::cast)
        .filter_map(|call| match call.expr() {
            Some(ast::Expr::PathExpr(path)) => path.path(),
            _ => None,
        })
        .filter(|path| path_names(path).as_ref() == Some(&expected))
        .map(|path| (path.syntax().clone(), false));
    let (callee, method) = one(
        methods.chain(functions),
        &format!("`{from}` call in {}", scope.description),
    )?;

    let replacement = named_path(to)?;
    if method {
        let replacement = path_names(&replacement)
            .filter(|segments| segments.len() == 1)
            .ok_or_else(|| format!("method replacement `{to}` is not a name"))?;
        editor.replace(callee, make::name_ref(&replacement[0]).syntax().clone());
    } else {
        editor.replace(callee, replacement.syntax().clone());
    }
    commit(source, editor)
}

pub fn set_parameter_type(
    source: &mut String,
    function: &str,
    parameter_name: &str,
    ty: &str,
) -> Result<(), Box<dyn Error>> {
    let (editor, root) = open(source)?;
    let function_node: ast::Fn = named(&root, function)?;
    let params = function_node
        .param_list()
        .ok_or_else(|| format!("function `{function}` has no parameter list"))?;
    let parameter = one(
        params.params().filter(|param| {
            matches!(param.pat(), Some(ast::Pat::IdentPat(pattern)) if pattern.name().is_some_and(|name| name.text() == parameter_name))
        }),
        &format!("parameter `{parameter_name}` in `{function}`"),
    )?;
    let old_type = parameter
        .ty()
        .ok_or_else(|| format!("parameter `{parameter_name}` has no type"))?;
    editor.replace(old_type.syntax(), make::ty(ty).syntax().clone());
    commit(source, editor)
}

pub enum ClosureContext<'a> {
    Move(&'a str),
    Clone(&'a str),
}

pub enum Call<'a> {
    Function(&'a str),
    Method(&'a str),
}

pub struct ClosureDelegate<'a> {
    pub scope: Scope<'a>,
    pub call: Call<'a>,
    pub helper: &'a str,
    pub context: &'a [ClosureContext<'a>],
    pub params: &'a [Param<'a>],
}

pub fn delegate_closure(
    source: &mut String,
    delegate: ClosureDelegate<'_>,
) -> Result<(), Box<dyn Error>> {
    let ClosureDelegate {
        scope,
        call,
        helper,
        context,
        params,
    } = delegate;
    let (editor, root) = open(source)?;
    let scope = scope.resolve(&root)?;
    let (arguments, call_name) = match call {
        Call::Function(function) => {
            let expected = path_expr(function)?;
            let ast::Expr::PathExpr(expected) = expected else {
                unreachable!();
            };
            let expected = expected
                .path()
                .and_then(|path| path_names(&path))
                .ok_or_else(|| format!("`{function}` is not a named path"))?;
            let call = one(
                scope
                    .syntax
                    .descendants()
                    .filter_map(ast::CallExpr::cast)
                    .filter(|call| {
                        matches!(
                            call.expr(),
                            Some(ast::Expr::PathExpr(path))
                                if path.path().and_then(|path| path_names(&path)).as_ref()
                                    == Some(&expected)
                        )
                    }),
                &format!("`{function}` call in {}", scope.description),
            )?;
            (
                call.arg_list().ok_or_else(|| {
                    format!(
                        "`{function}` call in {} has no argument list",
                        scope.description
                    )
                })?,
                function,
            )
        }
        Call::Method(method) => {
            let call = one(
                calls_in(&scope.syntax, method),
                &format!("`{method}` call in {}", scope.description),
            )?;
            (
                call.arg_list().ok_or_else(|| {
                    format!(
                        "`{method}` call in {} has no argument list",
                        scope.description
                    )
                })?,
                method,
            )
        }
    };
    let closure = one(
        arguments.args().filter_map(|argument| match argument {
            ast::Expr::ClosureExpr(closure) => Some(closure),
            ast::Expr::BlockExpr(block) => match block.tail_expr() {
                Some(ast::Expr::ClosureExpr(closure)) => Some(closure),
                _ => None,
            },
            _ => None,
        }),
        &format!(
            "closure-valued argument to `{call_name}` in {}",
            scope.description
        ),
    )?;

    let mut helper_arguments = Vec::with_capacity(context.len() + 1);
    for item in context {
        helper_arguments.push(match item {
            ClosureContext::Move(identifier) => identifier_expr(identifier)?,
            ClosureContext::Clone(identifier) => make::expr_method_call(
                identifier_expr(identifier)?,
                make::name_ref("clone"),
                make::arg_list(std::iter::empty()),
            )
            .into(),
        });
    }
    let original_closure = closure.clone();
    let closure = replace_closure_params(closure, params, call_name, &scope.description)?;
    helper_arguments.push(closure.into());
    let helper = path_expr(helper)?;
    let delegate = make::expr_call(helper, make::arg_list(helper_arguments));
    editor.replace(original_closure.syntax(), delegate.syntax().clone());
    commit(source, editor)
}

fn replace_closure_params(
    closure: ast::ClosureExpr,
    params: &[Param<'_>],
    call_name: &str,
    scope: &str,
) -> Result<ast::ClosureExpr, Box<dyn Error>> {
    let (editor, root) = SyntaxEditor::new(closure.syntax().clone());
    let old_params = ast::ClosureExpr::cast(root)
        .expect("closure editor root must be a closure")
        .param_list()
        .ok_or_else(|| format!("closure argument to `{call_name}` in {scope} has no parameters"))?;
    let new_params = make::expr_closure(
        params.iter().map(|param| {
            make::param(
                make::ident_pat(false, false, make::name(param.name)).into(),
                make::ty(param.ty),
            )
        }),
        make::ext::expr_unit(),
    )
    .param_list()
    .expect("generated closure must have parameters");
    editor.replace(old_params.syntax(), new_params.syntax().clone());
    ast::ClosureExpr::cast(editor.finish().new_root().clone())
        .ok_or_else(|| "edited closure is not a closure".into())
}

fn path_names(path: &ast::Path) -> Option<Vec<String>> {
    path.segments()
        .map(|segment| segment.name_ref().map(|name| name.text().to_string()))
        .collect()
}

fn path_ends_with(path: &ast::Path, names: &[&str]) -> bool {
    path_names(path).is_some_and(|segments| {
        segments.len() >= names.len()
            && segments[segments.len() - names.len()..]
                .iter()
                .map(String::as_str)
                .eq(names.iter().copied())
    })
}

fn identifier_expr(identifier: &str) -> Result<ast::Expr, Box<dyn Error>> {
    let expression = path_expr(identifier)?;
    let ast::Expr::PathExpr(path) = &expression else {
        unreachable!();
    };
    let path = path.path().ok_or("identifier has no path")?;
    let name = path
        .segment()
        .and_then(|segment| segment.name_ref())
        .ok_or_else(|| format!("`{identifier}` is not an identifier"))?;
    if path.qualifier().is_some() || name.text() != identifier {
        return Err(format!("`{identifier}` is not an identifier").into());
    }
    Ok(expression)
}

fn path_expr(path: &str) -> Result<ast::Expr, Box<dyn Error>> {
    let expression = expr_node(path)?;
    if matches!(expression, ast::Expr::PathExpr(_)) {
        Ok(expression)
    } else {
        Err(format!("`{path}` is not a path").into())
    }
}

fn named_path(path: &str) -> Result<ast::Path, Box<dyn Error>> {
    let ast::Expr::PathExpr(expression) = path_expr(path)? else {
        unreachable!();
    };
    expression
        .path()
        .ok_or_else(|| format!("`{path}` has no path").into())
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
