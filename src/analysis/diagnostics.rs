//! Diagnostic detection over completed document analysis.

use super::*;
use crate::diagnostic_declarations;

macro_rules! run_check_for_phase {
    (node, node, $kind:ident, $check_context:ident, $check:expr, $context:ident) => {{
        $context.kind = DiagnosticKind::$kind;
        let $check_context = &mut *$context;
        $check;
    }};
    (installation, installation, $kind:ident, $check_context:ident, $check:expr, $context:ident) => {{
        $context.kind = DiagnosticKind::$kind;
        let $check_context = &mut *$context;
        $check;
    }};
    (codomain, codomain, $kind:ident, $check_context:ident, $check:expr, $context:ident) => {{
        $context.kind = DiagnosticKind::$kind;
        let $check_context = &mut *$context;
        $check;
    }};
    (document, document, $kind:ident, $check_context:ident, $check:expr, $context:ident) => {{
        $context.kind = DiagnosticKind::$kind;
        let $check_context = &mut *$context;
        $check;
    }};
    ($expected:ident, $actual:ident, $kind:ident, $check_context:ident, $check:expr, $context:ident) => {};
}

macro_rules! node_diagnostic_checks {
    (diagnostics { $($kind:ident {
        code: $code:literal, name: $name:literal, severity: $severity:ident,
        legacy: [$($legacy:literal),* $(,)?],
        check: $phase:ident => |$check_context:ident| $check:expr, action: $action:tt,
    }),+ $(,)? } standalone_actions { $($standalone:ident => |$action_context:ident| $standalone_action:expr),* $(,)? }) => {
        |context: &mut NodeDiagnosticContext<'_, '_, '_, _, _>| {
            $(run_check_for_phase!(node, $phase, $kind, $check_context, $check, context);)+
        }
    };
}

macro_rules! installation_diagnostic_checks {
    (diagnostics { $($kind:ident {
        code: $code:literal, name: $name:literal, severity: $severity:ident,
        legacy: [$($legacy:literal),* $(,)?],
        check: $phase:ident => |$check_context:ident| $check:expr, action: $action:tt,
    }),+ $(,)? } standalone_actions { $($standalone:ident => |$action_context:ident| $standalone_action:expr),* $(,)? }) => {
        |context: &mut InstallationDiagnosticContext<'_, '_, _>| {
            $(run_check_for_phase!(installation, $phase, $kind, $check_context, $check, context);)+
        }
    };
}

macro_rules! codomain_diagnostic_checks {
    (diagnostics { $($kind:ident {
        code: $code:literal, name: $name:literal, severity: $severity:ident,
        legacy: [$($legacy:literal),* $(,)?],
        check: $phase:ident => |$check_context:ident| $check:expr, action: $action:tt,
    }),+ $(,)? } standalone_actions { $($standalone:ident => |$action_context:ident| $standalone_action:expr),* $(,)? }) => {
        |context: &mut CodomainDiagnosticContext<'_, '_, '_, _, _>| {
            $(run_check_for_phase!(codomain, $phase, $kind, $check_context, $check, context);)+
        }
    };
}

macro_rules! document_diagnostic_checks {
    (diagnostics { $($kind:ident {
        code: $code:literal, name: $name:literal, severity: $severity:ident,
        legacy: [$($legacy:literal),* $(,)?],
        check: $phase:ident => |$check_context:ident| $check:expr, action: $action:tt,
    }),+ $(,)? } standalone_actions { $($standalone:ident => |$action_context:ident| $standalone_action:expr),* $(,)? }) => {
        |context: &mut DocumentDiagnosticContext<'_, '_, '_, _>| {
            $(run_check_for_phase!(document, $phase, $kind, $check_context, $check, context);)+
        }
    };
}

struct NodeDiagnosticContext<'analysis, 'tree, 'source, Source: ?Sized, Knowledge: ?Sized> {
    analysis: &'analysis mut Analysis,
    kind: DiagnosticKind,
    node: M2Node<'tree>,
    source: &'source Source,
    knowledge: &'source Knowledge,
}

