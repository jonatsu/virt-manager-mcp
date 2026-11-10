//! Mouse input on a domain's USB tablet, through QEMU's `input-send-event`.
//!
//! The server sends the QEMU monitor nothing but the `input-send-event` commands [`plan`] builds. Every field of a
//! command is a number or a name this module writes, so no text a caller sends reaches the monitor.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use virt::domain::Domain;

use crate::display::ImageSize;

/// The largest value of QEMU's absolute pointer axes, which run from 0 to this.
const AXIS_MAX: u64 = 32767;
/// Pause between a move and a press, and between a press and its release. QEMU's USB tablet merges the events of one
/// command, so a press and its release sent together reach the guest as no click at all.
const PRESS_PAUSE: Duration = Duration::from_millis(50);
/// Pause between the two clicks of a double-click, well inside a guest's double-click interval of 400 ms or more.
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(120);
/// Moves a drag takes from its start to its end, so the guest sees the pointer travel with the button held.
const DRAG_STEPS: u32 = 10;
/// Pause after each move of a drag but the last, which waits [`PRESS_PAUSE`] before the release.
const DRAG_STEP_PAUSE: Duration = Duration::from_millis(20);
/// Wheel steps a scroll takes when `scroll_steps` is unset, and the most it takes.
const DEFAULT_SCROLL_STEPS: u32 = 3;
const MAX_SCROLL_STEPS: u32 = 20;

/// What the mouse does at `x`, `y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Move the pointer there.
    Move,
    /// Move there and click `button`.
    Click,
    /// Move there and click `button` twice.
    DoubleClick,
    /// Press `button` there, move to `to_x`, `to_y` with it held, and release it.
    Drag,
    /// Move there and turn the wheel `scroll_steps` steps in `direction`.
    Scroll,
}

impl Action {
    /// The action's name as a caller writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Click => "click",
            Self::DoubleClick => "double_click",
            Self::Drag => "drag",
            Self::Scroll => "scroll",
        }
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// The direction a scroll turns the wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
}

/// A button as QEMU's `input-send-event` names it; the wheel's two directions are buttons too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QmpButton {
    Left,
    Right,
    Middle,
    WheelUp,
    WheelDown,
}

impl QmpButton {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
            Self::WheelUp => "wheel-up",
            Self::WheelDown => "wheel-down",
        }
    }
}

impl From<Button> for QmpButton {
    fn from(button: Button) -> Self {
        match button {
            Button::Left => Self::Left,
            Button::Right => Self::Right,
            Button::Middle => Self::Middle,
        }
    }
}

