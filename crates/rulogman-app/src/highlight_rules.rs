//! The editor for a list of highlight rules, shared by both dialogs.
//!
//! A rule list is edited in two places and has to be the same thing in both:
//! the settings dialog edits the global list every followed file starts from,
//! and a row of the connection dialog's "Tail files" section edits the override
//! one file keeps for itself. They differ only in what the list *means* — which
//! is the host's business — so what a rule is on screen, and what happens to
//! the text typed into it, lives here once.
//!
//! The component is an entity rather than a function returning elements. Two
//! reasons, and both are about the per-file case, where a dialog can hold a
//! dozen of these at once:
//!
//! * a rendered [`Entity`] is its own element-id scope in gpui, so every
//!   instance can name its controls `("highlight-scope", 0)` without the
//!   twelfth list colliding with the first;
//! * the rows own [`TextInput`] entities, which have to survive between paints
//!   — a list rebuilt on every render would drop the caret and the selection of
//!   whichever field was being typed into.
//!
//! What is *not* here is the meaning of an empty list, which is the one thing
//! the two hosts genuinely disagree about: globally it is "highlight nothing",
//! per file it is "highlight nothing **for this file**", and neither of those
//! is a fact about a text field. The hosts read [`HighlightRuleList::fields`]
//! and decide for themselves; the component emits nothing at all.
//!
//! Compiling a pattern happens in [`collect_highlight_rules`], not in
//! `rulogman-core`: core persists the text and deliberately keeps the `regex`
//! crate out of its dependency list, so the app layer is where a half-typed
//! regex is caught and shown to the user instead of being written to disk.

use gpui::{
    App, Context, ElementId, Entity, IntoElement, Render, SharedString, Window, div, prelude::*, px,
};
use regex::Regex;
use rugpui::{Button, ButtonVariant, Checkbox, Segmented, TextInput, theme};
use rulogman_core::{HighlightColor, HighlightRule, HighlightScope};

use crate::i18n::{input_menu_labels, ts};

/// Indices one rule row occupies, in the order the row is drawn.
///
/// Eight controls, and every one of them is a tab stop: the pattern, the scope
/// picker and the remove button on the first line, then the two colours and the
/// three flags on the second.
const RULE_STRIDE: isize = 8;

/// Indices the whole list occupies, counted from the base it was built with.
///
/// A host reserves this much for a list and puts whatever follows it above the
/// top: the settings dialog's "Reset to preset" button, the next followed-file
/// row. Wide enough for forty-nine rules before the clamping starts, which is
/// well past the point where a list is still worth reading.
pub const TAB_SPAN: isize = 400;

/// Offset of the "Add rule" button, past every row the numbering can reach.
const ADD: isize = TAB_SPAN - 1;

/// Offset of the pattern field within a row.
const PATTERN: isize = 0;
/// Offset of the scope picker within a row.
const SCOPE: isize = 1;
/// Offset of the remove button within a row.
const REMOVE: isize = 2;
/// Offset of the text-colour field within a row.
const FOREGROUND: isize = 3;
/// Offset of the background-colour field within a row.
const BACKGROUND: isize = 4;
/// Offset of the bold tick within a row.
const BOLD: isize = 5;
/// Offset of the "Ignore case" tick within a row.
const IGNORE_CASE: isize = 6;
/// Offset of the "On" tick within a row.
const ENABLED: isize = 7;

/// Width of the scope picker at the end of a rule's first line.
const SCOPE_WIDTH: f32 = 128.;

/// Width of each of the two colour fields.
const COLOUR_WIDTH: f32 = 116.;

/// Width of the action column, matching the repeatable sections of both hosts.
const ACTION_WIDTH: f32 = 72.;

/// Placeholder of the pattern field.
///
/// A sample *value*, like the host names and paths the two dialogs hint their
/// own fields with: it reads the same in every language and is never
/// translated. It is also a rule worth having, so a user who types over it has
/// seen what one looks like.
const PATTERN_HINT: &str = r"\b(error|fail)\b";

/// Placeholder of the text-colour field: a scheme slot name.
const FOREGROUND_HINT: &str = "bright_red";

/// Placeholder of the background-colour field: the other spelling a colour has.
///
/// Deliberately the hex form while the foreground hint is a slot name, so the
/// pair shows both vocabularies without the hint text having to spell either
/// out twice.
const BACKGROUND_HINT: &str = "#7f1d1d";