struct InstallationDiagnosticContext<'analysis, 'source, Knowledge: ?Sized> {
    analysis: &'analysis Analysis,
    kind: DiagnosticKind,
    installation: &'analysis MethodInstallation,
    knowledge: &'source Knowledge,
    diagnostics: &'analysis mut Vec<M2Diagnostic>,
}

struct CodomainDiagnosticContext<'analysis, 'tree, 'source, Source: ?Sized, Knowledge: ?Sized> {
    analysis: &'analysis Analysis,
    kind: DiagnosticKind,
    node: M2Node<'tree>,
    source: &'source Source,
    knowledge: &'source Knowledge,
    diagnostics: &'analysis mut Vec<M2Diagnostic>,
}

struct DocumentDiagnosticContext<'analysis, 'tree, 'source, Source: ?Sized> {
    analysis: &'analysis mut Analysis,
    kind: DiagnosticKind,
    root: M2Node<'tree>,
    source: &'source Source,
}

impl<Source: SourceNavigation + ?Sized, Knowledge: TypeKnowledge + ?Sized>
    NodeDiagnosticContext<'_, '_, '_, Source, Knowledge>
{
    fn syntax_error(&mut self) {
        if self.node.is_error() {
            self.analysis.diagnostics.push(self.kind.at(
                self.source.remainder_of_line_range(self.node.start_byte()),
                "Syntax error",
            ));
        }
    }

    fn missing_node(&mut self) {
        if self.node.is_missing() {
            self.analysis.diagnostics.push(self.kind.at(
                self.source.range_for_node(self.node),
                format!("Missing: {}", self.node.syntax_label()),
            ));
        }
    }

    fn ambiguous_float_member_access(&mut self) {
        let Some(replacement) = ambiguous_float_member_access_rewrite(self.node) else {
            return;
        };
        self.analysis.diagnostics.push(self.kind.at(
            self.source.range_for_node(self.node),
            format!(
                "This is parsed as application to a float literal; use `{replacement}` for member access"
            ),
        ));
    }

    fn multiple_assignment_targets(&mut self) {
        if !self.node.is_assignment() {
            return;
        }
        let Some(left) = self.node.child_by_field_name("left") else {
            return;
        };
        let Some(operator) = self.node.child_by_field_name("operator") else {
            return;
        };
        let operator = operator.text();
        if matches!(operator, "=" | ":=")
            && self
                .analysis
                .installation_for(self.node, self.source)
                .is_none()
            && !multiple_assignment_targets_are_symbols(left)
        {
            self.analysis.diagnostics.push(self.kind.at(
                self.source.range_for_node(left),
                format!("{operator} multiple assignment targets must be symbols"),
            ));
        }
    }

    fn colon_equal_part_assignment(&mut self) {
        if self.node.binary_operator() != Some(":=") {
            return;
        }
        let Some(left) = self.node.child_by_field_name("left") else {
            return;
        };
        if left.binary_operator() == Some("#") {
            self.analysis.diagnostics.push(self.kind.at(
                self.source.range_for_node(left),
                "`:=` cannot assign to parts; use `=` for part assignment",
            ));
        }
    }

    fn parallel_assignment_arity(&mut self) {
        self.parallel_assignment();
    }

    fn option_key_convention(&mut self) {
        self.analysis
            .diagnose_option_key_convention(self.kind, self.node, self.source);
    }

    fn redundant_control_parentheses(&mut self) {
        let Some(inner) = redundant_control_parentheses_inner(self.node) else {
            return;
        };
        self.analysis.diagnostics.push(self.kind.at(
            self.source.range_for_node(self.node),
            format!(
                "Parentheses around this control expression are redundant; use `{}`",
                inner.text()
            ),
        ));
    }

    fn prefer_coalescence(&mut self) {
        let Some(replacement) = coalescence_rewrite(self.node) else {
            return;
        };
        self.analysis.diagnostics.push(self.kind.at(
            self.source.range_for_node(self.node),
            format!("This conditional can be simplified to `{replacement}`"),
        ));
    }

    fn install_needs_colon_equals(&mut self) {
        let position = self.source.position_for_node(self.node);
        let Some(name) =
            self.analysis
                .illegal_equals_install_head(self.node, position, self.knowledge)
        else {
            return;
        };
        self.analysis.diagnostics.push(self.kind.at(
            self.source.range_for_node(self.node),
            format!(
                "Installing a method on `{name}` must use `:=`, not `=`: M2 rejects this \
                 (\"no method for storing values of function {name}\"). Use `:=`."
            ),
        ));
    }

    fn protect_assigned_symbol(&mut self) {
        self.analysis
            .diagnose_protect_argument(self.kind, self.node, self.source, self.knowledge);
    }

    fn protect_computed_symbol(&mut self) {
        self.analysis
            .diagnose_protect_argument(self.kind, self.node, self.source, self.knowledge);
    }

    fn missing_output_cell(&mut self) {
        self.analysis
            .diagnose_output_reference(self.kind, self.node, self.source, self.knowledge);
    }

    fn invalid_control_transfer(&mut self) {
        self.analysis
            .diagnose_control_transfer(self.kind, self.node, self.source, self.knowledge);
    }

    fn parallel_assignment_type(&mut self) {
        self.parallel_assignment();
    }

    fn condition_type(&mut self) {
        self.analysis
            .diagnose_condition_type(self.kind, self.node, self.source, self.knowledge);
    }

    fn parallel_assignment(&mut self) {
        if !self.node.is_assignment()
            || !matches!(self.node.binary_operator(), Some("=") | Some(":="))
            || self
                .analysis
                .installation_for(self.node, self.source)
                .is_some()
        {
            return;
        }
        let (Some(left), Some(right)) = (
            self.node.child_by_field_name("left"),
            self.node.child_by_field_name("right"),
        ) else {
            return;
        };
        self.analysis.validate_parallel_assignment(
            self.kind,
            left,
            right,
            self.source,
            self.knowledge,
        );
    }
}

