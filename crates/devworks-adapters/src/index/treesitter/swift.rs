//! Swift (retained `SwiftCallSites` and the Swift hooks of `LanguageSpecs.Swift`): call sites with
//! argument labels and proven receivers, import declarations, argument-label signatures, the
//! parameter list of a callable's identity, type names, computed properties and the inheritance
//! clause rules.
//!
//! A call's receiver is a type only when the syntax or the file proves it: `self`/`Self` (the
//! enclosing type), an UpperCamelCase name (a type, by Swift naming convention), or a variable
//! whose type is written in the file (a parameter, a local or global declaration, or a property
//! of a type declared in the file, with a type annotation or an initializer-call value). Chains
//! follow property types through types declared in the file. Any other receiver is unresolved.
//! A call of a type (`Circle(radius: 1)`, `Circle.init(radius: 1)`, `.init(...)` with a
//! contextual type) is a call of that type's `init`; a bare call inside a type of a method the
//! type declares in this file is a call on `self`.
use super::spec::{Node, child_by_field, children, named_children, text};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// The retained `SwiftTypeReference` prefix of a symbolic type endpoint.
pub(super) const TYPE_REF: &str = "swift-type:";
/// The retained `ArgumentLabels.AnyLabel` (an unlabeled trailing closure).
const ANY_LABEL: &str = "*";
/// Guards receiver inference against cyclic declarations in malformed source.
const MAX_INFERENCE_DEPTH: usize = 8;

pub(super) fn type_ref(path: &str) -> String {
    format!("{TYPE_REF}{path}")
}

fn is_type_decl(kind: &str) -> bool {
    matches!(kind, "class_declaration" | "protocol_declaration")
}
fn is_callable(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration" | "init_declaration" | "subscript_declaration"
    )
}
fn ancestors(n: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(n.parent(), |p| p.parent())
}
fn key(n: Node<'_>) -> (usize, usize) {
    (n.start_byte(), n.end_byte())
}

/// A call read by the reader (the retained `CallSite`).
pub(super) struct CallSite {
    pub callee: String,
    pub receiver: Option<String>,
    /// `Bare`, `Resolved` or `Unresolved` (the retained `CallReceiverKind`).
    pub kind: &'static str,
    pub labels: String,
}

/// The retained `CallSiteContext`.
pub(super) struct CallContext<'a> {
    pub enclosing_type_qn: Option<&'a str>,
    pub resolve_local_type: &'a dyn Fn(&str) -> Option<String>,
}

/// A receiver's type: its dotted path as written, the receiver reported (a local type's qualified
/// name, else a type reference) and whether the expression names the type itself.
struct TypeRef {
    path: String,
    receiver: String,
    is_type: bool,
}

/// The call-site reader for one file: every type declaration and same-file extension block.
pub(super) struct Calls<'t> {
    src: &'t [u8],
    type_blocks: HashMap<String, Vec<Node<'t>>>,
    block_paths: HashMap<(usize, usize), String>,
}

impl<'t> Calls<'t> {
    pub(super) fn new(src: &'t [u8], root: Node<'t>) -> Self {
        let mut me = Self {
            src,
            type_blocks: HashMap::new(),
            block_paths: HashMap::new(),
        };
        // Pre-order over named children, as the retained recursive IndexTypes.
        let mut stack: Vec<(Node<'t>, Option<String>)> = named_children(root)
            .into_iter()
            .rev()
            .map(|c| (c, None))
            .collect();
        while let Some((child, prefix)) = stack.pop() {
            let mut next = prefix.clone();
            if is_type_decl(child.kind()) {
                let is_extension = child_by_field(child, "declaration_kind")
                    .is_some_and(|k| text(src, k) == "extension");
                let name = if is_extension {
                    type_path(src, child_by_field(child, "name"))
                } else {
                    child_by_field(child, "name").map(|n| text(src, n))
                };
                if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
                    let path = match (&prefix, is_extension) {
                        (Some(p), false) => format!("{p}.{name}"),
                        _ => name,
                    };
                    me.type_blocks.entry(path.clone()).or_default().push(child);
                    me.block_paths.insert(key(child), path.clone());
                    next = Some(path);
                }
            }
            for c in named_children(child).into_iter().rev() {
                stack.push((c, next.clone()));
            }
        }
        me
    }

    fn text(&self, n: Node<'_>) -> String {
        text(self.src, n)
    }

    // -- Calls ------------------------------------------------------------------------------

