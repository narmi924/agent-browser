use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use super::cdp::client::CdpClient;
use super::cdp::types::*;
use super::element::{
    resolve_element_center, resolve_element_object_id, session_viewport_offset, ElementPoint,
    RefMap,
};
use super::human::TypingStep;

/// How long a plain pointer move waits for Chrome's acknowledgement before
/// the tab is treated as hidden.
///
/// Chrome aligns mouse moves with animation frames and only acknowledges them
/// once the renderer has processed one, so waiting keeps consecutive moves on
/// a visible tab from being coalesced. A hidden tab renders no frames and its
/// acknowledgement only arrives after about five seconds.
const MOUSE_MOVE_ACK_GRACE: Duration = Duration::from_millis(250);

/// How long a click waits for the move that precedes it. The press sent next
/// flushes any queued move, so this wait exists only to notice a dialog opened
/// by a mouse-move handler before pressing.
const CLICK_MOVE_ACK_GRACE: Duration = Duration::from_millis(15);

/// Sends the pointer moves of one movement. The first move that goes
/// unacknowledged for `MOUSE_MOVE_ACK_GRACE` marks the tab as hidden, and the
/// rest of the movement is sent without waiting.
pub struct MouseMoves {
    wait_for_ack: bool,
}

impl MouseMoves {
    pub fn new() -> Self {
        Self { wait_for_ack: true }
    }

    /// Dispatch one move. Returns false when it was not acknowledged.
    pub async fn dispatch(
        &mut self,
        client: &CdpClient,
        session_id: &str,
        params: &DispatchMouseEventParams,
    ) -> Result<bool, String> {
        if !self.wait_for_ack {
            let params = serde_json::to_value(params)
                .map_err(|e| format!("Failed to serialize params: {}", e))?;
            client
                .send_command_no_wait("Input.dispatchMouseEvent", Some(params), Some(session_id))
                .await?;
            return Ok(false);
        }
        let send = client.send_command_typed::<_, Value>(
            "Input.dispatchMouseEvent",
            params,
            Some(session_id),
        );
        match tokio::time::timeout(MOUSE_MOVE_ACK_GRACE, send).await {
            Ok(result) => {
                result?;
                Ok(true)
            }
            Err(_) => {
                self.wait_for_ack = false;
                Ok(false)
            }
        }
    }
}

impl Default for MouseMoves {
    fn default() -> Self {
        Self::new()
    }
}

/// Timing for a click whose pointer has already been moved into place.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClickPacing {
    pub skip_move: bool,
    pub pre_press_ms: u64,
    pub hold_ms: u64,
}

/// Outcome of a click. `dialog_opened` is true if a JavaScript dialog opened
/// mid-sequence (the page is then blocked until `dialog accept`/`dismiss`).
/// `pending_release` is set only when the dialog opened after mousePressed but
/// before mouseReleased: the button is logically held until the caller
/// dispatches the release (done once the dialog is resolved), otherwise the
/// next click would register as a drag or double-click.
#[derive(Default)]
pub struct ClickResult {
    /// Final pointer position in the top-level page viewport, including dialogs.
    pub position: (f64, f64),
    pub dialog_opened: bool,
    pub pending_release: Option<PendingRelease>,
    pub x: f64,
    pub y: f64,
    pub button_pressed: bool,
}

pub struct PendingRelease {
    pub session_id: String,
    pub x: f64,
    pub y: f64,
    pub button: String,
}

