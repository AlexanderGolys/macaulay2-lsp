//! Static type-hierarchy requests backed by generated builtin metadata.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde_json::{json, Value};
use tower::Service;
use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::lsp_types::{Position, Range, TypeHierarchyItem, Url};

use crate::document::DocumentSnapshot;
use crate::package_index::SourceResolver;
use crate::partitioned_index::{PackagePartitionedIndex, ScopedIndex};
use crate::record_lsp::record_symbol_kind;
use crate::typesystem::{InstanceID, Record};

pub(crate) const TYPE_HIERARCHY_METHOD: &str = "textDocument/prepareTypeHierarchy";

/// The static-metadata side of type hierarchy: resolve a record by name,
/// walk its parent/subtype edges, and materialize `TypeHierarchyItem`s. Born
/// out of the `Backend` handlers — kept here so `main.rs` only wires LSP
/// requests to thin shims, matching the other capability modules.
pub(crate) struct TypeHierarchyContext<'a> {
    partitioned: &'a PackagePartitionedIndex,
    source_resolver: &'a SourceResolver,
}

impl<'a> TypeHierarchyContext<'a> {
    pub(crate) fn new(
        partitioned: &'a PackagePartitionedIndex,
        source_resolver: &'a SourceResolver,
    ) -> Self {
        Self {
            partitioned,
            source_resolver,
        }
    }

    /// Prepare the type-hierarchy root for the symbol at `position`, when that
    /// symbol names a top-level type/class record in scope. The caller owns
    /// the document guard lookup *and* the `ScopedIndex` prologue (so the
    /// package scoping rule stays single-sourced on the backend); everything
    /// past the snapshot lives here.
    pub(crate) fn prepare(
        &self,
        document: &DocumentSnapshot,
        scoped: &ScopedIndex<'_>,
        uri: &Url,
        position: Position,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let node = document.symbol_node_at_position(position)?;
        let name = node.text();
        let range = document.range_for(node);

        let (package, record) = scoped.get_record_with_package(&InstanceID::new(name))?;
        record.type_info.as_ref()?;

        Some(vec![self.item(
            package,
            &record,
            Some(uri.clone()),
            Some(range),
        )])
    }

    /// The parent type's item, if the originating item's record has one and we
    /// can resolve it (preferring the originating package, falling back to Core).
    /// A record with no resolvable parent yields an empty vec (rather than
    /// `None`, which the LSP treats as "no supertypes response at all").
    pub(crate) fn supertypes(&self, item: &TypeHierarchyItem) -> Option<Vec<TypeHierarchyItem>> {
        let package = Self::package_of(item);
        let (package, record) = self.record(package, &item.name)?;

        // Empty parent (no parent, self-parent, or unresolved edge) → empty
        // vec, not `None`.
        let resolved = (|| {
            let parent_name = record
                .type_info
                .as_ref()?
                .parent_type
                .as_ref()
                .filter(|parent| parent != &&record.name)?;
            let (parent_package, parent_record) = self.related_record(&package, parent_name)?;
            Some(self.item(&parent_package, &parent_record, None, None))
        })();
        Some(resolved.into_iter().collect())
    }

    /// Every resolved subtype item of the originating item's record (skipping
    /// self-edges and unresolved names).
    pub(crate) fn subtypes(&self, item: &TypeHierarchyItem) -> Option<Vec<TypeHierarchyItem>> {
        let package = Self::package_of(item);
        let (package, record) = self.record(package, &item.name)?;

        let mut items = Vec::new();
        if let Some(type_info) = &record.type_info {
            for subtype in &type_info.subtypes {
                if subtype == &record.name {
                    continue;
                }
                if let Some((subtype_package, subtype_record)) =
                    self.related_record(&package, subtype)
                {
                    items.push(self.item(&subtype_package, &subtype_record, None, None));
                }
            }
        }
        Some(items)
    }

    /// The originating package name stashed in the item's `data`, set when the
    /// item was first materialized (so follow-up supertypes/subtypes know which
    /// partition to resolve from before falling back to Core).
    fn package_of(item: &TypeHierarchyItem) -> Option<&str> {
        item.data
            .as_ref()
            .and_then(|data| data.get("package"))
            .and_then(|package| package.as_str())
    }

    /// Resolve the originating record itself, ensuring it carries type info (a
    /// non-type record has no hierarchy edges).
    fn record(&self, package: Option<&str>, name: &str) -> Option<(String, Record)> {
        let package = package.unwrap_or("Core");
        let index = self.partitioned.partition(package)?;
        let record = index.get_record(&InstanceID::new(name))?;
        record.type_info.as_ref()?;
        Some((package.to_string(), record))
    }

