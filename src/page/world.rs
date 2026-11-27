//! `hba`'s isolated world in each frame, calls into its Playwright engine, and page JavaScript.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Arg, Page, Retry, World};
use crate::cdp::Cdp;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteObject {
    #[serde(default)]
    object_id: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    unserializable_value: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Evaluated {
    result: RemoteObject,
    #[serde(default)]
    exception_details: Option<ExceptionDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionDetails {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub exception: Option<Value>,
    #[serde(default)]
    pub line_number: i64,
    #[serde(default)]
    pub url: Option<String>,
}

impl ExceptionDetails {
    /// The thrown value's message without the stack.
    pub fn message(&self) -> String {
        let description = self.exception.as_ref().and_then(|e| {
            e.get("description").and_then(Value::as_str).or_else(|| e.get("value").and_then(Value::as_str))
        });
        let text = description.unwrap_or(&self.text);
        let mut lines = text.lines().take_while(|line| !line.trim_start().starts_with("at "));
        let first = lines.next().unwrap_or("").to_owned();
        lines.fold(first, |mut acc, line| {
            acc.push('\n');
            acc.push_str(line);
            acc
        })
    }
}

impl Page {
    /// `hba`'s world in `frame`, created on first use in each document.
    pub async fn world(&self, frame: &str) -> Result<World> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Created {
            execution_context_id: i64,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Evaluate<'a> {
            expression: crate::injected::Bootstrap<'a>,
            context_id: i64,
        }
        let (session, seq) = {
            let state = self.state();
            let entry = state.frames.get(frame).with_context(|| format!("frame {frame} is gone"))?;
            if let Some(world) = &entry.world {
                return Ok(world.clone());
            }
            (Arc::clone(&entry.session), entry.seq)
        };
        // A navigation can replace the document between the two calls.
        let created: Created = self
            .renderer_call(
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": self.source.world_name, "grantUniversalAccess": false}),
                &session,
            )
            .await
            .map_err(navigated)?;
        let params = Evaluate { expression: self.source.bootstrap(seq), context_id: created.execution_context_id };
        let evaluated: Evaluated = self.renderer_call("Runtime.evaluate", params, &session).await.map_err(navigated)?;
        if let Some(details) = evaluated.exception_details {
            bail!("starting the page engine failed: {}", details.message());
        }
        let engine = evaluated.result.object_id.context("the page engine did not start")?;
        let world = World { session, frame: frame.to_owned(), context: created.execution_context_id, engine };
        if let Some(entry) = self.state().frames.get_mut(frame) {
            entry.world = Some(world.clone());
        }
        Ok(world)
    }

    pub async fn main_world(&self) -> Result<World> {
        let frame = self.main_frame()?;
        self.world(&frame).await
    }

    fn forget_world(&self, world: &World) {
        if let Some(entry) = self.state().frames.get_mut(&world.frame)
            && entry.world.as_ref().is_some_and(|w| w.context == world.context)
        {
            entry.world = None;
        }
    }

    /// Calls `function` with `this` = the Playwright engine and returns its JSON result.
    pub async fn call(&self, world: &World, function: &str, args: &[Arg<'_>]) -> Result<Value> {
        let object = self.call_raw(world, function, args, true).await?;
        Ok(object.value.unwrap_or(Value::Null))
    }

    /// Like [`Page::call`] but returns a remote object id (`None` for null/undefined).
    pub async fn call_handle(&self, world: &World, function: &str, args: &[Arg<'_>]) -> Result<Option<String>> {
        let object = self.call_raw(world, function, args, false).await?;
        Ok(object.object_id.filter(|_| object.subtype.as_deref() != Some("null")))
    }

    async fn call_raw(&self, world: &World, function: &str, args: &[Arg<'_>], by_value: bool) -> Result<RemoteObject> {
        let arguments: Vec<Value> = args
            .iter()
            .map(|arg| match arg {
                Arg::Value(value) => json!({"value": value}),
                Arg::Object(id) => json!({"objectId": id}),
            })
            .collect();
        let params = json!({
            "objectId": world.engine,
            "functionDeclaration": function,
            "arguments": arguments,
            "returnByValue": by_value,
            "awaitPromise": true,
            "objectGroup": "hba",
        });
        let result: Result<Evaluated> = self.renderer_call("Runtime.callFunctionOn", &params, &world.session).await;
        let evaluated = result.map_err(|error| {
            if is_stale(&error) {
                self.forget_world(world);
            }
            navigated(error)
        })?;
        if let Some(details) = evaluated.exception_details {
            bail!("{}", details.message());
        }
        Ok(evaluated.result)
    }

    /// Evaluates page JavaScript in the main world and returns its value, as [`evaluate_on`] does,
    /// failing fast when a dialog blocks the page; a navigation meanwhile is a retryable error.
    pub async fn evaluate(&self, expression: &str, user_gesture: bool) -> Result<Value> {
        let session = self.session()?;
        let timeout = crate::cdp::CALL_TIMEOUT;
        self.guarded(evaluate_on(&self.cdp, &session, None, expression, user_gesture, timeout)).await.map_err(navigated)
    }

    /// Evaluates page JavaScript in a frame's main world, reached through the frame's document
    /// node, the way [`Page::evaluate`] does in the page: as a user gesture, with top-level
    /// `await`, whatever the frame's content security policy. `Runtime` stays disabled, so the
    /// world has no context id to evaluate in; the code runs as a function on the document
    /// instead. An expression gives its value; statements run as the function's body, where
    /// `return` gives one.
    pub async fn evaluate_in_frame(&self, frame: &str, code: &str) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Described {
            node: Node,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            backend_node_id: i64,
        }
        #[derive(Deserialize)]
        struct Resolved {
            object: RemoteObject,
        }
        let world = self.world(frame).await?;
        let document = self
            .call_handle(&world, "function() { return this.document; }", &[])
            .await?
            .context("the frame has no document")?;
        let described: Described =
            self.renderer_call("DOM.describeNode", json!({"objectId": document}), &world.session).await?;
        let resolved: Resolved = self
            .renderer_call(
                "DOM.resolveNode",
                json!({"backendNodeId": described.node.backend_node_id, "objectGroup": "hba"}),
                &world.session,
            )
            .await?;
        let object = resolved.object.object_id.context("the frame document is gone")?;
        let mut failure = String::new();
        for form in frame_functions(code) {
            let params = json!({"objectId": object, "functionDeclaration": form, "returnByValue": false,
                                "awaitPromise": false, "userGesture": true, "objectGroup": "hba"});
            let evaluated: Evaluated =
                self.renderer_call("Runtime.callFunctionOn", params, &world.session).await.map_err(navigated)?;
            let Some(details) = evaluated.exception_details else {
                let value = value_of(&self.cdp, &world.session, evaluated.result, crate::cdp::CALL_TIMEOUT);
                return self.guarded(value).await.map_err(navigated);
            };
            failure = details.message();
        }
        bail!("{failure}")
    }

    /// Releases temporary remote objects held for the last command, without waiting for the
    /// replies: a busy renderer must not hold up the command's answer. A page showing a dialog
    /// answers nothing until it closes; its objects are released after a later command.
    pub fn release(&self) {
        if self.dialog().is_some() {
            return;
        }
        let sessions: HashSet<Arc<str>> = {
            let state = self.state();
            let worlds = state.frames.values().filter(|f| f.world.is_some()).map(|f| Arc::clone(&f.session));
            worlds.chain(state.primary.clone()).collect()
        };
        let params = json!({"objectGroup": "hba"});
        for session in &sessions {
            // The command is written now; dropping the reply's future only stops waiting for it.
            drop(self.cdp.start("Runtime.releaseObjectGroup", &params, Some(session), crate::cdp::CALL_TIMEOUT));
        }
    }
}

/// The ways [`Page::evaluate_in_frame`] runs `code`, in order: as an expression, as statements
/// returning the last one's value (as `js` on the main frame does), then as plain statements.
/// The code runs in an async function, so whatever it throws rejects the promise the call
/// returns: only code that does not compile as one form is tried as the next. A split inside a
/// string or comment does not compile, so it falls through to the next form.
fn frame_functions(code: &str) -> Vec<String> {
    let wrap =
        |body: String| format!("function() {{ return (async function() {{ {body}\n }}).call(this.defaultView); }}");
    let expression = code.trim_end().trim_end_matches(';');
    let mut forms = vec![wrap(format!("return ({expression}\n);"))];
    if let Some(split) = expression.rfind([';', '\n'])
        && !expression[split + 1..].trim().is_empty()
    {
        let (head, last) = (&expression[..=split], &expression[split + 1..]);
        forms.push(wrap(format!("{head}\nreturn ({last}\n);")));
    }
    forms.push(wrap(code.to_owned()));
    forms
}

fn is_stale(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("Cannot find context with specified id")
        || text.contains("Could not find object with given id")
        || text.contains("Execution context was destroyed")
        || text.contains("Inspected target navigated or closed")
}

/// A call that lost its document to a navigation fails with the retryable [`Retry`]; other
/// errors stay as they are.
fn navigated(error: anyhow::Error) -> anyhow::Error {
    if is_stale(&error) { Retry("the page navigated while hba was working on it").into() } else { error }
}

/// The JSON value of a `returnByValue` result, with JS-only values rendered as strings.
fn remote_value(object: RemoteObject) -> Value {
    if let Some(value) = object.unserializable_value {
        return Value::String(value);
    }
    match (object.kind.as_str(), object.value) {
        ("undefined", _) => Value::Null,
        (_, Some(value)) => value,
        (_, None) => object.description.map_or(Value::Null, Value::String),
    }
}

/// Evaluates `code` on `session` (in execution context `context`, else the session's main
/// world) like the browser console: statements and top-level `await` work, a resulting promise
/// is awaited, and the value comes back as JSON; DOM nodes, functions and objects JSON cannot
/// hold come back as their description. Each protocol call may take up to `timeout`.
pub async fn evaluate_on(
    cdp: &Cdp,
    session: &str,
    context: Option<i64>,
    code: &str,
    user_gesture: bool,
    timeout: Duration,
) -> Result<Value> {
    let mut params = json!({"expression": code, "returnByValue": false, "awaitPromise": true, "replMode": true,
                            "userGesture": user_gesture, "allowUnsafeEvalBlockedByCSP": true, "objectGroup": "hba"});
    if let Some(context) = context {
        params["contextId"] = json!(context);
    }
    let evaluated = runtime_call(cdp, "Runtime.evaluate", &params, session, timeout).await?;
    if let Some(details) = evaluated.exception_details {
        bail!("{}", details.message());
    }
    value_of(cdp, session, evaluated.result, timeout).await
}

async fn runtime_call(cdp: &Cdp, method: &str, params: &Value, session: &str, timeout: Duration) -> Result<Evaluated> {
    let raw = cdp.raw_within(method, params, Some(session), timeout).await?;
    serde_json::from_str(raw.get()).with_context(|| format!("decoding the {method} result"))
}

/// Turns a remote result into JSON, awaiting promises.
async fn value_of(cdp: &Cdp, session: &str, object: RemoteObject, timeout: Duration) -> Result<Value> {
    let Some(id) = object.object_id.clone() else { return Ok(remote_value(object)) };
    match object.subtype.as_deref() {
        Some("promise") => {
            let params = json!({"promiseObjectId": id});
            let awaited = runtime_call(cdp, "Runtime.awaitPromise", &params, session, timeout).await?;
            if let Some(details) = awaited.exception_details {
                bail!("{}", details.message());
            }
            Box::pin(value_of(cdp, session, awaited.result, timeout)).await
        }
        None | Some("array") if object.kind == "object" => {
            let params =
                json!({"objectId": id, "functionDeclaration": "function() { return this; }", "returnByValue": true});
            match runtime_call(cdp, "Runtime.callFunctionOn", &params, session, timeout).await {
                Ok(evaluated) if evaluated.exception_details.is_none() => Ok(remote_value(evaluated.result)),
                _ => Ok(object.description.map_or(Value::Null, Value::String)),
            }
        }
        _ => Ok(object.description.map_or(Value::Null, Value::String)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_code_is_an_expression_first_and_never_eval() {
        let forms = frame_functions("await fetch('/x');  ");
        assert_eq!(forms.len(), 2, "{forms:?}");
        assert!(forms[0].contains("return (await fetch('/x')\n)"), "{}", forms[0]);
        assert!(forms[1].contains("{ await fetch('/x');  \n }"), "{}", forms[1]);
        let forms = frame_functions("const a = 1; a + 1");
        assert!(forms[1].contains("const a = 1;\nreturn ( a + 1\n);"), "{forms:?}");
        // `eval` is subject to the page's content security policy; a function declaration is not.
        assert!(forms.iter().all(|form| !form.contains("eval")));
    }
}