pub async fn click(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    button: &str,
    click_count: i32,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, String> {
    click_element(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        button,
        click_count,
        iframe_sessions,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn click_element(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    button: &str,
    click_count: i32,
    iframe_sessions: &HashMap<String, String>,
    require_actionable: bool,
) -> Result<ClickResult, String> {
    let (x, y, effective_session_id) = resolve_element_center(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        require_actionable,
    )
    .await?;
    // A click-triggered dialog can fire on the frame's own session (OOPIF) or
    // on the top-level page session; both count as "ours". A dialog on any
    // other session belongs to a background tab and must not abort this click.
    let offset =
        session_viewport_offset(client, session_id, &effective_session_id, iframe_sessions).await?;
    let mut result = dispatch_click(
        client,
        &effective_session_id,
        &[effective_session_id.as_str(), session_id],
        ClickTarget {
            x,
            y,
            button,
            click_count,
        },
        ClickPacing::default(),
    )
    .await?;
    // Compute before dispatch: a click may navigate or open a blocking dialog.
    result.position = (x + offset.0, y + offset.1);
    (result.x, result.y) = result.position;
    Ok(result)
}

/// Press and release at a point the caller already resolved, typically after
/// moving the pointer there along a human path.
pub async fn click_at_point(
    client: &CdpClient,
    session_id: &str,
    point: &ElementPoint,
    viewport_offset: (f64, f64),
    pacing: ClickPacing,
) -> Result<ClickResult, String> {
    let target_session = point.session_id.as_str();
    let mut result = dispatch_click(
        client,
        target_session,
        &[target_session, session_id],
        ClickTarget {
            x: point.x,
            y: point.y,
            button: "left",
            click_count: 1,
        },
        pacing,
    )
    .await?;
    result.position = (point.x + viewport_offset.0, point.y + viewport_offset.1);
    (result.x, result.y) = result.position;
    Ok(result)
}

pub async fn dblclick(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ClickResult, String> {
    click(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        "left",
        2,
        iframe_sessions,
    )
    .await
}

pub struct HoverOutcome {
    /// Final pointer position in the top-level page viewport.
    pub position: (f64, f64),
    /// False when Chrome did not acknowledge the move right away, as in a
    /// hidden tab: the hover events are queued, not yet delivered.
    pub acknowledged: bool,
}

pub async fn hover(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<HoverOutcome, String> {
    let (x, y, effective_session_id) = resolve_element_center(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        false,
    )
    .await?;
    let offset =
        session_viewport_offset(client, session_id, &effective_session_id, iframe_sessions).await?;
    let acknowledged = MouseMoves::new()
        .dispatch(
            client,
            &effective_session_id,
            &DispatchMouseEventParams {
                event_type: "mouseMoved".to_string(),
                x,
                y,
                button: None,
                buttons: None,
                click_count: None,
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
        )
        .await?;
    Ok(HoverOutcome {
        position: (x + offset.0, y + offset.1),
        acknowledged,
    })
}

pub async fn fill(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    value: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Focus and clear in one round trip.
    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.focus();
                    this.select && this.select();
                    this.value = '';
                    this.dispatchEvent(new Event('input', { bubbles: true }));
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    // Insert text (keyboard input dispatched at page level, use parent session_id)
    client
        .send_command_typed::<_, Value>(
            "Input.insertText",
            &InsertTextParams {
                text: value.to_string(),
            },
            Some(session_id),
        )
        .await?;

    Ok(())
}

/// Focus a field and optionally clear it, ready for keystrokes.
pub async fn prepare_type_target(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    clear: bool,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Focus
    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { this.focus(); }".to_string(),
                object_id: Some(object_id.clone()),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    if clear {
        client
            .send_command_typed::<_, Value>(
                "Runtime.callFunctionOn",
                &CallFunctionOnParams {
                    function_declaration: r#"function() {
                        this.select && this.select();
                        this.value = '';
                        this.dispatchEvent(new Event('input', { bubbles: true }));
                    }"#
                    .to_string(),
                    object_id: Some(object_id),
                    arguments: None,
                    return_by_value: Some(true),
                    await_promise: Some(false),
                },
                Some(&effective_session_id),
            )
            .await?;
    }

    Ok(())
}

pub async fn type_text_into_active_context(
    client: &CdpClient,
    session_id: &str,
    text: &str,
    delay_ms: Option<u64>,
) -> Result<(), String> {
    let delay = delay_ms.unwrap_or(0);

    for ch in text.chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            let (key, code, key_code) = char_to_key_info(ch);
            let text_str = key_text(&key);
            client
                .send_command_typed::<_, Value>(
                    "Input.dispatchKeyEvent",
                    &DispatchKeyEventParams {
                        event_type: "keyDown".to_string(),
                        key: Some(key.clone()),
                        code: Some(code.clone()),
                        text: text_str.clone(),
                        unmodified_text: text_str,
                        windows_virtual_key_code: Some(key_code),
                        modifiers: None,
                    },
                    Some(session_id),
                )
                .await?;

            client
                .send_command_typed::<_, Value>(
                    "Input.dispatchKeyEvent",
                    &DispatchKeyEventParams {
                        event_type: "keyUp".to_string(),
                        key: Some(key),
                        code: Some(code),
                        text: None,
                        unmodified_text: None,
                        windows_virtual_key_code: Some(key_code),
                        modifiers: None,
                    },
                    Some(session_id),
                )
                .await?;
        } else {
            // VS Code/Electron webviews reject repeated dispatchKeyEvent calls
            // carrying printable `text`. Insert printable characters directly
            // and reserve key events for controls like Enter and Tab.
            client
                .send_command_typed::<_, Value>(
                    "Input.insertText",
                    &InsertTextParams {
                        text: ch.to_string(),
                    },
                    Some(session_id),
                )
                .await?;
        }

        if delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
        }
    }

    Ok(())
}

/// Type with a full key press for every character a US keyboard can produce,
/// and `Input.insertText` only for the rest (for example Chinese).
pub async fn type_keys_into_active_context(
    client: &CdpClient,
    session_id: &str,
    text: &str,
    delay_ms: Option<u64>,
) -> Result<(), String> {
    let delay = delay_ms.unwrap_or(0);
    for ch in text.chars() {
        type_char_with_keys(client, session_id, ch).await?;
        if delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
        }
    }
    Ok(())
}

/// Execute a planned sequence of keystrokes, pauses, and corrections.
pub async fn run_typing_plan(
    client: &CdpClient,
    session_id: &str,
    plan: &[TypingStep],
) -> Result<(), String> {
    for step in plan {
        match step {
            TypingStep::Wait(ms) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(*ms)).await;
            }
            TypingStep::Type(ch) => type_char_with_keys(client, session_id, *ch).await?,
            TypingStep::Backspace => press_key(client, session_id, "Backspace").await?,
        }
    }
    Ok(())
}

async fn type_char_with_keys(client: &CdpClient, session_id: &str, ch: char) -> Result<(), String> {
    if matches!(ch, '\n' | '\r' | '\t') {
        let (key, code, key_code) = char_to_key_info(ch);
        let text = key_text(&key);
        send_key_event(
            client,
            session_id,
            KeyEvent {
                kind: "keyDown",
                key: &key,
                code: &code,
                key_code,
                text: text.as_deref(),
                unmodified_text: text.as_deref(),
                modifiers: None,
            },
        )
        .await?;
        return send_key_event(
            client,
            session_id,
            KeyEvent {
                kind: "keyUp",
                key: &key,
                code: &code,
                key_code,
                text: None,
                unmodified_text: None,
                modifiers: None,
            },
        )
        .await;
    }
    match key_stroke_for_char(ch) {
        Some(stroke) => dispatch_key_stroke(client, session_id, &stroke).await,
        None => {
            client
                .send_command_typed::<_, Value>(
                    "Input.insertText",
                    &InsertTextParams {
                        text: ch.to_string(),
                    },
                    Some(session_id),
                )
                .await?;
            Ok(())
        }
    }
}

struct KeyEvent<'a> {
    kind: &'a str,
    key: &'a str,
    code: &'a str,
    key_code: i32,
    text: Option<&'a str>,
    unmodified_text: Option<&'a str>,
    modifiers: Option<i32>,
}

async fn send_key_event(
    client: &CdpClient,
    session_id: &str,
    event: KeyEvent<'_>,
) -> Result<(), String> {
    client
        .send_command_typed::<_, Value>(
            "Input.dispatchKeyEvent",
            &DispatchKeyEventParams {
                event_type: event.kind.to_string(),
                key: Some(event.key.to_string()),
                code: Some(event.code.to_string()),
                text: event.text.map(str::to_string),
                unmodified_text: event.unmodified_text.map(str::to_string),
                windows_virtual_key_code: Some(event.key_code),
                modifiers: event.modifiers,
            },
            Some(session_id),
        )
        .await?;
    Ok(())
}

/// One physical key press on a US layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStroke {
    /// The value of `KeyboardEvent.key` (the character produced).
    pub key: char,
    /// The value of `KeyboardEvent.code` (the physical key).
    pub code: String,
    /// The Windows virtual-key code, reported as `KeyboardEvent.keyCode`.
    pub key_code: i32,
    /// The character the key produces without Shift.
    pub base: char,
    pub shift: bool,
}

