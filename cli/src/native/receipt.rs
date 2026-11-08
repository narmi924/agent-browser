//! Action receipts: what a mutating command can prove about its own effect.
//!
//! A receipt is attached to the JSON of click, fill, type, select, check,
//! drag, and upload as `data.receipt`:
//!
//! ```json
//! { "verified": true,
//!   "checks": [{ "name": "value", "expected": "abc", "actual": "abc", "ok": true }],
//!   "effects": { "url_changed": false, "new_tab": false, "dialog_opened": false,
//!                "dom_changed": false, "target_removed": false } }
//! ```
//!
//! `verified` is true when every check passed, false when one failed, and null
//! when nothing could be checked. A probe installed before the action records
//! events and DOM mutations without touching page globals; it is read and
//! removed afterwards. Every page round trip here is bounded so a busy or
//! navigating renderer costs the receipt, never the action.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use super::cdp::client::CdpClient;
use super::cdp::types::CdpEvent;
use super::element::{resolve_element_object_id, top_document_lookup_expression, RefMap};

/// Bound on each page round trip made on behalf of a receipt.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a click waits for a tab or navigation it may have caused. Other
/// actions read only what has already arrived.
pub const CLICK_SETTLE: Duration = Duration::from_millis(12);

const MAX_RECORDED_TEXT: usize = 200;

/// Page-side probe. Runs against the target element and returns an object
/// whose handle stays in the page until `READ_PROBE_JS` removes its listeners.
const INSTALL_PROBE_JS: &str = r#"(el, mode) => {
    const doc = (el && el.ownerDocument) || document;
    const win = doc.defaultView || window;
    const attr = (name) => (el && el.getAttribute ? (el.getAttribute(name) || '') : '').toLowerCase();
    const identity = attr('name') + ' ' + attr('id');
    const sensitive = attr('type') === 'password'
        || /(^|\s)(cc-[a-z-]+|one-time-code|current-password|new-password|webauthn)(\s|$)/.test(attr('autocomplete'))
        || /(^|[-_\s])(cvv|cvc|otp|pin)([-_\s]|$)/.test(identity);
    const s = {
        el, win, mode, url: win.location.href, mutated: 0, events: [], sensitive,
        types: ['click', 'auxclick', 'contextmenu', 'drop', 'mouseup'],
        predicted: undefined, content: mode === 'drop' && el ? el.innerHTML : undefined,
    };
    s.mo = new win.MutationObserver((records) => { s.mutated += records.length; });
    s.mo.observe(doc, { subtree: true, childList: true, attributes: true, characterData: true });
    s.on = (event) => {
        if (!el) return;
        const path = event.composedPath ? event.composedPath() : [];
        if (path.includes(el) || event.target === el || (el.contains && event.target instanceof win.Node && el.contains(event.target))) {
            s.events.push([event.type, event.isTrusted]);
        }
    };
    for (const type of s.types) win.addEventListener(type, s.on, true);
    return s;
}"#;

/// Reads the probe, removes its listeners, and reports what the action did.
const READ_PROBE_JS: &str = r#"function() {
    const s = this;
    let flushed = [];
    try { flushed = s.mo.takeRecords(); s.mo.disconnect(); } catch (e) {}
    for (const type of s.types) s.win.removeEventListener(type, s.on, true);
    const el = s.el;
    const out = {
        connected: !el || el.isConnected,
        url: s.win.location.href,
        urlBefore: s.url,
        mutated: s.mutated + flushed.length > 0,
        events: s.events,
        sensitive: s.sensitive,
        predicted: s.predicted,
        hasTarget: !!el,
    };
    if (!el) return out;
    if (typeof el.value === 'string') out.value = el.value;
    else if (el.isContentEditable) { out.value = el.innerText; out.editable = true; }
    if (el.files) out.files = Array.from(el.files).map((file) => file.name);
    if (el.selectedOptions) out.selected = Array.from(el.selectedOptions).map((option) => ({
        value: option.value, label: option.label, text: (option.textContent || '').trim(),
    }));
    if (s.mode === 'drop') out.contentChanged = s.content !== el.innerHTML;
    return out;
}"#;