/// The two segments of the scope picker, in [`HighlightScope`] order.
///
/// Built per call rather than declared as a `const`, because the labels come
/// out of the active locale.
fn scope_options() -> [(&'static str, SharedString); 2] {
    [
        ("match", ts!("settings.highlights.scope_match")),
        ("line", ts!("settings.highlights.scope_line")),
    ]
}

/// Position of a scope in the picker.
fn scope_index(scope: HighlightScope) -> usize {
    match scope {
        HighlightScope::Match => 0,
        HighlightScope::Line => 1,
    }
}

/// The scope the segment at `index` stands for.
fn scope_at(index: usize) -> HighlightScope {
    if index == 1 {
        HighlightScope::Line
    } else {
        HighlightScope::Match
    }
}

/// One editable rule: three text fields and four switches.
struct RuleRow {
    /// Regular expression source, as typed.
    pattern: Entity<TextInput>,
    /// Text colour, as typed; blank means "leave the foreground alone".
    foreground: Entity<TextInput>,
    /// Background colour, as typed; blank means the same for the background.
    background: Entity<TextInput>,
    /// How much of the line a match recolours.
    ///
    /// A plain value rather than an [`Entity`], for the reason the dialogs'
    /// own checkbox state is one: a segmented control holds no buffer, so the
    /// list owns the answer and hands it back on every paint.
    scope: HighlightScope,
    /// Whether the highlighted span is drawn bold.
    bold: bool,
    /// Whether the pattern matches without regard to case.
    ignore_case: bool,
    /// Whether the rule is applied at all.
    enabled: bool,
    /// First tab index of this row, fixed when the row was built.
    ///
    /// Held rather than derived from the row's position, because the three text
    /// fields took their indices at construction and cannot be renumbered:
    /// deriving the switches and baking the fields would put a row's controls
    /// out of order the moment a row above it was removed.
    tab_base: isize,
}

/// The content of one rule row, read out of its controls.
///
/// Plain strings and flags, so that [`collect_highlight_rules`] — which is
/// where every rule about what a rule may say lives — can be exercised without
/// a window, exactly as the connection dialog's own field structs are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightRuleFields {
    /// Pattern as typed, verbatim: leading and trailing spaces can be
    /// significant in a regex, so nothing trims this.
    pub pattern: String,
    /// Text colour as typed; blank is "no colour".
    pub foreground: String,
    /// Background colour as typed; blank is "no colour".
    pub background: String,
    /// Whether the span is drawn bold.
    pub bold: bool,
    /// How much of the line the colours cover.
    pub scope: HighlightScope,
    /// Whether the pattern matches without regard to case.
    pub ignore_case: bool,
    /// Whether the rule is applied at all.
    pub enabled: bool,
}

impl Default for HighlightRuleFields {
    /// A blank row as the "Add rule" button produces one.
    ///
    /// Written out rather than derived because two of the flags default to
    /// *on*: a rule that ignores case and is enabled is what a person adding
    /// one means, and `#[derive(Default)]` would hand back the opposite of
    /// both — see [`HighlightRule`]'s own serde defaults, which agree.
    fn default() -> Self {
        Self {
            pattern: String::new(),
            foreground: String::new(),
            background: String::new(),
            bold: false,
            scope: HighlightScope::default(),
            ignore_case: true,
            enabled: true,
        }
    }
}

/// Why a rule list could not be turned into rules.
///
/// Carries the offending text rather than the row number: the message strip
/// quotes what the user typed, which is what they have to find and fix, and a
/// row number is not visible anywhere on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighlightProblem {
    /// A pattern the `regex` crate refuses to compile.
    BadPattern(String),
    /// A colour [`HighlightColor::parse`] does not recognise.
    BadColour(String),
}

/// Read one colour field: `None` when blank, an error when unrecognised.
///
/// Trimmed on the way through, so `" red "` stores as `"red"` — the same repair
/// [`rulogman_core::settings`] makes to a hand-edited file, done here so that
/// what is written matches what the dialog would accept back.
fn colour(text: &str) -> Result<Option<String>, HighlightProblem> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if HighlightColor::parse(trimmed).is_none() {
        return Err(HighlightProblem::BadColour(trimmed.to_owned()));
    }
    Ok(Some(trimmed.to_owned()))
}

