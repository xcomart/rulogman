//! The custom element: the gutter, the text, and the quads under and over it.
//!
//! # Why an element rather than `uniform_list`
//!
//! gpui's [`uniform_list`](gpui::uniform_list) virtualises a list by building
//! only the rows the viewport can reach, which is what the file panel's tree
//! uses. It is the wrong tool here for one reason: a caret. A caret is not a
//! row, a selection is not a row, the composing underline is not a row, and
//! every one of them has to be positioned against the *shaped* text — which
//! means the code that shapes a line and the code that places a quad on it have
//! to be the same code, holding the same [`ShapedLine`]. An element gets that; a
//! list of independently rendered rows does not.
//!
//! The virtualisation is the same trick nonetheless, and it is the load-bearing
//! one: [`EditorElement::prepaint`] works out which lines the viewport covers
//! from the scroll offset and the line height, and shapes **those lines and no
//! others**. A hundred thousand lines cost what forty cost, and
//! `EditorView::shaped_lines` is what the tests read to hold that down.
//!
//! # Where the metrics come from
//!
//! Not from the window's text style, which is the application's UI font, but
//! from the view: [`EditorView::font`], [`EditorView::font_size`] and
//! [`EditorView::line_height`], pushed in by the host from the session's
//! terminal settings. [`EditorElement::prepaint`] reads them once and derives
//! everything from that one triple — the gutter, the row pitch, the shaping,
//! and every quad placed against a shaped line — because a caret measured
//! against one font and drawn beside another is the bug this avoids having.
//!
//! # Painting order
//!
//! Back to front, because each layer is drawn over the one before it:
//!
//! 1. the caret's line;
//! 2. the find matches, the current one brighter;
//! 3. the selection;
//! 4. the text, with the composing run underlined;
//! 5. the caret;
//! 6. the line numbers, in the gutter.
//!
//! Layers 1 to 5 are drawn under a content mask that stops at the gutter's inner
//! edge. Horizontally scrolled text — and a selection over it — runs left past
//! that edge, and the mask is what keeps it out of the gutter. Clipping rather
//! than covering: a mask says only *where the text may be seen*, and so assumes
//! nothing about what fills the gutter behind the numbers or what colour it is.
//! Covering would have to be right about both, and would have to be redrawn
//! whenever either changed.

use std::ops::Range;

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, Font,
    GlobalElementId, Hsla, InspectorElementId, LayoutId, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextAlign, TextRun, UnderlineStyle, Window, fill, point, prelude::*, px,
    relative, size,
};

use crate::editor::EditorPalette;
use crate::editor::syntax::TokenKind;
use crate::editor::view::EditorView;

/// Space between the line numbers and the text.
const GUTTER_PADDING: f32 = 12.;

/// Space to the left of the line numbers.
const GUTTER_LEAD: f32 = 8.;

/// Width of the caret.
const CARET_WIDTH: f32 = 2.;

/// How far past the viewport lines are shaped, so that a partially visible row
/// at either edge is drawn rather than clipped away.
const OVERSCAN: usize = 1;

/// The element that draws one [`EditorView`].
pub struct EditorElement {
    /// The view it draws.
    editor: Entity<EditorView>,
}

impl EditorElement {
    /// An element over `editor`.
    pub const fn new(editor: Entity<EditorView>) -> Self {
        Self { editor }
    }
}

/// Everything [`EditorElement::prepaint`] hands over to `paint`.
pub struct PrepaintState {
    /// The shaped visible lines, as `(line index, shaped line)`.
    lines: Vec<(usize, ShapedLine)>,
    /// The line numbers, shaped, in the same order.
    numbers: Vec<ShapedLine>,
    /// Quads painted under the text.
    below: Vec<PaintQuad>,
    /// The caret, when it is visible.
    caret: Option<PaintQuad>,
    /// Width of the gutter.
    gutter: Pixels,
    /// Height of one line.
    line_height: Pixels,
    /// The scroll offset the frame was built at.
    scroll: Point<Pixels>,
    /// The first line drawn.
    first_line: usize,
    /// The widest shaped line, for the horizontal scroll extent.
    content_width: Pixels,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let editor = self.editor.read(cx);
        let palette = *editor.palette();
        // The one place the three of these are read. Everything below — the
        // gutter width, the row pitch, the shaping of every line, the caret's x
        // and the quads that sit under a selection — is derived from these and
        // never from the window's text style, so that a glyph and the mark
        // placed against it can never have been measured differently.
        let font = editor.font().clone();
        let font_size = editor.font_size();
        let line_height = editor.line_height();
        let buffer = editor.buffer();

        // The gutter is as wide as the largest line number, so it does not
        // twitch as the view scrolls past a power of ten.
        let digits = digit_count(buffer.line_count());
        let digit_width = window
            .text_system()
            .shape_line(
                SharedString::from("0".repeat(digits)),
                font_size,
                &[plain_run(digits, palette.gutter, &font)],
                None,
            )
            .width;
        let gutter = digit_width + px(GUTTER_PADDING + GUTTER_LEAD);