/// The key press that produces `ch` on a US keyboard, or `None` for characters
/// no key produces directly.
pub fn key_stroke_for_char(ch: char) -> Option<KeyStroke> {
    const SHIFTED_DIGITS: [(char, char); 10] = [
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
    ];
    if ch.is_ascii_lowercase() || ch.is_ascii_uppercase() {
        let upper = ch.to_ascii_uppercase();
        return Some(KeyStroke {
            key: ch,
            code: format!("Key{upper}"),
            key_code: upper as i32,
            base: ch.to_ascii_lowercase(),
            shift: ch.is_ascii_uppercase(),
        });
    }
    if ch.is_ascii_digit() {
        return Some(KeyStroke {
            key: ch,
            code: format!("Digit{ch}"),
            key_code: ch as i32,
            base: ch,
            shift: false,
        });
    }
    if let Some((_, digit)) = SHIFTED_DIGITS.iter().find(|(shifted, _)| *shifted == ch) {
        return Some(KeyStroke {
            key: ch,
            code: format!("Digit{digit}"),
            key_code: *digit as i32,
            base: *digit,
            shift: true,
        });
    }
    if ch == ' ' {
        return Some(KeyStroke {
            key: ' ',
            code: "Space".to_string(),
            key_code: 32,
            base: ' ',
            shift: false,
        });
    }
    let (code, key_code) = punctuation_key_info(ch);
    if key_code == 0 {
        return None;
    }
    let (base, shift) = match ch {
        ':' => (';', true),
        '+' => ('=', true),
        '<' => (',', true),
        '_' => ('-', true),
        '>' => ('.', true),
        '?' => ('/', true),
        '~' => ('`', true),
        '{' => ('[', true),
        '|' => ('\\', true),
        '}' => (']', true),
        '"' => ('\'', true),
        other => (other, false),
    };
    Some(KeyStroke {
        key: ch,
        code: code.to_string(),
        key_code,
        base,
        shift,
    })
}