/// Turn the rows of a rule list into rules, or say which one cannot be.
///
/// A row whose pattern is entirely blank is **dropped** rather than refused: it
/// is the empty row "Add rule" produced, and an unfinished list is not a
/// mistake. Everything else about such a row is dropped with it, colours
/// included — there is no rule for them to belong to.
///
/// The two refusals are the ones the user cannot discover any other way. A
/// pattern that does not compile would be stored happily by `rulogman-core` and
/// would then simply never match, which looks exactly like a rule that is
/// wrong about the log rather than about itself; and an unrecognised colour
/// would draw in the default colour, which looks exactly like a rule that did
/// not fire. Both are silent failures at the far end of a `tail -f`, so both
/// are caught here, while the text is still on screen next to the caret that
/// typed it.
///
/// The pattern is compiled the way the renderer will compile it — with the
/// `(?i)` prefix when the row ignores case — so that a pattern accepted here
/// cannot be rejected there. The compiled form is thrown away: what is
/// persisted is the source text, and the pane that follows the file owns the
/// cache of compiled rules.
pub fn collect_highlight_rules(
    rows: &[HighlightRuleFields],
) -> Result<Vec<HighlightRule>, HighlightProblem> {
    let mut rules = Vec::with_capacity(rows.len());
    for row in rows {
        if row.pattern.trim().is_empty() {
            continue;
        }
        let source = if row.ignore_case {
            format!("(?i){}", row.pattern)
        } else {
            row.pattern.clone()
        };
        if Regex::new(&source).is_err() {
            return Err(HighlightProblem::BadPattern(row.pattern.clone()));
        }
        rules.push(HighlightRule {
            pattern: row.pattern.clone(),
            foreground: colour(&row.foreground)?,
            background: colour(&row.background)?,
            bold: row.bold,
            scope: row.scope,
            ignore_case: row.ignore_case,
            enabled: row.enabled,
        });
    }
    Ok(rules)
}

/// An editable list of highlight rules.
///
/// Create it with [`cx.new`](gpui::App::new), give it the first tab index of
/// the block it may number inside, fill it with [`set_rules`](Self::set_rules),
/// render it as a child, and read it back with [`fields`](Self::fields) when
/// the host saves.
pub struct HighlightRuleList {
    /// The rules being edited, in the order they are drawn and applied.
    rows: Vec<RuleRow>,
    /// First tab index of the block this list numbers inside.
    tab_base: isize,
}

impl HighlightRuleList {
    /// Build an empty list numbering from `tab_base`.
    ///
    /// The list occupies `tab_base..tab_base + `[`TAB_SPAN`]; whatever the host
    /// draws after it belongs above that. Takes a context it does not currently
    /// need, so that it is created the way every other entity in the
    /// application is and can grow a control of its own without an API break.
    pub fn new(_cx: &mut Context<Self>, tab_base: isize) -> Self {
        Self {
            rows: Vec::new(),
            tab_base,
        }
    }

    /// First tab index of the row at `position`.
    ///
    /// Clamped to the last row the block can number, exactly as the repeatable
    /// sections of both dialogs clamp theirs: a list longer than the numbering
    /// allows must not push a row past the "Add rule" button and out of the tab
    /// ring's order.
    fn row_tab_base(&self, position: usize) -> isize {
        (self.tab_base + position as isize * RULE_STRIDE).min(self.tab_base + ADD - RULE_STRIDE)
    }

    /// Build one text field of a rule row.
    fn field(cx: &mut App, placeholder: SharedString, tab_index: isize) -> Entity<TextInput> {
        cx.new(|cx| {
            TextInput::new(cx)
                .context_menu(input_menu_labels)
                .placeholder(placeholder)
                .tab_index(tab_index)
        })
    }

    /// Build an empty rule row numbered for `position` in the list.
    fn row(&self, cx: &mut App, position: usize) -> RuleRow {
        let base = self.row_tab_base(position);
        RuleRow {
            pattern: Self::field(cx, PATTERN_HINT.into(), base + PATTERN),
            foreground: Self::field(cx, FOREGROUND_HINT.into(), base + FOREGROUND),
            background: Self::field(cx, BACKGROUND_HINT.into(), base + BACKGROUND),
            scope: HighlightScope::default(),
            bold: false,
            // Both on, for the reason `HighlightRuleFields::default` gives.
            ignore_case: true,
            enabled: true,
            tab_base: base,
        }
    }