impl<Knowledge: TypeKnowledge + ?Sized> InstallationDiagnosticContext<'_, '_, Knowledge> {
    fn install_no_effect(&mut self) {
        let method = &self.installation.method;
        if let MethodHead::Operator(operator) = &method.head {
            if operator.form == OperatorForm::Binary
                && operator.token.name() == "??"
                && self.installation.expected_rhs_arity() == method.domain.len()
            {
                self.diagnostics.push(self.kind.at(
                    self.installation.span,
                    "Installing a binary `??` method has no effect: M2 records the method, but `x ?? y` never dispatches to it. Install the prefix form `?? X := x -> ...` to customize how `X` behaves on the left of `??`.",
                ));
                return;
            }
        }
        let MethodHead::Function(name) = &method.head else {
            return;
        };
        if self.analysis.callable_head_kind(
            name.name(),
            self.installation.span.start,
            self.knowledge,
        ) != CallableHeadKind::PlainFunction
        {
            return;
        }
        self.diagnostics.push(self.kind.at(
            self.installation.span,
            format!(
                "Installing a method on `{name}` has no effect: `{name}` is not a method \
                 function. Define it with `{name} = method()` to make method installations take effect."
            ),
        ));
    }

    fn operator_not_flexible(&mut self) {
        let MethodHead::Operator(operator) = &self.installation.method.head else {
            return;
        };
        if self
            .analysis
            .operator_form_is_flexible(operator, self.knowledge)
            != Some(false)
        {
            return;
        }
        self.diagnostics.push(self.kind.at(
            self.installation.span,
            format!(
                "Cannot install a method on the {} operator `{}`: it is not flexible, so M2 rejects the assignment.",
                operator.form, operator.token
            ),
        ));
    }

