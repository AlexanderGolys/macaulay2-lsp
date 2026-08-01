//! Diagnostic detection over completed document analysis.

use super::*;

impl Analysis {
    pub(super) fn collect_diagnostics(
        &mut self,
        root: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        self.diagnose_installations(root, source, knowledge);
        self.diagnose_install_forms(root, source, knowledge);
        self.scan_diagnostics(root, source, knowledge);
        self.diagnose_unused_bindings(root, source);
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
            self.installation_diagnostics(installation, &knowledge, &mut diagnostics);
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
            if let Some(deduction) = self.method_codomain_deduction(node, source, &knowledge) {
                let (kind, message) = match deduction.edit {
                    MethodCodomainEdit::Replace(_) => (
                        DiagnosticKind::InstallCodomainMismatch,
                        format!(
                            "This method's lambda returns `{}`, which is incompatible with the \
                             annotated codomain. Change the annotation to `{}`.",
                            deduction.codomain, deduction.codomain
                        ),
                    ),
                    MethodCodomainEdit::Add(_) => (
                        DiagnosticKind::InstallCodomainMissing,
                        format!(
                            "This method's lambda has the deducible codomain `{}`. Add the \
                             codomain annotation.",
                            deduction.codomain
                        ),
                    ),
                };
                out.push(kind.at(deduction.diagnostic_range, message));
            }
        }
        for child in node.children() {
            self.scan_installation_codomain_diagnostics(child, source, knowledge_provider, out);
        }
    }

    fn diagnose_install_forms(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl PositionedTypeKnowledge + ?Sized),
    ) {
        let mut diagnostics = Vec::new();
        self.scan_install_form(node, source, knowledge, &mut diagnostics);
        self.diagnostics.extend(diagnostics);
    }

    fn scan_install_form(
        &self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge_provider: &(impl PositionedTypeKnowledge + ?Sized),
        out: &mut Vec<M2Diagnostic>,
    ) {
        let knowledge = knowledge_provider.at_position(source.position_for_node(node));
        if let Some(name) =
            self.illegal_equals_install_head(node, source.position_for_node(node), &knowledge)
        {
            out.push(DiagnosticKind::InstallNeedsColonEquals.at(
                source.range_for_node(node),
                format!(
                    "Installing a method on `{name}` must use `:=`, not `=`: M2 rejects this \
                     (\"no method for storing values of function {name}\"). Use `:=`."
                ),
            ));
        }
        for child in node.children() {
            self.scan_install_form(child, source, knowledge_provider, out);
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
        let (MethodHead::Function(name), _) = self.installation_shape(left, knowledge)? else {
            return None;
        };
        (self.callable_head_kind(name.name(), position, knowledge) != CallableHeadKind::Unknown)
            .then(|| name.name().to_string())
    }

    fn installation_diagnostics(
        &self,
        installation: &MethodInstallation,
        knowledge: &(impl TypeKnowledge + ?Sized),
        out: &mut Vec<M2Diagnostic>,
    ) {
        match &installation.method.head {
            MethodHead::Function(name) => {
                if self.callable_head_kind(name.name(), installation.span.start, knowledge)
                    == CallableHeadKind::PlainFunction
                {
                    out.push(DiagnosticKind::InstallNoEffect.at(
                        installation.span,
                        format!(
                            "Installing a method on `{name}` has no effect: `{name}` is not a \
                             method function. Define it with `{name} = method()` to make method \
                             installations take effect."
                        ),
                    ));
                }
            }
            MethodHead::Operator(operator) => {
                let form = operator.form;
                if self.operator_form_is_flexible(operator, knowledge) == Some(false) {
                    out.push(DiagnosticKind::OperatorNotFlexible.at(
                        installation.span,
                        format!(
                            "Cannot install a method on the {form} operator `{}`: it is not \
                             flexible, so M2 rejects the assignment.",
                            operator.token
                        ),
                    ));
                }
            }
        }

        if let Some(Dispatch::Fixed(actual)) = installation.rhs_lambda_dispatch {
            let expected = installation.expected_rhs_arity();
            if actual != expected {
                out.push(DiagnosticKind::InstallArity.at(
                    installation.span,
                    format!(
                        "This method's function takes {actual} argument(s) but the installation \
                         expects {expected}. Match the domain arity or use a variadic `x -> …`."
                    ),
                ));
            }
        }
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
        if node.is_error() {
            self.diagnostics.push(DiagnosticKind::SyntaxError.at(
                source.remainder_of_line_range(node.start_byte()),
                "Syntax error",
            ));
        } else if node.is_missing() {
            self.diagnostics.push(DiagnosticKind::MissingNode.at(
                source.range_for_node(node),
                format!("Missing: {}", node.syntax_label()),
            ));
        } else if let Some(replacement) = ambiguous_float_member_access_rewrite(node) {
            self.diagnostics
                .push(DiagnosticKind::AmbiguousFloatMemberAccess.at(
                    source.range_for_node(node),
                    format!(
                        "This is parsed as application to a float literal; use `{replacement}` \
                         for member access"
                    ),
                ));
        } else if node.is_assignment() {
            self.validate_assignment_form(node, source, &knowledge);
        }

        self.diagnose_option_key_convention(node, source);
        self.diagnose_control_transfer(node, source, &knowledge);
        self.diagnose_output_reference(node, source, &knowledge);
        self.diagnose_protect_argument(node, source, &knowledge);
        self.diagnose_condition_type(node, source, &knowledge);

        for child in node.children() {
            self.scan_diagnostics(child, source, knowledge_provider);
        }
    }

    fn diagnose_condition_type(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        let (kind, construct, condition) = match node.kind {
            NodeKind::IfStatement => (
                DiagnosticKind::IfConditionType,
                "if",
                node.child_by_field_name("condition"),
            ),
            NodeKind::WhileStatement => (
                DiagnosticKind::WhileConditionType,
                "while",
                node.named_child(0),
            ),
            _ => return,
        };
        let Some(condition) = condition else {
            return;
        };
        let Some(actual) = self.infer_expression_static_type(condition, source, knowledge) else {
            return;
        };
        if actual == TypeRole::Thing.object_name()
            || knowledge.is_subtype(&actual, &TypeRole::Boolean.object_name())
        {
            return;
        }
        self.diagnostics.push(kind.at(
            source.range_for_node(condition),
            format!(
                "{construct} condition must have type `Boolean`, but this expression has type `{}`",
                actual.name()
            ),
        ));
    }

    fn diagnose_control_transfer(
        &mut self,
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
        self.diagnostics.push(
            DiagnosticKind::InvalidControlTransfer.at(source.range_for_node(keyword), message),
        );
    }

    fn diagnose_output_reference(
        &mut self,
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

        self.diagnostics.push(DiagnosticKind::MissingOutputCell.at(
            source.range_for_node(node),
            format!(
                "`{}` does not reference an available output cell; it evaluates as an unassigned `Symbol`",
                node.text()
            ),
        ));
    }

    fn diagnose_protect_argument(
        &mut self,
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
            NodeKind::Symbol => {
                let name = argument.text();
                let position = source.position_for_node(argument);
                let has_source_binding = self.binding_id_at(name, position).is_some();
                let has_builtin_binding = knowledge.get_record(&ObjectName::new(name)).is_some();
                if has_source_binding || has_builtin_binding {
                    self.diagnostics
                        .push(DiagnosticKind::ProtectAssignedSymbol.at(
                            source.range_for_node(argument),
                            format!(
                                "`protect {name}` evaluates the current value of `{name}`; \
                             use `protect symbol {name}` to protect the symbol itself"
                            ),
                        ));
                }
            }
            _ => {
                let inferred = self.infer_expression_static_type(argument, source, knowledge);
                if inferred
                    .as_ref()
                    .is_none_or(|type_id| type_id.name() == "Symbol")
                {
                    self.diagnostics
                        .push(DiagnosticKind::ProtectComputedSymbol.at(
                            source.range_for_node(argument),
                            "`protect` evaluates this expression to choose a Symbol at runtime; \
                         the protected symbol is not statically apparent",
                        ));
                }
            }
        }
    }

    fn diagnose_option_key_convention(
        &mut self,
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
        self.diagnostics
            .push(DiagnosticKind::OptionKeyConvention.at(
                source.range_for_node(key),
                format!("Option key `{key_text}` should be capitalized by Macaulay2 convention"),
            ));
    }

    fn validate_assignment_form(
        &mut self,
        node: M2Node,
        source: &(impl SourceNavigation + ?Sized),
        knowledge: &(impl TypeKnowledge + ?Sized),
    ) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let Some(operator) = node.child_by_field_name("operator") else {
            return;
        };
        let operator = operator.text();
        let is_method_installation = self.installation_for(node, source).is_some();

        if matches!(operator, "=" | ":=")
            && !is_method_installation
            && !multiple_assignment_targets_are_symbols(left)
        {
            self.diagnostics
                .push(DiagnosticKind::MultipleAssignmentTargets.at(
                    source.range_for_node(left),
                    format!("{operator} multiple assignment targets must be symbols"),
                ));
        }

        if operator == ":=" && left.binary_operator() == Some("#") {
            self.diagnostics
                .push(DiagnosticKind::ColonEqualPartAssignment.at(
                    source.range_for_node(left),
                    "`:=` cannot assign to parts; use `=` for part assignment",
                ));
        }

        if matches!(operator, "=" | ":=") && !is_method_installation {
            if let Some(right) = node.child_by_field_name("right") {
                self.validate_parallel_assignment(left, right, source, knowledge);
            }
        }
    }

    fn validate_parallel_assignment(
        &mut self,
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
            self.diagnostics
                .push(DiagnosticKind::ParallelAssignmentType.at(
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
            self.diagnostics.push(DiagnosticKind::ParallelAssignmentArity.at(
                source.range_for_node(left),
                format!(
                    "parallel assignment binds {} targets but the right-hand side lists {}; their lengths must match",
                    target_nodes.len(),
                    value_nodes.len()
                ),
            ));
            return;
        }

        for (target, value) in target_nodes.iter().zip(value_nodes.iter()) {
            self.validate_parallel_assignment(*target, *value, source, knowledge);
        }
    }

    fn diagnose_unused_bindings(
        &mut self,
        root: M2Node,
        source: &(impl SourceNavigation + ?Sized),
    ) {
        let mut used_bindings = HashSet::new();
        for node in root.descendants() {
            if node.kind.is_symbol_like() {
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
                Some(
                    DiagnosticKind::UnusedBinding
                        .at(binding.range, format!("Unused {noun} {name}")),
                )
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
