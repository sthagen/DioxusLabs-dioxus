// API inspired by Reacts implementation of head only elements. We use components here instead of elements to simplify internals.

use std::{
    rc::Rc,
    task::{Context, Poll},
};

use generational_box::{AnyStorage, GenerationalBox, UnsyncStorage};

mod bindings;
#[allow(unused)]
pub use bindings::*;
mod eval;
pub use eval::*;

pub mod head;
pub use head::{Meta, MetaProps, Script, ScriptProps, Style, StyleProps, Title, TitleProps};

fn format_string_for_js(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn format_attributes(attributes: &[(&str, String)]) -> String {
    let mut formatted = String::from("[");
    for (key, value) in attributes {
        formatted.push_str(&format!(
            "[{}, {}],",
            format_string_for_js(key),
            format_string_for_js(value)
        ));
    }
    if formatted.ends_with(',') {
        formatted.pop();
    }
    formatted.push(']');
    formatted
}

fn create_element_in_head(
    tag: &str,
    attributes: &[(&str, String)],
    children: Option<String>,
) -> String {
    let helpers = include_str!("../js/head.js");
    let attributes = format_attributes(attributes);
    let children = children
        .as_deref()
        .map(format_string_for_js)
        .unwrap_or("null".to_string());
    let tag = format_string_for_js(tag);
    format!(r#"{helpers};window.createElementInHead({tag}, {attributes}, {children});"#)
}

/// A provider for document-related functionality. By default most methods are driven through [`eval`].
pub trait Document {
    /// Create a new evaluator for the document that evaluates JavaScript and facilitates communication between JavaScript and Rust.
    fn new_evaluator(&self, js: String) -> GenerationalBox<Box<dyn Evaluator>>;

    /// Set the title of the document
    fn set_title(&self, title: String) {
        self.new_evaluator(format!("document.title = {title:?};"));
    }

    /// Create a new meta tag
    fn create_meta(&self, props: MetaProps) {
        let attributes = props.attributes();
        let js = create_element_in_head("meta", &attributes, None);
        self.new_evaluator(js);
    }

    /// Create a new script tag
    fn create_script(&self, props: ScriptProps) {
        let attributes = props.attributes();
        let js = match (&props.src, props.script_contents()) {
            // The script has inline contents, render it as a script tag
            (_, Ok(contents)) => create_element_in_head("script", &attributes, Some(contents)),
            // The script has a src, render it as a script tag without a body
            (Some(_), _) => create_element_in_head("script", &attributes, None),
            // The script has neither contents nor src, log an error
            (None, Err(err)) => {
                err.log("Script");
                return;
            }
        };
        self.new_evaluator(js);
    }

    /// Create a new style tag
    fn create_style(&self, props: StyleProps) {
        let mut attributes = props.attributes();
        let js = match (&props.href, props.style_contents()) {
            // The style has inline contents, render it as a style tag
            (_, Ok(contents)) => create_element_in_head("style", &attributes, Some(contents)),
            // The style has a src, render it as a link tag
            (Some(_), _) => {
                attributes.push(("type", "text/css".into()));
                create_element_in_head("link", &attributes, None)
            }
            // The style has neither contents nor src, log an error
            (None, Err(err)) => {
                err.log("Style");
                return;
            }
        };
        self.new_evaluator(js);
    }

    /// Create a new link tag
    fn create_link(&self, props: head::LinkProps) {
        let attributes = props.attributes();
        let js = create_element_in_head("link", &attributes, None);
        self.new_evaluator(js);
    }

    /// Get a reference to the document as `dyn Any`
    fn as_any(&self) -> &dyn std::any::Any;
}

/// The default No-Op document
pub struct NoOpDocument;

impl Document for NoOpDocument {
    fn new_evaluator(&self, _js: String) -> GenerationalBox<Box<dyn Evaluator>> {
        tracing::error!("Eval is not supported on this platform. If you are using dioxus fullstack, you can wrap your code with `client! {{}}` to only include the code that runs eval in the client bundle.");
        UnsyncStorage::owner().insert(Box::new(NoOpEvaluator))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct NoOpEvaluator;
impl Evaluator for NoOpEvaluator {
    fn send(&self, _data: serde_json::Value) -> Result<(), EvalError> {
        Err(EvalError::Unsupported)
    }
    fn poll_recv(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<Result<serde_json::Value, EvalError>> {
        Poll::Ready(Err(EvalError::Unsupported))
    }
    fn poll_join(
        &mut self,
        _context: &mut Context<'_>,
    ) -> Poll<Result<serde_json::Value, EvalError>> {
        Poll::Ready(Err(EvalError::Unsupported))
    }
}

/// Get the document provider for the current platform or a no-op provider if the platform doesn't document functionality.
pub fn document() -> Rc<dyn Document> {
    dioxus_core::prelude::try_consume_context::<Rc<dyn Document>>()
        // Create a NoOp provider that always logs an error when trying to evaluate
        // That way, we can still compile and run the code without a real provider
        .unwrap_or_else(|| Rc::new(NoOpDocument) as Rc<dyn Document>)
}