    fn install_arity(&mut self) {
        let Some(Dispatch::Fixed(actual)) = self.installation.rhs_lambda_dispatch else {
            return;
        };
        let expected = self.installation.expected_rhs_arity();
        if actual == expected {
            return;
        }
        self.diagnostics.push(self.kind.at(
            self.installation.span,
            format!(
                "This method's function takes {actual} argument(s) but the installation expects \
                 {expected}. Match the domain arity or use a variadic `x -> …`."
            ),
        ));
    }
}

impl<Source: SourceNavigation + ?Sized, Knowledge: TypeKnowledge + ?Sized>
    CodomainDiagnosticContext<'_, '_, '_, Source, Knowledge>
{
    fn missing_codomain(&mut self) {
        let Some(deduction) =
            self.analysis
                .method_codomain_deduction(self.node, self.source, self.knowledge)
        else {
            return;
        };
        if !matches!(deduction.edit, MethodCodomainEdit::Add(_)) {
            return;
        }
        self.diagnostics.push(self.kind.at(
            deduction.diagnostic_range,
            format!(
                "This method's lambda has the deducible codomain `{}`. Add the codomain annotation.",
                deduction.codomain
            ),
        ));
    }

    fn codomain_mismatch(&mut self) {
        let Some(deduction) =
            self.analysis
                .method_codomain_deduction(self.node, self.source, self.knowledge)
        else {
            return;
        };
        if !matches!(deduction.edit, MethodCodomainEdit::Replace) {
            return;
        }
        let Some(annotated) = deduction.annotated_codomain else {
            return;
        };
        self.diagnostics.push(self.kind.at(
            deduction.diagnostic_range,
            format!(
                "This method's inferred result type `{}` is incompatible with its annotated codomain `{annotated}`.",
                deduction.codomain
            ),
        ));
    }
}

impl<Source: SourceNavigation + ?Sized> DocumentDiagnosticContext<'_, '_, '_, Source> {
    fn unused_bindings(&mut self) {
        self.analysis
            .diagnose_unused_bindings(self.kind, self.root, self.source);
    }
}

impl Analysis {
    pub fn collect_diagnostics(
        &mut self,
        root: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        self.diagnose_installations(root, source, knowledge);
        self.scan_diagnostics(root, source, knowledge);
        let mut context = DocumentDiagnosticContext {
            analysis: self,
            kind: DiagnosticKind::SyntaxError,
            root,
            source,
        };
        diagnostic_declarations!(document_diagnostic_checks)(&mut context);
    }

    fn diagnose_installations(
        &mut self,
        root: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        if !knowledge_provider.at_position(pos!()).is_available() {
            return;
        }
        let mut diagnostics = Vec::new();
        for installation in &self.registry.installations {
            let knowledge = knowledge_provider.at_position(installation.span.start);
            let mut context = InstallationDiagnosticContext {
                analysis: self,
                kind: DiagnosticKind::SyntaxError,
                installation,
                knowledge: &knowledge,
                diagnostics: &mut diagnostics,
            };
            diagnostic_declarations!(installation_diagnostic_checks)(&mut context);
        }
        self.scan_installation_codomain_diagnostics(
            root,
            source,
            knowledge_provider,
            &mut diagnostics,
        );
        self.diagnostics.extend(diagnostics);
    }

    fn scan_installation_codomain_diagnostics(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
        out: &mut Vec<M2Diagnostic>,
    ) {
        if node.is_assignment() {
            let knowledge = knowledge_provider.at_position(source.position_for_node(node));
            let mut context = CodomainDiagnosticContext {
                analysis: self,
                kind: DiagnosticKind::SyntaxError,
                node,
                source,
                knowledge: &knowledge,
                diagnostics: out,
            };
            diagnostic_declarations!(codomain_diagnostic_checks)(&mut context);
        }
        for child in node.children() {
            self.scan_installation_codomain_diagnostics(child, source, knowledge_provider, out);
        }
    }