/// Records what the field should contain once `text` is typed at the caret.
const CAPTURE_BASELINE_JS: &str = r#"function(text) {
    const el = this.el;
    if (!el || typeof el.value !== 'string') return;
    const value = el.value;
    let start = null, end = null;
    try { start = el.selectionStart; end = el.selectionEnd; } catch (e) {}
    if (start === null || start === undefined) { start = value.length; end = value.length; }
    const entered = el.tagName === 'INPUT' ? text.replace(/[\r\n]/g, '') : text;
    this.predicted = value.slice(0, start) + entered + value.slice(end);
}"#;

/// Where a receipt's probe should attach.
pub struct WatchScope<'a> {
    pub client: &'a CdpClient,
    pub page_session: &'a str,
    pub page_target: Option<&'a str>,
    pub page_url: Option<&'a str>,
    pub ref_map: &'a RefMap,
    pub iframe_sessions: &'a HashMap<String, String>,
}

/// What the command expected to be true afterwards.
pub enum Expectation<'a> {
    /// A pointer press should have reached the target.
    Click,
    /// The target's value should equal exactly this.
    Value(&'a str),
    /// The field should hold `text` at the caret captured before typing.
    Typed(&'a str),
    /// These options should be selected.
    Selected(&'a [String]),
    /// The control's checked state should be this.
    Checked {
        expected: bool,
        actual: Option<bool>,
    },
    /// The file input should list these file names.
    Files(&'a [String]),
    /// The drop target should have received the drag.
    Drop,
}

#[derive(Debug, Default, Clone)]
pub struct Effects {
    pub url_changed: bool,
    pub new_tab: bool,
    pub dialog_opened: bool,
    pub dom_changed: bool,
    pub target_removed: bool,
}

#[derive(Debug, Default)]
struct Verdict {
    checks: Vec<Value>,
    verified: Option<bool>,
}

impl Verdict {
    /// Record one check. A failure is sticky; a pass only upgrades "unknown".
    fn push(&mut self, ok: Option<bool>, check: Value) {
        let mut check = check;
        if let Some(obj) = check.as_object_mut() {
            obj.insert("ok".into(), ok.map(Value::Bool).unwrap_or(Value::Null));
        }
        self.checks.push(check);
        self.verified = match (self.verified, ok) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (_, Some(true)) => Some(true),
            (current, None) => current,
        };
    }
}

fn truncated(text: &str) -> String {
    if text.chars().count() <= MAX_RECORDED_TEXT {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_RECORDED_TEXT).collect();
    format!("{head}...")
}

fn same_text(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

/// Observes one action. Construction never fails: when the probe cannot be
/// installed the receipt reports `verified: null` for the page-side checks.
pub struct ReceiptWatch {
    events: broadcast::Receiver<CdpEvent>,
    page_session: String,
    page_target: Option<String>,
    page_url: Option<String>,
    probe: Option<(String, String)>,
}

impl ReceiptWatch {
    fn unattached(scope: &WatchScope<'_>) -> Self {
        Self {
            events: scope.client.subscribe(),
            page_session: scope.page_session.to_string(),
            page_target: scope.page_target.map(str::to_string),
            page_url: scope.page_url.map(str::to_string),
            probe: None,
        }
    }

    /// Install a probe on the element `selector` resolves to.
    pub async fn on_element(scope: &WatchScope<'_>, selector: &str, mode: &str) -> Self {
        let mut watch = Self::unattached(scope);
        if let Some(lookup) = top_document_lookup_expression(selector) {
            // One round trip: the probe keeps the element it found.
            let expression = format!(
                "(() => {{ const el = {lookup}; return el ? ({INSTALL_PROBE_JS})(el, {mode:?}) : null; }})()"
            );
            let attached = tokio::time::timeout(
                PROBE_TIMEOUT,
                scope.client.send_command(
                    "Runtime.evaluate",
                    Some(json!({ "expression": expression, "returnByValue": false })),
                    Some(scope.page_session),
                ),
            )
            .await;
            if let Ok(Ok(result)) = attached {
                if let Some(probe) = result["result"]["objectId"].as_str() {
                    watch.probe = Some((probe.to_string(), scope.page_session.to_string()));
                }
            }
            return watch;
        }
        let attached = tokio::time::timeout(PROBE_TIMEOUT, async {
            let (object_id, session) = resolve_element_object_id(
                scope.client,
                scope.page_session,
                scope.ref_map,
                selector,
                scope.iframe_sessions,
            )
            .await?;
            let result = scope
                .client
                .send_command(
                    "Runtime.callFunctionOn",
                    Some(json!({
                        "objectId": object_id,
                        "functionDeclaration": format!(
                            "function(mode) {{ return ({INSTALL_PROBE_JS})(this, mode); }}"
                        ),
                        "arguments": [{ "value": mode }],
                        "returnByValue": false,
                    })),
                    Some(&session),
                )
                .await?;
            Ok::<_, String>((result, session))
        })
        .await;
        if let Ok(Ok((result, session))) = attached {
            if let Some(probe) = result["result"]["objectId"].as_str() {
                watch.probe = Some((probe.to_string(), session));
            }
        }
        watch
    }

    /// Install a probe on whatever currently has keyboard focus.
    pub async fn on_focused(scope: &WatchScope<'_>) -> Self {
        let mut watch = Self::unattached(scope);
        let expression = format!(
            r#"(() => {{
                let active = document.activeElement;
                while (active && active.shadowRoot && active.shadowRoot.activeElement) {{
                    active = active.shadowRoot.activeElement;
                }}
                const editable = active && (typeof active.value === 'string' || active.isContentEditable);
                return ({INSTALL_PROBE_JS})(editable ? active : null, 'value');
            }})()"#
        );
        let attached = tokio::time::timeout(
            PROBE_TIMEOUT,
            scope.client.send_command(
                "Runtime.evaluate",
                Some(json!({ "expression": expression, "returnByValue": false })),
                Some(scope.page_session),
            ),
        )
        .await;
        if let Ok(Ok(result)) = attached {
            if let Some(probe) = result["result"]["objectId"].as_str() {
                watch.probe = Some((probe.to_string(), scope.page_session.to_string()));
            }
        }
        watch
    }

    async fn call_probe(
        &self,
        client: &CdpClient,
        function: &str,
        arguments: Value,
    ) -> Option<Value> {
        let (object_id, session) = self.probe.as_ref()?;
        let response = tokio::time::timeout(
            PROBE_TIMEOUT,
            client.send_command(
                "Runtime.callFunctionOn",
                Some(json!({
                    "objectId": object_id,
                    "functionDeclaration": function,
                    "arguments": arguments,
                    "returnByValue": true,
                })),
                Some(session),
            ),
        )
        .await
        .ok()?
        .ok()?;
        if response.get("exceptionDetails").is_some() {
            return None;
        }
        Some(response["result"]["value"].clone())
    }

    /// Whether the field must not receive deliberate typing mistakes. A field
    /// that cannot be inspected counts as sensitive.
    pub async fn is_sensitive_field(&self, client: &CdpClient) -> bool {
        self.call_probe(client, "function() { return this.sensitive; }", json!([]))
            .await
            .and_then(|value| value.as_bool())
            .unwrap_or(true)
    }

    /// Predict the field's final value from its value and caret right now.
    /// Call after focus (and any `--clear`), before the first keystroke.
    pub async fn capture_typing_baseline(&self, client: &CdpClient, text: &str) {
        let _ = self
            .call_probe(client, CAPTURE_BASELINE_JS, json!([{ "value": text }]))
            .await;
    }

    /// Read the probe and any browser events, and build the receipt.
    pub async fn finish(
        mut self,
        client: &CdpClient,
        expectation: Expectation<'_>,
        dialog_opened: bool,
        settle: Duration,
    ) -> Value {
        let mut effects = Effects {
            dialog_opened,
            ..Effects::default()
        };
        let page = if dialog_opened {
            None
        } else {
            let mode = json!([]);
            self.call_probe(client, READ_PROBE_JS, mode).await
        };
        self.collect_events(&mut effects, settle).await;

        let mut verdict = Verdict::default();
        if let Some(page) = &page {
            effects.dom_changed = page["mutated"].as_bool().unwrap_or(false);
            effects.target_removed = !page["connected"].as_bool().unwrap_or(true);
            if page["url"] != page["urlBefore"] {
                effects.url_changed = true;
            }
        }
        record_checks(&mut verdict, &expectation, page.as_ref(), &effects);
        json!({
            "verified": verdict.verified,
            "checks": verdict.checks,
            "effects": {
                "url_changed": effects.url_changed,
                "new_tab": effects.new_tab,
                "dialog_opened": effects.dialog_opened,
                "dom_changed": effects.dom_changed,
                "target_removed": effects.target_removed,
            },
        })
    }

    async fn collect_events(&mut self, effects: &mut Effects, settle: Duration) {
        let deadline = tokio::time::Instant::now() + settle;
        loop {
            let event = match self.events.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Lagged(_)) => continue,
                Err(TryRecvError::Closed) => return,
                Err(TryRecvError::Empty) => {
                    match tokio::time::timeout_at(deadline, self.events.recv()).await {
                        Ok(Ok(event)) => event,
                        Ok(Err(RecvError::Lagged(_))) => continue,
                        Ok(Err(RecvError::Closed)) | Err(_) => return,
                    }
                }
            };
            self.observe(&event, effects);
        }
    }

    fn is_own_session(&self, event: &CdpEvent) -> bool {
        event
            .session_id
            .as_deref()
            .is_none_or(|session| session == self.page_session)
    }

    fn is_main_frame(&self, frame_id: Option<&str>) -> bool {
        match (&self.page_target, frame_id) {
            (Some(target), Some(frame)) => target == frame,
            _ => true,
        }
    }

    fn marks_url_change(&self, url: Option<&str>) -> bool {
        match (&self.page_url, url) {
            (Some(before), Some(after)) => before != after,
            _ => true,
        }
    }

    fn observe(&self, event: &CdpEvent, effects: &mut Effects) {
        match event.method.as_str() {
            "Target.targetCreated" => {
                let info = &event.params["targetInfo"];
                let opener = info["openerId"].as_str();
                let ours = match (&self.page_target, opener) {
                    (Some(target), Some(opener)) => target == opener,
                    _ => true,
                };
                if info["type"] == "page" && ours {
                    effects.new_tab = true;
                }
            }
            "Page.javascriptDialogOpening" if self.is_own_session(event) => {
                effects.dialog_opened = true;
            }
            "Page.frameRequestedNavigation" if self.is_own_session(event) => {
                if self.is_main_frame(event.params["frameId"].as_str()) {
                    match event.params["disposition"].as_str() {
                        Some("newTab" | "newWindow") => effects.new_tab = true,
                        _ => {
                            if self.marks_url_change(event.params["url"].as_str()) {
                                effects.url_changed = true;
                            }
                        }
                    }
                }
            }
            "Page.frameNavigated" if self.is_own_session(event) => {
                let frame = &event.params["frame"];
                if frame.get("parentId").is_none() && self.marks_url_change(frame["url"].as_str()) {
                    effects.url_changed = true;
                }
            }
            "Page.navigatedWithinDocument"
                if self.is_own_session(event)
                    && self.is_main_frame(event.params["frameId"].as_str())
                    && self.marks_url_change(event.params["url"].as_str()) =>
            {
                effects.url_changed = true;
            }
            _ => {}
        }
    }
}

