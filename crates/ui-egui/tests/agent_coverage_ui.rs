//! Partial agent-reachability audit. The real app runs headless under `egui_kittest`; for the main
//! window and every panel, each interactive widget that egui exposes in the accessibility tree must
//! have an automation id (so `ui.click {id}` / the MCP `ui_click` tool can reach it).
//!
//! LIMITATION: most FilmCraft controls are custom-painted and never reach the accessibility tree
//! (about 10 nodes per screen against about 200 automation elements), so this only covers the
//! stock egui widgets. A pass is NOT evidence that every button is automatable. Dialogs opened by
//! menu commands are not walked yet. `FILMCRAFT_COVERAGE_REPORT=<path>` writes the gaps as JSON.

use std::collections::BTreeMap;
use std::sync::mpsc::{Sender, channel};

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::NodeT;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn new() -> Self {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let mut app = FilmcraftApp::new(s).with_control(rx);
        // never hand paths to the real OS opener from a test
        app.hooks.open_path = Some(Box::new(|_, _| Ok(())));
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).expect("control channel open");
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        json!({"ok": false, "error": format!("no reply to {method}")})
    }

    /// Interactive widgets on screen that no automation element covers: `(role, label)`.
    fn uncovered(&mut self) -> Vec<(String, String)> {
        let elements = self.call("ui.elements", json!({}));
        let rects: Vec<[f32; 4]> = elements["result"]
            .as_array()
            .map(|a| a.iter().filter_map(|e| serde_json::from_value::<[f32; 4]>(e["rect"].clone()).ok()).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        let mut seen = 0usize;
        for n in self.harness.root().children_recursive() {
            let ak = n.accesskit_node();
            let role = ak.role();
            if !is_interactive(role) || ak.is_hidden() {
                continue;
            }
            let r = n.rect();
            if r.width() < 1.0 || r.height() < 1.0 {
                continue;
            }
            seen += 1;
            let c = r.center();
            let covered = rects.iter().any(|e| {
                let (x, y, w, h) = (e[0], e[1], e[2], e[3]);
                (c.x >= x && c.x <= x + w && c.y >= y && c.y <= y + h)
                    || (x + w / 2.0 >= r.min.x && x + w / 2.0 <= r.max.x && y + h / 2.0 >= r.min.y && y + h / 2.0 <= r.max.y)
            });
            if !covered {
                out.push((format!("{role:?}"), ak.label().unwrap_or_default().to_string()));
            }
        }
        println!("  interactive nodes: {seen}, automation elements: {}", rects.len());
        out
    }
}

fn is_interactive(r: Role) -> bool {
    matches!(
        r,
        Role::Button
            | Role::CheckBox
            | Role::RadioButton
            | Role::ComboBox
            | Role::Slider
            | Role::SpinButton
            | Role::TextInput
            | Role::MultilineTextInput
            | Role::Link
            | Role::Tab
            | Role::MenuItem
            | Role::MenuItemCheckBox
            | Role::MenuItemRadio
    )
}

#[test]
fn stock_egui_widgets_have_automation_ids() {
    let mut d = Driver::new();
    let menu = d.call("ui.menu.list", json!({}));
    let items: Vec<String> = menu["result"].as_array().map(|a| a.iter().filter_map(|i| i["id"].as_str().map(String::from)).collect()).unwrap_or_default();
    let mut gaps: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let base = d.uncovered();
    gaps.insert("(main window)".into(), base);
    for id in items.iter().filter(|i| i.starts_with("window.panel.")) {
        let r = d.call("ui.menu.invoke", json!({"id": id}));
        d.frames(3);
        if r["ok"] == json!(true) {
            let g = d.uncovered();
            gaps.insert(id.clone(), g);
        }
    }
    let total: usize = gaps.values().map(Vec::len).sum();
    println!("uncovered interactive widgets: {total}");
    assert_eq!(total, 0, "stock egui widgets without an automation id: {gaps:?}");
    for (k, v) in &gaps {
        println!("{k}: {}", v.len());
        for (role, label) in v.iter().take(40) {
            println!("    {role} {label:?}");
        }
    }
    if let Ok(path) = std::env::var("FILMCRAFT_COVERAGE_REPORT") {
        let _ = std::fs::write(path, serde_json::to_string_pretty(&gaps).unwrap_or_default());
    }
}