    fn illegal_equals_install_head(
        &self,
        node: M2Node,
        position: Position,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<String> {
        if node.binary_operator() != Some("=") {
            return None;
        }
        let right = node.child_by_field_name("right")?;
        if right.kind != NodeKind::LambdaExpression {
            return None;
        }
        let left = node.child_by_field_name("left")?;
        let (MethodHead::Function(name), _) = self.installation_shape(left, position, knowledge)?
        else {
            return None;
        };
        (self.callable_head_kind(name.name(), position, knowledge) != CallableHeadKind::Unknown)
            .then(|| name.name().to_string())
    }

    fn operator_form_is_flexible(
        &self,
        operator: &Operator,
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) -> Option<bool> {
        knowledge
            .get_record(&operator.token)?
            .operator_info()
            .map(|operator_info| operator_info.is_flexible(operator.form))
    }
}

impl Analysis {
    fn scan_diagnostics(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        let knowledge = knowledge_provider.at_position(source.position_for_node(node));
        let mut context = NodeDiagnosticContext {
            analysis: self,
            kind: DiagnosticKind::SyntaxError,
            node,
            source,
            knowledge: &knowledge,
        };
        diagnostic_declarations!(node_diagnostic_checks)(&mut context);

        for child in node.children() {
            self.scan_diagnostics(child, source, knowledge_provider);
        }
    }

    fn diagnose_condition_type(
        &mut self,
        kind: DiagnosticKind,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        let (construct, condition) = match node.kind {
            NodeKind::IfStatement => ("if", node.child_by_field_name("condition")),
            NodeKind::WhileStatement => ("while", node.named_child(0)),
            _ => return,
        };
        let Some(condition) = condition else {
            return;
        };
        let position = source.position_for_node(condition);
        let scope_idx = self.find_scope_at(position).unwrap_or(0);
        let checker = TypeChecker::new(self);
        let actual = checker.type_of(condition, source, scope_idx, knowledge);
        if actual.possibility_by(&TypeRole::Boolean.object_name(), |candidate, bound| {
            checker.subtype_evidence(candidate, bound, position, knowledge)
        }) != SubtypeEvidence::Disproven
        {
            return;
        }
        self.diagnostics.push(kind.at(
            source.range_for_node(condition),
            format!(
                "{construct} condition must have type `Boolean`, but this expression has type `{}`",
                actual.label().unwrap_or_else(|| "unknown".to_string())
            ),
        ));
    }

    fn diagnose_control_transfer(
        &mut self,
        kind: DiagnosticKind,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        if !node.kind.is_control_transfer() {
            return;
        }
        let target = self.control_transfer_target(node, source, knowledge);
        if target.is_some_and(|target| target.accepts(node)) {
            return;
        }

        let message = match node.kind {
            NodeKind::ReturnStatement => "`return` can only be used inside a function body",
            NodeKind::BreakStatement => {
                "`break` can only be used inside a loop body or an `apply`/`scan` callback"
            }
            NodeKind::ContinueStatement
                if node.named_child(0).is_some()
                    && matches!(
                        target,
                        Some(
                            ControlTransferTarget::DoLoop(_)
                                | ControlTransferTarget::LoopCallback { .. }
                        )
                    ) =>
            {
                "`continue` with a value requires a `list` clause"
            }
            NodeKind::ContinueStatement => {
                "`continue` can only be used inside a `list` or `do` loop body"
            }
            _ => return,
        };
        let keyword = node.child(0).unwrap_or(node);
        self.diagnostics
            .push(kind.at(source.range_for_node(keyword), message));
    }