/// Press and release a key: `rawKeyDown`, `char`, `keyUp`, the sequence a
/// keyboard produces, wrapped in Shift when the character needs it.
async fn dispatch_key_stroke(
    client: &CdpClient,
    session_id: &str,
    stroke: &KeyStroke,
) -> Result<(), String> {
    const SHIFT: i32 = 8;
    let modifiers = stroke.shift.then_some(SHIFT);
    let key = stroke.key.to_string();
    let text = key.clone();
    let unmodified = stroke.base.to_string();
    if stroke.shift {
        send_key_event(
            client,
            session_id,
            KeyEvent {
                kind: "rawKeyDown",
                key: "Shift",
                code: "ShiftLeft",
                key_code: 16,
                text: None,
                unmodified_text: None,
                modifiers,
            },
        )
        .await?;
    }
    send_key_event(
        client,
        session_id,
        KeyEvent {
            kind: "rawKeyDown",
            key: &key,
            code: &stroke.code,
            key_code: stroke.key_code,
            text: None,
            unmodified_text: None,
            modifiers,
        },
    )
    .await?;
    send_key_event(
        client,
        session_id,
        KeyEvent {
            kind: "char",
            key: &key,
            code: &stroke.code,
            key_code: stroke.key_code,
            text: Some(&text),
            unmodified_text: Some(&unmodified),
            modifiers,
        },
    )
    .await?;
    send_key_event(
        client,
        session_id,
        KeyEvent {
            kind: "keyUp",
            key: &key,
            code: &stroke.code,
            key_code: stroke.key_code,
            text: None,
            unmodified_text: None,
            modifiers,
        },
    )
    .await?;
    if stroke.shift {
        send_key_event(
            client,
            session_id,
            KeyEvent {
                kind: "keyUp",
                key: "Shift",
                code: "ShiftLeft",
                key_code: 16,
                text: None,
                unmodified_text: None,
                modifiers: None,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn press_key(client: &CdpClient, session_id: &str, key: &str) -> Result<(), String> {
    press_key_with_modifiers(client, session_id, key, None).await
}

/// Dispatch a keyDown+keyUp sequence for `key` with an optional CDP modifier bitmask.
///
/// Modifier values follow the CDP `Input.dispatchKeyEvent` spec:
/// 1 = Alt, 2 = Control, 4 = Meta (Cmd), 8 = Shift.
///
/// Callers that need a platform-appropriate modifier (e.g. Cmd on macOS,
/// Ctrl elsewhere) must choose the value themselves -- see `cfg!(target_os)`.
pub async fn press_key_with_modifiers(
    client: &CdpClient,
    session_id: &str,
    key: &str,
    modifiers: Option<i32>,
) -> Result<(), String> {
    let (key_name, code, key_code) = named_key_info(key);

    // Suppress text insertion when Control (2) or Meta (4) modifiers are active,
    // since these are command chords (e.g. Ctrl+A = select-all), not text input.
    let has_command_modifier = modifiers.is_some_and(|m| m & (2 | 4) != 0);
    let text = if has_command_modifier {
        None
    } else {
        key_text(&key_name)
    };

    client
        .send_command_typed::<_, Value>(
            "Input.dispatchKeyEvent",
            &DispatchKeyEventParams {
                event_type: "keyDown".to_string(),
                key: Some(key_name.clone()),
                code: Some(code.clone()),
                text: text.clone(),
                unmodified_text: text.clone(),
                windows_virtual_key_code: Some(key_code),
                modifiers,
            },
            Some(session_id),
        )
        .await?;

    client
        .send_command_typed::<_, Value>(
            "Input.dispatchKeyEvent",
            &DispatchKeyEventParams {
                event_type: "keyUp".to_string(),
                key: Some(key_name),
                code: Some(code),
                text: None,
                unmodified_text: None,
                windows_virtual_key_code: Some(key_code),
                modifiers,
            },
            Some(session_id),
        )
        .await?;

    Ok(())
}

pub async fn scroll(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: Option<&str>,
    delta_x: f64,
    delta_y: f64,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    if let Some(sel) = selector_or_ref {
        let (object_id, effective_session_id) =
            resolve_element_object_id(client, session_id, ref_map, sel, iframe_sessions).await?;
        let js = "function(dx, dy) { this.scrollBy(dx, dy); }".to_string();
        client
            .send_command_typed::<_, Value>(
                "Runtime.callFunctionOn",
                &CallFunctionOnParams {
                    function_declaration: js,
                    object_id: Some(object_id),
                    arguments: Some(vec![
                        CallArgument {
                            value: Some(serde_json::json!(delta_x)),
                            object_id: None,
                        },
                        CallArgument {
                            value: Some(serde_json::json!(delta_y)),
                            object_id: None,
                        },
                    ]),
                    return_by_value: Some(true),
                    await_promise: Some(false),
                },
                Some(&effective_session_id),
            )
            .await?;
    } else {
        let js = format!("window.scrollBy({}, {})", delta_x, delta_y);
        client
            .send_command_typed::<_, Value>(
                "Runtime.evaluate",
                &EvaluateParams {
                    expression: js,
                    return_by_value: Some(true),
                    await_promise: Some(false),
                },
                Some(session_id),
            )
            .await?;
    }
    Ok(())
}

pub async fn select_option(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    values: &[String],
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    // Matching nothing must be an error, not a silent success: an agent that
    // selects a misspelled option otherwise sees "Done", and only discovers
    // the page state is wrong after more commands. List what was available.
    let js = r#"function(vals) {
            const normalize = (value) => String(value ?? '')
                .replace(/[\u200B\u200C\u200D\u2060\uFEFF]/g, '')
                .replace(/\s+/g, ' ')
                .trim();
            const options = Array.from(this.options);
            const wanted = new Set();
            for (const value of vals) {
                let matches = options.filter((opt) =>
                    value === opt.value ||
                    value === opt.label.trim() ||
                    value === opt.textContent.trim()
                );
                if (matches.length === 0) {
                    const normalizedValue = normalize(value);
                    matches = options.filter((opt) =>
                        normalize(opt.label) === normalizedValue
                    );
                    if (matches.length > 1) {
                        return { error: 'Multiple options matched ' + JSON.stringify(value) + ' after whitespace normalization' };
                    }
                }
                if (matches.length === 0) {
                    const available = options.map(o => o.value + ' ("' + normalize(o.label) + '")').join(', ');
                    return { error: 'No option matched ' + JSON.stringify(vals) + '. Available options: ' + available };
                }
                for (const opt of matches) wanted.add(opt);
            }
            if (wanted.size === 0) {
                const available = options.map(o => o.value + ' ("' + normalize(o.label) + '")').join(', ');
                return { error: 'No option matched ' + JSON.stringify(vals) + '. Available options: ' + available };
            }
            for (const opt of options) opt.selected = wanted.has(opt);
            this.dispatchEvent(new Event('change', { bubbles: true }));
            return { matched: wanted.size };
        }"#
    .to_string();

    let result = client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js,
                object_id: Some(object_id),
                arguments: Some(vec![CallArgument {
                    value: Some(serde_json::json!(values)),
                    object_id: None,
                }]),
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    if let Some(error) = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("error"))
        .and_then(|e| e.as_str())
    {
        return Err(error.to_string());
    }

    Ok(())
}

pub async fn check(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Option<(f64, f64)>, String> {
    let mut position = None;
    let is_checked = super::element::is_element_checked(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    if !is_checked {
        let result = click_element(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            "left",
            1,
            iframe_sessions,
            false,
        )
        .await?;
        position = Some(result.position);

        // Verify the click changed the state (Playwright parity: _setChecked re-checks).
        // If the coordinate-based click missed (e.g. hidden input, overlay), retry
        // with a JS .click() on the element and its associated input.
        if !super::element::is_element_checked(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            iframe_sessions,
        )
        .await?
        {
            js_click_checkbox(
                client,
                session_id,
                ref_map,
                selector_or_ref,
                iframe_sessions,
            )
            .await?;
        }
    }
    Ok(position)
}

pub async fn uncheck(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Option<(f64, f64)>, String> {
    let mut position = None;
    let is_checked = super::element::is_element_checked(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;
    if is_checked {
        let result = click_element(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            "left",
            1,
            iframe_sessions,
            false,
        )
        .await?;
        position = Some(result.position);

        // Same verify-and-retry as check().
        if super::element::is_element_checked(
            client,
            session_id,
            ref_map,
            selector_or_ref,
            iframe_sessions,
        )
        .await?
        {
            js_click_checkbox(
                client,
                session_id,
                ref_map,
                selector_or_ref,
                iframe_sessions,
            )
            .await?;
        }
    }
    Ok(position)
}

/// Fallback for when the coordinate-based CDP click did not toggle the
/// checkbox/radio state. This mirrors how Playwright dispatches clicks
/// through the DOM rather than via raw Input.dispatchMouseEvent coordinates.
///
/// Uses the same follow-label resolution as `is_element_checked`:
/// 1. If the element is a native input → `.click()` it directly.
/// 2. If the element is inside a `<label>` → `.click()` the label's `.control`.
/// 3. If the element has a nested `<input>` → `.click()` that input.
/// 4. Otherwise → `.click()` the element itself (handles ARIA role controls).
async fn js_click_checkbox(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let js = r#"function() {
            var el = this;
            var tag = el.tagName && el.tagName.toUpperCase();
            // 1. Native input — click it directly
            if (tag === 'INPUT' && (el.type === 'checkbox' || el.type === 'radio')) {
                el.click();
                return;
            }
            // 2. Follow label → control association
            var label = tag === 'LABEL' ? el : (el.closest && el.closest('label'));
            if (label && label.tagName && label.tagName.toUpperCase() === 'LABEL' && label.control) {
                label.control.click();
                return;
            }
            // 3. Nested native input
            var input = el.querySelector && el.querySelector('input[type="checkbox"], input[type="radio"]');
            if (input) {
                input.click();
                return;
            }
            // 4. ARIA role control — click the element itself
            el.click();
        }"#;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js.to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn focus(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: "function() { this.focus(); }".to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn clear(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.focus();
                    this.value = '';
                    this.dispatchEvent(new Event('input', { bubbles: true }));
                    this.dispatchEvent(new Event('change', { bubbles: true }));
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn select_all(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.focus();
                    if (typeof this.select === 'function') {
                        this.select();
                    } else {
                        const range = document.createRange();
                        range.selectNodeContents(this);
                        const sel = window.getSelection();
                        sel.removeAllRanges();
                        sel.addRange(range);
                    }
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn scroll_into_view(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration:
                    "function() { this.scrollIntoView({ block: 'center', inline: 'center' }); }"
                        .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn dispatch_event(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    event_type: &str,
    event_init: Option<&Value>,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    let init_json = event_init
        .map(|v| serde_json::to_string(v).unwrap_or("{}".to_string()))
        .unwrap_or_else(|| "{ bubbles: true }".to_string());

    let js = format!(
        "function() {{ this.dispatchEvent(new Event({}, {})); }}",
        serde_json::to_string(event_type).unwrap_or_default(),
        init_json
    );

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: js,
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn highlight(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (object_id, effective_session_id) = resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
    )
    .await?;

    client
        .send_command_typed::<_, Value>(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    this.style.outline = '2px solid red';
                    this.style.outlineOffset = '2px';
                    const el = this;
                    setTimeout(() => {
                        el.style.outline = '';
                        el.style.outlineOffset = '';
                    }, 3000);
                }"#
                .to_string(),
                object_id: Some(object_id),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

pub async fn tap_touch(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector_or_ref: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<(), String> {
    let (x, y, effective_session_id) = resolve_element_center(
        client,
        session_id,
        ref_map,
        selector_or_ref,
        iframe_sessions,
        false,
    )
    .await?;

    client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(serde_json::json!({
                "type": "touchStart",
                "touchPoints": [{ "x": x, "y": y }],
            })),
            Some(&effective_session_id),
        )
        .await?;

    client
        .send_command(
            "Input.dispatchTouchEvent",
            Some(serde_json::json!({
                "type": "touchEnd",
                "touchPoints": [],
            })),
            Some(&effective_session_id),
        )
        .await?;

    Ok(())
}

/// Dispatches one mouse event and waits for the browser to ack it, but
/// returns Ok(true) if a JavaScript dialog opens first. A synchronous dialog
/// (confirm/prompt/alert in the event handler) blocks the renderer's main
/// thread, so the input ack cannot arrive until the dialog is resolved;
/// without this the command hangs until the client read timeout and the agent
/// never sees the pending-dialog warning.
async fn dispatch_mouse_or_dialog(
    client: &CdpClient,
    session_id: &str,
    accept_sessions: &[&str],
    params: &DispatchMouseEventParams,
) -> Result<bool, String> {
    use tokio::sync::broadcast::error::RecvError;

    // Subscribe before sending so the dialog event cannot slip past us.
    let mut events = client.subscribe();
    let send =
        client.send_command_typed::<_, Value>("Input.dispatchMouseEvent", params, Some(session_id));
    tokio::pin!(send);
    loop {
        tokio::select! {
            res = &mut send => {
                res?;
                return Ok(false);
            }
            event = events.recv() => {
                match event {
                    Ok(e) if e.method == "Page.javascriptDialogOpening" => {
                        // Only a dialog on this click's frame/page session
                        // aborts it; a background-tab dialog must not. A
                        // session-less event has no flat session and is
                        // treated as the top-level page (i.e. ours).
                        let ours = match e.session_id.as_deref() {
                            Some(sid) => accept_sessions.contains(&sid),
                            None => true,
                        };
                        if ours {
                            return Ok(true);
                        }
                        continue;
                    }
                    Ok(_) => continue,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => {
                        (&mut send).await?;
                        return Ok(false);
                    }
                }
            }
        }
    }
}

struct ClickTarget<'a> {
    x: f64,
    y: f64,
    button: &'a str,
    click_count: i32,
}

async fn dispatch_click(
    client: &CdpClient,
    session_id: &str,
    accept_sessions: &[&str],
    target: ClickTarget<'_>,
    pacing: ClickPacing,
) -> Result<ClickResult, String> {
    let ClickTarget {
        x,
        y,
        button,
        click_count,
    } = target;
    // Move. The ack is optional (see CLICK_MOVE_ACK_GRACE): in a hidden tab
    // the press below is what makes Chrome deliver this move.
    if !pacing.skip_move {
        let moved = tokio::time::timeout(
            CLICK_MOVE_ACK_GRACE,
            dispatch_mouse_or_dialog(
                client,
                session_id,
                accept_sessions,
                &DispatchMouseEventParams {
                    event_type: "mouseMoved".to_string(),
                    x,
                    y,
                    button: None,
                    buttons: None,
                    click_count: None,
                    delta_x: None,
                    delta_y: None,
                    modifiers: None,
                },
            ),
        )
        .await;
        if matches!(moved, Ok(Ok(true))) {
            // No button was pressed yet, nothing to release.
            return Ok(ClickResult {
                position: (x, y),
                dialog_opened: true,
                pending_release: None,
                x,
                y,
                button_pressed: false,
            });
        }
        if let Ok(Err(error)) = moved {
            return Err(error);
        }
    }

    if pacing.pre_press_ms > 0 {
        tokio::time::sleep(Duration::from_millis(pacing.pre_press_ms)).await;
    }

    let button_value = match button {
        "right" => 2,
        "middle" => 4,
        _ => 1,
    };

    // Press
    if dispatch_mouse_or_dialog(
        client,
        session_id,
        accept_sessions,
        &DispatchMouseEventParams {
            event_type: "mousePressed".to_string(),
            x,
            y,
            button: Some(button.to_string()),
            buttons: Some(button_value),
            click_count: Some(click_count),
            delta_x: None,
            delta_y: None,
            modifiers: None,
        },
    )
    .await?
    {
        // Dialog opened from the mousedown handler: the button is held and the
        // release will never arrive on its own. Hand the caller what it needs
        // to release once the dialog is resolved.
        return Ok(ClickResult {
            position: (x, y),
            dialog_opened: true,
            pending_release: Some(PendingRelease {
                session_id: session_id.to_string(),
                x,
                y,
                button: button.to_string(),
            }),
            x,
            y,
            button_pressed: true,
        });
    }

    if pacing.hold_ms > 0 {
        tokio::time::sleep(Duration::from_millis(pacing.hold_ms)).await;
    }

    // Release. A dialog here fired from the click/mouseup handler, which runs
    // after the button is already up, so there is nothing left to release.
    let dialog_opened = dispatch_mouse_or_dialog(
        client,
        session_id,
        accept_sessions,
        &DispatchMouseEventParams {
            event_type: "mouseReleased".to_string(),
            x,
            y,
            button: Some(button.to_string()),
            buttons: Some(0),
            click_count: Some(click_count),
            delta_x: None,
            delta_y: None,
            modifiers: None,
        },
    )
    .await?;
    Ok(ClickResult {
        position: (x, y),
        dialog_opened,
        pending_release: None,
        x,
        y,
        button_pressed: true,
    })
}

/// Best-effort mouseReleased to clear a button left logically down when a
/// dialog opened mid-click. Called after the dialog is resolved.
pub async fn dispatch_pending_release(
    client: &CdpClient,
    release: &PendingRelease,
) -> Result<(), String> {
    client
        .send_command_typed::<_, Value>(
            "Input.dispatchMouseEvent",
            &DispatchMouseEventParams {
                event_type: "mouseReleased".to_string(),
                x: release.x,
                y: release.y,
                button: Some(release.button.clone()),
                buttons: Some(0),
                click_count: Some(1),
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
            Some(&release.session_id),
        )
        .await?;
    Ok(())
}

fn char_to_key_info(ch: char) -> (String, String, i32) {
    match ch {
        '\n' | '\r' => ("Enter".to_string(), "Enter".to_string(), 13),
        '\t' => ("Tab".to_string(), "Tab".to_string(), 9),
        ' ' => (" ".to_string(), "Space".to_string(), 32),
        _ => {
            let key = ch.to_string();
            if ch.is_ascii_alphabetic() {
                // For letters the Windows VK code equals the uppercase ASCII value.
                let upper = ch.to_ascii_uppercase();
                let code = format!("Key{}", upper);
                let key_code = upper as i32;
                (key, code, key_code)
            } else if ch.is_ascii_digit() {
                let code = format!("Digit{}", ch);
                let key_code = ch as i32;
                (key, code, key_code)
            } else {
                let (code, key_code) = punctuation_key_info(ch);
                (key, code.to_string(), key_code)
            }
        }
    }
}

/// Return the DOM `KeyboardEvent.code` value and Windows virtual-key code for
/// a punctuation / symbol character assuming a US keyboard layout.
///
/// The Windows virtual-key codes (VK_OEM_*) differ from ASCII values for
/// punctuation.  Using the raw ASCII code would misidentify characters – e.g.
/// '.' (ASCII 46) collides with VK_DELETE (0x2E = 46), causing the period to
/// be swallowed.
fn punctuation_key_info(ch: char) -> (&'static str, i32) {
    match ch {
        // VK_OEM_1 (0xBA = 186) — ";:" key on US layout
        ';' | ':' => ("Semicolon", 186),
        // VK_OEM_PLUS (0xBB = 187) — "=+" key
        '=' | '+' => ("Equal", 187),
        // VK_OEM_COMMA (0xBC = 188) — ",<" key
        ',' | '<' => ("Comma", 188),
        // VK_OEM_MINUS (0xBD = 189) — "-_" key
        '-' | '_' => ("Minus", 189),
        // VK_OEM_PERIOD (0xBE = 190) — ".>" key
        '.' | '>' => ("Period", 190),
        // VK_OEM_2 (0xBF = 191) — "/?" key
        '/' | '?' => ("Slash", 191),
        // VK_OEM_3 (0xC0 = 192) — "`~" key
        '`' | '~' => ("Backquote", 192),
        // VK_OEM_4 (0xDB = 219) — "[{" key
        '[' | '{' => ("BracketLeft", 219),
        // VK_OEM_5 (0xDC = 220) — "\\|" key
        '\\' | '|' => ("Backslash", 220),
        // VK_OEM_6 (0xDD = 221) — "]}" key
        ']' | '}' => ("BracketRight", 221),
        // VK_OEM_7 (0xDE = 222) — "'\""" key
        '\'' | '"' => ("Quote", 222),
        _ => ("", 0),
    }
}

/// Return the `text` value that CDP `Input.dispatchKeyEvent` needs on the
/// `keyDown` event so that Chrome performs the default action for the key.
/// For example Enter needs `"\r"` to actually submit a form, and Tab needs
/// `"\t"` to move focus.  Non-printable / navigation keys return `None`.
fn key_text(key_name: &str) -> Option<String> {
    match key_name {
        "Enter" => Some("\r".to_string()),
        "Tab" => Some("\t".to_string()),
        " " => Some(" ".to_string()),
        _ => {
            // Single printable characters carry themselves as text.
            if key_name.len() == 1 {
                Some(key_name.to_string())
            } else {
                None
            }
        }
    }
}

fn named_key_info(key: &str) -> (String, String, i32) {
    match key.to_lowercase().as_str() {
        "enter" | "return" => ("Enter".to_string(), "Enter".to_string(), 13),
        "tab" => ("Tab".to_string(), "Tab".to_string(), 9),
        "escape" | "esc" => ("Escape".to_string(), "Escape".to_string(), 27),
        "backspace" => ("Backspace".to_string(), "Backspace".to_string(), 8),
        "delete" => ("Delete".to_string(), "Delete".to_string(), 46),
        "arrowup" | "up" => ("ArrowUp".to_string(), "ArrowUp".to_string(), 38),
        "arrowdown" | "down" => ("ArrowDown".to_string(), "ArrowDown".to_string(), 40),
        "arrowleft" | "left" => ("ArrowLeft".to_string(), "ArrowLeft".to_string(), 37),
        "arrowright" | "right" => ("ArrowRight".to_string(), "ArrowRight".to_string(), 39),
        "home" => ("Home".to_string(), "Home".to_string(), 36),
        "end" => ("End".to_string(), "End".to_string(), 35),
        "pageup" => ("PageUp".to_string(), "PageUp".to_string(), 33),
        "pagedown" => ("PageDown".to_string(), "PageDown".to_string(), 34),
        "space" | " " => (" ".to_string(), "Space".to_string(), 32),
        _ => {
            if key.len() == 1 {
                let ch = key.chars().next().unwrap();
                char_to_key_info(ch)
            } else {
                (key.to_string(), key.to_string(), 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `char_to_key_info` returns the correct (key, code,
    /// windowsVirtualKeyCode) triple for every character in Playwright's
    /// USKeyboardLayout.  The expected values below are taken verbatim from
    /// playwright-core/lib/server/usKeyboardLayout.js so that any drift from
    /// Playwright's behaviour is caught immediately.
    #[test]
    fn test_char_to_key_info_matches_playwright_layout() {
        // (character, expected_code, expected_vk_code)
        let cases: &[(char, &str, i32)] = &[
            // Letters – VK code must equal the uppercase ASCII value.
            ('a', "KeyA", 65),
            ('z', "KeyZ", 90),
            ('A', "KeyA", 65),
            // Digits
            ('0', "Digit0", 48),
            ('9', "Digit9", 57),
            // Punctuation – these are the values from Playwright's layout.
            // The bug that prompted this test sent '.' as VK 46 (= VK_DELETE).
            ('.', "Period", 190),
            (',', "Comma", 188),
            ('/', "Slash", 191),
            (';', "Semicolon", 186),
            ('\'', "Quote", 222),
            ('[', "BracketLeft", 219),
            (']', "BracketRight", 221),
            ('\\', "Backslash", 220),
            ('`', "Backquote", 192),
            ('-', "Minus", 189),
            ('=', "Equal", 187),
            // Shifted variants produced by the same physical keys.
            ('>', "Period", 190),
            ('<', "Comma", 188),
            ('?', "Slash", 191),
            (':', "Semicolon", 186),
            ('"', "Quote", 222),
            ('{', "BracketLeft", 219),
            ('}', "BracketRight", 221),
            ('|', "Backslash", 220),
            ('~', "Backquote", 192),
            ('_', "Minus", 189),
            ('+', "Equal", 187),
            // Whitespace / control
            (' ', "Space", 32),
            ('\n', "Enter", 13),
            ('\t', "Tab", 9),
        ];

        for &(ch, expected_code, expected_vk) in cases {
            let (key, code, vk) = char_to_key_info(ch);
            assert_eq!(
                code, expected_code,
                "char {:?}: expected code {:?}, got {:?}",
                ch, expected_code, code
            );
            assert_eq!(
                vk, expected_vk,
                "char {:?}: expected VK {}, got {} (ASCII would be {})",
                ch, expected_vk, vk, ch as i32
            );
            // key should be the character itself (except control chars).
            if !ch.is_control() {
                assert_eq!(key, ch.to_string(), "char {:?}: key mismatch", ch);
            }
        }
    }

    /// Regression test: period must NEVER map to VK 46 (VK_DELETE).
    #[test]
    fn test_period_is_not_vk_delete() {
        let (_, _, vk) = char_to_key_info('.');
        assert_ne!(
            vk, 46,
            "Period must not use VK code 46 (VK_DELETE); expected 190 (VK_OEM_PERIOD)"
        );
        assert_eq!(vk, 190);
    }

    /// Characters outside the US keyboard layout should return (key, "", 0)
    /// so that `type_text` falls back to `Input.insertText`.
    #[test]
    fn test_unmapped_chars_return_zero_keycode() {
        for ch in ['@', '#', '$', '%', '^', '&', '*', '(', ')', '€', '£', '你'] {
            let (key, code, vk) = char_to_key_info(ch);
            assert_eq!(
                code, "",
                "char {:?}: unmapped char should have empty code, got {:?}",
                ch, code
            );
            assert_eq!(
                vk, 0,
                "char {:?}: unmapped char should have VK 0, got {}",
                ch, vk
            );
            assert_eq!(key, ch.to_string());
        }
    }

    #[test]
    fn key_stroke_table_matches_a_us_keyboard() {
        let cases: &[(char, &str, i32, char, bool)] = &[
            ('a', "KeyA", 65, 'a', false),
            ('A', "KeyA", 65, 'a', true),
            ('z', "KeyZ", 90, 'z', false),
            ('0', "Digit0", 48, '0', false),
            ('9', "Digit9", 57, '9', false),
            ('!', "Digit1", 49, '1', true),
            ('@', "Digit2", 50, '2', true),
            ('#', "Digit3", 51, '3', true),
            ('$', "Digit4", 52, '4', true),
            ('%', "Digit5", 53, '5', true),
            ('^', "Digit6", 54, '6', true),
            ('&', "Digit7", 55, '7', true),
            ('*', "Digit8", 56, '8', true),
            ('(', "Digit9", 57, '9', true),
            (')', "Digit0", 48, '0', true),
            (' ', "Space", 32, ' ', false),
            ('.', "Period", 190, '.', false),
            ('>', "Period", 190, '.', true),
            ('-', "Minus", 189, '-', false),
            ('_', "Minus", 189, '-', true),
            ('=', "Equal", 187, '=', false),
            ('+', "Equal", 187, '=', true),
            ('/', "Slash", 191, '/', false),
            ('?', "Slash", 191, '/', true),
            (';', "Semicolon", 186, ';', false),
            (':', "Semicolon", 186, ';', true),
            ('\'', "Quote", 222, '\'', false),
            ('"', "Quote", 222, '\'', true),
            ('[', "BracketLeft", 219, '[', false),
            ('{', "BracketLeft", 219, '[', true),
            (']', "BracketRight", 221, ']', false),
            ('}', "BracketRight", 221, ']', true),
            ('\\', "Backslash", 220, '\\', false),
            ('|', "Backslash", 220, '\\', true),
            ('`', "Backquote", 192, '`', false),
            ('~', "Backquote", 192, '`', true),
            (',', "Comma", 188, ',', false),
            ('<', "Comma", 188, ',', true),
        ];
        for &(ch, code, key_code, base, shift) in cases {
            let stroke = key_stroke_for_char(ch).expect("printable ASCII has a key");
            assert_eq!(stroke.key, ch);
            assert_eq!(stroke.code, code, "code of {ch:?}");
            assert_eq!(stroke.key_code, key_code, "keyCode of {ch:?}");
            assert_eq!(stroke.base, base, "base of {ch:?}");
            assert_eq!(stroke.shift, shift, "shift of {ch:?}");
        }
    }

    #[test]
    fn characters_no_key_produces_have_no_key_stroke() {
        for ch in ['你', '好', 'é', '€', '£', '，', '😀'] {
            assert_eq!(key_stroke_for_char(ch), None, "{ch:?}");
        }
    }

    /// A fake browser that records each CDP method and its params.
    async fn recording_browser(
        hold_mouse_moves: bool,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_for_server = log.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                let Ok(text) = message.to_text() else {
                    continue;
                };
                let request: Value = serde_json::from_str(text).unwrap();
                log_for_server.lock().unwrap().push(request.clone());
                let is_move = request["method"] == "Input.dispatchMouseEvent"
                    && request["params"]["type"] == "mouseMoved";
                if hold_mouse_moves && is_move {
                    continue;
                }
                let reply = serde_json::json!({ "id": request["id"], "result": {} });
                ws.send(tokio_tungstenite::tungstenite::Message::Text(
                    reply.to_string(),
                ))
                .await
                .unwrap();
            }
        });
        (url, log, server)
    }

    /// Chrome does not acknowledge mouseMoved in a hidden tab until a frame
    /// is produced, about five seconds later. The click must not wait for it.
    #[tokio::test]
    async fn click_finishes_promptly_when_the_mouse_move_is_never_acknowledged() {
        let (url, log, server) = recording_browser(true).await;
        let client = CdpClient::connect(&url).await.unwrap();
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            dispatch_click(
                &client,
                "S1",
                &["S1"],
                ClickTarget {
                    x: 40.0,
                    y: 50.0,
                    button: "left",
                    click_count: 1,
                },
                ClickPacing::default(),
            ),
        )
        .await
        .expect("click must not wait for the mouseMoved ack")
        .unwrap();
        assert!(!result.dialog_opened);
        assert!(result.button_pressed);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "click took {:?}",
            started.elapsed()
        );
        let methods: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["params"]["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(methods, ["mouseMoved", "mousePressed", "mouseReleased"]);
        server.abort();
    }

    /// A movement in a hidden tab waits for the first ack only, then stops
    /// waiting, so a multi-step path cannot stall for seconds per step.
    #[tokio::test]
    async fn mouse_moves_stop_waiting_after_the_first_unacknowledged_move() {
        let (url, log, server) = recording_browser(true).await;
        let client = CdpClient::connect(&url).await.unwrap();
        let params = DispatchMouseEventParams {
            event_type: "mouseMoved".to_string(),
            x: 1.0,
            y: 2.0,
            button: None,
            buttons: None,
            click_count: None,
            delta_x: None,
            delta_y: None,
            modifiers: None,
        };
        let mut moves = MouseMoves::new();
        let started = tokio::time::Instant::now();
        assert!(!moves.dispatch(&client, "S1", &params).await.unwrap());
        assert!(!moves.dispatch(&client, "S1", &params).await.unwrap());
        assert!(!moves.dispatch(&client, "S1", &params).await.unwrap());
        assert!(
            started.elapsed() < MOUSE_MOVE_ACK_GRACE + Duration::from_millis(200),
            "later moves must not wait, took {:?}",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            log.lock().unwrap().len(),
            3,
            "every move must still be sent"
        );
        server.abort();
    }

    #[tokio::test]
    async fn click_with_prompt_ack_still_moves_presses_and_releases_in_order() {
        let (url, log, server) = recording_browser(false).await;
        let client = CdpClient::connect(&url).await.unwrap();
        dispatch_click(
            &client,
            "S1",
            &["S1"],
            ClickTarget {
                x: 1.0,
                y: 2.0,
                button: "left",
                click_count: 1,
            },
            ClickPacing::default(),
        )
        .await
        .unwrap();
        let methods: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["params"]["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(methods, ["mouseMoved", "mousePressed", "mouseReleased"]);
        server.abort();
    }

    /// Without keyDown there is no keydown event for the page's handlers.
    /// Regression: printable keys used to go through Input.insertText.
    #[tokio::test]
    async fn typing_keys_sends_down_char_up_with_key_details() {
        let (url, log, server) = recording_browser(false).await;
        let client = CdpClient::connect(&url).await.unwrap();
        type_keys_into_active_context(&client, "S1", "aB", None)
            .await
            .unwrap();
        let events: Vec<(String, Value)> = log
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                assert_eq!(r["method"], "Input.dispatchKeyEvent");
                (
                    r["params"]["type"].as_str().unwrap().to_string(),
                    r["params"].clone(),
                )
            })
            .collect();
        let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "rawKeyDown",
                "char",
                "keyUp",
                "rawKeyDown",
                "rawKeyDown",
                "char",
                "keyUp",
                "keyUp"
            ]
        );
        assert_eq!(events[0].1["key"], "a");
        assert_eq!(events[0].1["code"], "KeyA");
        assert_eq!(events[0].1["windowsVirtualKeyCode"], 65);
        assert_eq!(events[1].1["text"], "a");
        assert_eq!(events[3].1["key"], "Shift");
        assert_eq!(events[4].1["key"], "B");
        assert_eq!(events[4].1["modifiers"], 8);
        assert_eq!(events[5].1["text"], "B");
        assert_eq!(events[5].1["unmodifiedText"], "b");
        assert_eq!(events[7].1["key"], "Shift");
        server.abort();
    }

    #[tokio::test]
    async fn typing_keys_inserts_only_characters_no_key_produces() {
        let (url, log, server) = recording_browser(false).await;
        let client = CdpClient::connect(&url).await.unwrap();
        type_keys_into_active_context(&client, "S1", "a好", None)
            .await
            .unwrap();
        let calls = log.lock().unwrap();
        let methods: Vec<&str> = calls
            .iter()
            .map(|r| r["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            [
                "Input.dispatchKeyEvent",
                "Input.dispatchKeyEvent",
                "Input.dispatchKeyEvent",
                "Input.insertText"
            ]
        );
        assert_eq!(calls[3]["params"]["text"], "好");
        server.abort();
    }

    /// Plain type keeps the insert path that Electron webviews require.
    #[tokio::test]
    async fn plain_type_still_uses_insert_text_for_printable_characters() {
        let (url, log, server) = recording_browser(false).await;
        let client = CdpClient::connect(&url).await.unwrap();
        type_text_into_active_context(&client, "S1", "ab", None)
            .await
            .unwrap();
        let calls = log.lock().unwrap();
        assert!(calls.iter().all(|r| r["method"] == "Input.insertText"));
        assert_eq!(calls.len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn typing_plan_runs_wait_type_and_backspace_in_order() {
        let (url, log, server) = recording_browser(false).await;
        let client = CdpClient::connect(&url).await.unwrap();
        run_typing_plan(
            &client,
            "S1",
            &[
                TypingStep::Type('a'),
                TypingStep::Backspace,
                TypingStep::Type('好'),
            ],
        )
        .await
        .unwrap();
        let calls = log.lock().unwrap();
        let summary: Vec<String> = calls
            .iter()
            .map(|r| {
                format!(
                    "{}:{}",
                    r["method"]
                        .as_str()
                        .unwrap()
                        .split('.')
                        .next_back()
                        .unwrap(),
                    r["params"]["key"].as_str().unwrap_or("-")
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                "dispatchKeyEvent:a",
                "dispatchKeyEvent:a",
                "dispatchKeyEvent:a",
                "dispatchKeyEvent:Backspace",
                "dispatchKeyEvent:Backspace",
                "insertText:-",
            ]
        );
        server.abort();
    }

    #[test]
    fn test_key_text_returns_correct_text_for_special_keys() {
        assert_eq!(key_text("Enter"), Some("\r".to_string()));
        assert_eq!(key_text("Tab"), Some("\t".to_string()));
        assert_eq!(key_text(" "), Some(" ".to_string()));
        // Single printable characters carry themselves.
        assert_eq!(key_text("a"), Some("a".to_string()));
        assert_eq!(key_text("Z"), Some("Z".to_string()));
        // Non-printable named keys return None.
        assert_eq!(key_text("Escape"), None);
        assert_eq!(key_text("ArrowUp"), None);
        assert_eq!(key_text("Backspace"), None);
        assert_eq!(key_text("Delete"), None);
    }
}
