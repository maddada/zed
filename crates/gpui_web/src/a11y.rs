//! Accessibility on the web: the AccessKit tree gpui builds for every frame, mirrored into the
//! page's DOM behind the canvas.
//!
//! AccessKit has no browser adapter, and a canvas is one opaque node to the browser. So every
//! node of the tree becomes an element with the matching ARIA role, label and state, placed at
//! the node's bounds and letting pointer events through, so that a screen reader, DevTools,
//! Playwright or `getByRole` sees the same tree the desktop platforms expose, and a click on a
//! mirrored element lands on the canvas at that spot. `window.gpuiA11y` also dispatches AccessKit
//! actions straight to gpui for tests that want to press a button without a pointer.
//!
//! The mirror is on unless the page is loaded with `?a11y=off`, because the tree's main use in a
//! browser is automated testing. It is diffed per frame: gpui sends the whole tree, and the DOM
//! only changes where the tree did.
use std::cell::RefCell;
use std::rc::Rc;

use accesskit::{ActionRequest, NodeId, TreeUpdate};
use gpui::A11yCallbacks;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
const ROLES = {
  Window: 'application', Button: 'button', CheckBox: 'checkbox', Switch: 'switch', Tab: 'tab',
  TabList: 'tablist', TabPanel: 'tabpanel', TreeItem: 'treeitem', Tree: 'tree', Group: 'group',
  Navigation: 'navigation', Document: 'document', Article: 'article', Label: 'text',
  StaticText: 'text', TextRun: 'text', TextInput: 'textbox', MultilineTextInput: 'textbox',
  SearchInput: 'searchbox', Menu: 'menu', MenuItem: 'menuitem', MenuItemCheckBox: 'menuitemcheckbox',
  MenuItemRadio: 'menuitemradio', MenuBar: 'menubar', ListBox: 'listbox', ListBoxOption: 'option',
  List: 'list', ListItem: 'listitem', Heading: 'heading', Link: 'link', Status: 'status',
  Alert: 'alert', Dialog: 'dialog', Tooltip: 'tooltip', Terminal: 'log', Log: 'log',
  ScrollBar: 'scrollbar', Slider: 'slider', SpinButton: 'spinbutton', ProgressIndicator: 'progressbar',
  RadioButton: 'radio', RadioGroup: 'radiogroup', Image: 'img', Toolbar: 'toolbar', Region: 'region',
  Paragraph: 'paragraph', Row: 'row', Cell: 'cell', Table: 'table', Grid: 'grid', Form: 'form',
};

let root = null;
let elements = new Map();
let act = null;
let disabled = false;

export function gpui_a11y_start(dispatch) {
  disabled = new URLSearchParams(location.search).get('a11y') === 'off';
  if (disabled) return false;
  act = dispatch;
  root = document.createElement('div');
  root.id = 'gpui-a11y';
  root.setAttribute('role', 'application');
  root.setAttribute('aria-label', 'GPUI');
  Object.assign(root.style, { position: 'fixed', inset: '0', pointerEvents: 'none', zIndex: '999', overflow: 'hidden' });
  document.body.appendChild(root);
  window.gpuiA11y = {
    // Dispatch an AccessKit action ("Click", "Focus", "Blur", ...) to the node with this id.
    act(id, action = 'Click') { act(String(id), action); },
    // The element mirroring a node, for tests that already hold an id.
    element(id) { return elements.get(String(id)) ?? null; },
    // Every node as {id, role, label, description, expanded, selected, toggled, bounds}: a cheap snapshot for a test to assert on.
    snapshot() {
      return [...elements.values()].map((el) => ({
        id: el.dataset.gpuiId, role: el.getAttribute('role'), label: el.getAttribute('aria-label') ?? '',
        description: el.getAttribute('aria-description') ?? undefined,
        expanded: el.getAttribute('aria-expanded') ?? undefined, selected: el.getAttribute('aria-selected') ?? undefined,
        checked: el.getAttribute('aria-checked') ?? undefined, focused: el.dataset.gpuiFocused === '1' || undefined,
        bounds: (({ left, top, width, height }) => [left, top, width, height].map(Math.round))(el.getBoundingClientRect()),
      }));
    },
  };
  return true;
}

