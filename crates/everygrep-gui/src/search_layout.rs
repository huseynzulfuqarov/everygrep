//! Geometry shared by the editor and its selectors when the window resizes.

use crate::SearchRowGeometry;

pub fn calculate(
    width: f32,
    search_min: f32,
    filter_width: f32,
    show_scope: bool,
    show_menu: bool,
    margin: f32,
    gap: f32,
) -> SearchRowGeometry {
    let selectors = filter_width
        + if show_scope { 170.0 + gap } else { 0.0 }
        + if show_menu { 0.0 } else { 34.0 + gap };
    // SearchField's measured minimum already contains a one-pixel viewport.
    let stacked = width < 2.0 * margin + search_min - 1.0 + 200.0 + gap + selectors;
    let row_width = if stacked {
        width - 2.0 * margin
    } else {
        selectors
    };
    SearchRowGeometry {
        stacked,
        search_width: width - 2.0 * margin - if stacked { 0.0 } else { selectors + gap },
        selectors_x: if stacked {
            margin
        } else {
            width - margin - selectors
        },
        selectors_width: row_width,
        selector_width: (row_width
            - if show_scope { gap } else { 0.0 }
            - if show_menu { 0.0 } else { 34.0 + gap })
            / if show_scope { 2.0 } else { 1.0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    }

    #[test]
    fn narrow_search_controls_do_not_overlap_at_any_width() {
        // Guard the UI connection as well as the calculation: an expanding
        // layout spacer would leave the editor full-width at the breakpoint.
        let ui = include_str!("../ui/app.slint");
        let search = ui.split_once("search := SearchField {").unwrap().1;
        let search = search.split_once("select-on-click:").unwrap().0;
        assert!(search.contains("width: search-bar.geometry.search-width;"));
        let selectors = ui.split_once("selectors := HorizontalLayout {").unwrap().1;
        assert!(selectors.contains("x: search-bar.geometry.selectors-x;"));
        assert!(selectors.contains("width: search-bar.geometry.selectors-width;"));
        let mut cases = 0;
        for compact in [false, true] {
            let field = if compact { 30.0 } else { 34.0 };
            let top = if compact { 40.0 } else { 48.0 };
            for own_dropdown in [false, true] {
                // The minimum filter width and wider ones.
                for filter in [160.0, 176.0, 800.0] {
                    for show_scope in [false, true] {
                        for show_menu in [false, true] {
                            for scale in [1.0, 2.0] {
                                for width in 560..=1100 {
                                    let g = calculate(
                                        width as f32,
                                        223.0,
                                        filter,
                                        show_scope,
                                        show_menu,
                                        16.0,
                                        8.0,
                                    );
                                    let y = (top - field) / 2.0;
                                    let sy = y + if g.stacked { field + 8.0 } else { 0.0 };
                                    let search = Rect {
                                        x: 16.0,
                                        y,
                                        w: g.search_width,
                                        h: field,
                                    };
                                    let mut controls = vec![search];
                                    let mut x = g.selectors_x;
                                    if show_scope {
                                        let w = if g.stacked { g.selector_width } else { 170.0 };
                                        controls.push(Rect {
                                            x,
                                            y: sy,
                                            w,
                                            h: field,
                                        });
                                        x += w + 8.0;
                                    }
                                    let w = if g.stacked { g.selector_width } else { filter };
                                    controls.push(Rect {
                                        x,
                                        y: sy,
                                        w,
                                        h: field,
                                    });
                                    if !show_menu {
                                        controls.push(Rect {
                                            x: x + w + 8.0,
                                            y: sy,
                                            w: 34.0,
                                            h: field,
                                        });
                                    }
                                    let context = (
                                        compact,
                                        own_dropdown,
                                        filter,
                                        show_scope,
                                        show_menu,
                                        scale,
                                        width,
                                    );
                                    assert!(
                                        (g.search_width - 222.0) * scale >= 200.0 * scale,
                                        "text viewport {context:?}: {g:?}"
                                    );
                                    for (i, a) in controls.iter().enumerate() {
                                        assert!(
                                            a.x * scale >= 16.0 * scale
                                                && a.w > 0.0
                                                && (a.x + a.w) * scale
                                                    <= (width as f32 - 16.0) * scale,
                                            "right edge {context:?}: {a:?}"
                                        );
                                        for b in &controls[i + 1..] {
                                            assert!(
                                                a.x + a.w <= b.x
                                                    || b.x + b.w <= a.x
                                                    || a.y + a.h <= b.y
                                                    || b.y + b.h <= a.y,
                                                "overlap {context:?}: {a:?} {b:?}"
                                            );
                                        }
                                    }
                                    cases += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 51_936);
        let g = calculate(816.0, 223.0, 176.0, true, true, 16.0, 8.0);
        assert!(!g.stacked);
        assert_eq!(g.search_width, 422.0);
        assert_eq!(g.selectors_x, 446.0);
        assert_eq!(g.search_width - 222.0, 200.0);
    }
}