    pub(super) fn read(&self, call: Node<'t>, cx: &CallContext<'_>) -> Option<CallSite> {
        if call.kind() == "constructor_expression" {
            let constructed = type_path(self.src, child_by_field(call, "constructed_type"))?;
            let suffix = named_children(call)
                .into_iter()
                .find(|c| c.kind() == "constructor_suffix");
            return Some(CallSite {
                callee: "init".into(),
                receiver: Some(resolve_type(&constructed, cx)),
                kind: "Resolved",
                labels: self.labels(suffix),
            });
        }
        let named = named_children(call);
        let suffix = named.iter().find(|c| c.kind() == "call_suffix").copied()?;
        let function = named.first().copied()?;
        if function.kind() == "call_suffix" {
            return None;
        }
        self.read_callee(function, call, self.labels(Some(suffix)), cx)
    }

    fn read_callee(
        &self,
        function: Node<'t>,
        call: Node<'t>,
        labels: String,
        cx: &CallContext<'_>,
    ) -> Option<CallSite> {
        let site = |callee: String, receiver: Option<String>, kind: &'static str| CallSite {
            callee,
            receiver,
            kind,
            labels: labels.clone(),
        };
        match function.kind() {
            "simple_identifier" => {
                let name = self.text(function);
                if is_type_name(&name) {
                    return Some(site(
                        "init".into(),
                        Some(resolve_type(&name, cx)),
                        "Resolved",
                    ));
                }
                // An unqualified call of a method the enclosing type declares is a call on self.
                if let Some(enclosing) = self.enclosing_type_path(call)
                    && self.declares_function(&enclosing, &name)
                {
                    let receiver = cx
                        .enclosing_type_qn
                        .map(str::to_owned)
                        .unwrap_or_else(|| type_ref(&enclosing));
                    return Some(site(name, Some(receiver), "Resolved"));
                }
                // Calling a closure held by a parameter, local or property is not a call of a
                // function of that name.
                if self.is_bound_variable(&name, function) {
                    return Some(site(name, None, "Unresolved"));
                }
                Some(site(name, None, "Bare"))
            }
            "navigation_expression" => {
                let member = child_by_field(function, "suffix")
                    .and_then(|s| child_by_field(s, "suffix"))
                    .filter(|m| m.kind() == "simple_identifier")?;
                let receiver = self.infer_type(child_by_field(function, "target"), cx, 0);
                Some(match receiver {
                    None => site(self.text(member), None, "Unresolved"),
                    Some(r) => site(self.text(member), Some(r.receiver), "Resolved"),
                })
            }
            "prefix_expression" => {
                let target = child_by_field(function, "target")?;
                // `!isValid()` / `-value()`: the operator applies to the call's result.
                if children(function).first().map(|c| c.kind()) != Some(".") {
                    return self.read_callee(target, call, labels, cx);
                }
                // `.init(...)` / `.make(...)`: an implicit member of the contextual type.
                if target.kind() != "simple_identifier" {
                    return None;
                }
                Some(match self.contextual_type(call, cx) {
                    None => site(self.text(target), None, "Unresolved"),
                    Some(r) => site(self.text(target), Some(r.receiver), "Resolved"),
                })
            }
            _ => None,
        }
    }

    /// The call's argument-label signature: one segment per argument (`_` when unlabeled), then
    /// one per trailing closure (`*` for the first, its label for each later one).
    fn labels(&self, suffix: Option<Node<'t>>) -> String {
        let Some(suffix) = suffix else {
            return String::new();
        };
        let mut labels: Vec<String> = Vec::new();
        if let Some(arguments) = named_children(suffix)
            .into_iter()
            .find(|c| c.kind() == "value_arguments")
        {
            for argument in named_children(arguments)
                .into_iter()
                .filter(|c| c.kind() == "value_argument")
            {
                let label = child_by_field(argument, "name")
                    .map(|n| self.text(n).trim().to_owned())
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| "_".into());
                labels.push(label);
            }
        }
        let mut pending: Option<String> = None;
        let mut trailing = 0;
        for (i, child) in children(suffix).into_iter().enumerate() {
            if suffix.field_name_for_child(i as u32) == Some("name") {
                pending = Some(self.text(child));
            } else if child.kind() == "lambda_literal" {
                let label = if trailing == 0 {
                    ANY_LABEL.to_owned()
                } else {
                    pending.clone().unwrap_or_else(|| "_".into())
                };
                trailing += 1;
                labels.push(label);
                pending = None;
            }
        }
        labels.iter().map(|l| format!("{l}:")).collect()
    }

    // -- Receiver inference -----------------------------------------------------------------

    fn infer_type(
        &self,
        expression: Option<Node<'t>>,
        cx: &CallContext<'_>,
        depth: usize,
    ) -> Option<TypeRef> {
        let expression = expression?;
        if depth > MAX_INFERENCE_DEPTH {
            return None;
        }
        match expression.kind() {
            "self_expression" => self.enclosing_type(expression, cx, false),
            "simple_identifier" => {
                let name = self.text(expression);
                if name == "Self" {
                    return self.enclosing_type(expression, cx, true);
                }
                if is_type_name(&name) {
                    let receiver = resolve_type(&name, cx);
                    return Some(TypeRef {
                        path: name,
                        receiver,
                        is_type: true,
                    });
                }
                let path = self.lookup_variable(&name, expression, cx, depth)?;
                Some(TypeRef {
                    receiver: resolve_type(&path, cx),
                    path,
                    is_type: false,
                })
            }
            "navigation_expression" => {
                let owner = self.infer_type(child_by_field(expression, "target"), cx, depth + 1)?;
                let member = child_by_field(expression, "suffix")
                    .and_then(|s| child_by_field(s, "suffix"))
                    .filter(|m| m.kind() == "simple_identifier")?;
                let member = self.text(member);
                if owner.is_type && is_type_name(&member) {
                    let nested = format!("{}.{member}", owner.path);
                    return Some(TypeRef {
                        receiver: resolve_type(&nested, cx),
                        path: nested,
                        is_type: true,
                    });
                }
                let path = self.member_type(&owner.path, &member, cx, depth + 1)?;
                Some(TypeRef {
                    receiver: resolve_type(&path, cx),
                    path,
                    is_type: false,
                })
            }
            // `value!` / `value?` unwrap to the value's type.
            "postfix_expression" => {
                self.infer_type(child_by_field(expression, "target"), cx, depth + 1)
            }
            "try_expression" | "await_expression" => {
                self.infer_type(child_by_field(expression, "expr"), cx, depth + 1)
            }
            // An initializer call yields a value of its type.
            "call_expression" | "constructor_expression" => {
                let path = self.initialized_type_path(expression)?;
                Some(TypeRef {
                    receiver: resolve_type(&path, cx),
                    path,
                    is_type: false,
                })
            }
            _ => None,
        }
    }

    /// The written path of the type an initializer call constructs (`T(...)`, `T.init(...)`),
    /// read from syntax alone. Null for any other call.
    fn initialized_type_path(&self, call: Node<'t>) -> Option<String> {
        if call.kind() == "constructor_expression" {
            return type_path(self.src, child_by_field(call, "constructed_type"));
        }
        let function = named_children(call).into_iter().next()?;
        match function.kind() {
            "simple_identifier" => {
                let t = self.text(function);
                is_type_name(&t).then_some(t)
            }
            "navigation_expression"
                if child_by_field(function, "suffix")
                    .and_then(|s| child_by_field(s, "suffix"))
                    .is_some_and(|s| self.text(s) == "init") =>
            {
                let target = child_by_field(function, "target")?;
                let t = self.text(target);
                (target.kind() == "simple_identifier" && is_type_name(&t)).then_some(t)
            }
            _ => None,
        }
    }

    fn enclosing_type(&self, n: Node<'t>, cx: &CallContext<'_>, is_type: bool) -> Option<TypeRef> {
        let path = self.enclosing_type_path(n)?;
        Some(TypeRef {
            receiver: cx
                .enclosing_type_qn
                .map(str::to_owned)
                .unwrap_or_else(|| type_ref(&path)),
            path,
            is_type,
        })
    }

    /// The type an implicit member expression (`.init(...)`) is resolved against.
    fn contextual_type(&self, call: Node<'t>, cx: &CallContext<'_>) -> Option<TypeRef> {
        let parent = call.parent();
        let mut annotation: Option<Node<'t>> = None;
        match parent.map(|p| p.kind()) {
            Some("property_declaration") => {
                annotation = parent
                    .and_then(|p| {
                        named_children(p)
                            .into_iter()
                            .find(|c| c.kind() == "type_annotation")
                    })
                    .and_then(|a| child_by_field(a, "name"));
            }
            Some("control_transfer_statement") => {
                let parent = parent?;
                let callable = ancestors(parent)
                    .find(|a| is_callable(a.kind()) || a.kind() == "lambda_literal");
                annotation = callable
                    .filter(|c| c.kind() == "function_declaration")
                    .and_then(|c| child_by_field(c, "return_type"));
            }
            _ => {}
        }
        if annotation.is_some_and(|a| self.text(a) == "Self") {
            return self.enclosing_type(call, cx, true);
        }
        let path = type_path(self.src, annotation)?;
        Some(TypeRef {
            receiver: resolve_type(&path, cx),
            path,
            is_type: true,
        })
    }

    /// The written type of the variable `name` visible at `from`, or None when it is unknown
    /// (including when a closure parameter or loop variable shadows it).
    fn lookup_variable(
        &self,
        name: &str,
        from: Node<'t>,
        cx: &CallContext<'_>,
        depth: usize,
    ) -> Option<String> {
        let mut child = from;
        let mut scope = from.parent();
        while let Some(s) = scope {
            match s.kind() {
                "statements" | "source_file" => {
                    let mut binding: Option<Node<'t>> = None;
                    for statement in named_children(s) {
                        if s.kind() == "statements" && statement.start_byte() >= child.start_byte()
                        {
                            break;
                        }
                        if (statement.kind() == "property_declaration"
                            && self.bound_name(statement).as_deref() == Some(name))
                            || (statement.kind() == "guard_statement"
                                && self.binds_name(statement, name))
                        {
                            binding = Some(statement);
                        }
                    }
                    if let Some(b) = binding {
                        return if b.kind() == "property_declaration" {
                            self.declared_type(b, cx, depth + 1)
                        } else {
                            self.conditional_binding_type(b, name, cx, depth + 1)
                        };
                    }
                }
                "if_statement" | "while_statement" if self.binds_name(s, name) => {
                    return self.conditional_binding_type(s, name, cx, depth + 1);
                }
                "for_statement"
                    if child_by_field(s, "item")
                        .and_then(|i| child_by_field(i, "bound_identifier"))
                        .is_some_and(|b| self.text(b) == name) =>
                {
                    return None;
                }
                "lambda_literal" if self.closure_parameters(s).iter().any(|p| p == name) => {
                    return None;
                }
                "function_declaration" | "init_declaration" | "subscript_declaration" => {
                    for parameter in named_children(s)
                        .into_iter()
                        .filter(|c| c.kind() == "parameter")
                    {
                        let (_, internal, ty) = self.parameter_parts(parameter);
                        if internal.as_deref() == Some(name) {
                            return type_path(self.src, ty);
                        }
                    }
                }
                // Inside a type body, an unqualified name may be a property of the type.
                "class_body" | "protocol_body" | "enum_class_body" => {
                    if let Some(path) = s.parent().and_then(|d| self.path_of(d))
                        && let Some(t) = self.member_type(&path, name, cx, depth + 1)
                    {
                        return Some(t);
                    }
                }
                _ => {}
            }
            child = s;
            scope = s.parent();
        }
        None
    }

    /// Whether `name` names a variable visible at `from` (parameter, closure parameter, loop
    /// variable, `if let`/`guard let` binding, local or global declaration, or a property of an
    /// enclosing type declared in the file).
    fn is_bound_variable(&self, name: &str, from: Node<'t>) -> bool {
        let mut child = from;
        let mut scope = from.parent();
        while let Some(s) = scope {
            let bound = match s.kind() {
                "statements" | "source_file" => named_children(s)
                    .into_iter()
                    .take_while(|st| {
                        s.kind() != "statements" || st.start_byte() < child.start_byte()
                    })
                    .any(|st| {
                        (st.kind() == "property_declaration"
                            && self.bound_name(st).as_deref() == Some(name))
                            || (st.kind() == "guard_statement" && self.binds_name(st, name))
                    }),
                "if_statement" | "while_statement" => self.binds_name(s, name),
                "for_statement" => child_by_field(s, "item")
                    .and_then(|i| child_by_field(i, "bound_identifier"))
                    .is_some_and(|b| self.text(b) == name),
                "lambda_literal" => self.closure_parameters(s).iter().any(|p| p == name),
                "function_declaration" | "init_declaration" | "subscript_declaration" => {
                    named_children(s).into_iter().any(|p| {
                        p.kind() == "parameter"
                            && self.parameter_parts(p).1.as_deref() == Some(name)
                    })
                }
                "class_body" | "protocol_body" | "enum_class_body" => s
                    .parent()
                    .and_then(|d| self.path_of(d))
                    .and_then(|path| self.type_blocks.get(&path))
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            child_by_field(*b, "body").is_some_and(|body| {
                                named_children(body).into_iter().any(|m| {
                                    matches!(
                                        m.kind(),
                                        "property_declaration" | "protocol_property_declaration"
                                    ) && self.bound_name(m).as_deref() == Some(name)
                                })
                            })
                        })
                    }),
                _ => false,
            };
            if bound {
                return true;
            }
            child = s;
            scope = s.parent();
        }
        false
    }

    /// The written type of property `member` of the type at `type_path`.
    fn member_type(
        &self,
        type_path: &str,
        member: &str,
        cx: &CallContext<'_>,
        depth: usize,
    ) -> Option<String> {
        if depth > MAX_INFERENCE_DEPTH {
            return None;
        }
        for block in self.type_blocks.get(type_path)? {
            let Some(body) = child_by_field(*block, "body") else {
                continue;
            };
            for declaration in named_children(body) {
                if matches!(
                    declaration.kind(),
                    "property_declaration" | "protocol_property_declaration"
                ) && self.bound_name(declaration).as_deref() == Some(member)
                    && let Some(t) = self.declared_type(declaration, cx, depth + 1)
                {
                    return Some(t);
                }
            }
        }
        None
    }

    /// A declaration's type annotation, else the type of its initial value.
    fn declared_type(
        &self,
        declaration: Node<'t>,
        cx: &CallContext<'_>,
        depth: usize,
    ) -> Option<String> {
        if let Some(annotation) = named_children(declaration)
            .into_iter()
            .find(|c| c.kind() == "type_annotation")
        {
            return type_path(
                self.src,
                child_by_field(annotation, "name")
                    .or_else(|| named_children(annotation).into_iter().last()),
            );
        }
        self.infer_type(child_by_field(declaration, "value"), cx, depth)
            .filter(|v| !v.is_type)
            .map(|v| v.path)
    }

    /// The type an `if let` / `guard let` binding of `name` unwraps.
    fn conditional_binding_type(
        &self,
        statement: Node<'t>,
        name: &str,
        cx: &CallContext<'_>,
        depth: usize,
    ) -> Option<String> {
        let all = children(statement);
        let index = all.iter().enumerate().position(|(i, c)| {
            statement.field_name_for_child(i as u32) == Some("bound_identifier")
                && self.text(*c) == name
        })?;
        let value = all[index + 1..]
            .iter()
            .find(|c| c.is_named() && c.kind() != "value_binding_pattern")
            .copied();
        // `if let value {` (Swift 5.7 shorthand) unwraps the outer variable of the same name.
        match value {
            None => self.lookup_variable(name, statement, cx, depth + 1),
            Some(v) if matches!(v.kind(), "statements" | "else") => {
                self.lookup_variable(name, statement, cx, depth + 1)
            }
            Some(v) => self
                .infer_type(Some(v), cx, depth)
                .filter(|t| !t.is_type)
                .map(|t| t.path),
        }
    }

    fn binds_name(&self, statement: Node<'t>, name: &str) -> bool {
        children(statement).into_iter().enumerate().any(|(i, c)| {
            statement.field_name_for_child(i as u32) == Some("bound_identifier")
                && self.text(c) == name
        })
    }

    fn closure_parameters(&self, closure: Node<'t>) -> Vec<String> {
        let mut names = Vec::new();
        let Some(ty) = child_by_field(closure, "type") else {
            return names;
        };
        let mut stack: Vec<Node<'t>> = named_children(ty).into_iter().rev().collect();
        while let Some(n) = stack.pop() {
            if n.kind() == "lambda_parameter"
                && let Some(p) = child_by_field(n, "name")
            {
                names.push(self.text(p));
            } else if n.kind() == "simple_identifier"
                && n.parent()
                    .is_some_and(|p| p.kind() == "lambda_function_type_parameters")
            {
                names.push(self.text(n));
            }
            stack.extend(named_children(n).into_iter().rev());
        }
        names
    }

    fn bound_name(&self, declaration: Node<'t>) -> Option<String> {
        let pattern = child_by_field(declaration, "name")?;
        Some(
            child_by_field(pattern, "bound_identifier")
                .map(|b| self.text(b))
                .unwrap_or_else(|| self.text(pattern)),
        )
    }

    // -- Types declared in the file ---------------------------------------------------------

    fn path_of(&self, declaration: Node<'_>) -> Option<String> {
        self.block_paths.get(&key(declaration)).cloned()
    }

    /// Path of the type (or extended type) whose body contains `n`.
    fn enclosing_type_path(&self, n: Node<'t>) -> Option<String> {
        ancestors(n)
            .filter(|a| is_type_decl(a.kind()))
            .find_map(|a| self.path_of(a))
    }

    fn declares_function(&self, type_path: &str, name: &str) -> bool {
        self.type_blocks.get(type_path).is_some_and(|blocks| {
            blocks.iter().any(|b| {
                child_by_field(*b, "body").is_some_and(|body| {
                    named_children(body).into_iter().any(|m| {
                        matches!(
                            m.kind(),
                            "function_declaration" | "protocol_function_declaration"
                        ) && child_by_field(m, "name").is_some_and(|n| self.text(n) == name)
                    })
                })
            })
        })
    }

    fn parameter_parts(
        &self,
        parameter: Node<'t>,
    ) -> (Option<String>, Option<String>, Option<Node<'t>>) {
        parameter_parts(self.src, parameter)
    }
}