        let scroll = editor.scroll_offset();
        // *The* virtualisation. Everything below shapes exactly these lines.
        let first_line = ((f32::from(scroll.y) / f32::from(line_height)) as usize)
            .saturating_sub(OVERSCAN)
            .min(buffer.line_count().saturating_sub(1));
        let rows = (f32::from(bounds.size.height) / f32::from(line_height)).ceil() as usize;
        let last_line = (first_line + rows + 2 * OVERSCAN).min(buffer.line_count() - 1);

        let text_left = bounds.left() + gutter - scroll.x;
        let top_of = |line: usize| {
            bounds.top()
                + line_height * ((line as f32) - f32::from(scroll.y) / f32::from(line_height))
        };

        let selection = editor.selection();
        let caret_offset = editor.caret();
        let caret_line = buffer.line_of(caret_offset);
        let current_match = editor.current_match();

        let mut lines = Vec::with_capacity(last_line - first_line + 1);
        let mut numbers = Vec::with_capacity(last_line - first_line + 1);
        let mut below = Vec::new();
        let mut caret = None;
        let mut content_width = px(0.);

        for line in first_line..=last_line {
            let start = buffer.line_start(line);
            let text = buffer.line_text(line).into_owned();
            let end = start + text.len();
            let top = top_of(line);
            let row = Bounds::from_corners(
                point(bounds.left() + gutter, top),
                point(bounds.right(), top + line_height),
            );

            // 1. the caret's own line.
            if line == caret_line && selection.is_empty() {
                below.push(fill(row, palette.line_highlight));
            }

            let runs = runs_for(editor, line, &text, start, &palette, &font);
            let shaped = window.text_system().shape_line(
                SharedString::from(text.clone()),
                font_size,
                &runs,
                None,
            );
            content_width = content_width.max(shaped.width);

            let x_at = |offset: usize| {
                text_left + shaped.x_for_index(offset.saturating_sub(start).min(shaped.len()))
            };

            // 2. find matches.
            for found in editor.find_matches() {
                if found.end < start || found.start > end {
                    continue;
                }
                let color = if current_match.as_ref() == Some(found) {
                    palette.find_current
                } else {
                    palette.find_match
                };
                below.push(fill(
                    span_bounds(&found.clone(), start, end, x_at, top, line_height),
                    color,
                ));
            }

            // 3. the selection.
            if !selection.is_empty() && selection.end >= start && selection.start <= end {
                let mut quad = span_bounds(&selection, start, end, x_at, top, line_height);
                // A selection that runs past the end of this line covers the
                // line break too, so a multi-line selection reads as a block.
                if selection.end > end {
                    quad.size.width = bounds.right() - quad.origin.x;
                }
                below.push(fill(quad, palette.selection));
            }

            // 5. the caret, once the line it is on has been shaped.
            if line == caret_line && selection.is_empty() {
                caret = Some(fill(
                    Bounds::new(
                        point(x_at(caret_offset), top),
                        size(px(CARET_WIDTH), line_height),
                    ),
                    palette.cursor,
                ));
            }

            // 6. the line number.
            let number = format!("{}", line + 1);
            let color = if line == caret_line {
                palette.gutter_active
            } else {
                palette.gutter
            };
            numbers.push(window.text_system().shape_line(
                SharedString::from(number.clone()),
                font_size,
                &[plain_run(number.len(), color, &font)],
                None,
            ));

            lines.push((line, shaped));
        }

        PrepaintState {
            lines,
            numbers,
            below,
            caret,
            gutter,
            line_height,
            scroll,
            first_line,
            content_width,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).input_focus();
        let read_only = self.editor.read(cx).is_read_only();
        let focused = self.editor.read(cx).is_focused(window);

        // Even a read-only editor takes the handler: without it the platform
        // has no way to report the selection, and copy stops working.
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        let line_height = prepaint.line_height;
        let scroll = prepaint.scroll;
        let gutter = prepaint.gutter;

        // The body minus the gutter. Everything that belongs to the text is
        // clipped to it, because a horizontally scrolled line — and a selection
        // over it — extends to the left of it, and the gutter is not where it may
        // be seen. gpui intersects a nested mask with the one it sits inside, so
        // the body mask around it still holds the other three edges.
        let text_area = Bounds::from_corners(
            point(bounds.left() + gutter, bounds.top()),
            bounds.bottom_right(),
        );

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.with_content_mask(Some(ContentMask { bounds: text_area }), |window| {
                for quad in prepaint.below.drain(..) {
                    window.paint_quad(quad);
                }

                let text_left = bounds.left() + gutter - scroll.x;
                for (line, shaped) in &prepaint.lines {
                    let top = bounds.top()
                        + line_height
                            * ((*line as f32) - f32::from(scroll.y) / f32::from(line_height));
                    shaped
                        .paint(
                            point(text_left, top),
                            line_height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        )
                        .ok();
                }

                if focused
                    && !read_only
                    && let Some(caret) = prepaint.caret.take()
                {
                    window.paint_quad(caret);
                }
            });

