//! Font coverage. egui bundles a specific subset of glyphs; anything outside
//! it renders as a tofu box, which reads as a broken app rather than a missing
//! font. This asserts every symbol the UI draws is one it can actually show.

#[test]
fn every_ui_symbol_renders() {
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
    out.textures_delta.clear();

    // Proportional only: buttons use it, and `has_glyph` reports false for
    // every character of a family nothing has rendered yet — including plain
    // ASCII — so checking Monospace here would only produce false alarms.
    let id = egui::FontId::proportional(14.0);
    assert!(
        ctx.fonts_mut(|f| f.has_glyph(&id, 'A')),
        "probe is broken if it cannot find 'A'"
    );

    let missing: Vec<String> = clip_editor::glyphs::ALL
        .iter()
        .flat_map(|symbol| symbol.chars())
        .filter(|c| !ctx.fonts_mut(|f| f.has_glyph(&id, *c)))
        .map(|c| format!("{c} (U+{:04X})", c as u32))
        .collect();
    assert!(
        missing.is_empty(),
        "these render as tofu boxes: {missing:?}"
    );
}