/// A receiver type: a local type's qualified name, else a type reference to the dotted path.
fn resolve_type(path: &str, cx: &CallContext<'_>) -> String {
    (cx.resolve_local_type)(path).unwrap_or_else(|| type_ref(path))
}

/// Swift types are UpperCamelCase; values and functions are lowerCamelCase.
fn is_type_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

// -- Shared syntax helpers ------------------------------------------------------------------

/// Dotted path of a nominal type without generic arguments (`T?`/`T!` unwrap to `T`); None for
/// structural types.
pub(super) fn type_path(src: &[u8], ty: Option<Node<'_>>) -> Option<String> {
    let ty = ty?;
    match ty.kind() {
        "type_identifier" => Some(text(src, ty)),
        "optional_type" => type_path(
            src,
            child_by_field(ty, "wrapped").or_else(|| named_children(ty).into_iter().next()),
        ),
        "user_type" => {
            let parts: Vec<String> = named_children(ty)
                .into_iter()
                .filter(|c| c.kind() == "type_identifier")
                .map(|c| text(src, c))
                .collect();
            (!parts.is_empty()).then(|| parts.join("."))
        }
        _ => None,
    }
}

/// A parameter's external label (if written), internal name and type node.
fn parameter_parts<'t>(
    src: &[u8],
    parameter: Node<'t>,
) -> (Option<String>, Option<String>, Option<Node<'t>>) {
    let mut internal: Option<String> = None;
    let mut ty: Option<Node<'t>> = None;
    for (i, c) in children(parameter).into_iter().enumerate() {
        if parameter.field_name_for_child(i as u32) != Some("name") {
            continue;
        }
        if c.kind() == "simple_identifier" && internal.is_none() {
            internal = Some(text(src, c));
        } else if ty.is_none() {
            ty = Some(c);
        }
    }
    (
        child_by_field(parameter, "external_name").map(|e| text(src, e)),
        internal,
        ty,
    )
}