function setAttr(el, name, value) {
  if (value === undefined || value === null) {
    if (el.hasAttribute(name)) el.removeAttribute(name);
  } else if (el.getAttribute(name) !== String(value)) {
    el.setAttribute(name, String(value));
  }
}

// `nodes` is a JSON array of [id, {role, label, description, bounds, expanded, selected, toggled, actions, children}], and `focus` the focused id.
// gpui sends a tree per drawn frame; the DOM is brought up to date with the latest one at most ten times a second, so a streaming chat does not spend its frames on ARIA attributes.
let latest = null;
let flushTimer = null;
export function gpui_a11y_update(nodesJson, focus, dpr) {
  if (disabled || root === null) return;
  latest = [nodesJson, dpr];
  if (flushTimer === null) {
    flushTimer = setTimeout(() => { flushTimer = null; const [json, ratio] = latest; latest = null; applyTree(json, ratio); }, 100);
  }
}

function applyTree(nodesJson, dpr) {
  const nodes = JSON.parse(nodesJson);
  const seen = new Set();
  const byId = new Map();
  for (const [id, node] of nodes) byId.set(id, node);
  for (const [id, node] of nodes) {
    seen.add(id);
    let el = elements.get(id);
    if (!el) {
      el = document.createElement('div');
      el.dataset.gpuiId = id;
      el.style.position = 'fixed';
      el.style.pointerEvents = 'none';
      elements.set(id, el);
    }
    setAttr(el, 'role', ROLES[node.role] ?? 'group');
    setAttr(el, 'aria-label', node.label);
    setAttr(el, 'aria-description', node.description);
    setAttr(el, 'aria-expanded', node.expanded);
    setAttr(el, 'aria-selected', node.selected);
    setAttr(el, 'aria-checked', node.toggled);
    setAttr(el, 'aria-keyshortcuts', node.shortcut);
    setAttr(el, 'data-gpui-actions', node.actions && node.actions.length ? node.actions.join(' ') : undefined);
    if (node.focused) el.dataset.gpuiFocused = '1'; else delete el.dataset.gpuiFocused;
    if (node.bounds) {
      const [x0, y0, x1, y1] = node.bounds;
      const style = `left:${x0 / dpr}px;top:${y0 / dpr}px;width:${(x1 - x0) / dpr}px;height:${(y1 - y0) / dpr}px;`;
      // Fixed, not absolute: the elements nest like the tree, and an absolute child would be placed from its parent's corner instead of the page's.
      if (el.dataset.gpuiBox !== style) { el.dataset.gpuiBox = style; el.style.cssText = `position:fixed;pointer-events:none;${style}`; }
    }
  }
  for (const [id, el] of elements) {
    if (!seen.has(id)) { el.remove(); elements.delete(id); }
  }
  // Parent each element under its parent's element, in the tree's child order; nodes are fixed-positioned, so nesting only carries structure.
  for (const [id, node] of nodes) {
    const el = elements.get(id);
    const children = node.children ?? [];
    let previous = null;
    for (const childId of children) {
      const child = elements.get(childId);
      if (!child) continue;
      const expectedPrevious = previous;
      if (child.parentElement !== el || child.previousElementSibling !== expectedPrevious) {
        if (expectedPrevious) expectedPrevious.after(child); else el.prepend(child);
      }
      previous = child;
    }
  }
  // gpui lists the root last (it pops it off its stack after every child), so the root is the one node nobody lists as a child.
  const childIds = new Set();
  for (const [, node] of nodes) for (const childId of node.children ?? []) childIds.add(childId);
  for (const [id] of nodes) {
    if (childIds.has(id)) continue;
    const top = elements.get(id);
    if (top && top.parentElement !== root) root.appendChild(top);
  }
}
"#)]
extern "C" {
    fn gpui_a11y_start(dispatch: &Closure<dyn FnMut(String, String)>) -> bool;
    fn gpui_a11y_update(nodes_json: &str, focus: &str, dpr: f64);
}