    fn diagnose_output_reference(
        &mut self,
        kind: DiagnosticKind,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        if node.kind != NodeKind::Symbol
            || node
                .parent()
                .is_some_and(|parent| parent.kind == NodeKind::QuoteExpression)
        {
            return;
        }
        let Some(reference) = OutputReference::parse(node.text()) else {
            return;
        };
        let position = source.position_for_node(node);
        if self
            .visible_source_binding_at(node.text(), position, knowledge)
            .is_some()
            || reference.referenced_value(node).is_some()
        {
            return;
        }

        self.diagnostics.push(kind.at(
            source.range_for_node(node),
            format!(
                "`{}` does not reference an available output cell; it evaluates as an unassigned `Symbol`",
                node.text()
            ),
        ));
    }

    fn diagnose_protect_argument(
        &mut self,
        kind: DiagnosticKind,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        if !node.is_space_application() {
            return;
        }
        let (Some(callable), Some(argument)) = (
            node.child_by_field_name("left"),
            node.child_by_field_name("right"),
        ) else {
            return;
        };
        if callable.kind != NodeKind::Symbol || callable.text() != "protect" {
            return;
        }
        if self
            .binding_id_at(callable.text(), source.position_for_node(callable))
            .is_some()
        {
            return;
        }

        match argument.kind {
            NodeKind::QuoteExpression => {}
            NodeKind::Symbol if kind == DiagnosticKind::ProtectAssignedSymbol => {
                let name = argument.text();
                let position = source.position_for_node(argument);
                let has_source_binding = self.binding_id_at(name, position).is_some();
                let has_builtin_binding = knowledge.get_record(&ObjectName::new(name)).is_some();
                if has_source_binding || has_builtin_binding {
                    self.diagnostics.push(kind.at(
                        source.range_for_node(argument),
                        format!(
                            "`protect {name}` evaluates the current value of `{name}`; \
                             use `protect symbol {name}` to protect the symbol itself"
                        ),
                    ));
                }
            }
            NodeKind::Symbol => {}
            _ if kind == DiagnosticKind::ProtectComputedSymbol => {
                let inferred = self.infer_expression_static_type(argument, source, knowledge);
                if inferred
                    .as_ref()
                    .is_none_or(|type_id| type_id.name() == "Symbol")
                {
                    self.diagnostics.push(kind.at(
                        source.range_for_node(argument),
                        "`protect` evaluates this expression to choose a Symbol at runtime; \
                         the protected symbol is not statically apparent",
                    ));
                }
            }
            _ => {}
        }
    }

    fn diagnose_option_key_convention(
        &mut self,
        kind: DiagnosticKind,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        if node.binary_operator() != Some("=>") {
            return;
        }
        let Some(key) = node.child_by_field_name("left") else {
            return;
        };
        if key.kind != NodeKind::Symbol {
            return;
        }
        let key_text = key.text();
        let starts_lowercase = key_text
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase());
        if !starts_lowercase || !is_function_option_context(node) {
            return;
        }
        self.diagnostics.push(kind.at(
            source.range_for_node(key),
            format!("Option key `{key_text}` should be capitalized by Macaulay2 convention"),
        ));
    }

