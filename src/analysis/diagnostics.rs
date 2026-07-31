use super::*;

impl Analysis {
    pub(super) fn collect_installation_diagnostics(
        &mut self,
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
        self.diagnostics.extend(diagnostics);
    }

    pub(super) fn collect_install_form_diagnostics(
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
        if let Some(name) = self.illegal_equals_install_head(node, &knowledge) {
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
        Some(name.name().to_string())
    }

    fn installation_diagnostics(
        &self,
        installation: &MethodInstallation,
        knowledge: &(impl TypeKnowledge + ?Sized),
        out: &mut Vec<M2Diagnostic>,
    ) {
        match &installation.method.head {
            MethodHead::Function(name) => {
                if !self.head_is_method_function(name.name(), installation.span.start, knowledge) {
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