/// A declaration's parameter list as `(label:Type,...)`: each parameter's external label (else
/// its name, `_` when unlabeled) and its written type.
pub(super) fn declaration_parameters(src: &[u8], declaration: Node<'_>) -> String {
    let parts: Vec<String> = named_children(declaration)
        .into_iter()
        .filter(|c| c.kind() == "parameter")
        .map(|p| {
            let (external, internal, _) = parameter_parts(src, p);
            let t = text(src, p);
            let ty = t.find(':').map_or("", |c| t[c + 1..].trim());
            format!(
                "{}:{ty}",
                external.or(internal).unwrap_or_else(|| "_".into())
            )
        })
        .collect();
    format!("({})", parts.join(","))
}

/// A function's or initializer's argument-label signature: each parameter's external label, else
/// its name; `=` marks a default value and `...` a variadic parameter.
pub(super) fn declaration_labels(src: &[u8], declaration: Node<'_>) -> Option<String> {
    if !matches!(
        declaration.kind(),
        "function_declaration" | "init_declaration" | "protocol_function_declaration"
    ) {
        return None;
    }
    let mut labels: Vec<String> = Vec::new();
    for (i, c) in children(declaration).into_iter().enumerate() {
        if c.kind() == "parameter" {
            let (external, internal, _) = parameter_parts(src, c);
            let label = external.or(internal).unwrap_or_else(|| "_".into());
            labels.push(if text(src, c).trim_end().ends_with("...") {
                format!("{label}...")
            } else {
                label
            });
        } else if declaration.field_name_for_child(i as u32) == Some("default_value")
            && let Some(last) = labels.last_mut()
        {
            last.push('=');
        }
    }
    Some(labels.iter().map(|l| format!("{l}:")).collect())
}