impl From<ScrollDirection> for QmpButton {
    fn from(direction: ScrollDirection) -> Self {
        match direction {
            ScrollDirection::Up => Self::WheelUp,
            ScrollDirection::Down => Self::WheelDown,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct MouseParams {
    pub vm: String,
    pub action: Action,
    /// Horizontal position in pixels of a full-screen image from `screenshot` without a region or from a wait's
    /// attachment; for a drag, where it starts.
    pub x: u32,
    /// Vertical position in pixels of that image; for a drag, where it starts.
    pub y: u32,
    /// Width of the full-screen image the coordinates come from, as its reply gave it.
    #[schemars(required)]
    pub image_width: Option<u32>,
    /// Height of the full-screen image the coordinates come from, as its reply gave it.
    #[schemars(required)]
    pub image_height: Option<u32>,
    /// Where a drag ends, in pixels of the same image; only for `drag`, which needs it.
    pub to_x: Option<u32>,
    /// Where a drag ends, in pixels of the same image; only for `drag`, which needs it.
    pub to_y: Option<u32>,
    /// Button for `click`, `double_click` and `drag` (default left).
    pub button: Option<Button>,
    /// Wheel direction; only for `scroll`, which needs it.
    pub direction: Option<ScrollDirection>,
    /// Wheel steps for `scroll`, 1 to 20 (default 3).
    pub scroll_steps: Option<u32>,
    /// Display head (default 0). Only head 0 is supported.
    #[serde(default)]
    pub screen: u32,
}

/// One `input-send-event` command, the pause to keep after it, and the button it leaves held down, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub command: Value,
    pub pause_after: Duration,
    pub holds: Option<QmpButton>,
}

/// A position on QEMU's absolute axes, each 0 to 32767.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AxisPoint {
    x: u64,
    y: u64,
}

/// Maps pixel `v` of an axis `dim` pixels long onto 0 to 32767, so the first pixel maps to 0 and the last to 32767.
/// An axis of one pixel maps to 0. The caller guarantees `v < dim`.
fn to_axis(v: u32, dim: u32) -> u64 {
    match dim {
        0 | 1 => 0,
        _ => u64::from(v) * AXIS_MAX / u64::from(dim - 1),
    }
}

/// Checks that `x`, `y` lies inside `image` and maps it onto the axes.
fn axis_point(image: ImageSize, x: u32, y: u32, what: &str) -> Result<AxisPoint> {
    if x >= image.width() || y >= image.height() {
        bail!("{what} ({x}, {y}) lies outside the {image} image it refers to");
    }
    Ok(AxisPoint {
        x: to_axis(x, image.width()),
        y: to_axis(y, image.height()),
    })
}

fn input_send_event(events: Value) -> Value {
    json!({ "execute": "input-send-event", "arguments": { "events": events } })
}

fn move_to(p: AxisPoint) -> Value {
    input_send_event(json!([
        { "type": "abs", "data": { "axis": "x", "value": p.x } },
        { "type": "abs", "data": { "axis": "y", "value": p.y } },
    ]))
}

fn button(b: QmpButton, down: bool) -> Value {
    input_send_event(json!([
        { "type": "btn", "data": { "down": down, "button": b.name() } },
    ]))
}

/// Collects the steps of a sequence.
struct Sequence {
    steps: Vec<Step>,
    held: Option<QmpButton>,
}

impl Sequence {
    fn new() -> Self {
        Self {
            steps: Vec::new(),
            held: None,
        }
    }

    fn push(&mut self, command: Value, pause_after: Duration) {
        self.steps.push(Step {
            command,
            pause_after,
            holds: self.held,
        });
    }

    fn move_to(&mut self, p: AxisPoint, pause_after: Duration) {
        self.push(move_to(p), pause_after);
    }

    fn press(&mut self, b: QmpButton) {
        self.held = Some(b);
        self.push(button(b, true), PRESS_PAUSE);
    }

    fn release(&mut self, b: QmpButton, pause_after: Duration) {
        self.held = None;
        self.push(button(b, false), pause_after);
    }