    fn validate_parallel_assignment(
        &mut self,
        kind: DiagnosticKind,
        left: M2Node,
        right: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        if !left.kind.is_collection_expression() {
            return;
        }

        let target_nodes = left.collection_elements().collect::<Vec<_>>();
        if !right.kind.is_collection_expression() {
            if kind != DiagnosticKind::ParallelAssignmentType {
                return;
            }
            if target_nodes.len() < 2 || !knowledge.is_available() {
                return;
            }
            let mut value = right;
            while value.kind == NodeKind::ParenthesizedExpression {
                let Some(inner) = value.final_value_child() else {
                    break;
                };
                value = inner;
            }
            if value.kind == NodeKind::Symbol {
                let position = source.position_for_node(value);
                let has_source_binding = self
                    .visible_source_binding_at(value.text(), position, knowledge)
                    .is_some();
                let has_indexed_value = knowledge
                    .get_record(&ObjectName::new(value.text()))
                    .is_some();
                if !has_source_binding && !has_indexed_value {
                    return;
                }
            }
            let Some(right_type) = self.infer_expression_static_type(right, source, knowledge)
            else {
                return;
            };
            if right_type == TypeRole::Thing.object_name()
                || knowledge.has_type_role(&right_type, TypeRole::VisibleList)
            {
                return;
            }
            self.diagnostics.push(kind.at(
                source.range_for_node(right),
                format!(
                    "parallel assignment binds {} targets, but the right-hand side has incompatible type `{}`",
                    target_nodes.len(),
                    right_type.name()
                ),
            ));
            return;
        }

        let value_nodes = right.collection_elements().collect::<Vec<_>>();
        if target_nodes.len() != value_nodes.len() {
            if kind == DiagnosticKind::ParallelAssignmentArity {
                self.diagnostics.push(kind.at(
                    source.range_for_node(left),
                    format!(
                        "parallel assignment binds {} targets but the right-hand side lists {}; their lengths must match",
                        target_nodes.len(),
                        value_nodes.len()
                    ),
                ));
            }
            return;
        }

        for (target, value) in target_nodes.iter().zip(value_nodes.iter()) {
            self.validate_parallel_assignment(kind, *target, *value, source, knowledge);
        }
    }

    fn diagnose_unused_bindings(
        &mut self,
        kind: DiagnosticKind,
        root: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let mut used_bindings = HashSet::new();
        for node in root.symbols() {
            let name = node.text();
            let position = source.position_for_node(node);
            if let Some(binding_id) = self.binding_id_at(name, position) {
                if let Some(binding) = self.get_binding_at(name, position) {
                    let node_range = source.range_for_node(node);
                    if node_range != binding.range {
                        used_bindings.insert(binding_id);
                    }
                }
            }
        }

        let diagnostics = self
            .bindings()
            .filter(|binding| binding.role == BindingRole::Ordinary)
            .filter(|binding| !binding.potential_export)
            .filter(|binding| !used_bindings.contains(&binding.binding_id))
            .filter_map(|binding| {
                let name = binding.name.name();
                if name.starts_with('_') {
                    return None;
                }
                let noun = if binding.state.presentation_kind == SymbolKind::FUNCTION {
                    "function"
                } else {
                    "variable"
                };
                Some(kind.at(binding.range, format!("Unused {noun} {name}")))
            })
            .collect::<Vec<_>>();
        self.diagnostics.extend(diagnostics);
    }
}

fn is_function_option_context(option: M2Node<'_>) -> bool {
    let mut current = option;
    while let Some(parent) = current.parent() {
        match parent.kind {
            NodeKind::Sequence => return true,
            NodeKind::List | NodeKind::Array | NodeKind::AngleBarList => return false,
            _ => current = parent,
        }
    }
    false
}