pub(crate) struct WebA11y {
    callbacks: A11yCallbacks,
    _dispatch: Closure<dyn FnMut(String, String)>,
    active: bool,
}

impl WebA11y {
    pub(crate) fn start(callbacks: A11yCallbacks) -> Rc<RefCell<Self>> {
        let this = Rc::new(RefCell::new(Self {
            callbacks,
            _dispatch: Closure::<dyn FnMut(String, String)>::new(|_, _| {}),
            active: false,
        }));
        let for_dispatch = Rc::clone(&this);
        let dispatch = Closure::<dyn FnMut(String, String)>::new(move |id: String, action: String| {
            let (Ok(id), Some(action)) = (id.parse::<u64>(), action_from_name(&action)) else {
                return;
            };
            let request = ActionRequest {
                action,
                target_tree: accesskit::TreeId::ROOT,
                target_node: NodeId(id),
                data: None,
            };
            // The callback may re-enter gpui, which may be mid-frame when a test calls in; deliver from the closure's own turn.
            if let Ok(a11y) = for_dispatch.try_borrow() {
                (a11y.callbacks.action)(request);
            }
        });
        let started = gpui_a11y_start(&dispatch);
        {
            let mut a11y = this.borrow_mut();
            a11y._dispatch = dispatch;
            a11y.active = started;
            if started {
                // The page is the assistive client: the tree is wanted from the first frame.
                (a11y.callbacks.activation)();
            }
        }
        this
    }

    pub(crate) fn tree_update(&self, update: TreeUpdate, dpr: f64) {
        if !self.active {
            return;
        }
        let mut json = String::from("[");
        for (index, (id, node)) in update.nodes.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            json.push_str(&format!("[\"{}\",{{", id.0));
            json.push_str(&format!("\"role\":\"{:?}\"", node.role()));
            if let Some(label) = node.label() {
                json.push_str(&format!(",\"label\":{}", json_string(label)));
            }
            if let Some(description) = node.description() {
                json.push_str(&format!(",\"description\":{}", json_string(description)));
            }
            if let Some(shortcut) = node.keyboard_shortcut() {
                json.push_str(&format!(",\"shortcut\":{}", json_string(shortcut)));
            }
            if let Some(expanded) = node.is_expanded() {
                json.push_str(&format!(",\"expanded\":{expanded}"));
            }
            if node.is_selected().is_some() {
                json.push_str(&format!(",\"selected\":{}", node.is_selected().unwrap()));
            }
            if let Some(toggled) = node.toggled() {
                json.push_str(&format!(
                    ",\"toggled\":\"{}\"",
                    match toggled {
                        accesskit::Toggled::True => "true",
                        accesskit::Toggled::False => "false",
                        accesskit::Toggled::Mixed => "mixed",
                    }
                ));
            }
            if let Some(bounds) = node.bounds() {
                json.push_str(&format!(
                    ",\"bounds\":[{},{},{},{}]",
                    bounds.x0, bounds.y0, bounds.x1, bounds.y1
                ));
            }
            if *id == update.focus {
                json.push_str(",\"focused\":true");
            }
            let actions: Vec<String> = all_actions()
                .filter(|action| node.supports_action(*action))
                .map(|action| format!("{action:?}"))
                .collect();
            if !actions.is_empty() {
                json.push_str(&format!(",\"actions\":{}", json_string_array(&actions)));
            }
            let children: Vec<String> = node.children().iter().map(|child| child.0.to_string()).collect();
            if !children.is_empty() {
                json.push_str(&format!(",\"children\":{}", json_string_array(&children)));
            }
            json.push_str("}]");
        }
        json.push(']');
        gpui_a11y_update(&json, &update.focus.0.to_string(), dpr);
    }
}

/// Every AccessKit action, through the `enumn` constructor the crate derives for it.
fn all_actions() -> impl Iterator<Item = accesskit::Action> {
    (0..64).filter_map(accesskit::Action::n)
}

fn action_from_name(name: &str) -> Option<accesskit::Action> {
    all_actions().find(|action| format!("{action:?}") == name)
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_string_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json_string(item));
    }
    out.push(']');
    out
}
