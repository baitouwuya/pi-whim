//! Scroll containers that keep wheel input inside nested UI surfaces.
//!
//! GPUI's built-in overflow listener updates a scroll offset but deliberately
//! leaves the event propagating. That is useful for composing scroll regions,
//! but it makes a bounded menu or tool payload move the transcript behind it as
//! well. The helpers here keep the native scroll behaviour (including keyboard
//! item positioning) and stop the event whenever the innermost container has
//! vertical overflow, including after it reaches either boundary.

use gpui::{
    Div, ElementId, InteractiveElement, ScrollHandle, Stateful, StatefulInteractiveElement, Styled,
    div, px,
};

/// A vertically scrollable viewport that consumes wheel input while it has
/// vertical overflow.
///
/// Children are intentionally added by the caller after this function returns.
/// That keeps them as direct children of the tracked element, which is required
/// by [`ScrollHandle::scroll_to_item`] for keyboard navigation in menus.
pub fn isolated_vertical_scroll_area(
    id: impl Into<ElementId>,
    scroll_handle: &ScrollHandle,
) -> Stateful<Div> {
    let wheel_scroll = scroll_handle.clone();

    div()
        .id(id)
        .relative()
        .w_full()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .on_scroll_wheel(move |event, window, cx| {
            let delta = event.delta.pixel_delta(window.line_height()).y;
            if delta != px(0.0) && wheel_scroll.max_offset().y > px(0.0) {
                // The native GPUI listener has already updated the handle by
                // this point. Only stop the bubble phase; doing the update a
                // second time would double the wheel delta.
                cx.stop_propagation();
            }
        })
}

/// A non-scrolling surface that still owns wheel input while `scroll_handle`
/// reports overflow.
///
/// This is useful when the caller needs to render an explicit scrollbar or a
/// custom content mask and therefore cannot use the native overflow style.
pub fn isolated_manual_vertical_scroll_area(
    id: impl Into<ElementId>,
    scroll_handle: &ScrollHandle,
) -> Stateful<Div> {
    let wheel_scroll = scroll_handle.clone();

    div()
        .id(id)
        .relative()
        .w_full()
        .overflow_hidden()
        .track_scroll(scroll_handle)
        .on_scroll_wheel(move |event, window, cx| {
            let delta = event.delta.pixel_delta(window.line_height()).y;
            if delta == px(0.0) || wheel_scroll.max_offset().y <= px(0.0) {
                return;
            }

            let mut offset = wheel_scroll.offset();
            offset.y += delta;
            wheel_scroll.set_offset(offset);
            window.refresh();
            cx.stop_propagation();
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, IntoElement, ParentElement, Render, ScrollDelta, ScrollWheelEvent, TestAppContext,
        VisualTestContext, Window, point,
    };

    struct NestedScrollHarness {
        inner: ScrollHandle,
        outer: ScrollHandle,
        native: bool,
        inner_items: usize,
    }

    impl Render for NestedScrollHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let inner = if self.native {
                isolated_vertical_scroll_area("nested-inner-scroll", &self.inner)
            } else {
                isolated_manual_vertical_scroll_area("nested-inner-scroll", &self.inner)
            };

            div()
                .id("outer-conversation-scroll")
                .w(px(100.0))
                .h(px(100.0))
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&self.outer)
                .child(
                    inner
                        .h(px(50.0))
                        .flex_none()
                        .children((0..self.inner_items).map(|_| div().h(px(40.0)).flex_none()))
                        .debug_selector(|| "nested-scroll-area".to_owned()),
                )
                .child(div().h(px(200.0)).flex_none())
        }
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
    }

    fn assert_nested_scroll_is_isolated(cx: &mut TestAppContext, native: bool) {
        cx.update(|cx| {
            // The helper itself has no theme dependency; initializing the
            // component library gives the test window the same font metrics as
            // the application.
            gpui_component::init(cx);
        });
        let inner = ScrollHandle::new();
        let outer = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let inner = inner.clone();
            let outer = outer.clone();
            move |_, _| NestedScrollHarness {
                inner: inner.clone(),
                outer: outer.clone(),
                native,
                inner_items: 5,
            }
        });
        let cx: &mut VisualTestContext = cx;
        draw(cx);
        assert!(inner.max_offset().y > px(0.0));
        let bounds = cx
            .debug_bounds("nested-scroll-area")
            .expect("nested viewport bounds");

        let wheel = |delta| ScrollWheelEvent {
            position: point(bounds.left() + px(25.0), bounds.top() + px(25.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(delta))),
            ..Default::default()
        };

        cx.simulate_event(wheel(-40.0));
        draw(cx);
        assert!(inner.offset().y < px(0.0));
        assert_eq!(outer.offset().y, px(0.0));

        cx.simulate_event(wheel(-10_000.0));
        draw(cx);
        let bottom = inner.offset().y;
        cx.simulate_event(wheel(-40.0));
        draw(cx);

        assert_eq!(inner.offset().y, bottom);
        assert_eq!(outer.offset().y, px(0.0));
    }

    #[gpui::test]
    fn manual_nested_scroll_does_not_move_the_outer_view(cx: &mut TestAppContext) {
        assert_nested_scroll_is_isolated(cx, false);
    }

    #[gpui::test]
    fn native_nested_scroll_does_not_move_the_outer_view(cx: &mut TestAppContext) {
        assert_nested_scroll_is_isolated(cx, true);
    }

    #[gpui::test]
    fn native_scroll_keeps_direct_child_keyboard_positioning(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let inner = ScrollHandle::new();
        let outer = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let inner = inner.clone();
            let outer = outer.clone();
            move |_, _| NestedScrollHarness {
                inner: inner.clone(),
                outer: outer.clone(),
                native: true,
                inner_items: 5,
            }
        });
        let cx: &mut VisualTestContext = cx;
        draw(cx);

        inner.scroll_to_item(4);
        draw(cx);

        assert!(inner.offset().y < px(0.0));
        assert_eq!(outer.offset().y, px(0.0));
    }

    #[gpui::test]
    fn an_inner_view_without_overflow_leaves_wheel_input_to_the_outer_view(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let inner = ScrollHandle::new();
        let outer = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let inner = inner.clone();
            let outer = outer.clone();
            move |_, _| NestedScrollHarness {
                inner: inner.clone(),
                outer: outer.clone(),
                native: true,
                inner_items: 1,
            }
        });
        let cx: &mut VisualTestContext = cx;
        draw(cx);
        assert_eq!(inner.max_offset().y, px(0.0));
        let bounds = cx
            .debug_bounds("nested-scroll-area")
            .expect("nested viewport bounds");

        cx.simulate_event(ScrollWheelEvent {
            position: point(bounds.left() + px(25.0), bounds.top() + px(25.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-40.0))),
            ..Default::default()
        });
        draw(cx);

        assert_eq!(inner.offset().y, px(0.0));
        assert!(outer.offset().y < px(0.0));
    }
}