pub fn redundant_control_parentheses_inner(node: M2Node<'_>) -> Option<M2Node<'_>> {
    if node.kind != NodeKind::ParenthesizedExpression {
        return None;
    }
    let parent = node.parent()?;
    let is_control_expression = match parent.kind {
        NodeKind::IfStatement => parent
            .child_by_field_name("condition")
            .is_some_and(|condition| condition.id() == node.id()),
        NodeKind::WhileStatement | NodeKind::TryStatement => parent
            .named_child(0)
            .is_some_and(|condition| condition.id() == node.id()),
        NodeKind::FromClause | NodeKind::ToClause | NodeKind::InClause | NodeKind::WhenClause => {
            parent
                .named_child(0)
                .is_some_and(|value| value.id() == node.id())
        }
        _ => false,
    };
    is_control_expression
        .then(|| parenthesized_value(node))
        .flatten()
}

pub fn coalescence_rewrite(node: M2Node<'_>) -> Option<String> {
    if node.binary_operator() == Some("=") {
        let target = node.child_by_field_name("left")?;
        let conditional = node.child_by_field_name("right")?;
        if target.kind != NodeKind::Symbol || conditional.kind != NodeKind::IfStatement {
            return None;
        }
        let (subject, fallback) = coalescence_parts(conditional)?;
        return (target.text() == subject.text())
            .then(|| format!("{} ??= {}", target.text(), fallback.text()));
    }
    if node.kind != NodeKind::IfStatement {
        return None;
    }
    if node
        .parent()
        .is_some_and(|parent| coalescence_rewrite(parent).is_some())
    {
        return None;
    }
    let (subject, fallback) = coalescence_parts(node)?;
    Some(format!("{} ?? {}", subject.text(), fallback.text()))
}

fn coalescence_parts(if_statement: M2Node<'_>) -> Option<(M2Node<'_>, M2Node<'_>)> {
    let condition = if_statement.child_by_field_name("condition")?;
    let operator = condition.binary_operator()?;
    let left = condition.child_by_field_name("left")?;
    let right = condition.child_by_field_name("right")?;
    let (subject, null_when_true) = match (is_null_value(left), is_null_value(right), operator) {
        (true, false, "===") => (right, true),
        (false, true, "===") => (left, true),
        (true, false, "=!=") => (right, false),
        (false, true, "=!=") => (left, false),
        _ => return None,
    };
    if subject.kind != NodeKind::Symbol {
        return None;
    }
    let then_value = clause_value(clause_of(if_statement, NodeKind::ThenClause)?)?;
    let else_value = clause_value(clause_of(if_statement, NodeKind::ElseClause)?)?;
    let (fallback, repeated_subject) = if null_when_true {
        (then_value, else_value)
    } else {
        (else_value, then_value)
    };
    let repeated_subject = parenthesized_value(repeated_subject)?;
    (repeated_subject.kind == NodeKind::Symbol && repeated_subject.text() == subject.text())
        .then_some((subject, fallback))
}

fn is_null_value(node: M2Node<'_>) -> bool {
    node.kind == NodeKind::Symbol && node.text() == "null"
}

fn multiple_assignment_targets_are_symbols(node: M2Node) -> bool {
    if !node.kind.is_collection_expression() {
        return true;
    }

    node.collection_elements().all(|child| {
        child.kind == NodeKind::Symbol
            || (child.kind.is_collection_expression()
                && multiple_assignment_targets_are_symbols(child))
    })
}

pub fn ambiguous_float_member_access_rewrite(node: M2Node<'_>) -> Option<String> {
    if !node.is_space_application() {
        return None;
    }

    let left = node.child_by_field_name("left")?;
    let right = node.child_by_field_name("right")?;
    if symbol_node_text(left).is_none()
        || right.kind != NodeKind::FloatLiteral
        || left.end_byte() != right.start_byte()
    {
        return None;
    }

    let member_index = member_index_for_ambiguous_float_literal(right.text())?;
    Some(format!("{}#{member_index}", left.text()))
}

fn member_index_for_ambiguous_float_literal(float_text: &str) -> Option<String> {
    let fractional_part = float_text.strip_prefix('.')?;
    (!fractional_part.is_empty() && fractional_part.chars().all(|ch| ch.is_ascii_digit()))
        .then(|| fractional_part.to_string())
}

#[cfg(test)]
mod tests {
    use super::member_index_for_ambiguous_float_literal;

    #[test]
    fn ambiguous_member_access_helper_requires_dot_prefixed_float() {
        assert_eq!(
            member_index_for_ambiguous_float_literal(".3"),
            Some("3".to_string())
        );
        assert_eq!(
            member_index_for_ambiguous_float_literal(".123"),
            Some("123".to_string())
        );
        assert_eq!(member_index_for_ambiguous_float_literal("3"), None);
        assert_eq!(member_index_for_ambiguous_float_literal("."), None);
        assert_eq!(member_index_for_ambiguous_float_literal(".3e2"), None);
    }
}