            // The line numbers are the only thing drawn in the gutter, and so the
            // only thing outside the mask above. Nothing here fills the gutter
            // behind them — the editor's own surface has already done that — which
            // is the whole economy of clipping instead of covering.
            for (index, number) in prepaint.numbers.iter().enumerate() {
                let line = prepaint.first_line + index;
                let top = bounds.top()
                    + line_height * ((line as f32) - f32::from(scroll.y) / f32::from(line_height));
                let left = bounds.left() + gutter - px(GUTTER_PADDING) - number.width;
                number
                    .paint(
                        point(left, top),
                        line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
            }
        });

        let lines = std::mem::take(&mut prepaint.lines);
        let content_width = prepaint.content_width;
        self.editor.update(cx, |editor, _cx| {
            editor.layout.bounds = Some(bounds);
            editor.layout.gutter = gutter;
            editor.layout.line_height = line_height;
            editor.layout.content_width = editor.layout.content_width.max(content_width);
            editor.layout.lines = lines;
        });
    }
}

/// The quad covering the part of `span` that falls on one line.
fn span_bounds(
    span: &Range<usize>,
    line_start: usize,
    line_end: usize,
    x_at: impl Fn(usize) -> Pixels,
    top: Pixels,
    line_height: Pixels,
) -> Bounds<Pixels> {
    let from = span.start.max(line_start);
    let to = span.end.min(line_end);
    Bounds::from_corners(
        point(x_at(from), top),
        point(x_at(to.max(from)), top + line_height),
    )
}

/// A run of `len` bytes in one color, in the editor's font.
fn plain_run(len: usize, color: Hsla, font: &Font) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// The coloured runs of one line: the lexer's tokens, with the composing
/// underline laid over them.
///
/// The virtualisation holds through here. Only the visible lines are lexed —
/// [`Highlighter::tokens`](crate::editor::highlight::Highlighter::tokens) works
/// from the cached state of the line *before*, so reaching row fifty thousand
/// is a lookup and not fifty thousand lines of scanning — and only the visible
/// lines are shaped.
fn runs_for(
    editor: &EditorView,
    line: usize,
    text: &str,
    start: usize,
    palette: &EditorPalette,
    font: &Font,
) -> Vec<TextRun> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut runs: Vec<TextRun> = editor
        .highlighter()
        .tokens(editor.buffer(), line)
        .into_iter()
        .map(|token| {
            plain_run(
                token.end - token.start,
                color_for(token.kind, palette),
                font,
            )
        })
        .collect();
    // A line the lexer produced no tokens for cannot happen -- the tokens tile
    // the line, and the empty case returned above -- but a defensive fallback
    // costs one branch and saves a mis-shaped line if that guarantee ever moves.
    if runs.iter().map(|run| run.len).sum::<usize>() != text.len() {
        runs = vec![plain_run(text.len(), palette.foreground, font)];
    }

    let Some(marked) = editor.marked() else {
        return runs;
    };
    let end = start + text.len();
    if marked.end < start || marked.start > end {
        return runs;
    }

    // Split the runs at the composition's edges and underline what is between
    // them. The underline is the only signal that a syllable is still being
    // composed, and it has to survive whatever color the lexer gave the run.
    let from = marked.start.max(start) - start;
    let to = marked.end.min(end) - start;
    let mut split = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let run_end = at + run.len;
        for (piece_start, piece_end) in [
            (at, run_end.min(from)),
            (at.max(from), run_end.min(to)),
            (at.max(to), run_end),
        ] {
            if piece_end <= piece_start {
                continue;
            }
            let underlined = piece_start >= from && piece_end <= to;
            split.push(TextRun {
                len: piece_end - piece_start,
                underline: underlined.then(|| UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..run.clone()
            });
        }
        at = run_end;
    }
    split
}

/// The palette slot a token kind draws in.
///
/// `Literal` shares the number's slot: `true` and `null` are constants in every
/// format here, exactly as `1` is, and giving them a colour of their own would
/// claim a distinction none of these formats makes. See
/// [`palette_for`](crate::editor::palette_for) for the rest of the mapping.
const fn color_for(kind: TokenKind, palette: &EditorPalette) -> Hsla {
    match kind {
        TokenKind::Plain => palette.foreground,
        TokenKind::Comment => palette.comment,
        TokenKind::String => palette.string,
        TokenKind::Number | TokenKind::Literal => palette.number,
        TokenKind::Keyword => palette.keyword,
        TokenKind::Key => palette.key,
        TokenKind::Variable => palette.variable,
    }
}

/// How many decimal digits `n` needs, at least two.
const fn digit_count(n: usize) -> usize {
    let mut digits = 1;
    let mut left = n;
    while left >= 10 {
        left /= 10;
        digits += 1;
    }
    if digits < 2 { 2 } else { digits }
}