fn record_checks(
    verdict: &mut Verdict,
    expectation: &Expectation<'_>,
    page: Option<&Value>,
    effects: &Effects,
) {
    let Some(page) = page else {
        record_unreadable(verdict, expectation, effects);
        return;
    };
    match expectation {
        Expectation::Click => {
            let events: Vec<&Value> = page["events"]
                .as_array()
                .map(|all| all.iter().filter(|e| e[0] != "mouseup").collect())
                .unwrap_or_default();
            if events.is_empty() {
                verdict.push(Some(false), json!({ "name": "click_received" }));
            } else {
                let trusted = events.iter().any(|e| e[1] == true);
                verdict.push(
                    Some(true),
                    json!({ "name": "click_received", "trusted": trusted }),
                );
            }
        }
        Expectation::Value(expected) => {
            record_value(verdict, page, Some(expected));
        }
        Expectation::Typed(typed) => {
            let predicted = page["predicted"].as_str();
            if predicted.is_some() {
                record_value(verdict, page, predicted);
            } else if page["editable"] == true {
                let actual = page["value"].as_str().unwrap_or_default();
                let ok = actual.contains(*typed);
                record_redactable(
                    verdict,
                    page,
                    "value_contains_typed_text",
                    typed,
                    actual,
                    ok,
                );
            } else {
                verdict.push(
                    None,
                    json!({ "name": "value", "reason": "no editable field had focus" }),
                );
            }
        }
        Expectation::Selected(expected) => {
            let actual = page["selected"].as_array();
            let selected = actual.map(|items| {
                items
                    .iter()
                    .map(|option| option["label"].as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>()
            });
            let ok = actual.map(|items| {
                expected.iter().all(|want| {
                    items.iter().any(|option| {
                        option["value"] == want.as_str()
                            || option["label"] == want.as_str()
                            || option["text"] == want.as_str()
                    })
                })
            });
            verdict.push(
                ok,
                json!({ "name": "selected", "expected": expected, "actual": selected }),
            );
        }
        Expectation::Checked { expected, actual } => {
            verdict.push(
                actual.map(|value| value == *expected),
                json!({ "name": "checked", "expected": expected, "actual": actual }),
            );
        }
        Expectation::Files(expected) => {
            let actual: Vec<String> = page["files"]
                .as_array()
                .map(|names| {
                    names
                        .iter()
                        .filter_map(|name| name.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let wanted: Vec<String> = expected
                .iter()
                .map(|path| {
                    std::path::Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone())
                })
                .collect();
            let ok = wanted.len() == actual.len() && wanted.iter().all(|w| actual.contains(w));
            verdict.push(
                Some(ok),
                json!({ "name": "files", "expected": wanted, "actual": actual }),
            );
        }
        Expectation::Drop => {
            let changed = page["contentChanged"].as_bool().unwrap_or(false);
            let dropped = page["events"].as_array().is_some_and(|all| {
                all.iter()
                    .any(|e| matches!(e[0].as_str(), Some("drop" | "mouseup")))
            });
            verdict.push(
                Some(changed || dropped),
                json!({ "name": "drop_target", "content_changed": changed, "drop_received": dropped }),
            );
        }
    }
}

/// The page could not be read: a dialog is blocking it, or the action replaced
/// the document. A navigation still proves a click arrived.
fn record_unreadable(verdict: &mut Verdict, expectation: &Expectation<'_>, effects: &Effects) {
    let reason = if effects.dialog_opened {
        "a JavaScript dialog is blocking the page"
    } else if effects.url_changed {
        "the page navigated"
    } else {
        "the page did not answer"
    };
    match expectation {
        Expectation::Click if effects.url_changed || effects.new_tab => {
            verdict.push(
                Some(true),
                json!({ "name": "click_received", "via": "navigation" }),
            );
        }
        _ => verdict.push(None, json!({ "name": "page_state", "reason": reason })),
    }
}

fn record_value(verdict: &mut Verdict, page: &Value, expected: Option<&str>) {
    let (Some(expected), Some(actual)) = (expected, page["value"].as_str()) else {
        verdict.push(
            None,
            json!({ "name": "value", "reason": "the target has no readable value" }),
        );
        return;
    };
    let ok = same_text(expected, actual);
    record_redactable(verdict, page, "value", expected, actual, ok);
}

/// Report a value comparison, keeping the text out of the receipt for
/// passwords, card numbers, and one-time codes.
fn record_redactable(
    verdict: &mut Verdict,
    page: &Value,
    name: &str,
    expected: &str,
    actual: &str,
    ok: bool,
) {
    let check = if page["sensitive"] == true {
        json!({ "name": name, "redacted": true })
    } else {
        json!({ "name": name, "expected": truncated(expected), "actual": truncated(actual) })
    };
    verdict.push(Some(ok), check);
}

/// Receipt for `click --new-tab`, which opens the link in a tab it created.
pub fn new_tab_opened(url: &str) -> Value {
    json!({
        "verified": true,
        "checks": [{ "name": "tab_opened", "url": url, "ok": true }],
        "effects": {
            "url_changed": false,
            "new_tab": true,
            "dialog_opened": false,
            "dom_changed": false,
            "target_removed": false,
        },
    })
}

/// A receipt for a backend that cannot observe the page (WebDriver).
pub fn unobserved() -> Value {
    json!({ "verified": null, "checks": [], "effects": null })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(value: Value) -> Value {
        value
    }

    fn built(expectation: Expectation<'_>, page: Option<Value>, effects: Effects) -> Verdict {
        let mut verdict = Verdict::default();
        record_checks(&mut verdict, &expectation, page.as_ref(), &effects);
        verdict
    }

    #[test]
    fn value_mismatch_is_verified_false_and_carries_both_values() {
        let verdict = built(
            Expectation::Value("abc"),
            Some(page(json!({ "value": "ab" }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(false));
        assert_eq!(verdict.checks[0]["expected"], "abc");
        assert_eq!(verdict.checks[0]["actual"], "ab");
        assert_eq!(verdict.checks[0]["ok"], false);
    }

    #[test]
    fn value_read_back_treats_crlf_like_lf() {
        let verdict = built(
            Expectation::Value("a\r\nb"),
            Some(page(json!({ "value": "a\nb" }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(true));
    }

    #[test]
    fn sensitive_values_never_appear_in_the_receipt() {
        let verdict = built(
            Expectation::Value("hunter2"),
            Some(page(json!({ "value": "hunter2", "sensitive": true }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(true));
        let text = verdict.checks[0].to_string();
        assert!(!text.contains("hunter2"), "leaked secret: {text}");
        assert_eq!(verdict.checks[0]["redacted"], true);
    }

    #[test]
    fn typed_compares_against_the_prediction_for_the_caret() {
        let verdict = built(
            Expectation::Typed("xy"),
            Some(page(json!({ "value": "aXYb", "predicted": "axyb" }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(false));
    }

    #[test]
    fn typing_with_no_editable_focus_cannot_be_checked() {
        let verdict = built(
            Expectation::Typed("hello"),
            Some(page(json!({ "hasTarget": false }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, None);
    }

    #[test]
    fn click_received_records_trust_and_ignores_plain_mouseup() {
        let trusted = built(
            Expectation::Click,
            Some(page(
                json!({ "events": [["mouseup", true], ["click", true]] }),
            )),
            Effects::default(),
        );
        assert_eq!(trusted.verified, Some(true));
        assert_eq!(trusted.checks[0]["trusted"], true);

        let missed = built(
            Expectation::Click,
            Some(page(json!({ "events": [["mouseup", true]] }))),
            Effects::default(),
        );
        assert_eq!(missed.verified, Some(false));
    }

    #[test]
    fn click_that_replaced_the_page_counts_as_received() {
        let effects = Effects {
            url_changed: true,
            ..Effects::default()
        };
        let verdict = built(Expectation::Click, None, effects);
        assert_eq!(verdict.verified, Some(true));
        assert_eq!(verdict.checks[0]["via"], "navigation");
    }

    #[test]
    fn unreadable_page_is_unknown_not_a_pass() {
        let effects = Effects {
            dialog_opened: true,
            ..Effects::default()
        };
        let verdict = built(Expectation::Click, None, effects);
        assert_eq!(verdict.verified, None);
        assert!(verdict.checks[0]["reason"]
            .as_str()
            .unwrap()
            .contains("dialog"));
    }

    #[test]
    fn drop_reports_content_change_and_passes_on_drop_event() {
        let changed = built(
            Expectation::Drop,
            Some(page(json!({ "contentChanged": true, "events": [] }))),
            Effects::default(),
        );
        assert_eq!(changed.verified, Some(true));
        assert_eq!(changed.checks[0]["content_changed"], true);

        let dropped = built(
            Expectation::Drop,
            Some(page(
                json!({ "contentChanged": false, "events": [["drop", true]] }),
            )),
            Effects::default(),
        );
        assert_eq!(dropped.verified, Some(true));
        assert_eq!(dropped.checks[0]["content_changed"], false);

        let nothing = built(
            Expectation::Drop,
            Some(page(json!({ "contentChanged": false, "events": [] }))),
            Effects::default(),
        );
        assert_eq!(nothing.verified, Some(false));
    }

    #[test]
    fn selected_matches_by_value_label_or_text() {
        let expected = vec!["Two".to_string()];
        let verdict = built(
            Expectation::Selected(&expected),
            Some(page(
                json!({ "selected": [{ "value": "2", "label": "Two", "text": "Two" }] }),
            )),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(true));
        let wrong = vec!["Three".to_string()];
        let verdict = built(
            Expectation::Selected(&wrong),
            Some(page(
                json!({ "selected": [{ "value": "2", "label": "Two", "text": "Two" }] }),
            )),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(false));
    }

    #[test]
    fn upload_compares_file_names_only() {
        let expected = vec!["/tmp/report.pdf".to_string()];
        let verdict = built(
            Expectation::Files(&expected),
            Some(page(json!({ "files": ["report.pdf"] }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(true));
        assert_eq!(verdict.checks[0]["expected"][0], "report.pdf");
    }

    #[test]
    fn checked_state_mismatch_fails() {
        let verdict = built(
            Expectation::Checked {
                expected: true,
                actual: Some(false),
            },
            Some(page(json!({}))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(false));
    }

    #[test]
    fn long_text_is_truncated_in_receipts() {
        let long = "x".repeat(5000);
        let verdict = built(
            Expectation::Value(&long),
            Some(page(json!({ "value": long.clone() }))),
            Effects::default(),
        );
        assert_eq!(verdict.verified, Some(true));
        assert!(verdict.checks[0]["expected"].as_str().unwrap().len() < 300);
    }

    fn watch_for(page_url: Option<&str>) -> ReceiptWatch {
        let (tx, events) = broadcast::channel(4);
        drop(tx);
        ReceiptWatch {
            events,
            page_session: "S1".to_string(),
            page_target: Some("T1".to_string()),
            page_url: page_url.map(str::to_string),
            probe: None,
        }
    }

    fn event(method: &str, session: Option<&str>, params: Value) -> CdpEvent {
        CdpEvent {
            method: method.to_string(),
            params,
            session_id: session.map(str::to_string),
        }
    }

    #[test]
    fn effects_detect_new_tab_from_the_acting_tab_only() {
        let watch = watch_for(Some("https://a.test/"));
        let mut effects = Effects::default();
        watch.observe(
            &event(
                "Target.targetCreated",
                None,
                json!({ "targetInfo": { "type": "page", "openerId": "OTHER" } }),
            ),
            &mut effects,
        );
        assert!(!effects.new_tab, "another tab opened it");
        watch.observe(
            &event(
                "Target.targetCreated",
                None,
                json!({ "targetInfo": { "type": "page", "openerId": "T1" } }),
            ),
            &mut effects,
        );
        assert!(effects.new_tab);
    }

    #[test]
    fn effects_detect_navigation_on_the_main_frame_only() {
        let watch = watch_for(Some("https://a.test/"));
        let mut effects = Effects::default();
        watch.observe(
            &event(
                "Page.frameNavigated",
                Some("S1"),
                json!({ "frame": { "id": "F2", "parentId": "T1", "url": "https://b.test/" } }),
            ),
            &mut effects,
        );
        assert!(!effects.url_changed, "subframe navigation");
        watch.observe(
            &event(
                "Page.frameNavigated",
                Some("OTHER-SESSION"),
                json!({ "frame": { "id": "T9", "url": "https://b.test/" } }),
            ),
            &mut effects,
        );
        assert!(!effects.url_changed, "another tab navigated");
        watch.observe(
            &event(
                "Page.frameRequestedNavigation",
                Some("S1"),
                json!({ "frameId": "T1", "disposition": "currentTab", "url": "https://b.test/" }),
            ),
            &mut effects,
        );
        assert!(effects.url_changed);
    }

    #[test]
    fn effects_ignore_navigation_to_the_same_url_and_flag_hash_changes() {
        let watch = watch_for(Some("https://a.test/"));
        let mut effects = Effects::default();
        watch.observe(
            &event(
                "Page.frameNavigated",
                Some("S1"),
                json!({ "frame": { "id": "T1", "url": "https://a.test/" } }),
            ),
            &mut effects,
        );
        assert!(!effects.url_changed);
        watch.observe(
            &event(
                "Page.navigatedWithinDocument",
                Some("S1"),
                json!({ "frameId": "T1", "url": "https://a.test/#x" }),
            ),
            &mut effects,
        );
        assert!(effects.url_changed);
    }

    #[test]
    fn verdict_failure_is_sticky_and_pass_upgrades_unknown() {
        let mut verdict = Verdict::default();
        verdict.push(None, json!({ "name": "a" }));
        assert_eq!(verdict.verified, None);
        verdict.push(Some(true), json!({ "name": "b" }));
        assert_eq!(verdict.verified, Some(true));
        verdict.push(Some(false), json!({ "name": "c" }));
        assert_eq!(verdict.verified, Some(false));
        verdict.push(Some(true), json!({ "name": "d" }));
        assert_eq!(verdict.verified, Some(false));
    }
}