/// A read import (the retained `ImportDeclaration`).
pub(super) struct Import {
    pub module: String,
    pub symbol: Option<String>,
    pub symbol_kind: Option<String>,
    pub exported: bool,
}

/// `import M`, `@testable import M`, `import M.Sub`; `import struct M.Symbol` (or class, enum,
/// protocol, typealias, func, let, var) imports one symbol of module `M`.
pub(super) fn read_import(src: &[u8], declaration: Node<'_>) -> Option<Import> {
    let path = named_children(declaration)
        .into_iter()
        .find(|c| c.kind() == "identifier");
    let parts: Vec<String> = path
        .map(|p| {
            named_children(p)
                .into_iter()
                .filter(|c| c.kind() == "simple_identifier")
                .map(|c| text(src, c))
                .collect()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        return None;
    }
    let all = children(declaration);
    let kind = all
        .iter()
        .find(|c| {
            !c.is_named()
                && matches!(
                    c.kind(),
                    "typealias" | "struct" | "class" | "enum" | "protocol" | "let" | "var" | "func"
                )
        })
        .map(|c| c.kind().to_owned());
    // `@_exported import M` re-exports M to the importers of this file's module.
    let exported = all
        .iter()
        .take_while(|c| c.kind() != "import")
        .any(|c| exported_attribute(&text(src, *c)));
    Some(match kind {
        Some(k) if parts.len() > 1 => Import {
            module: parts[..parts.len() - 1].join("."),
            symbol: parts.last().cloned(),
            symbol_kind: Some(k),
            exported,
        },
        _ => Import {
            module: parts.join("."),
            symbol: None,
            symbol_kind: None,
            exported,
        },
    })
}

/// `@_exported\b`.
fn exported_attribute(t: &str) -> bool {
    t.match_indices("@_exported").any(|(i, m)| {
        t[i + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })
}

// -- Swift spec hooks (LanguageSpecs.Swift) -------------------------------------------------

pub(super) fn declaration_kind(src: &[u8], n: Node<'_>) -> Option<String> {
    child_by_field(n, "declaration_kind").map(|k| text(src, k))
}

/// Initializers, deinitializers and subscripts are named by their keyword.
pub(super) fn callable_name(src: &[u8], n: Node<'_>) -> Option<String> {
    match n.kind() {
        "init_declaration" => Some("init".into()),
        "deinit_declaration" => Some("deinit".into()),
        "subscript_declaration" => Some("subscript".into()),
        _ => child_by_field(n, "name").map(|x| text(src, x)),
    }
}

/// The declared type's name; an extension names its owner by the last simple type identifier.
pub(super) fn type_name(src: &[u8], n: Node<'_>) -> Option<String> {
    let name = child_by_field(n, "name")?;
    if declaration_kind(src, n).as_deref() != Some("extension") || name.kind() == "type_identifier"
    {
        return Some(text(src, name));
    }
    Some(
        children(name)
            .into_iter()
            .rfind(|c| c.kind() == "type_identifier")
            .map(|c| text(src, c))
            .unwrap_or_else(|| text(src, name)),
    )
}

pub(super) fn class_label(src: &[u8], n: Node<'_>) -> &'static str {
    match declaration_kind(src, n).as_deref() {
        Some("protocol") => "Interface",
        Some("struct") => "Struct",
        Some("enum") => "Enum",
        _ => "Class",
    }
}

/// A computed property or a protocol property requirement (stored properties are not nodes).
pub(super) fn property(src: &[u8], n: Node<'_>) -> Option<(String, Map<String, Value>)> {
    let requirement = n.kind() == "protocol_property_declaration";
    let computed = child_by_field(n, "computed_value");
    if !requirement && computed.is_none() {
        return None;
    }
    let pattern = child_by_field(n, "name");
    let name = pattern.map(|p| {
        child_by_field(p, "bound_identifier")
            .map(|b| text(src, b))
            .unwrap_or_else(|| text(src, p))
    })?;
    if name.trim().is_empty() {
        return None;
    }
    let mut p = Map::new();
    p.insert(
        if requirement {
            "requirement"
        } else {
            "computed"
        }
        .into(),
        json!(true),
    );
    let named = named_children(n);
    if let Some(ty) = named.iter().find(|c| c.kind() == "type_annotation")
        && let Some(t) =
            child_by_field(*ty, "name").or_else(|| named_children(*ty).into_iter().last())
    {
        let t = text(src, t);
        if !t.is_empty() {
            p.insert("type".into(), json!(t));
        }
    }
    let is_static = named
        .iter()
        .find(|c| c.kind() == "modifiers")
        .is_some_and(|m| {
            named_children(*m).into_iter().any(|c| {
                c.kind() == "property_modifier"
                    && matches!(text(src, c).as_str(), "static" | "class")
            })
        });
    p.insert("is_static".into(), json!(is_static));
    let accessor_parent = if requirement {
        named
            .iter()
            .find(|c| c.kind() == "protocol_property_requirements")
            .copied()
    } else {
        computed
    };
    let mut accessors: Vec<&str> = Vec::new();
    for c in accessor_parent.map(named_children).unwrap_or_default() {
        let a = match c.kind() {
            "computed_getter" | "getter_specifier" => "get",
            "computed_setter" | "setter_specifier" => "set",
            "computed_modify" | "modify_specifier" => "modify",
            _ => continue,
        };
        if !accessors.contains(&a) {
            accessors.push(a);
        }
    }
    // A computed property with only a body is a read-only getter.
    p.insert(
        "accessors".into(),
        json!(if accessors.is_empty() {
            "get".to_owned()
        } else {
            accessors.join(",")
        }),
    );
    Some((name, p))
}

const WELL_KNOWN_PROTOCOLS: &[&str] = &[
    "Actor",
    "AnyActor",
    "App",
    "AsyncIteratorProtocol",
    "AsyncSequence",
    "BidirectionalCollection",
    "CaseIterable",
    "Codable",
    "Collection",
    "Comparable",
    "CustomDebugStringConvertible",
    "CustomStringConvertible",
    "Decodable",
    "Encodable",
    "Equatable",
    "Error",
    "ExpressibleByArrayLiteral",
    "ExpressibleByDictionaryLiteral",
    "ExpressibleByIntegerLiteral",
    "ExpressibleByStringLiteral",
    "Hashable",
    "Identifiable",
    "IteratorProtocol",
    "LocalizedError",
    "MutableCollection",
    "NSCoding",
    "NSCopying",
    "NSSecureCoding",
    "ObservableObject",
    "OptionSet",
    "PreviewProvider",
    "RandomAccessCollection",
    "RawRepresentable",
    "RangeReplaceableCollection",
    "Scene",
    "Sendable",
    "Sequence",
    "SetAlgebra",
    "Strideable",
    "View",
    "ViewModifier",
];
const RAW_VALUE_TYPES: &[&str] = &[
    "String",
    "Character",
    "Int",
    "Int8",
    "Int16",
    "Int32",
    "Int64",
    "UInt",
    "UInt8",
    "UInt16",
    "UInt32",
    "UInt64",
    "Float",
    "Float16",
    "Float80",
    "Double",
    "CGFloat",
];
const WELL_KNOWN_CLASSES: &[&str] = &[
    "UITableViewDiffableDataSource",
    "UICollectionViewDiffableDataSource",
    "NSTableViewDiffableDataSource",
    "NSCollectionViewDiffableDataSource",
    "UITableViewDiffableDataSourceReference",
    "UICollectionViewDiffableDataSourceReference",
    "NSCollectionViewDiffableDataSourceReference",
];

fn looks_like_protocol(path: &str) -> bool {
    let simple = path.rsplit('.').next().unwrap_or(path);
    if WELL_KNOWN_CLASSES.contains(&simple) {
        return false;
    }
    WELL_KNOWN_PROTOCOLS.contains(&simple)
        || ["Protocol", "Delegate", "DataSource"]
            .iter()
            .any(|s| simple.len() > s.len() && simple.ends_with(s))
}

/// One inheritance or conformance relation (the retained `PendingEdge`).
pub(super) struct Relation {
    pub kind: &'static str,
    pub target: String,
    pub properties: Map<String, Value>,
}

/// Inheritance and conformance edges from a Swift type's inheritance clause (the retained
/// `AddSwiftTypeRelations`): struct, enum, actor and extension entries are IMPLEMENTS (an enum's
/// first raw-value entry is `raw_type`); protocol entries are INHERITS (`AnyObject` is
/// `class_bound`); a class's first entry is INHERITS/IMPLEMENTS by a local type's label, else by
/// the protocol naming heuristic marked `inheritance_inferred`; later entries are IMPLEMENTS.
/// `resolve` is the lexical lookup of a local type (qualified name and label).
pub(super) fn relations(
    src: &[u8],
    n: Node<'_>,
    type_qn: &str,
    origin_file: &str,
    type_props: Option<&mut Map<String, Value>>,
    resolve: &dyn Fn(&str) -> Option<(String, &'static str)>,
) -> Vec<Relation> {
    let kind = declaration_kind(src, n);
    let kind = kind.as_deref();
    let entries: Vec<Option<String>> = named_children(n)
        .into_iter()
        .filter(|c| c.kind() == "inheritance_specifier")
        .map(|c| {
            type_path(
                src,
                child_by_field(c, "inherits_from").or_else(|| named_children(c).into_iter().next()),
            )
        })
        .collect();
    let mut type_props = type_props;
    let mut out = Vec::new();
    for (index, path) in entries.into_iter().enumerate() {
        let Some(path) = path else {
            continue;
        };
        if kind == Some("enum") && index == 0 && RAW_VALUE_TYPES.contains(&path.as_str()) {
            if let Some(p) = type_props.as_deref_mut() {
                p.insert("raw_type".into(), json!(path));
            }
            continue;
        }
        if kind == Some("protocol") && path == "AnyObject" {
            if let Some(p) = type_props.as_deref_mut() {
                p.insert("class_bound".into(), json!(true));
            }
            continue;
        }
        let local = resolve(&path);
        let mut inferred = false;
        let edge = if kind == Some("protocol") {
            "INHERITS"
        } else if kind == Some("class") && index == 0 {
            inferred = local.is_none();
            match &local {
                Some((_, "Interface")) => "IMPLEMENTS",
                Some(_) => "INHERITS",
                None if looks_like_protocol(&path) => "IMPLEMENTS",
                None => "INHERITS",
            }
        } else {
            "IMPLEMENTS"
        };
        let target = local.map(|(qn, _)| qn).unwrap_or_else(|| type_ref(&path));
        let mut p = Map::new();
        p.insert(
            "confidence".into(),
            json!(if inferred { "medium" } else { "high" }),
        );
        if inferred {
            p.insert("inheritance_inferred".into(), json!(true));
        }
        if kind == Some("protocol") {
            p.insert("swift_protocol_target".into(), json!(true));
        }
        // An edge with a symbolic endpoint is owned by this file.
        let symbolic = |q: &str| q.starts_with(TYPE_REF) && q.len() > TYPE_REF.len();
        if symbolic(type_qn) || symbolic(&target) {
            p.insert("origin_file".into(), json!(origin_file));
        }
        out.push(Relation {
            kind: edge,
            target,
            properties: p,
        });
    }
    out
}