    /// The steps, with no pause after the last one.
    fn finish(mut self) -> Vec<Step> {
        if let Some(last) = self.steps.last_mut() {
            last.pause_after = Duration::ZERO;
        }
        self.steps
    }
}

fn refuse_unless(allowed: bool, given: bool, field: &str, action: Action) -> Result<()> {
    if given && !allowed {
        bail!("{field} does not apply to the {} action", action.name());
    }
    Ok(())
}

/// Validates a mouse request and builds its `input-send-event` commands with the pauses between them.
pub fn plan(p: &MouseParams) -> Result<Vec<Step>> {
    if p.screen != 0 {
        bail!(
            "mouse supports display head 0 only: addressing head {} needs the video device's identifier",
            p.screen
        );
    }
    let image = ImageSize::new(p.image_width, p.image_height)?;
    let at = axis_point(image, p.x, p.y, "the position")?;
    let action = p.action;
    let clicks = matches!(action, Action::Click | Action::DoubleClick | Action::Drag);
    refuse_unless(clicks, p.button.is_some(), "button", action)?;
    refuse_unless(
        action == Action::Drag,
        p.to_x.is_some() || p.to_y.is_some(),
        "to_x and to_y",
        action,
    )?;
    refuse_unless(
        action == Action::Scroll,
        p.direction.is_some(),
        "direction",
        action,
    )?;
    refuse_unless(
        action == Action::Scroll,
        p.scroll_steps.is_some(),
        "scroll_steps",
        action,
    )?;
    let b = QmpButton::from(p.button.unwrap_or(Button::Left));

    let mut seq = Sequence::new();
    match action {
        Action::Move => seq.move_to(at, Duration::ZERO),
        Action::Click => {
            seq.move_to(at, PRESS_PAUSE);
            seq.press(b);
            seq.release(b, Duration::ZERO);
        }
        Action::DoubleClick => {
            seq.move_to(at, PRESS_PAUSE);
            seq.press(b);
            seq.release(b, DOUBLE_CLICK_GAP);
            seq.press(b);
            seq.release(b, Duration::ZERO);
        }
        Action::Drag => {
            let (Some(to_x), Some(to_y)) = (p.to_x, p.to_y) else {
                bail!("drag needs to_x and to_y, where it ends");
            };
            let end = axis_point(image, to_x, to_y, "the drag's end")?;
            seq.move_to(at, PRESS_PAUSE);
            seq.press(b);
            for i in 1..=DRAG_STEPS {
                let pause = if i == DRAG_STEPS {
                    PRESS_PAUSE
                } else {
                    DRAG_STEP_PAUSE
                };
                seq.move_to(between(at, end, i, DRAG_STEPS), pause);
            }
            seq.release(b, Duration::ZERO);
        }
        Action::Scroll => {
            let Some(direction) = p.direction else {
                bail!("scroll needs a direction, up or down");
            };
            let steps = p.scroll_steps.unwrap_or(DEFAULT_SCROLL_STEPS);
            if !(1..=MAX_SCROLL_STEPS).contains(&steps) {
                bail!("scroll_steps must be 1 to {MAX_SCROLL_STEPS}");
            }
            let wheel = QmpButton::from(direction);
            seq.move_to(at, PRESS_PAUSE);
            for _ in 0..steps {
                seq.press(wheel);
                seq.release(wheel, PRESS_PAUSE);
            }
        }
    }
    Ok(seq.finish())
}

/// The point `i` of `n` steps along the line from `a` to `b`; step `n` is `b`.
fn between(a: AxisPoint, b: AxisPoint, i: u32, n: u32) -> AxisPoint {
    let lerp = |from: u64, to: u64| {
        let (from, to) = (from as i64, to as i64);
        let v = from + (to - from) * i64::from(i) / i64::from(n);
        u64::try_from(v).expect("a point between two axis values lies on the axis")
    };
    AxisPoint {
        x: lerp(a.x, b.x),
        y: lerp(a.y, b.y),
    }
}

/// Whether the domain XML `xml` has a tablet input device, which gives the mouse absolute positions.
fn has_tablet(xml: &str) -> Result<bool> {
    let doc = roxmltree::Document::parse(xml).context("parsing the domain XML")?;
    Ok(doc
        .root_element()
        .children()
        .filter(|n| n.has_tag_name("devices"))
        .flat_map(|devices| devices.children())
        .any(|n| n.has_tag_name("input") && n.attribute("type") == Some("tablet")))
}

/// Refuses a running domain without a tablet in its live XML.
pub fn require_tablet(dom: &Domain, vm: &str) -> Result<()> {
    let xml = dom
        .get_xml_desc(0)
        .map_err(|e| anyhow!("reading the XML of VM '{vm}': {}", e.message()))?;
    if !has_tablet(&xml)? {
        bail!(
            "VM '{vm}' has no USB tablet (<input type='tablet' bus='usb'/>), which the mouse needs for absolute \
             positions; VMs from create_vm and create_vm_from_template have one"
        );
    }
    Ok(())
}

/// Checks a QMP reply from the monitor for an error.
fn check_reply(reply: &str) -> Result<()> {
    let reply: Value = serde_json::from_str(reply).context("reading the QEMU monitor's reply")?;
    if let Some(error) = reply.get("error") {
        let desc = error
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or("no description");
        bail!("QEMU refused input-send-event: {desc}");
    }
    Ok(())
}

fn send_one(dom: &Domain, command: &Value) -> Result<()> {
    let reply = dom
        .qemu_monitor_command(&command.to_string(), 0)
        .map_err(|e| anyhow!("sending input-send-event: {}", e.message()))?;
    check_reply(&reply)
}

/// Sends `steps` through the QEMU monitor in QMP mode, keeping each pause. When a step fails while an earlier step
/// holds a button down, it tries once to release that button, so a failed drag does not leave it held.
pub fn send(dom: &Domain, steps: &[Step]) -> Result<()> {
    let mut held = None;
    for step in steps {
        if let Err(e) = send_one(dom, &step.command) {
            if let Some(b) = held {
                let _ = send_one(dom, &button(b, false));
            }
            return Err(e);
        }
        held = step.holds;
        std::thread::sleep(step.pause_after);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn params(action: Action) -> MouseParams {
        MouseParams {
            vm: "trial".into(),
            action,
            x: 1218,
            y: 774,
            image_width: Some(1280),
            image_height: Some(800),
            to_x: None,
            to_y: None,
            button: None,
            direction: None,
            scroll_steps: None,
            screen: 0,
        }
    }

    fn mv(x: u64, y: u64) -> Value {
        move_to(AxisPoint { x, y })
    }

    fn btn(name: &str, down: bool) -> Value {
        json!({ "execute": "input-send-event", "arguments": { "events": [
            { "type": "btn", "data": { "down": down, "button": name } }
        ] } })
    }

    fn step(command: Value, pause_ms: u64, holds: Option<QmpButton>) -> Step {
        Step {
            command,
            pause_after: Duration::from_millis(pause_ms),
            holds,
        }
    }

    fn refusal(p: &MouseParams) -> String {
        format!("{:#}", plan(p).unwrap_err())
    }

    #[test]
    fn the_axis_maps_corners_and_centre() {
        assert_eq!(to_axis(0, 1280), 0);
        assert_eq!(to_axis(1279, 1280), 32767);
        assert_eq!(to_axis(0, 800), 0);
        assert_eq!(to_axis(799, 800), 32767);
        // 640 * 32767 / 1279 = 16396.3 and 400 * 32767 / 799 = 16403.98, rounded down.
        assert_eq!(to_axis(640, 1280), 16396);
        assert_eq!(to_axis(400, 800), 16404);
        // The click the host's QEMU took on the Debian installer's Continue button.
        assert_eq!(to_axis(1218, 1280), 31204);
        assert_eq!(to_axis(774, 800), 31741);
        assert_eq!(to_axis(0, 1), 0);
        assert_eq!(to_axis(1, 2), 32767);
    }

    #[test]
    fn a_move_is_one_command_with_both_axes() {
        let steps = plan(&params(Action::Move)).unwrap();
        assert_eq!(
            steps[0].command,
            json!({ "execute": "input-send-event", "arguments": { "events": [
                { "type": "abs", "data": { "axis": "x", "value": 31204 } },
                { "type": "abs", "data": { "axis": "y", "value": 31741 } },
            ] } })
        );
        assert_eq!(steps, [step(mv(31204, 31741), 0, None)]);
    }

    #[test]
    fn a_click_presses_and_releases_in_separate_commands() {
        let steps = plan(&params(Action::Click)).unwrap();
        let left = Some(QmpButton::Left);
        assert_eq!(
            steps,
            [
                step(mv(31204, 31741), 50, None),
                step(btn("left", true), 50, left),
                step(btn("left", false), 0, None),
            ]
        );
    }

    #[test]
    fn a_click_uses_the_button_it_names() {
        for (button, name) in [(Button::Right, "right"), (Button::Middle, "middle")] {
            let mut p = params(Action::Click);
            p.button = Some(button);
            let steps = plan(&p).unwrap();
            assert_eq!(steps[1].command, btn(name, true));
            assert_eq!(steps[2].command, btn(name, false));
        }
    }

    #[test]
    fn a_double_click_spaces_its_clicks_inside_the_interval() {
        let steps = plan(&params(Action::DoubleClick)).unwrap();
        let left = Some(QmpButton::Left);
        assert_eq!(
            steps,
            [
                step(mv(31204, 31741), 50, None),
                step(btn("left", true), 50, left),
                step(btn("left", false), 120, None),
                step(btn("left", true), 50, left),
                step(btn("left", false), 0, None),
            ]
        );
    }

    #[test]
    fn a_drag_presses_moves_in_steps_and_releases() {
        let mut p = params(Action::Drag);
        (p.x, p.y, p.to_x, p.to_y) = (0, 0, Some(1279), Some(799));
        let steps = plan(&p).unwrap();
        let left = Some(QmpButton::Left);
        let mut want = vec![step(mv(0, 0), 50, None), step(btn("left", true), 50, left)];
        for i in 1..=10 {
            let v = 32767 * i / 10;
            want.push(step(mv(v, v), if i == 10 { 50 } else { 20 }, left));
        }
        want.push(step(btn("left", false), 0, None));
        assert_eq!(steps, want);
    }

    #[test]
    fn a_drag_can_run_backwards() {
        let mut p = params(Action::Drag);
        (p.x, p.y, p.to_x, p.to_y) = (1279, 799, Some(0), Some(0));
        let steps = plan(&p).unwrap();
        assert_eq!(steps[2].command, mv(29491, 29491));
        assert_eq!(steps[11].command, mv(0, 0));
    }

    #[test]
    fn a_scroll_turns_the_wheel_one_step_per_press_and_release() {
        let mut p = params(Action::Scroll);
        p.direction = Some(ScrollDirection::Down);
        p.scroll_steps = Some(2);
        let wheel = Some(QmpButton::WheelDown);
        assert_eq!(
            plan(&p).unwrap(),
            [
                step(mv(31204, 31741), 50, None),
                step(btn("wheel-down", true), 50, wheel),
                step(btn("wheel-down", false), 50, None),
                step(btn("wheel-down", true), 50, wheel),
                step(btn("wheel-down", false), 0, None),
            ]
        );
        p.direction = Some(ScrollDirection::Up);
        p.scroll_steps = None;
        let steps = plan(&p).unwrap();
        assert_eq!(steps.len(), 1 + 2 * DEFAULT_SCROLL_STEPS as usize);
        assert_eq!(steps[1].command, btn("wheel-up", true));
    }

    #[test]
    fn a_scroll_needs_a_direction_and_a_bounded_step_count() {
        let mut p = params(Action::Scroll);
        assert!(refusal(&p).contains("needs a direction"));
        p.direction = Some(ScrollDirection::Up);
        for steps in [0, MAX_SCROLL_STEPS + 1] {
            p.scroll_steps = Some(steps);
            assert!(refusal(&p).contains("scroll_steps must be 1 to 20"));
        }
        p.scroll_steps = Some(MAX_SCROLL_STEPS);
        assert!(plan(&p).is_ok());
    }

    #[test]
    fn another_display_head_is_refused() {
        let mut p = params(Action::Click);
        p.screen = 1;
        let err = refusal(&p);
        assert!(err.contains("display head 0 only"), "{err}");
    }

    #[test]
    fn coordinates_need_the_image_size() {
        for (w, h) in [(None, Some(800)), (Some(1280), None), (None, None)] {
            let mut p = params(Action::Click);
            (p.image_width, p.image_height) = (w, h);
            assert!(refusal(&p).contains("need image_width and image_height"));
        }
        let mut p = params(Action::Click);
        p.image_width = Some(0);
        assert!(refusal(&p).contains("at least 1"));
    }

    #[test]
    fn coordinates_must_lie_inside_the_image() {
        let mut p = params(Action::Click);
        (p.x, p.y) = (1280, 0);
        assert!(refusal(&p).contains("outside the 1280x800 image"));
        (p.x, p.y) = (0, 800);
        assert!(refusal(&p).contains("outside"));
        let mut p = params(Action::Drag);
        (p.to_x, p.to_y) = (Some(10), Some(800));
        assert!(refusal(&p).contains("the drag's end (10, 800) lies outside"));
    }

    #[test]
    fn a_one_pixel_image_maps_to_the_origin() {
        let mut p = params(Action::Move);
        (p.x, p.y, p.image_width, p.image_height) = (0, 0, Some(1), Some(1));
        assert_eq!(plan(&p).unwrap()[0].command, mv(0, 0));
    }

    #[test]
    fn fields_of_other_actions_are_refused() {
        let mut p = params(Action::Move);
        p.button = Some(Button::Right);
        assert!(refusal(&p).contains("button does not apply to the move action"));
        let mut p = params(Action::Click);
        p.to_x = Some(1);
        assert!(refusal(&p).contains("to_x and to_y does not apply"));
        let mut p = params(Action::Drag);
        p.to_x = Some(1);
        assert!(refusal(&p).contains("drag needs to_x and to_y"));
        let mut p = params(Action::Click);
        p.direction = Some(ScrollDirection::Up);
        assert!(refusal(&p).contains("direction does not apply"));
        let mut p = params(Action::DoubleClick);
        p.scroll_steps = Some(1);
        assert!(refusal(&p).contains("scroll_steps does not apply"));
        let mut p = params(Action::Scroll);
        (p.direction, p.button) = (Some(ScrollDirection::Up), Some(Button::Left));
        assert!(refusal(&p).contains("button does not apply to the scroll action"));
    }

    /// Every key and string value in `v`.
    fn strings(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) => {
                out.insert(s.clone());
            }
            Value::Array(items) => items.iter().for_each(|i| strings(i, out)),
            Value::Object(map) => {
                for (k, v) in map {
                    out.insert(k.clone());
                    strings(v, out);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    #[test]
    fn no_caller_text_reaches_a_command() {
        let fixed: BTreeSet<String> = [
            "execute",
            "input-send-event",
            "arguments",
            "events",
            "type",
            "abs",
            "btn",
            "data",
            "axis",
            "x",
            "y",
            "value",
            "down",
            "button",
            "left",
            "right",
            "middle",
            "wheel-up",
            "wheel-down",
        ]
        .map(String::from)
        .into();
        let actions = [
            Action::Move,
            Action::Click,
            Action::DoubleClick,
            Action::Drag,
            Action::Scroll,
        ];
        let buttons = [
            None,
            Some(Button::Left),
            Some(Button::Right),
            Some(Button::Middle),
        ];
        let sizes = [(1, 1), (2, 3), (1280, 800), (u32::MAX, u32::MAX)];
        let mut seen = BTreeSet::new();
        let mut plans = 0;
        for action in actions {
            for button in buttons {
                for direction in [ScrollDirection::Up, ScrollDirection::Down] {
                    for (w, h) in sizes {
                        for (x, y) in [(0, 0), (w - 1, h - 1), (w / 2, h / 3)] {
                            let mut p = params(action);
                            p.vm = "\"}]},{\"execute\":\"quit\"}".into();
                            (p.x, p.y, p.image_width, p.image_height) = (x, y, Some(w), Some(h));
                            if matches!(action, Action::Click | Action::DoubleClick | Action::Drag)
                            {
                                p.button = button;
                            }
                            if action == Action::Drag {
                                (p.to_x, p.to_y) = (Some(w - 1 - x), Some(h - 1 - y));
                            }
                            if action == Action::Scroll {
                                (p.direction, p.scroll_steps) = (Some(direction), Some(2));
                            }
                            for s in plan(&p).unwrap() {
                                assert_eq!(s.command["execute"], "input-send-event");
                                for e in s.command["arguments"]["events"].as_array().unwrap() {
                                    for v in [&e["data"]["value"], &e["data"]["down"]] {
                                        assert!(
                                            v.is_null()
                                                || v.is_boolean()
                                                || v.as_u64().is_some_and(|n| n <= AXIS_MAX)
                                        );
                                    }
                                }
                                strings(&s.command, &mut seen);
                            }
                            plans += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(plans, 5 * 4 * 2 * 4 * 3);
        assert!(seen.is_subset(&fixed), "{:?}", seen.difference(&fixed));
    }

    #[test]
    fn the_tablet_check_reads_the_devices() {
        let with = "<domain type='kvm'><name>t</name><devices><input type='mouse' bus='ps2'/>\
                    <input type='tablet' bus='usb'><address type='usb' bus='0' port='1'/></input></devices></domain>";
        assert!(has_tablet(with).unwrap());
        let without = "<domain type='kvm'><name>tablet</name><description>input type='tablet'</description>\
                       <devices><input type='mouse' bus='ps2'/><input type='keyboard' bus='ps2'/></devices></domain>";
        assert!(!has_tablet(without).unwrap());
        assert!(!has_tablet("<domain/>").unwrap());
        assert!(has_tablet("<domain").is_err());
    }

    #[test]
    fn qmp_errors_are_reported() {
        assert!(check_reply(r#"{"return":{},"id":"libvirt-42"}"#).is_ok());
        let err = check_reply(
            r#"{"id":"libvirt-43","error":{"class":"GenericError","desc":"no tablet"}}"#,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "QEMU refused input-send-event: no tablet");
        assert!(check_reply("not json").is_err());
    }
}