    /// Replace every row with one per rule of `rules`.
    ///
    /// Rebuilt from scratch rather than reconciled, which is what keeps an edit
    /// the user walked away from — a cancelled dialog, a per-file override that
    /// was unticked — out of the next list drawn here.
    pub fn set_rules(&mut self, rules: &[HighlightRule], cx: &mut Context<Self>) {
        self.clear(cx);
        self.rows.reserve(rules.len());
        for (position, rule) in rules.iter().enumerate() {
            let mut row = self.row(cx, position);
            row.pattern
                .update(cx, |input, cx| input.set_content(rule.pattern.clone(), cx));
            for (input, colour) in [
                (&row.foreground, &rule.foreground),
                (&row.background, &rule.background),
            ] {
                input.update(cx, |input, cx| {
                    input.set_content(colour.clone().unwrap_or_default(), cx);
                });
            }
            row.scope = rule.scope;
            row.bold = rule.bold;
            row.ignore_case = rule.ignore_case;
            row.enabled = rule.enabled;
            self.rows.push(row);
        }
        cx.notify();
    }

    /// Append an empty rule row.
    pub fn add_row(&mut self, cx: &mut Context<Self>) {
        let row = self.row(cx, self.rows.len());
        self.rows.push(row);
        cx.notify();
    }

    /// Drop the rule row at `index`.
    pub fn remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.rows.len() {
            return;
        }
        self.rows.remove(index);
        cx.notify();
    }

    /// Drop every row, leaving a list that says "highlight nothing".
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        self.rows.clear();
        cx.notify();
    }

    /// The content of every row, in order.
    pub fn fields(&self, cx: &App) -> Vec<HighlightRuleFields> {
        self.rows
            .iter()
            .map(|row| HighlightRuleFields {
                // Verbatim, not trimmed: a regex may mean its spaces.
                pattern: row.pattern.read(cx).content().to_owned(),
                foreground: row.foreground.read(cx).content().to_owned(),
                background: row.background.read(cx).content().to_owned(),
                bold: row.bold,
                scope: row.scope,
                ignore_case: row.ignore_case,
                enabled: row.enabled,
            })
            .collect()
    }

    /// Switch one row's scope.
    fn set_scope(&mut self, index: usize, scope: HighlightScope, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.scope = scope;
        cx.notify();
    }

    /// Toggle one of a row's three flags.
    fn set_flag(&mut self, index: usize, offset: isize, on: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        match offset {
            BOLD => row.bold = on,
            IGNORE_CASE => row.ignore_case = on,
            _ => row.enabled = on,
        }
        cx.notify();
    }
}

impl Render for HighlightRuleList {
    /// Two lines per rule, like a jump host, and for the same reason: seven
    /// controls in one row would either be unreadably narrow or scroll
    /// sideways. The first line is what the rule *matches*, the second is what
    /// it *does* — so a list can be skimmed down its left edge for patterns
    /// without the colours getting in the way.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chrome = theme(cx);
        let this = cx.entity();
        let label = |words: SharedString| {
            div()
                .flex_none()
                .text_size(px(11.))
                .text_color(chrome.text_muted)
                .child(words)
        };

        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let base = row.tab_base;

                let scope = Segmented::new(("highlight-scope", index))
                    .options(scope_options())
                    .selected(scope_index(row.scope))
                    .tab_index(base + SCOPE)
                    .on_select({
                        let this = this.clone();
                        move |picked, _window, cx| {
                            this.update(cx, |list, cx| {
                                list.set_scope(index, scope_at(picked), cx);
                            });
                        }
                    });

                let remove = Button::new(
                    ElementId::from(("highlight-remove", index)),
                    ts!("settings.highlights.remove"),
                )
                .variant(ButtonVariant::Ghost)
                .compact()
                .tab_index(base + REMOVE)
                .on_click({
                    let this = this.clone();
                    move |_, _window, cx| {
                        this.update(cx, |list, cx| list.remove_row(index, cx));
                    }
                });

