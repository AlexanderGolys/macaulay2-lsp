use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde_json::{json, Value};
use tower::Service;
use tower_lsp::jsonrpc::{Request, Response};

pub(crate) const TYPE_HIERARCHY_METHOD: &str = "textDocument/prepareTypeHierarchy";

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