    /// Resolve a related type (parent/subtype) record, preferring the
    /// originating package's partition and falling back to Core (cross-package
    /// edges into the Core lattice resolve there).
    fn related_record(&self, package: &str, name: &InstanceID) -> Option<(String, Record)> {
        if let Some(record) = self
            .partitioned
            .partition(package)
            .and_then(|index| index.get_record(name))
        {
            return Some((package.to_string(), record));
        }

        self.partitioned
            .partition("Core")
            .and_then(|core| core.get_record(name))
            .map(|record| ("Core".to_string(), record))
    }

    /// Materialize a `TypeHierarchyItem` for `record`, filling in the
    /// occurrence URI/range when the request comes from an editor position, and
    /// the resolved source location otherwise (so a builtin type without an
    /// in-editor occurrence still has a navigable target).
    fn item(
        &self,
        package: &str,
        record: &Record,
        occurrence_uri: Option<Url>,
        occurrence_range: Option<Range>,
    ) -> TypeHierarchyItem {
        let location = self.source_resolver.record_location(record);
        let uri = occurrence_uri
            .or_else(|| location.as_ref().map(|location| location.uri.clone()))
            .unwrap_or_else(|| Url::parse("macaulay2:/builtins").expect("valid builtin URI"));
        let range = occurrence_range
            .or_else(|| location.as_ref().map(|location| location.range))
            .unwrap_or_else(|| Range::new(Position::new(0, 0), Position::new(0, 0)));
        let detail = record
            .type_info
            .as_ref()
            .and_then(|type_info| type_info.parent_type.as_ref())
            .filter(|parent| parent != &&record.name)
            .map(|parent| format!("Parent: {parent}"));

        TypeHierarchyItem {
            name: record.name.0.clone(),
            kind: record_symbol_kind(record),
            tags: None,
            detail,
            uri,
            range,
            selection_range: range,
            data: Some(json!({
                "name": record.name.0.clone(),
                "package": package,
            })),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TypeHierarchyCapabilityService<S> {
    inner: S,
}

impl<S> TypeHierarchyCapabilityService<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Service<Request> for TypeHierarchyCapabilityService<S>
where
    S: Service<Request, Response = Option<Response>> + Send + 'static,
    S::Error: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let should_advertise_type_hierarchy = req.method() == "initialize"
            && !request_type_hierarchy_dynamic_registration(req.params());
        let fut = self.inner.call(req);

        Box::pin(async move {
            let response = fut.await?;
            if should_advertise_type_hierarchy {
                Ok(response.map(advertise_type_hierarchy_capability))
            } else {
                Ok(response)
            }
        })
    }
}

/// Whether the client's `initialize` params advertise dynamic registration for
/// type hierarchy. When false (or absent), the server must declare the
/// capability statically in its `InitializeResult` instead.
pub(crate) fn request_type_hierarchy_dynamic_registration(params: Option<&Value>) -> bool {
    params
        .and_then(|params| params.get("capabilities"))
        .and_then(|capabilities| capabilities.get("textDocument"))
        .and_then(|text_document| text_document.get("typeHierarchy"))
        .and_then(|type_hierarchy| type_hierarchy.get("dynamicRegistration"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Inject `typeHierarchyProvider: true` into a successful `initialize` response.
/// Used when the client does not support dynamic registration, so the capability
/// is advertised statically (tower-lsp has no typed field for it).
pub(crate) fn advertise_type_hierarchy_capability(response: Response) -> Response {
    if !response.is_ok() {
        return response;
    }

    let (id, body) = response.into_parts();
    Response::from_parts(
        id,
        body.map(|mut result| {
            if let Some(capabilities) = result
                .get_mut("capabilities")
                .and_then(Value::as_object_mut)
            {
                capabilities
                    .entry("typeHierarchyProvider")
                    .or_insert_with(|| json!(true));
            }
            result
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_result_advertises_static_type_hierarchy() {
        let response = Response::from_ok(
            1.into(),
            json!({
                "capabilities": {
                    "hoverProvider": true
                }
            }),
        );

        let response = advertise_type_hierarchy_capability(response);
        let result = response
            .result()
            .expect("response should remain successful");

        assert_eq!(result["capabilities"]["typeHierarchyProvider"], json!(true));
    }

    #[test]
    fn type_hierarchy_dynamic_registration_detection_defaults_to_static() {
        let dynamic = Request::build("initialize")
            .params(json!({
                "capabilities": {
                    "textDocument": {
                        "typeHierarchy": {
                            "dynamicRegistration": true
                        }
                    }
                }
            }))
            .id(1)
            .finish();
        let static_only = Request::build("initialize")
            .params(json!({
                "capabilities": {
                    "textDocument": {
                        "typeHierarchy": {
                            "dynamicRegistration": false
                        }
                    }
                }
            }))
            .id(2)
            .finish();

        assert!(request_type_hierarchy_dynamic_registration(
            dynamic.params()
        ));
        assert!(!request_type_hierarchy_dynamic_registration(
            static_only.params()
        ));
        assert!(!request_type_hierarchy_dynamic_registration(None));
    }
}