                let flag = |id: &'static str, offset: isize, words: SharedString, on: bool| {
                    Checkbox::new(ElementId::from((id, index)), words)
                        .checked(on)
                        .tab_index(base + offset)
                        .on_toggle({
                            let this = this.clone();
                            move |checked, _window, cx| {
                                this.update(cx, |list, cx| {
                                    list.set_flag(index, offset, checked, cx);
                                });
                            }
                        })
                };

                let first = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(label(ts!("settings.highlights.pattern")))
                    .child(div().flex_1().min_w_0().child(row.pattern.clone()))
                    .child(div().flex_none().w(px(SCOPE_WIDTH)).child(scope))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .w(px(ACTION_WIDTH))
                            .justify_end()
                            .child(remove),
                    );

                let second = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(label(ts!("settings.highlights.foreground")))
                    .child(
                        div()
                            .flex_none()
                            .w(px(COLOUR_WIDTH))
                            .child(row.foreground.clone()),
                    )
                    .child(label(ts!("settings.highlights.background")))
                    .child(
                        div()
                            .flex_none()
                            .w(px(COLOUR_WIDTH))
                            .child(row.background.clone()),
                    )
                    .child(flag(
                        "highlight-bold",
                        BOLD,
                        ts!("settings.highlights.bold"),
                        row.bold,
                    ))
                    .child(flag(
                        "highlight-ignore-case",
                        IGNORE_CASE,
                        ts!("settings.highlights.ignore_case"),
                        row.ignore_case,
                    ))
                    .child(flag(
                        "highlight-enabled",
                        ENABLED,
                        ts!("settings.highlights.enabled"),
                        row.enabled,
                    ))
                    // Keeps the second line clear of the remove action's
                    // column, so the two lines of a rule end on the same edge.
                    .child(div().flex_none().w(px(ACTION_WIDTH)));

                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(first)
                    .child(second)
            })
            .collect::<Vec<_>>();

        let add = Button::new("highlight-add", ts!("settings.highlights.add"))
            .variant(ButtonVariant::Secondary)
            .tab_index(self.tab_base + ADD)
            .on_click({
                let this = this.clone();
                move |_, _window, cx| {
                    this.update(cx, |list, cx| list.add_row(cx));
                }
            });

        div()
            .flex()
            .flex_col()
            // Wider than the gap inside a rule, so that the space between two
            // rules reads as larger than the space between a rule's own lines.
            .gap(px(10.))
            .children(rows)
            .child(div().flex().flex_row().pt(px(2.)).child(add))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finished row: a pattern and whatever else the caller cares about.
    fn typed(pattern: &str) -> HighlightRuleFields {
        HighlightRuleFields {
            pattern: pattern.to_owned(),
            ..HighlightRuleFields::default()
        }
    }

    #[test]
    fn a_fresh_row_ignores_case_and_is_enabled() {
        // The two flags a rule is useless without, and the two whose `false`
        // a derived Default would have handed back.
        let row = HighlightRuleFields::default();
        assert!(row.ignore_case);
        assert!(row.enabled);
        assert_eq!(row.scope, HighlightScope::Match);
    }

    #[test]
    fn rules_keep_their_order_and_drop_the_blank_rows() {
        let rows = [
            typed(r"\bfatal\b"),
            HighlightRuleFields::default(),
            typed(r"\bwarn\b"),
            // Whitespace only is still an empty row, not a pattern.
            typed("   "),
        ];
        let rules = collect_highlight_rules(&rows).expect("every row is usable");
        let patterns: Vec<&str> = rules.iter().map(|rule| rule.pattern.as_str()).collect();
        assert_eq!(patterns, [r"\bfatal\b", r"\bwarn\b"]);
    }

    #[test]
    fn a_blank_row_carrying_colours_is_dropped_with_them() {
        // Nothing on an empty row can be a mistake: there is no rule for the
        // colour to belong to, so a junk one must not refuse the form.
        let row = HighlightRuleFields {
            foreground: "not-a-colour".to_owned(),
            ..HighlightRuleFields::default()
        };
        assert_eq!(collect_highlight_rules(&[row]), Ok(Vec::new()));
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_refused() {
        let rows = [typed("(unclosed")];
        assert_eq!(
            collect_highlight_rules(&rows),
            Err(HighlightProblem::BadPattern("(unclosed".to_owned()))
        );
    }

    #[test]
    fn a_pattern_is_compiled_the_way_the_renderer_will_compile_it() {
        // The `(?i)` prefix goes on before the check, so a pattern that already
        // carries flags of its own cannot pass here and fail there.
        let mut row = typed("(?i)boom");
        row.ignore_case = true;
        assert!(collect_highlight_rules(&[row]).is_ok());

        // And a pattern whose *only* fault is the doubled prefix is not
        // invented: a case-sensitive row compiles the source verbatim.
        let mut sensitive = typed(r"\bWARN\b");
        sensitive.ignore_case = false;
        assert!(collect_highlight_rules(&[sensitive]).is_ok());
    }

    #[test]
    fn a_colour_nothing_can_parse_is_refused() {
        let mut row = typed("boom");
        row.background = "reddish".to_owned();
        assert_eq!(
            collect_highlight_rules(&[row]),
            Err(HighlightProblem::BadColour("reddish".to_owned()))
        );

        let mut short = typed("boom");
        short.foreground = "#12".to_owned();
        assert_eq!(
            collect_highlight_rules(&[short]),
            Err(HighlightProblem::BadColour("#12".to_owned()))
        );
    }

    #[test]
    fn a_blank_colour_is_an_absence_rather_than_an_empty_name() {
        let mut row = typed("boom");
        row.foreground = "   ".to_owned();
        let rules = collect_highlight_rules(&[row]).expect("blank is not a colour");
        assert_eq!(rules[0].foreground, None);
        assert_eq!(rules[0].background, None);
    }

    #[test]
    fn a_colour_is_stored_trimmed() {
        let mut row = typed("boom");
        row.foreground = "  bright_red\t".to_owned();
        let rules = collect_highlight_rules(&[row]).expect("the colour parses");
        assert_eq!(rules[0].foreground.as_deref(), Some("bright_red"));
    }

    #[test]
    fn every_field_of_a_row_reaches_the_rule_it_becomes() {
        let row = HighlightRuleFields {
            // Kept verbatim: the spaces are part of the pattern.
            pattern: " OOM ".to_owned(),
            foreground: "bright_white".to_owned(),
            background: "#7f1d1d".to_owned(),
            bold: true,
            scope: HighlightScope::Line,
            ignore_case: false,
            enabled: false,
        };
        let rules = collect_highlight_rules(&[row]).expect("the row is usable");
        assert_eq!(
            rules,
            vec![HighlightRule {
                pattern: " OOM ".to_owned(),
                foreground: Some("bright_white".to_owned()),
                background: Some("#7f1d1d".to_owned()),
                bold: true,
                scope: HighlightScope::Line,
                ignore_case: false,
                enabled: false,
            }]
        );
    }

    #[test]
    fn the_scope_picker_and_the_scope_it_stands_for_agree() {
        for scope in [HighlightScope::Match, HighlightScope::Line] {
            assert_eq!(scope_at(scope_index(scope)), scope, "{scope:?}");
        }
        assert_eq!(scope_options().len(), 2);
    }

    #[test]
    fn a_rule_row_stays_inside_the_indices_the_list_was_given() {
        // Every control of every row, and the "Add rule" button above them all,
        // has to fall inside the span a host reserved — otherwise a long list
        // would tab into whatever the host drew next.
        let list = HighlightRuleList {
            rows: Vec::new(),
            tab_base: 1000,
        };
        for position in [0, 1, 7, (ADD / RULE_STRIDE) as usize, 10_000] {
            let base = list.row_tab_base(position);
            assert!(base >= 1000, "row {position} numbers below the base");
            assert!(
                base + RULE_STRIDE - 1 < 1000 + ADD,
                "row {position} reaches the Add button"
            );
        }
        const { assert!(ADD < TAB_SPAN) };
    }

    #[test]
    fn every_word_the_rule_list_asks_for_has_a_translation() {
        // The list looks its words up by key as it draws, so a key that is not
        // in `locales/*.yml` reaches the screen as the key path itself and
        // nothing else would notice.
        for key in [
            "settings.highlights.pattern",
            "settings.highlights.scope_match",
            "settings.highlights.scope_line",
            "settings.highlights.foreground",
            "settings.highlights.background",
            "settings.highlights.bold",
            "settings.highlights.ignore_case",
            "settings.highlights.enabled",
            "settings.highlights.add",
            "settings.highlights.remove",
        ] {
            let label = ts!(key);
            assert!(!label.is_empty(), "{key} is empty");
            assert!(
                !label.contains("settings."),
                "untranslated {key}: {label:?}"
            );
        }
    }
}
